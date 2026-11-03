use serde_json::json;
use tower_lsp_server::ls_types::{
    Command, CompletionItem, CompletionItemKind, CompletionTextEdit, Documentation, Location,
    Position, Range, TextEdit, Uri,
};

use crate::package_json::{CATALOG_PREFIX, Reference};
use crate::workspace::{CatalogName, Resolved};

pub const GO_TO_CATALOG_COMMAND: &str = "catalog-lens.goToCatalog";
const GO_TO_LOCATIONS_COMMAND: &str = "editor.action.goToLocations";

pub fn hint_label(resolved: &Resolved, named_catalog_label: bool) -> String {
    let version = shown_version(&resolved.version);
    match &resolved.catalog {
        CatalogName::Named(name) if named_catalog_label => format!("{name} → {version}"),
        _ => version.to_owned(),
    }
}

fn shown_version(version: &str) -> &str {
    if version.is_empty() { "\"\"" } else { version }
}

pub fn hint_tooltip(resolved: &Resolved) -> String {
    format!("catalog `{}` · {}", resolved.catalog, resolved.file)
}

pub fn lens_title(reference: &Reference, resolved: &Resolved) -> String {
    format!("\"{}\": \"{}\"", reference.key, resolved.version)
}

pub fn hover_markdown(reference: &Reference, resolved: &Resolved) -> String {
    format!(
        "**{}** · catalog `{}`\n\nVersion: `{}`\n\nDefined in: `{}:{}`",
        reference.package,
        resolved.catalog,
        shown_version(&resolved.version),
        resolved.file,
        resolved.location.range.start.line + 1
    )
}

fn catalog_phrase(catalog: &CatalogName) -> String {
    match catalog {
        CatalogName::Default => "the default catalog".to_owned(),
        CatalogName::Named(name) => format!("catalog \"{name}\""),
    }
}

pub fn missing_entry_message(package: &str, catalog: &CatalogName, file: &str) -> String {
    format!(
        "\"{package}\" is not defined in {} ({file})",
        catalog_phrase(catalog)
    )
}

pub fn invalid_entry_message(
    package: &str,
    catalog: &CatalogName,
    protocol: &str,
    file: &str,
    manager: &str,
) -> String {
    format!(
        "\"{package}\" in {} uses the {protocol}: protocol ({file}); {manager} rejects this",
        catalog_phrase(catalog)
    )
}

pub fn missing_catalog_message(name: &str, file: &str) -> String {
    format!("Catalog \"{name}\" is not defined in {file}")
}

pub fn stale_workspace_message(file: &str) -> String {
    format!("{file} does not parse; showing its last good catalogs")
}

pub fn unavailable_workspace_message(file: &str) -> String {
    format!("{file} does not parse; no catalogs available")
}

pub fn both_defaults_message(manager: &str, file: &str) -> String {
    format!("`catalog` and `catalogs.default` are both defined in {file}; {manager} rejects this")
}

pub fn ambiguous_message(package: &str, file: &str) -> String {
    format!(
        "\"{package}\" is defined in both `catalog` and `catalogs.default` in {file}; package managers reject this"
    )
}

pub const NO_WORKSPACE_MESSAGE: &str = "No catalog definitions found: expected pnpm-workspace.yaml, .yarnrc.yml, or a package.json with a `catalog` field in a parent directory";

pub fn completion_items(
    candidates: impl IntoIterator<Item = (CatalogName, String)>,
    file: &str,
    replace: Range,
) -> Vec<CompletionItem> {
    candidates
        .into_iter()
        .map(|(name, version)| {
            let label = match &name {
                CatalogName::Default => CATALOG_PREFIX.to_owned(),
                CatalogName::Named(name) => format!("{CATALOG_PREFIX}{name}"),
            };
            CompletionItem {
                label: label.clone(),
                kind: Some(CompletionItemKind::VALUE),
                detail: Some(version),
                documentation: Some(Documentation::String(file.to_owned())),
                filter_text: Some(label.clone()),
                text_edit: Some(CompletionTextEdit::Edit(TextEdit::new(replace, label))),
                ..CompletionItem::default()
            }
        })
        .collect()
}

pub fn navigation_command(
    show_document_supported: bool,
    source: &Uri,
    at: Position,
    target: &Location,
) -> Command {
    if show_document_supported {
        Command::new(
            String::new(),
            GO_TO_CATALOG_COMMAND.to_owned(),
            Some(vec![json!(target)]),
        )
    } else {
        Command::new(
            String::new(),
            GO_TO_LOCATIONS_COMMAND.to_owned(),
            Some(vec![json!(source), json!(at), json!([target])]),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn resolved(catalog: CatalogName) -> Resolved {
        Resolved {
            version: "^18.3.1".into(),
            location: Location::new(
                Uri::from_str("file:///ws/pnpm-workspace.yaml").unwrap(),
                Range::new(Position::new(12, 4), Position::new(12, 11)),
            ),
            catalog,
            file: "pnpm-workspace.yaml",
            ambiguous: false,
        }
    }

    fn reference(key: &str, package: &str) -> Reference {
        Reference {
            key: key.into(),
            package: package.into(),
            raw_catalog: String::new(),
            range: Range::default(),
            entry: Range::default(),
        }
    }

    #[test]
    fn hint_label_names_the_catalog_only_when_asked_and_named() {
        let named = resolved(CatalogName::Named("react18".into()));
        assert_eq!(hint_label(&named, false), "^18.3.1");
        assert_eq!(hint_label(&named, true), "react18 → ^18.3.1");
        assert_eq!(hint_label(&resolved(CatalogName::Default), true), "^18.3.1");
        assert_eq!(
            hint_tooltip(&named),
            "catalog `react18` · pnpm-workspace.yaml"
        );
    }

    #[test]
    fn an_empty_version_shows_as_an_empty_string_literal() {
        let mut named = resolved(CatalogName::Named("react18".into()));
        named.version = String::new();
        assert_eq!(hint_label(&named, false), "\"\"");
        assert_eq!(hint_label(&named, true), "react18 → \"\"");
        assert_eq!(
            hover_markdown(&reference("react", "react"), &named),
            "**react** · catalog `react18`\n\nVersion: `\"\"`\n\nDefined in: `pnpm-workspace.yaml:13`"
        );
        assert_eq!(
            lens_title(&reference("react", "react"), &named),
            "\"react\": \"\""
        );
    }

    #[test]
    fn lens_title_mirrors_the_line_with_the_raw_key() {
        let named = resolved(CatalogName::Named("react18".into()));
        assert_eq!(
            lens_title(&reference("foo>react-dom@^17", "react-dom"), &named),
            "\"foo>react-dom@^17\": \"^18.3.1\""
        );
    }

    #[test]
    fn hover_markdown_matches_the_spec_layout() {
        let named = resolved(CatalogName::Named("react18".into()));
        assert_eq!(
            hover_markdown(&reference("react-dom", "react-dom"), &named),
            "**react-dom** · catalog `react18`\n\nVersion: `^18.3.1`\n\nDefined in: `pnpm-workspace.yaml:13`"
        );
    }

    #[test]
    fn diagnostic_messages_are_verbatim() {
        assert_eq!(
            missing_entry_message("foo", &CatalogName::Default, "pnpm-workspace.yaml"),
            "\"foo\" is not defined in the default catalog (pnpm-workspace.yaml)"
        );
        assert_eq!(
            missing_entry_message(
                "foo",
                &CatalogName::Named("react18".into()),
                "pnpm-workspace.yaml"
            ),
            "\"foo\" is not defined in catalog \"react18\" (pnpm-workspace.yaml)"
        );
        assert_eq!(
            missing_catalog_message("react17", "pnpm-workspace.yaml"),
            "Catalog \"react17\" is not defined in pnpm-workspace.yaml"
        );
        assert_eq!(
            ambiguous_message("react", "pnpm-workspace.yaml"),
            "\"react\" is defined in both `catalog` and `catalogs.default` in pnpm-workspace.yaml; package managers reject this"
        );
    }

    #[test]
    fn completion_labels_name_the_catalog_and_replace_the_value() {
        let items = completion_items(
            [
                (CatalogName::Default, "^1.0.0".to_owned()),
                (CatalogName::Named("react18".into()), "^18.3.1".to_owned()),
            ],
            ".yarnrc.yml",
            Range::default(),
        );
        let labels: Vec<&str> = items.iter().map(|item| item.label.as_str()).collect();
        assert_eq!(labels, vec!["catalog:", "catalog:react18"]);
        assert_eq!(items[1].detail.as_deref(), Some("^18.3.1"));
        assert_eq!(
            items[0].documentation,
            Some(Documentation::String(".yarnrc.yml".into()))
        );
        assert_eq!(
            items[1].text_edit,
            Some(CompletionTextEdit::Edit(TextEdit::new(
                Range::default(),
                "catalog:react18".into()
            )))
        );
    }

    #[test]
    fn navigation_command_depends_on_show_document_support() {
        let source = Uri::from_str("file:///ws/package.json").unwrap();
        let target = resolved(CatalogName::Default).location;
        let direct = navigation_command(true, &source, Position::new(1, 2), &target);
        assert_eq!(direct.command, GO_TO_CATALOG_COMMAND);
        assert_eq!(direct.arguments, Some(vec![json!(target)]));
        let fallback = navigation_command(false, &source, Position::new(1, 2), &target);
        assert_eq!(fallback.command, GO_TO_LOCATIONS_COMMAND);
        assert_eq!(
            fallback.arguments,
            Some(vec![
                json!(source),
                json!(Position::new(1, 2)),
                json!([target])
            ])
        );
    }
}
