//! Navigation: go-to-definition, references, document highlight, rename.

use std::collections::HashMap;

use leekscript_rs::analysis::ResolvedSymbol;
use leekscript_rs::syntax::Kind;
use leekscript_rs::{scope_at_offset, LineIndex, ScopeId};
use tower_lsp::lsp_types::{Url, *};

use crate::document::{DocumentState, RootSymbolKind};
use crate::include::{include_path_at_offset, tree_file_contents};
use crate::resolve::symbol_matches;
use crate::util::{
    canonical_path, line_col_utf16_to_byte, parse_uri, path_to_uri, span_to_position, uri_to_path,
};

/// Resolve the definition location(s) for the symbol at the given position.
pub fn goto_definition_at(
    docs: &HashMap<String, DocumentState>,
    uri: &str,
    line: u32,
    character: u32,
) -> Option<Vec<Location>> {
    let state = docs.get(uri)?;
    let source = state.source.as_str();
    let line_index = &state.line_index;
    let root = state.root.as_ref()?;

    // When the document has includes, try go-to-definition on the include statement first.
    if let (Some(ref tree), Some(ref main_path)) = (&state.include_tree, &state.main_path) {
        let file_contents = tree_file_contents(tree);
        let main_byte_offset = line_col_utf16_to_byte(
            state.source.as_str(),
            &state.line_index,
            line,
            character,
        );
        if let Some(offset) = main_byte_offset {
            if let Some(included_path) = include_path_at_offset(
                state.source.as_str(),
                main_path,
                offset as usize,
                &file_contents,
            ) {
                if let Some(loc_uri) = path_to_uri(&included_path) {
                    return Some(vec![Location {
                        uri: loc_uri,
                        range: Range {
                            start: Position { line: 0, character: 0 },
                            end: Position { line: 0, character: 0 },
                        },
                    }]);
                }
            }
        }
    }

    let byte_offset = line_col_utf16_to_byte(source, line_index, line, character)?;
    let token = root.token_at_offset(byte_offset)?;
    if token.kind_as::<Kind>() != Some(Kind::TokIdent) {
        return None;
    }
    let name = token.text().to_string();
    let scope_id = scope_at_offset(&state.scope_extents, byte_offset);
    let sym = state.scope_store.resolve(scope_id, &name)?;
    let root_scope = state.scope_store.get(ScopeId(0))?;
    // Use definition_map when available so definitions in included files get the correct URI.
    if let (Some(ref tree), Some(ref main_path)) = (&state.include_tree, &state.main_path) {
        let kind = match &sym {
            ResolvedSymbol::Class(_) => RootSymbolKind::Class,
            ResolvedSymbol::Function(_, _) => RootSymbolKind::Function,
            ResolvedSymbol::Global(_) => RootSymbolKind::Global,
            ResolvedSymbol::Variable(_) => {
                let def_span = match &sym {
                    ResolvedSymbol::Variable(v) => v.span,
                    _ => unreachable!(),
                };
                let url = parse_uri(uri)?;
                let range = Range {
                    start: span_to_position(source, line_index, def_span.start),
                    end: span_to_position(source, line_index, def_span.end),
                };
                return Some(vec![Location { uri: url, range }]);
            }
        };
        if let Some((ref def_path, start, end)) = state.definition_map.get(&(name.clone(), kind)) {
            let def_source = tree.source_for_path(main_path, def_path)?;
            let def_line_index = LineIndex::new(def_source.as_bytes());
            let url = path_to_uri(def_path)?;
            let range = Range {
                start: span_to_position(def_source, &def_line_index, *start),
                end: span_to_position(def_source, &def_line_index, *end),
            };
            return Some(vec![Location { uri: url, range }]);
        }
    }
    let def_span = match &sym {
        ResolvedSymbol::Variable(v) => v.span,
        ResolvedSymbol::Function(n, _) => root_scope.get_function_first_span(n)?,
        ResolvedSymbol::Class(n) => root_scope.get_class_first_span(n)?,
        ResolvedSymbol::Global(_) => return None,
    };
    let url = parse_uri(uri)?;
    let range = Range {
        start: span_to_position(source, line_index, def_span.start),
        end: span_to_position(source, line_index, def_span.end),
    };
    Some(vec![Location { uri: url, range }])
}

/// Find all references to the symbol at the given position.
pub fn references_at(
    docs: &HashMap<String, DocumentState>,
    uri: &str,
    line: u32,
    character: u32,
    include_declaration: bool,
) -> Option<Vec<Location>> {
    let state = docs.get(uri)?;
    let source = state.source.as_str();
    let line_index = &state.line_index;
    let root = state.root.as_ref()?;

    let byte_offset = line_col_utf16_to_byte(source, line_index, line, character)?;
    let token = root.token_at_offset(byte_offset)?;
    if token.kind_as::<Kind>() != Some(Kind::TokIdent) {
        return None;
    }
    let name = token.text().to_string();
    let scope_id = scope_at_offset(&state.scope_extents, byte_offset);
    let target_sym = state.scope_store.resolve(scope_id, &name)?;
    let root_scope = state.scope_store.get(ScopeId(0))?;
    let def_span = match &target_sym {
        ResolvedSymbol::Variable(v) => v.span,
        ResolvedSymbol::Function(n, _) => root_scope.get_function_first_span(n)?,
        ResolvedSymbol::Class(n) => root_scope.get_class_first_span(n)?,
        ResolvedSymbol::Global(_) => return None,
    };

    let mut locations = Vec::new();

    if let (Some(ref tree), Some(ref main_path)) = (&state.include_tree, &state.main_path) {
        // Document has includes: add definition from map if root-level, then search main + all included files.
        let kind = match &target_sym {
            ResolvedSymbol::Class(_) => RootSymbolKind::Class,
            ResolvedSymbol::Function(_, _) => RootSymbolKind::Function,
            ResolvedSymbol::Global(_) => RootSymbolKind::Global,
            ResolvedSymbol::Variable(_) => {
                if include_declaration && def_span.start < def_span.end {
                    if let Some(url) = parse_uri(uri) {
                        locations.push(Location {
                            uri: url,
                            range: Range {
                                start: span_to_position(source, line_index, def_span.start),
                                end: span_to_position(source, line_index, def_span.end),
                            },
                        });
                    }
                }
                // Search only main file for variable refs (locals are single-file).
                for tok in root.descendant_tokens() {
                    if tok.kind_as::<Kind>() != Some(Kind::TokIdent) || tok.text() != name {
                        continue;
                    }
                    let tr = tok.text_range();
                    if include_declaration && tr.start == def_span.start && tr.end == def_span.end {
                        continue;
                    }
                    let ref_scope = scope_at_offset(&state.scope_extents, tr.start);
                    let Some(ref ref_sym) = state.scope_store.resolve(ref_scope, &name) else {
                        continue;
                    };
                    if !symbol_matches(&target_sym, ref_sym) {
                        continue;
                    }
                    if let Some(url) = parse_uri(uri) {
                        locations.push(Location {
                            uri: url,
                            range: Range {
                                start: span_to_position(source, line_index, tr.start),
                                end: span_to_position(source, line_index, tr.end),
                            },
                        });
                    }
                }
                return Some(locations);
            }
        };
        if include_declaration {
            if let Some((ref def_path, start, end)) = state.definition_map.get(&(name.clone(), kind)) {
                if let Some(def_source) = tree.source_for_path(main_path, def_path) {
                    let def_line_index = LineIndex::new(def_source.as_bytes());
                    if let Some(def_uri) = path_to_uri(def_path) {
                        locations.push(Location {
                            uri: def_uri,
                            range: Range {
                                start: span_to_position(def_source, &def_line_index, *start),
                                end: span_to_position(def_source, &def_line_index, *end),
                            },
                        });
                    }
                }
            }
        }
        // Search main file.
        let main_uri_str = uri.to_string();
        for tok in root.descendant_tokens() {
            if tok.kind_as::<Kind>() != Some(Kind::TokIdent) || tok.text() != name {
                continue;
            }
            let tr = tok.text_range();
            if include_declaration && tr.start == def_span.start && tr.end == def_span.end {
                continue;
            }
            let ref_scope = scope_at_offset(&state.scope_extents, tr.start);
            let Some(ref ref_sym) = state.scope_store.resolve(ref_scope, &name) else {
                continue;
            };
            if !symbol_matches(&target_sym, ref_sym) {
                continue;
            }
            if let Some(url) = parse_uri(&main_uri_str) {
                locations.push(Location {
                    uri: url,
                    range: Range {
                        start: span_to_position(source, line_index, tr.start),
                        end: span_to_position(source, line_index, tr.end),
                    },
                });
            }
        }
        // Search each included file (root-level resolution).
        for (inc_path, child) in &tree.includes {
            if let Some(ref inc_root) = child.root {
                let inc_source = child.source.as_str();
                let inc_line_index = LineIndex::new(inc_source.as_bytes());
                let inc_uri = match path_to_uri(inc_path) {
                    Some(u) => u,
                    None => continue,
                };
                for tok in inc_root.descendant_tokens() {
                    if tok.kind_as::<Kind>() != Some(Kind::TokIdent) || tok.text() != name {
                        continue;
                    }
                    let tr = tok.text_range();
                    let ref_sym = state.scope_store.resolve(ScopeId(0), &name);
                    let Some(ref ref_sym) = ref_sym else {
                        continue;
                    };
                    if !symbol_matches(&target_sym, ref_sym) {
                        continue;
                    }
                    if include_declaration && def_span.start == tr.start && def_span.end == tr.end {
                        continue;
                    }
                    locations.push(Location {
                        uri: inc_uri.clone(),
                        range: Range {
                            start: span_to_position(inc_source, &inc_line_index, tr.start),
                            end: span_to_position(inc_source, &inc_line_index, tr.end),
                        },
                    });
                }
            }
        }
        return Some(locations);
    }

    // No include tree: add definition in current file, search current file, then search any document that includes this file.
    if include_declaration && def_span.start < def_span.end {
        if let Some(url) = parse_uri(uri) {
            locations.push(Location {
                uri: url,
                range: Range {
                    start: span_to_position(source, line_index, def_span.start),
                    end: span_to_position(source, line_index, def_span.end),
                },
            });
        }
    }
    for tok in root.descendant_tokens() {
        if tok.kind_as::<Kind>() != Some(Kind::TokIdent) || tok.text() != name {
            continue;
        }
        let tr = tok.text_range();
        if include_declaration && tr.start == def_span.start && tr.end == def_span.end {
            continue;
        }
        let ref_scope = scope_at_offset(&state.scope_extents, tr.start);
        let Some(ref ref_sym) = state.scope_store.resolve(ref_scope, &name) else {
            continue;
        };
        if !symbol_matches(&target_sym, ref_sym) {
            continue;
        }
        if let Some(url) = parse_uri(uri) {
            locations.push(Location {
                uri: url,
                range: Range {
                    start: span_to_position(source, line_index, tr.start),
                    end: span_to_position(source, line_index, tr.end),
                },
            });
        }
    }
    let current_path = match uri_to_path(uri) {
        Some(p) => canonical_path(&p),
        None => return Some(locations),
    };
    for (other_uri, other_state) in docs.iter() {
        if other_uri == uri {
            continue;
        }
        let (Some(ref other_tree), Some(_)) =
            (&other_state.include_tree, &other_state.main_path) else {
            continue;
        };
        let includes_current = other_tree.includes.iter().any(|(inc_path, _)| {
            canonical_path(inc_path) == current_path
        });
        if !includes_current {
            continue;
        }
        let other_root = match &other_state.root {
            Some(r) => r,
            None => continue,
        };
        let other_source = other_state.source.as_str();
        let other_line_index = &other_state.line_index;
        let other_url = match parse_uri(other_uri) {
            Some(u) => u,
            None => continue,
        };
        for tok in other_root.descendant_tokens() {
            if tok.kind_as::<Kind>() != Some(Kind::TokIdent) || tok.text() != name {
                continue;
            }
            let tr = tok.text_range();
            let ref_scope = scope_at_offset(&other_state.scope_extents, tr.start);
            let Some(ref ref_sym) = other_state.scope_store.resolve(ref_scope, &name) else {
                continue;
            };
            if !symbol_matches(&target_sym, ref_sym) {
                continue;
            }
            locations.push(Location {
                uri: other_url.clone(),
                range: Range {
                    start: span_to_position(other_source, other_line_index, tr.start),
                    end: span_to_position(other_source, other_line_index, tr.end),
                },
            });
        }
    }
    Some(locations)
}

/// Compute document highlights (read/write) for the symbol at the given position.
pub fn document_highlight_at(
    docs: &HashMap<String, DocumentState>,
    uri: &str,
    line: u32,
    character: u32,
) -> Option<Vec<DocumentHighlight>> {
    let def_location = goto_definition_at(docs, uri, line, character)?.first().cloned();
    let refs = references_at(docs, uri, line, character, true)?;
    let highlights: Vec<DocumentHighlight> = refs
        .into_iter()
        .map(|loc| {
            let kind = if def_location.as_ref() == Some(&loc) {
                Some(DocumentHighlightKind::WRITE)
            } else {
                Some(DocumentHighlightKind::READ)
            };
            DocumentHighlight {
                range: loc.range,
                kind: Some(kind.unwrap_or(DocumentHighlightKind::READ)),
            }
        })
        .collect();
    Some(highlights)
}

/// Compute a workspace edit that renames the symbol at the given position to `new_name`.
pub fn rename_at(
    docs: &HashMap<String, DocumentState>,
    uri: &str,
    line: u32,
    character: u32,
    new_name: &str,
) -> Option<WorkspaceEdit> {
    let locations = references_at(docs, uri, line, character, true)?;
    let mut changes: HashMap<Url, Vec<TextEdit>> = HashMap::new();
    for loc in locations {
        changes
            .entry(loc.uri)
            .or_default()
            .push(TextEdit {
                range: loc.range,
                new_text: new_name.to_string(),
            });
    }
    if changes.is_empty() {
        return None;
    }
    Some(WorkspaceEdit {
        changes: Some(changes),
        document_changes: None,
        change_annotations: None,
    })
}

/// If the position is on a renameable identifier, return its range (for prepareRename).
pub fn prepare_rename_at(
    docs: &HashMap<String, DocumentState>,
    uri: &str,
    line: u32,
    character: u32,
) -> Option<PrepareRenameResponse> {
    let state = docs.get(uri)?;
    let source = state.source.as_str();
    let line_index = &state.line_index;
    let root = state.root.as_ref()?;

    let byte_offset = line_col_utf16_to_byte(source, line_index, line, character)?;
    let token = root.token_at_offset(byte_offset)?;
    if token.kind_as::<Kind>() != Some(Kind::TokIdent) {
        return None;
    }
    let scope_id = scope_at_offset(&state.scope_extents, byte_offset);
    let _sym = state.scope_store.resolve(scope_id, &token.text().to_string())?;
    if matches!(_sym, ResolvedSymbol::Global(_)) {
        return None;
    }
    let range = token.text_range();
    let lsp_range = Range {
        start: span_to_position(source, line_index, range.start),
        end: span_to_position(source, line_index, range.end),
    };
    Some(PrepareRenameResponse::Range(lsp_range))
}
