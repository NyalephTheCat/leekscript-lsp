//! LSP configuration: settings and application from workspace/initialization options.

#[derive(Debug, Clone)]
pub struct LspSettings {
    /// Load embedded stdlib .sig files (constants + functions). Default true.
    pub load_stdlib_signatures: bool,
    /// Additional .sig file paths (resolved by the client / workspace).
    pub signature_files: Vec<String>,
    /// Show inlay hints for variable types. Default true.
    pub inlay_hints_enabled: bool,
    /// Show inlay hints at the end of scopes (e.g. "// end Cell"). Default true.
    pub inlay_hints_scope_end: bool,
    /// When true, send verbose (LOG) messages for each request. Default false.
    pub trace: bool,
}

impl Default for LspSettings {
    fn default() -> Self {
        Self {
            load_stdlib_signatures: true,
            signature_files: Vec::new(),
            inlay_hints_enabled: true,
            inlay_hints_scope_end: true,
            trace: false,
        }
    }
}

/// Apply JSON config object (e.g. from workspace config or initialization options) to settings.
pub fn apply_config_from_value(settings: &mut LspSettings, value: &serde_json::Value) {
    if let Some(b) = value.get("loadStdlibSignatures").and_then(|v| v.as_bool()) {
        settings.load_stdlib_signatures = b;
    }
    if let Some(arr) = value.get("signatureFiles").and_then(|v| v.as_array()) {
        settings.signature_files = arr
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect();
    }
    if let Some(b) = value.get("inlayHints").and_then(|v| v.get("enabled")).and_then(|v| v.as_bool()) {
        settings.inlay_hints_enabled = b;
    }
    if let Some(b) = value.get("inlayHints").and_then(|v| v.get("scopeEnd")).and_then(|v| v.as_bool()) {
        settings.inlay_hints_scope_end = b;
    }
    if let Some(b) = value.get("trace").and_then(|v| v.as_bool()) {
        settings.trace = b;
    }
}
