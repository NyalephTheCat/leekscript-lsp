//! URI/path and position conversion utilities for LSP.

use std::path::{Path, PathBuf};

use tower_lsp::lsp_types::Url;

pub use leekscript_rs::line_col_utf16_to_byte;

/// Parse a string as an LSP URL. Returns None if the string is not a valid URI.
#[must_use]
pub fn parse_uri(uri: &str) -> Option<Url> {
    Url::parse(uri).ok()
}

/// If the URI is a file URI, return its path; otherwise None.
pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let url = parse_uri(uri)?;
    if url.scheme() != "file" {
        return None;
    }
    url.to_file_path().ok()
}

/// Convert a file path to an LSP document URI. Returns None if the path cannot be represented as a file URL.
#[allow(dead_code)]
#[must_use]
pub fn path_to_uri(path: &Path) -> Option<Url> {
    Url::from_file_path(path).ok()
}

/// Apply LSP content changes to the current document. Returns the new source and, when
/// exactly one range-based edit was applied, the corresponding sipha `TextEdit` for reparse.
///
/// Delegates to [`leekscript_rs::apply_content_changes`].
pub fn apply_content_changes(
    state: &crate::document::DocumentState,
    content_changes: Vec<tower_lsp::lsp_types::TextDocumentContentChangeEvent>,
) -> (String, Option<leekscript_rs::TextEdit>) {
    leekscript_rs::apply_content_changes(state, content_changes)
}
