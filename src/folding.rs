//! Folding and selection range providers.

use std::collections::HashMap;

use leekscript_rs::analysis::{class_decl_info, function_decl_info};
use leekscript_rs::syntax::Kind;
use sipha::types::IntoSyntaxKind;
use tower_lsp::lsp_types::*;

use crate::document::DocumentState;
use crate::util::{line_col_utf16_to_byte, span_to_position};

/// Compute folding ranges for the document.
pub fn folding_ranges_at(
    docs: &HashMap<String, DocumentState>,
    uri: &str,
) -> Option<Vec<FoldingRange>> {
    let state = docs.get(uri)?;
    let source = state.source.as_str();
    let line_index = &state.line_index;
    let root = state.root.as_ref()?;

    let mut ranges = Vec::new();
    for node in root.find_all_nodes(Kind::NodeClassDecl.into_syntax_kind()) {
        let tr = node.text_range();
        let start_pos = span_to_position(source, line_index, tr.start);
        let end_pos = span_to_position(source, line_index, tr.end);
        let collapsed_text = class_decl_info(&node).map(|info| format!("class {}", info.name));
        ranges.push(FoldingRange {
            start_line: start_pos.line,
            start_character: Some(start_pos.character),
            end_line: end_pos.line,
            end_character: Some(end_pos.character),
            kind: None,
            collapsed_text,
        });
    }
    for node in root.find_all_nodes(Kind::NodeFunctionDecl.into_syntax_kind()) {
        let tr = node.text_range();
        let start_pos = span_to_position(source, line_index, tr.start);
        let end_pos = span_to_position(source, line_index, tr.end);
        let collapsed_text = function_decl_info(&node).map(|info| format!("function {}()", info.name));
        ranges.push(FoldingRange {
            start_line: start_pos.line,
            start_character: Some(start_pos.character),
            end_line: end_pos.line,
            end_character: Some(end_pos.character),
            kind: None,
            collapsed_text,
        });
    }
    for node in root.find_all_nodes(Kind::NodeBlock.into_syntax_kind()) {
        let tr = node.text_range();
        let start_pos = span_to_position(source, line_index, tr.start);
        let end_pos = span_to_position(source, line_index, tr.end);
        let collapsed_text = source
            .lines()
            .nth(start_pos.line as usize)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .or_else(|| Some("{ ... }".to_string()));
        ranges.push(FoldingRange {
            start_line: start_pos.line,
            start_character: Some(start_pos.character),
            end_line: end_pos.line,
            end_character: Some(end_pos.character),
            kind: None,
            collapsed_text,
        });
    }
    Some(ranges)
}

/// Compute selection ranges for the given positions.
pub fn selection_ranges_at(
    docs: &HashMap<String, DocumentState>,
    uri: &str,
    positions: &[Position],
) -> Option<Vec<SelectionRange>> {
    let state = docs.get(uri)?;
    let source = state.source.as_str();
    let line_index = &state.line_index;
    let root = state.root.as_ref()?;

    let mut result = Vec::with_capacity(positions.len());
    for pos in positions {
        let byte_offset = line_col_utf16_to_byte(source, line_index, pos.line, pos.character)
            .unwrap_or(0);
        let mut current: Option<SelectionRange> = None;
        if let Some(start_node) = root.clone().node_at_offset(byte_offset) {
            for anc in std::iter::once(start_node.clone()).chain(start_node.ancestors(root)) {
                let tr = anc.text_range();
                let range = Range {
                    start: span_to_position(source, line_index, tr.start),
                    end: span_to_position(source, line_index, tr.end),
                };
                current = Some(SelectionRange {
                    range,
                    parent: current.map(Box::new),
                });
            }
        }
        result.push(current.unwrap_or(SelectionRange {
            range: Range {
                start: *pos,
                end: *pos,
            },
            parent: None,
        }));
    }
    Some(result)
}
