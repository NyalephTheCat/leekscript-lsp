//! LSP configuration: settings and application from workspace/initialization options.

#[derive(Debug, Clone, Default)]
pub struct LspSettings {
    /// When true, send verbose (LOG) messages for each request. Default false.
    pub trace: bool,
}

/// Apply JSON config object (e.g. from workspace config or initialization options) to settings.
pub fn apply_config_from_value(settings: &mut LspSettings, value: &serde_json::Value) {
    if let Some(b) = value.get("trace").and_then(serde_json::Value::as_bool) {
        settings.trace = b;
    }
}
