//! Convert semantic diagnostics to LSP Diagnostic.
//!
//! Uses [`leekscript_rs::to_lsp_diagnostic`] (requires leekscript-rs `lsp` feature).

use leekscript_rs::LineIndex;
use tower_lsp::lsp_types::Diagnostic;

/// Convert one semantic diagnostic to LSP Diagnostic (with range in line:character).
#[inline]
pub fn semantic_to_lsp(
    d: &leekscript_rs::SemanticDiagnostic,
    source: &str,
    line_index: &LineIndex,
) -> Diagnostic {
    leekscript_rs::to_lsp_diagnostic(d, source, line_index)
}
