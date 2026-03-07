//! URI/path and position conversion utilities for LSP.

use std::path::PathBuf;

use leekscript_rs::formatter::{FormatterOptions, IndentStyle};
use leekscript_rs::{LineIndex, TextEdit as SiphaTextEdit};
use tower_lsp::lsp_types::{FormattingOptions, Position, Range, TextDocumentContentChangeEvent, Url};

/// Map LSP formatting options to the LeekScript formatter options (indent style: tabs vs spaces).
#[must_use]
pub fn formatter_options_from_lsp(lsp: &FormattingOptions) -> FormatterOptions {
    let indent_style = if lsp.insert_spaces {
        IndentStyle::Spaces(lsp.tab_size)
    } else {
        IndentStyle::Tabs
    };
    FormatterOptions {
        indent_style,
        ..FormatterOptions::default()
    }
}

pub use leekscript_rs::{line_col_utf16_to_byte, line_prefix_utf16};

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

/// Convert a file path to a file:// URL for LSP.
pub fn path_to_uri(path: &std::path::Path) -> Option<Url> {
    Url::from_file_path(path).ok()
}

/// Canonicalize path for comparison; falls back to the given path if canonicalization fails.
pub fn canonical_path(path: &std::path::Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// Convert byte offset to LSP Position using line index (UTF-16).
pub fn span_to_position(source: &str, line_index: &LineIndex, byte_offset: u32) -> Position {
    let (line_0, col_utf16) = leekscript_rs::byte_offset_to_line_col_utf16(source, line_index, byte_offset);
    Position {
        line: line_0,
        character: col_utf16,
    }
}

/// Convert one LSP content change (with range) to sipha's TextEdit (byte offsets).
pub fn lsp_change_to_sipha_edit(
    source: &str,
    line_index: &LineIndex,
    change: &TextDocumentContentChangeEvent,
) -> Option<SiphaTextEdit> {
    let range = change.range.as_ref()?;
    let start = line_col_utf16_to_byte(
        source,
        line_index,
        range.start.line,
        range.start.character,
    )?;
    let end =
        line_col_utf16_to_byte(source, line_index, range.end.line, range.end.character)?;
    Some(SiphaTextEdit {
        start,
        end,
        new_text: change.text.as_bytes().to_vec(),
    })
}

/// Return the source text in the given LSP range (UTF-16 line/character). Returns empty string if conversion fails.
#[must_use]
pub fn source_text_in_range(source: &str, line_index: &LineIndex, range: &Range) -> String {
    let start = match line_col_utf16_to_byte(
        source,
        line_index,
        range.start.line,
        range.start.character,
    ) {
        Some(s) => s as usize,
        None => return String::new(),
    };
    let end = match line_col_utf16_to_byte(source, line_index, range.end.line, range.end.character) {
        Some(e) => e as usize,
        None => return String::new(),
    };
    if start <= end && end <= source.len() {
        source[start..end].to_string()
    } else {
        String::new()
    }
}

/// Apply LSP content changes to the current document. Returns the new source and, when
/// exactly one range-based edit was applied, the corresponding sipha TextEdit for reparse.
pub fn apply_content_changes(
    state: &crate::document::DocumentState,
    content_changes: Vec<TextDocumentContentChangeEvent>,
) -> (String, Option<SiphaTextEdit>) {
    if content_changes.is_empty() {
        return (state.source.clone(), None);
    }
    if content_changes.iter().any(|c| c.range.is_none()) {
        let new_source = content_changes
            .into_iter()
            .find(|c| c.range.is_none())
            .map(|c| c.text)
            .unwrap_or_else(|| state.source.clone());
        return (new_source, None);
    }
    let mut edits: Vec<(u32, u32, String)> = Vec::with_capacity(content_changes.len());
    for change in &content_changes {
        let range = match &change.range {
            Some(r) => r,
            None => continue,
        };
        let start = match line_col_utf16_to_byte(
            &state.source,
            &state.line_index,
            range.start.line,
            range.start.character,
        ) {
            Some(s) => s,
            None => continue,
        };
        let end = match line_col_utf16_to_byte(
            &state.source,
            &state.line_index,
            range.end.line,
            range.end.character,
        ) {
            Some(e) => e,
            None => continue,
        };
        edits.push((start, end, change.text.clone()));
    }
    if edits.is_empty() {
        return (state.source.clone(), None);
    }
    edits.sort_by(|a, b| b.0.cmp(&a.0));
    let mut new_source = state.source.clone();
    for (start, end, text) in edits {
        let start = start as usize;
        let end = end as usize;
        if end <= new_source.len() && start <= end {
            new_source.replace_range(start..end, &text);
        }
    }
    let single_sipha_edit = if content_changes.len() == 1 {
        lsp_change_to_sipha_edit(&state.source, &state.line_index, &content_changes[0])
    } else {
        None
    };
    (new_source, single_sipha_edit)
}
