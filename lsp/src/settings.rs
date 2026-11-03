use serde::Deserialize;
use serde_json::{Map, Value};
use serde_with::{DefaultOnError, serde_as};

use crate::display::DisplayMode;

#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub display: DisplayMode,
    pub named_catalog_label: bool,
    pub hover: bool,
    pub diagnostics: bool,
    pub document_colors: bool,
    pub color_alpha: f32,
    pub completion: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            display: DisplayMode::Auto,
            named_catalog_label: false,
            hover: true,
            diagnostics: true,
            document_colors: true,
            color_alpha: 0.6,
            completion: true,
        }
    }
}

/// Every field is an `Option` because `DefaultOnError` falls back to the
/// type's own default, and `hover` and friends default to `true`.
#[serde_as]
#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct RawSettings {
    #[serde_as(as = "DefaultOnError")]
    display: Option<DisplayMode>,
    #[serde_as(as = "DefaultOnError")]
    named_catalog_label: Option<bool>,
    #[serde_as(as = "DefaultOnError")]
    hover: Option<bool>,
    #[serde_as(as = "DefaultOnError")]
    diagnostics: Option<bool>,
    #[serde_as(as = "DefaultOnError")]
    document_colors: Option<bool>,
    #[serde_as(as = "DefaultOnError")]
    color_alpha: Option<f32>,
    #[serde_as(as = "DefaultOnError")]
    completion: Option<bool>,
    #[serde(flatten)]
    unknown: Map<String, Value>,
}

impl Settings {
    pub fn parse(value: &Value) -> Self {
        let Ok(raw) = RawSettings::deserialize(value) else {
            return Settings::default();
        };
        for key in raw.unknown.keys() {
            tracing::debug!(key, "unknown setting");
        }
        let defaults = Settings::default();
        Settings {
            display: raw.display.unwrap_or(defaults.display),
            named_catalog_label: raw
                .named_catalog_label
                .unwrap_or(defaults.named_catalog_label),
            hover: raw.hover.unwrap_or(defaults.hover),
            diagnostics: raw.diagnostics.unwrap_or(defaults.diagnostics),
            document_colors: raw.document_colors.unwrap_or(defaults.document_colors),
            color_alpha: raw
                .color_alpha
                .map_or(defaults.color_alpha, |alpha| alpha.clamp(0.0, 1.0)),
            completion: raw.completion.unwrap_or(defaults.completion),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn empty_or_non_object_settings_give_defaults() {
        assert_eq!(Settings::parse(&json!({})), Settings::default());
        assert_eq!(Settings::parse(&json!(null)), Settings::default());
    }

    #[test]
    fn each_key_is_parsed_independently() {
        let settings = Settings::parse(&json!({
            "display": "codeLens",
            "namedCatalogLabel": "yes",
            "hover": false,
            "colorAlpha": 7,
            "completion": null,
            "surprise": 1
        }));
        assert_eq!(settings.display, DisplayMode::CodeLens);
        assert!(!settings.named_catalog_label);
        assert!(!settings.hover);
        assert_eq!(settings.color_alpha, 1.0);
        assert!(settings.completion);
    }

    #[test]
    fn unknown_display_falls_back_to_auto() {
        let settings = Settings::parse(&json!({ "display": "sideways" }));
        assert_eq!(settings.display, DisplayMode::Auto);
        let settings = Settings::parse(&json!({ "display": "both" }));
        assert_eq!(settings.display, DisplayMode::Both);
    }
}
