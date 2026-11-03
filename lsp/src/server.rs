use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::*;
use tower_lsp_server::{Client, LanguageServer};

use crate::diagnostics;
use crate::display::{
    self, DisplayMode, Effects, HintDecision, LENS_PROBE_GRACE, LensEvent, LensTracker, ProbeId,
};
use crate::documents::{Document, DocumentStore};
use crate::package_json::{self, CATALOG_PREFIX, Reference};
use crate::render;
use crate::settings::Settings;
use crate::watch::{self, Debouncer};
use crate::workspace::{Resolution, Workspace, WorkspaceCache};

const WATCH_DEBOUNCE: Duration = Duration::from_millis(100);

pub struct CatalogLens {
    client: Client,
    settings: RwLock<Settings>,
    show_document_supported: AtomicBool,
    documents: DocumentStore,
    workspaces: WorkspaceCache,
    lenses: tokio::sync::watch::Sender<LensTracker>,
    watch_debouncer: Debouncer,
}

/// `impl LanguageServer for Arc<CatalogLens>` would break the orphan rule,
/// so the trait lives on this wrapper and detached tasks clone the `Arc`.
pub struct Backend {
    lens: Arc<CatalogLens>,
}

impl Backend {
    pub fn new(client: Client) -> Self {
        Self {
            lens: Arc::new(CatalogLens {
                client,
                settings: RwLock::new(Settings::default()),
                show_document_supported: AtomicBool::new(false),
                documents: DocumentStore::default(),
                workspaces: WorkspaceCache::new(),
                lenses: tokio::sync::watch::Sender::new(LensTracker::default()),
                watch_debouncer: Debouncer::new(WATCH_DEBOUNCE),
            }),
        }
    }
}

impl CatalogLens {
    fn settings(&self) -> Settings {
        self.settings
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn show_document_supported(&self) -> bool {
        self.show_document_supported.load(Ordering::Relaxed)
    }

    fn workspace_for(&self, document: &Document) -> Option<Arc<Workspace>> {
        self.workspaces
            .workspace_for(&document.path, &|path| self.documents.buffer(path))
    }

    fn resolve_all(&self, document: &Document) -> Vec<(usize, Resolution)> {
        let Some(workspace) = self.workspace_for(document) else {
            return Vec::new();
        };
        document
            .references
            .iter()
            .enumerate()
            .map(|(i, reference)| (i, workspace.resolve(reference)))
            .collect()
    }

    fn resolve_at(
        &self,
        uri: &Uri,
        position: Position,
        span: fn(&Reference) -> Range,
    ) -> Option<(Reference, Resolution)> {
        let document = self.documents.get(uri)?;
        let reference = package_json::reference_at(&document.references, position, span)?.clone();
        let workspace = self.workspace_for(&document)?;
        let resolution = workspace.resolve(&reference);
        Some((reference, resolution))
    }

    async fn publish(&self, document: &Document) {
        let diagnostics = if self.settings().diagnostics {
            let workspace = self.workspace_for(document);
            diagnostics::diagnostics(&document.references, workspace.as_deref())
        } else {
            Vec::new()
        };
        self.client
            .publish_diagnostics(document.uri.clone(), diagnostics, Some(document.version))
            .await;
    }

    async fn publish_after_edit(&self, document: &Document) {
        if self.workspaces.is_definition(&document.path) {
            self.republish_all().await;
        } else {
            self.publish(document).await;
        }
    }

    /// Zed sends the whole settings object after `initialized` and again on
    /// every settings change, including changes to other servers' settings.
    async fn apply_settings(self: &Arc<Self>, settings: Settings) {
        let previous = std::mem::replace(
            &mut *self
                .settings
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
            settings.clone(),
        );
        if settings.display == DisplayMode::Auto {
            self.track(LensEvent::SettingsChanged);
        }
        if previous == settings {
            return;
        }
        tracing::info!(?settings, "settings changed");
        self.refresh(true, true);
        self.republish_all().await;
    }

    fn track(self: &Arc<Self>, event: LensEvent) {
        let mut effects = Effects::default();
        self.lenses.send_modify(|lenses| effects = lenses.on(event));
        if let Some(id) = effects.start_probe {
            self.probe(id);
        }
        self.refresh(effects.refresh_hints, false);
    }

    fn probe(self: &Arc<Self>, id: ProbeId) {
        let lens = self.clone();
        tokio::spawn(async move {
            lens.refresh(false, true);
            tokio::time::sleep(LENS_PROBE_GRACE).await;
            lens.track(LensEvent::ProbeExpired(id));
        });
    }

    fn refresh(&self, hints: bool, lenses: bool) {
        // Zed sends its first settings before any didOpen, when nothing of
        // ours is on screen yet.
        if self.documents.all().is_empty() {
            return;
        }
        if hints {
            let client = self.client.clone();
            tokio::spawn(async move {
                if let Err(error) = client.inlay_hint_refresh().await {
                    tracing::debug!(%error, "workspace/inlayHint/refresh failed");
                }
            });
        }
        if lenses {
            let client = self.client.clone();
            tokio::spawn(async move {
                if let Err(error) = client.code_lens_refresh().await {
                    tracing::debug!(%error, "workspace/codeLens/refresh failed");
                }
            });
        }
    }

    async fn republish_all(&self) {
        for document in self.documents.all() {
            self.publish(&document).await;
        }
    }

    async fn hint_decision(self: &Arc<Self>) -> HintDecision {
        let mode = self.settings().display;
        if mode == DisplayMode::Auto {
            self.track(LensEvent::HintsRequested);
        }
        let mut lenses = self.lenses.subscribe();
        let decision = lenses
            .wait_for(|lenses| display::hint_decision(mode, lenses) != HintDecision::Wait)
            .await
            .map(|lenses| display::hint_decision(mode, &lenses));
        decision.unwrap_or(HintDecision::Show)
    }

    fn inlay_hints(&self, document: &Document) -> Vec<InlayHint> {
        let settings = self.settings();
        self.resolve_all(document)
            .into_iter()
            .filter_map(|(i, resolution)| match resolution {
                Resolution::Resolved(resolved) => Some((i, resolved)),
                _ => None,
            })
            .map(|(i, resolved)| {
                let reference = &document.references[i];
                let part = InlayHintLabelPart {
                    value: render::hint_label(&resolved, settings.named_catalog_label),
                    tooltip: Some(InlayHintLabelPartTooltip::String(render::hint_tooltip(
                        &resolved,
                    ))),
                    location: Some(resolved.location.clone()),
                    command: Some(render::navigation_command(
                        self.show_document_supported(),
                        &document.uri,
                        reference.range.end,
                        &resolved.location,
                    )),
                };
                InlayHint {
                    position: reference.range.end,
                    label: InlayHintLabel::LabelParts(vec![part]),
                    kind: None,
                    text_edits: None,
                    tooltip: None,
                    padding_left: Some(true),
                    padding_right: None,
                    data: None,
                }
            })
            .collect()
    }

    fn code_lenses(&self, document: &Document) -> Vec<CodeLens> {
        self.resolve_all(document)
            .into_iter()
            .filter_map(|(i, resolution)| match resolution {
                Resolution::Resolved(resolved) => Some((i, resolved)),
                _ => None,
            })
            .map(|(i, resolved)| {
                let reference = &document.references[i];
                let mut command = render::navigation_command(
                    self.show_document_supported(),
                    &document.uri,
                    reference.range.start,
                    &resolved.location,
                );
                command.title = render::lens_title(reference, &resolved);
                CodeLens {
                    range: reference.entry,
                    command: Some(command),
                    data: None,
                }
            })
            .collect()
    }

    fn document_colors(&self, document: &Document) -> Vec<ColorInformation> {
        let alpha = self.settings().color_alpha;
        let Some(workspace) = self.workspace_for(document) else {
            return Vec::new();
        };
        let names = workspace.catalog_names();
        document
            .references
            .iter()
            .filter_map(|reference| {
                let name = workspace.manager.lookup(&reference.raw_catalog).name();
                let index = names.iter().position(|n| *n == name)?;
                Some(ColorInformation {
                    range: reference.range,
                    color: crate::color::catalog_color(index, names.len(), alpha),
                })
            })
            .collect()
    }

    fn completions(&self, uri: &Uri, position: Position) -> Vec<CompletionItem> {
        let Some(document) = self.documents.get(uri) else {
            return Vec::new();
        };
        let Some(slot) = package_json::value_slot_at(&document.text, position) else {
            return Vec::new();
        };
        let typed = slot.typed.as_str();
        let wants_catalog = !typed.is_empty()
            && (CATALOG_PREFIX.starts_with(typed) || typed.starts_with(CATALOG_PREFIX));
        if !wants_catalog {
            return Vec::new();
        }
        let Some(workspace) = self.workspace_for(&document) else {
            return Vec::new();
        };
        let candidates = workspace.catalog_names().into_iter().filter_map(|name| {
            let entry = workspace.entry(&name, &slot.package)?;
            Some((name, entry.version.clone()))
        });
        render::completion_items(candidates, workspace.file_name(), slot.content_range)
    }
}

fn package_json_path(uri: &Uri) -> Option<std::path::PathBuf> {
    let path = uri.to_file_path()?.into_owned();
    (path.file_name() == Some("package.json".as_ref())).then_some(path)
}

impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        let show_document = params
            .capabilities
            .window
            .and_then(|window| window.show_document)
            .is_some_and(|show| show.support);
        self.lens
            .show_document_supported
            .store(show_document, Ordering::Relaxed);
        Ok(InitializeResult {
            server_info: Some(ServerInfo {
                name: env!("CARGO_PKG_NAME").to_owned(),
                version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            }),
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                inlay_hint_provider: Some(OneOf::Left(true)),
                code_lens_provider: Some(CodeLensOptions {
                    resolve_provider: Some(false),
                }),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                definition_provider: Some(OneOf::Left(true)),
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec![":".to_owned(), "\"".to_owned()]),
                    ..CompletionOptions::default()
                }),
                color_provider: Some(ColorProviderCapability::Simple(true)),
                execute_command_provider: show_document.then(|| ExecuteCommandOptions {
                    commands: vec![render::GO_TO_CATALOG_COMMAND.to_owned()],
                    ..ExecuteCommandOptions::default()
                }),
                ..ServerCapabilities::default()
            },
            offset_encoding: None,
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        if let Err(error) = self
            .lens
            .client
            .register_capability(vec![watch::registration()])
            .await
        {
            tracing::warn!(%error, "client refused file watching; discovery cache disabled");
            self.lens.workspaces.disable_discovery_cache();
        }
    }

    async fn did_change_configuration(&self, params: DidChangeConfigurationParams) {
        self.lens
            .apply_settings(Settings::parse(&params.settings))
            .await;
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let TextDocumentItem {
            uri, version, text, ..
        } = params.text_document;
        let Some(path) = package_json_path(&uri) else {
            return;
        };
        if let Some(document) = self
            .lens
            .documents
            .upsert_if_newer(uri, path, version, text)
        {
            self.lens.publish_after_edit(&document).await;
        }
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri;
        let Some(path) = package_json_path(&uri) else {
            return;
        };
        let Some(change) = params.content_changes.into_iter().last() else {
            return;
        };
        let version = params.text_document.version;
        let Some(document) = self
            .lens
            .documents
            .upsert_if_newer(uri, path, version, change.text)
        else {
            return;
        };
        self.lens.publish_after_edit(&document).await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri;
        let Some(closed) = self.lens.documents.remove(&uri) else {
            return;
        };
        self.lens
            .client
            .publish_diagnostics(uri, Vec::new(), None)
            .await;
        if self.lens.workspaces.is_definition(&closed.path) {
            self.lens.republish_all().await;
        }
    }

    async fn did_change_watched_files(&self, params: DidChangeWatchedFilesParams) {
        let open: Vec<_> = self
            .lens
            .documents
            .all()
            .into_iter()
            .map(|doc| doc.path)
            .collect();
        let mut relevant = false;
        for change in params.changes {
            let Some(path) = change.uri.to_file_path() else {
                continue;
            };
            if !watch::is_relevant(&path, open.iter().map(|p| p.as_path())) {
                continue;
            }
            relevant = true;
            if let Some(dir) = path.parent() {
                self.lens.workspaces.forget_discovery(dir);
            }
        }
        if relevant {
            let lens = self.lens.clone();
            self.lens
                .watch_debouncer
                .schedule(async move { lens.republish_all().await });
        }
    }

    async fn inlay_hint(&self, params: InlayHintParams) -> Result<Option<Vec<InlayHint>>> {
        let Some(document) = self.lens.documents.get(&params.text_document.uri) else {
            return Ok(None);
        };
        match self.lens.hint_decision().await {
            HintDecision::Show => Ok(Some(self.lens.inlay_hints(&document))),
            HintDecision::Hide | HintDecision::Wait => Ok(Some(Vec::new())),
        }
    }

    async fn code_lens(&self, params: CodeLensParams) -> Result<Option<Vec<CodeLens>>> {
        let mode = self.lens.settings().display;
        if mode == DisplayMode::Inlay {
            return Ok(Some(Vec::new()));
        }
        let Some(document) = self.lens.documents.get(&params.text_document.uri) else {
            return Ok(None);
        };
        if mode == DisplayMode::Auto {
            self.lens.track(LensEvent::LensRequested);
        }
        Ok(Some(self.lens.code_lenses(&document)))
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        if !self.lens.settings().hover {
            return Ok(None);
        }
        let position = params.text_document_position_params;
        let Some((reference, Resolution::Resolved(resolved))) = self.lens.resolve_at(
            &position.text_document.uri,
            position.position,
            |reference| reference.entry,
        ) else {
            return Ok(None);
        };
        Ok(Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: render::hover_markdown(&reference, &resolved),
            }),
            range: Some(reference.entry),
        }))
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let position = params.text_document_position_params;
        let Some((_, Resolution::Resolved(resolved))) = self.lens.resolve_at(
            &position.text_document.uri,
            position.position,
            |reference| reference.range,
        ) else {
            return Ok(None);
        };
        Ok(Some(GotoDefinitionResponse::Scalar(resolved.location)))
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        if !self.lens.settings().completion {
            return Ok(None);
        }
        let position = params.text_document_position;
        let items = self
            .lens
            .completions(&position.text_document.uri, position.position);
        Ok(Some(CompletionResponse::Array(items)))
    }

    async fn document_color(&self, params: DocumentColorParams) -> Result<Vec<ColorInformation>> {
        if !self.lens.settings().document_colors {
            return Ok(Vec::new());
        }
        let Some(document) = self.lens.documents.get(&params.text_document.uri) else {
            return Ok(Vec::new());
        };
        Ok(self.lens.document_colors(&document))
    }

    async fn color_presentation(
        &self,
        _: ColorPresentationParams,
    ) -> Result<Vec<ColorPresentation>> {
        Ok(Vec::new())
    }

    async fn execute_command(&self, params: ExecuteCommandParams) -> Result<Option<LSPAny>> {
        if params.command != render::GO_TO_CATALOG_COMMAND {
            return Err(tower_lsp_server::jsonrpc::Error::method_not_found());
        }
        let location: Location = params
            .arguments
            .into_iter()
            .next()
            .and_then(|value| serde_json::from_value(value).ok())
            .ok_or_else(|| {
                tower_lsp_server::jsonrpc::Error::invalid_params("expected a Location")
            })?;
        let shown = self
            .lens
            .client
            .show_document(ShowDocumentParams {
                uri: location.uri,
                external: None,
                take_focus: Some(true),
                selection: Some(location.range),
            })
            .await?;
        Ok(Some(LSPAny::Bool(shown)))
    }
}
