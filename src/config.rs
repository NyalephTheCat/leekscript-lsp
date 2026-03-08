//! LSP configuration: settings and application from workspace/initialization options.

#[derive(Debug, Clone)]
pub struct LspSettings {
    /// When true, send verbose (LOG) messages for each request. Default false.
    pub trace: bool,
    /// Paths to .sig files or directories of .sig files (from leekscript.signatureFiles).
    /// When set by the client, the LSP loads signatures from these paths at initialize.
    pub signature_paths: Option<Vec<String>>,
    /// When true and no `signature_paths` provided, try default env/dir (`LEEKSCRIPT_SIGNATURES_DIR` or examples/signatures).
    pub load_stdlib_signatures: bool,
}

impl Default for LspSettings {
    fn default() -> Self {
        Self {
            trace: false,
            signature_paths: None,
            load_stdlib_signatures: true,
        }
    }
}

/// Apply JSON config object (e.g. from workspace config or initialization options) to settings.
pub fn apply_config_from_value(settings: &mut LspSettings, value: &serde_json::Value) {
    if let Some(b) = value.get("trace").and_then(serde_json::Value::as_bool) {
        settings.trace = b;
    }
    if let Some(arr) = value
        .get("signatureFiles")
        .and_then(serde_json::Value::as_array)
    {
        let paths: Vec<String> = arr
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect();
        settings.signature_paths = Some(paths);
    }
    if let Some(b) = value
        .get("loadStdlibSignatures")
        .and_then(serde_json::Value::as_bool)
    {
        settings.load_stdlib_signatures = b;
    }
}
