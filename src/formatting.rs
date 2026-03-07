//! Document formatting (full-doc and range).

use std::collections::HashMap;

use leekscript_rs::formatter::FormatterOptions;
use leekscript_rs::format;
use tower_lsp::lsp_types::*;

use crate::document::DocumentState;

/// Format the given range by formatting the whole document and replacing the range with the corresponding slice of formatted output.
pub fn format_range_at(
    docs: &HashMap<String, DocumentState>,
    uri: &str,
    range: &Range,
    options: &FormatterOptions,
) -> Option<Vec<TextEdit>> {
    let state = docs.get(uri)?;
    let root = state.root.as_ref()?;
    let formatted = format(root, options);
    let lines: Vec<&str> = formatted.lines().collect();
    let start_line = range.start.line as usize;
    let end_line = range.end.line as usize;
    if end_line < start_line || start_line >= lines.len() {
        return Some(vec![]);
    }
    let end_line = end_line.min(lines.len().saturating_sub(1));
    let new_text = lines[start_line..=end_line].join("\n");
    Some(vec![TextEdit {
        range: range.clone(),
        new_text,
    }])
}
