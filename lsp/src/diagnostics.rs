use tower_lsp_server::ls_types::{Diagnostic, DiagnosticSeverity, Range};

use crate::package_json::Reference;
use crate::render;
use crate::workspace::{CatalogState, Resolution, Source, Workspace};

const SOURCE: &str = "catalog-lens";

pub fn diagnostics(references: &[Reference], workspace: Option<&Workspace>) -> Vec<Diagnostic> {
    let Some(first) = references.first() else {
        return Vec::new();
    };
    let Some(workspace) = workspace else {
        return vec![warning(
            first.range,
            render::NO_WORKSPACE_MESSAGE.to_owned(),
        )];
    };
    let mut out = Vec::new();
    match (&workspace.state, workspace.source) {
        (CatalogState::Unavailable, _) => {
            return vec![warning(
                first.range,
                render::unavailable_workspace_message(workspace.file_name()),
            )];
        }
        (CatalogState::Stale(_), Source::Disk { .. }) => out.push(warning(
            first.range,
            render::stale_workspace_message(workspace.file_name()),
        )),
        _ => {}
    }
    if workspace.rejects_both_defaults() {
        out.push(warning(
            first.range,
            render::both_defaults_message(workspace.manager.name(), workspace.file_name()),
        ));
    }
    for reference in references {
        let message = match workspace.resolve(reference) {
            Resolution::Resolved(resolved) if resolved.ambiguous => {
                render::ambiguous_message(&reference.package, resolved.file)
            }
            Resolution::Resolved(_) | Resolution::NoCatalogs => continue,
            Resolution::MissingEntry { catalog, file } => {
                render::missing_entry_message(&reference.package, &catalog, file)
            }
            Resolution::MissingCatalog { name, file } => {
                render::missing_catalog_message(&name, file)
            }
            Resolution::InvalidEntry {
                catalog,
                file,
                rejection,
            } => render::invalid_entry_message(
                &reference.package,
                &catalog,
                rejection.protocol(),
                file,
                workspace.manager.name(),
            ),
        };
        out.push(warning(reference.range, message));
    }
    out
}

fn warning(range: Range, message: String) -> Diagnostic {
    Diagnostic {
        range,
        severity: Some(DiagnosticSeverity::WARNING),
        source: Some(SOURCE.to_owned()),
        message,
        ..Diagnostic::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::package_json::references;
    use crate::workspace::{CatalogState, Catalogs, Manager};
    use std::str::FromStr;
    use std::time::SystemTime;
    use tower_lsp_server::ls_types::Uri;

    fn workspace(state: CatalogState, source: Source) -> Workspace {
        Workspace {
            manager: Manager::Pnpm,
            uri: Uri::from_str("file:///ws/pnpm-workspace.yaml").unwrap(),
            source,
            state,
        }
    }

    fn messages(diagnostics: &[Diagnostic]) -> Vec<&str> {
        diagnostics.iter().map(|d| d.message.as_str()).collect()
    }

    const TEXT: &str = r#"{ "dependencies": { "react": "catalog:", "vue": "catalog:" } }"#;

    #[test]
    fn no_workspace_yields_one_warning_at_the_first_reference() {
        let refs = references(TEXT);
        let out = diagnostics(&refs, None);
        assert_eq!(messages(&out), vec![render::NO_WORKSPACE_MESSAGE]);
        assert_eq!(out[0].range, refs[0].range);
        assert_eq!(out[0].severity, Some(DiagnosticSeverity::WARNING));
        assert!(diagnostics(&[], None).is_empty());
    }

    #[test]
    fn broken_workspace_reports_by_state_and_source() {
        let refs = references(TEXT);
        let disk = Source::Disk {
            mtime: SystemTime::UNIX_EPOCH,
            len: 1,
        };
        let unavailable = workspace(CatalogState::Unavailable, disk);
        assert_eq!(
            messages(&diagnostics(&refs, Some(&unavailable))),
            vec!["pnpm-workspace.yaml does not parse; no catalogs available"]
        );
        let stale_disk = workspace(CatalogState::Stale(Catalogs::default()), disk);
        assert_eq!(
            messages(&diagnostics(&refs, Some(&stale_disk))),
            vec![
                "pnpm-workspace.yaml does not parse; showing its last good catalogs",
                "\"react\" is not defined in the default catalog (pnpm-workspace.yaml)",
                "\"vue\" is not defined in the default catalog (pnpm-workspace.yaml)",
            ]
        );
        let stale_buffer = workspace(
            CatalogState::Stale(Catalogs::default()),
            Source::Buffer { version: 1 },
        );
        assert_eq!(diagnostics(&refs, Some(&stale_buffer)).len(), 2);
    }
}
