//! Document and workspace symbol providers.

use std::collections::HashMap;

use leekscript_rs::analysis::{
    class_decl_info, class_field_info, function_decl_info, var_decl_info, VarDeclKind,
};
use leekscript_rs::syntax::Kind;
use sipha::types::IntoSyntaxKind;
use tower_lsp::lsp_types::{Url, *};

use crate::document::DocumentState;
use crate::util::span_to_position;

/// Returns Some(0) for prefix match, Some(1) for substring match, None for no match.
fn workspace_symbol_match_rank(query_lower: &str, name: &str) -> Option<u8> {
    if query_lower.is_empty() {
        return Some(0);
    }
    let n = name.to_lowercase();
    if n.starts_with(query_lower) {
        Some(0)
    } else if n.contains(query_lower) {
        Some(1)
    } else {
        None
    }
}

/// Recursively collect matching symbols into `out` with match rank (0 = prefix, 1 = substring).
#[allow(deprecated)]
fn workspace_symbol_collect(
    sym: &DocumentSymbol,
    container_name: Option<&str>,
    query_lower: &str,
    uri_url: &Url,
    out: &mut Vec<(SymbolInformation, u8)>,
) {
    if let Some(rank) = workspace_symbol_match_rank(query_lower, &sym.name) {
        out.push((
            SymbolInformation {
                name: sym.name.clone(),
                kind: sym.kind,
                tags: None,
                deprecated: None,
                location: Location {
                    uri: uri_url.clone(),
                    range: sym.range,
                },
                container_name: container_name.map(String::from),
            },
            rank,
        ));
    }
    if let Some(ref children) = sym.children {
        for child in children {
            workspace_symbol_collect(child, Some(&sym.name), query_lower, uri_url, out);
        }
    }
}

/// Return the document outline (classes, functions, global variables) for the given URI.
pub fn document_symbols_at(
    docs: &HashMap<String, DocumentState>,
    uri: &str,
) -> Option<DocumentSymbolResponse> {
    let state = docs.get(uri)?;
    let source = state.source.as_str();
    let line_index = &state.line_index;
    let root = state.root.as_ref()?;

    let in_main = |start: u32, end: u32| -> Option<Range> {
        Some(Range {
            start: span_to_position(source, line_index, start),
            end: span_to_position(source, line_index, end),
        })
    };

    let class_ranges: Vec<(u32, u32)> = root
        .find_all_nodes(Kind::NodeClassDecl.into_syntax_kind())
        .into_iter()
        .map(|n| {
            let r = n.text_range();
            (r.start, r.end)
        })
        .collect();
    let inside_any_class = |start: u32, end: u32| {
        class_ranges
            .iter()
            .any(|&(s, e)| s < start && end < e)
    };

    let mut symbols = Vec::new();
    for node in root.find_all_nodes(Kind::NodeClassDecl.into_syntax_kind()) {
        let info = class_decl_info(&node)?;
        let class_range = node.text_range();
        let r = in_main(class_range.start, class_range.end)?;
        let sel = in_main(info.name_span.start, info.name_span.end)?;
        let mut children: Vec<DocumentSymbol> = Vec::new();
        for func in root.find_all_nodes(Kind::NodeFunctionDecl.into_syntax_kind()) {
            let fr = func.text_range();
            if fr.start > class_range.start && fr.end < class_range.end {
                if let Some(fi) = function_decl_info(&func) {
                    if let (Some(rf), Some(self_f)) =
                        (in_main(fr.start, fr.end), in_main(fi.name_span.start, fi.name_span.end))
                    {
                        children.push(DocumentSymbol {
                            name: fi.name,
                            detail: None,
                            kind: SymbolKind::METHOD,
                            tags: None,
                            deprecated: None,
                            range: rf,
                            selection_range: self_f,
                            children: None,
                        });
                    }
                }
            }
        }
        for field in root.find_all_nodes(Kind::NodeClassField.into_syntax_kind()) {
            let fr = field.text_range();
            if fr.start > class_range.start && fr.end < class_range.end {
                if let Some((name, _, _)) = class_field_info(&field) {
                    if let Some(rf) = in_main(fr.start, fr.end) {
                        children.push(DocumentSymbol {
                            name,
                            detail: None,
                            kind: SymbolKind::FIELD,
                            tags: None,
                            deprecated: None,
                            range: rf.clone(),
                            selection_range: rf,
                            children: None,
                        });
                    }
                }
            }
        }
        symbols.push(DocumentSymbol {
            name: info.name,
            detail: None,
            kind: SymbolKind::CLASS,
            tags: None,
            deprecated: None,
            range: r,
            selection_range: sel,
            children: Some(children),
        });
    }
    for node in root.find_all_nodes(Kind::NodeFunctionDecl.into_syntax_kind()) {
        let info = function_decl_info(&node)?;
        let range = node.text_range();
        if inside_any_class(range.start, range.end) {
            continue;
        }
        if let Some(r) = in_main(range.start, range.end) {
            if let Some(sel) = in_main(info.name_span.start, info.name_span.end) {
                symbols.push(DocumentSymbol {
                    name: info.name,
                    detail: None,
                    kind: SymbolKind::FUNCTION,
                    tags: None,
                    deprecated: None,
                    range: r,
                    selection_range: sel,
                    children: None,
                });
            }
        }
    }
    for node in root.find_all_nodes(Kind::NodeVarDecl.into_syntax_kind()) {
        let info = var_decl_info(&node)?;
        if info.kind != VarDeclKind::Global {
            continue;
        }
        let range = node.text_range();
        if inside_any_class(range.start, range.end) {
            continue;
        }
        if let Some(r) = in_main(range.start, range.end) {
            if let Some(sel) = in_main(info.name_span.start, info.name_span.end) {
                symbols.push(DocumentSymbol {
                    name: info.name,
                    detail: None,
                    kind: SymbolKind::VARIABLE,
                    tags: None,
                    deprecated: None,
                    range: r,
                    selection_range: sel,
                    children: None,
                });
            }
        }
    }

    Some(DocumentSymbolResponse::Nested(symbols))
}

/// Search all open documents for symbols matching the query.
#[allow(deprecated)]
pub fn workspace_symbols_at(
    docs: &HashMap<String, DocumentState>,
    query: &str,
) -> Vec<SymbolInformation> {
    let query_lower = query.to_lowercase();
    let mut with_rank: Vec<(SymbolInformation, u8)> = Vec::new();
    for (uri, _state) in docs.iter() {
        let Some(symbols) = document_symbols_at(docs, uri) else {
            continue;
        };
        let DocumentSymbolResponse::Nested(symbols) = symbols else {
            continue;
        };
        let uri_url = match crate::util::parse_uri(uri) {
            Some(u) => u,
            None => continue,
        };
        for sym in symbols {
            workspace_symbol_collect(
                &sym,
                None,
                &query_lower,
                &uri_url,
                &mut with_rank,
            );
        }
    }
    with_rank.sort_by(|a, b| {
        a.1.cmp(&b.1)
            .then_with(|| a.0.name.to_lowercase().cmp(&b.0.name.to_lowercase()))
    });
    with_rank.into_iter().map(|(si, _)| si).collect()
}
