//! Call and type hierarchy providers.

use std::collections::HashMap;

use leekscript_rs::analysis::{
    class_decl_info, function_decl_info,
    member_expr_member_name, member_expr_receiver_name, primary_expr_resolvable_name,
    ResolvedSymbol,
};
use leekscript_rs::syntax::Kind;
use leekscript_rs::{scope_at_offset, LineIndex, ScopeId};
use sipha::types::IntoSyntaxKind;
use tower_lsp::lsp_types::*;

use crate::document::{DocumentState, RootSymbolKind};
use crate::resolve::{current_class_at_offset, symbol_matches};
use crate::util::{line_col_utf16_to_byte, parse_uri, path_to_uri, span_to_position};

/// Prepare call hierarchy: return the item(s) at the given position that can have incoming/outgoing calls.
pub fn prepare_call_hierarchy_at(
    docs: &HashMap<String, DocumentState>,
    params: &CallHierarchyPrepareParams,
) -> Option<Vec<CallHierarchyItem>> {
    let uri = params.text_document_position_params.text_document.uri.to_string();
    let pos = params.text_document_position_params.position;
    let state = docs.get(&uri)?;
    let source = state.source.as_str();
    let line_index = &state.line_index;
    let root = state.root.as_ref()?;

    let byte_offset = line_col_utf16_to_byte(source, line_index, pos.line, pos.character)?;
    let node = root.node_at_offset(byte_offset)?;

    if let Some(Kind::NodeFunctionDecl) = node.kind_as::<Kind>() {
        let info = function_decl_info(&node)?;
        let range = node.text_range();
        let url = parse_uri(&uri)?;
        return Some(vec![CallHierarchyItem {
            name: info.name.clone(),
            kind: SymbolKind::FUNCTION,
            tags: None,
            detail: Some(format!("function {}(...)", info.name)),
            uri: url.clone(),
            range: Range {
                start: span_to_position(source, line_index, range.start),
                end: span_to_position(source, line_index, range.end),
            },
            selection_range: Range {
                start: span_to_position(source, line_index, info.name_span.start),
                end: span_to_position(source, line_index, info.name_span.end),
            },
            data: None,
        }]);
    }

    if let Some(Kind::NodeClassDecl) = node.kind_as::<Kind>() {
        let info = class_decl_info(&node)?;
        let range = node.text_range();
        let url = parse_uri(&uri)?;
        return Some(vec![CallHierarchyItem {
            name: info.name.clone(),
            kind: SymbolKind::CLASS,
            tags: None,
            detail: info.super_class.as_ref().map(|s| format!("extends {}", s)),
            uri: url.clone(),
            range: Range {
                start: span_to_position(source, line_index, range.start),
                end: span_to_position(source, line_index, range.end),
            },
            selection_range: Range {
                start: span_to_position(source, line_index, info.name_span.start),
                end: span_to_position(source, line_index, info.name_span.end),
            },
            data: None,
        }]);
    }

    let token = root.token_at_offset(byte_offset)?;
    if token.kind_as::<Kind>() != Some(Kind::TokIdent) {
        return None;
    }
    let name = token.text().to_string();
    let scope_id = scope_at_offset(&state.scope_extents, byte_offset);
    let sym = state.scope_store.resolve(scope_id, &name)?;
    let root_scope = state.scope_store.get(ScopeId(0))?;

    match &sym {
        ResolvedSymbol::Function(n, _) => {
            let span = root_scope.get_function_first_span(n)?;
            let url = parse_uri(&uri)?;
            Some(vec![CallHierarchyItem {
                name: name.clone(),
                kind: SymbolKind::FUNCTION,
                tags: None,
                detail: Some(format!("function {}", name)),
                uri: url.clone(),
                range: Range {
                    start: span_to_position(source, line_index, span.start),
                    end: span_to_position(source, line_index, span.end),
                },
                selection_range: Range {
                    start: span_to_position(source, line_index, span.start),
                    end: span_to_position(source, line_index, span.end),
                },
                data: None,
            }])
        }
        ResolvedSymbol::Class(n) => {
            let span = root_scope.get_class_first_span(n)?;
            let url = parse_uri(&uri)?;
            Some(vec![CallHierarchyItem {
                name: name.clone(),
                kind: SymbolKind::CLASS,
                tags: None,
                detail: None,
                uri: url.clone(),
                range: Range {
                    start: span_to_position(source, line_index, span.start),
                    end: span_to_position(source, line_index, span.end),
                },
                selection_range: Range {
                    start: span_to_position(source, line_index, span.start),
                    end: span_to_position(source, line_index, span.end),
                },
                data: None,
            }])
        }
        _ => None,
    }
}

/// Find incoming calls to the item (e.g. who calls this function).
pub fn call_hierarchy_incoming(
    docs: &HashMap<String, DocumentState>,
    params: &CallHierarchyIncomingCallsParams,
) -> Option<Vec<CallHierarchyIncomingCall>> {
    let item = &params.item;
    let def_uri = item.uri.to_string();
    let name = &item.name;

    let state = docs.get(&def_uri)?;
    let source = state.source.as_str();
    let line_index = &state.line_index;
    let root = state.root.as_ref()?;

    let def_range = &item.range;
    let def_byte = line_col_utf16_to_byte(
        source,
        line_index,
        def_range.start.line,
        def_range.start.character,
    )?;
    let scope_id = scope_at_offset(&state.scope_extents, def_byte);
    let target_sym = state.scope_store.resolve(scope_id, name);
    let method_class = current_class_at_offset(root, def_byte);
    let root_scope = state.scope_store.get(ScopeId(0))?;
    if target_sym.is_none() && method_class.is_none() {
        return None;
    }
    if let Some(ref sym) = target_sym {
        let _ = match sym {
            ResolvedSymbol::Function(n, _) => root_scope.get_function_first_span(n)?,
            ResolvedSymbol::Class(n) => root_scope.get_class_first_span(n)?,
            _ => return None,
        };
    }

    let mut incoming = Vec::new();
    for call_node in root.find_all_nodes(Kind::NodeCallExpr.into_syntax_kind()) {
        let callee = call_node.child_nodes().next()?;
        let call_name = if callee.kind_as::<Kind>() == Some(Kind::NodeMemberExpr) {
            member_expr_member_name(&callee)
        } else {
            primary_expr_resolvable_name(&callee)
        };
        let call_name = match &call_name {
            Some(n) if n == name => n.clone(),
            _ => continue,
        };
        let is_method_call = if let Some(ref class_name) = method_class {
            if callee.kind_as::<Kind>() != Some(Kind::NodeMemberExpr) {
                false
            } else {
                let receiver = member_expr_receiver_name(&callee).unwrap_or_default();
                receiver == *class_name
                    || (receiver == "this"
                        && current_class_at_offset(root, call_node.text_range().start)
                            .as_deref()
                            == Some(class_name))
            }
        } else {
            false
        };
        let matches = if is_method_call {
            true
        } else if let Some(ref target_sym) = target_sym {
            let call_scope_id =
                scope_at_offset(&state.scope_extents, call_node.text_range().start);
            state
                .scope_store
                .resolve(call_scope_id, &call_name)
                .as_ref()
                .map(|ref_sym| symbol_matches(target_sym, ref_sym))
                .unwrap_or(false)
        } else {
            false
        };
        if !matches {
            continue;
        }
        let caller_decl = call_node.find_ancestor(root, Kind::NodeFunctionDecl.into_syntax_kind());
        let (from_item, from_ranges) = if let Some(decl) = caller_decl {
            let info = function_decl_info(&decl)?;
            let decl_range = decl.text_range();
            let from_item = CallHierarchyItem {
                name: info.name.clone(),
                kind: SymbolKind::FUNCTION,
                tags: None,
                detail: Some(format!("function {}(...)", info.name)),
                uri: item.uri.clone(),
                range: Range {
                    start: span_to_position(source, line_index, decl_range.start),
                    end: span_to_position(source, line_index, decl_range.end),
                },
                selection_range: Range {
                    start: span_to_position(source, line_index, info.name_span.start),
                    end: span_to_position(source, line_index, info.name_span.end),
                },
                data: None,
            };
            let call_range = call_node.text_range();
            let from_ranges = vec![Range {
                start: span_to_position(source, line_index, call_range.start),
                end: span_to_position(source, line_index, call_range.end),
            }];
            (from_item, from_ranges)
        } else {
            let call_range = call_node.text_range();
            let from_item = CallHierarchyItem {
                name: "<top level>".to_string(),
                kind: SymbolKind::FUNCTION,
                tags: None,
                detail: None,
                uri: item.uri.clone(),
                range: Range {
                    start: span_to_position(source, line_index, 0),
                    end: span_to_position(source, line_index, source.len() as u32),
                },
                selection_range: Range {
                    start: span_to_position(source, line_index, call_range.start),
                    end: span_to_position(source, line_index, call_range.end),
                },
                data: None,
            };
            let from_ranges = vec![Range {
                start: span_to_position(source, line_index, call_range.start),
                end: span_to_position(source, line_index, call_range.end),
            }];
            (from_item, from_ranges)
        };
        incoming.push(CallHierarchyIncomingCall {
            from: from_item,
            from_ranges,
        });
    }
    Some(incoming)
}

/// Find outgoing calls from the item (e.g. what this function calls).
pub fn call_hierarchy_outgoing(
    docs: &HashMap<String, DocumentState>,
    params: &CallHierarchyOutgoingCallsParams,
) -> Option<Vec<CallHierarchyOutgoingCall>> {
    let item = &params.item;
    let def_uri = item.uri.to_string();
    let item_range = &item.range;

    let state = docs.get(&def_uri)?;
    let source = state.source.as_str();
    let line_index = &state.line_index;
    let root = state.root.as_ref()?;

    let item_byte_start = line_col_utf16_to_byte(
        source,
        line_index,
        item_range.start.line,
        item_range.start.character,
    )?;
    let item_byte_end = line_col_utf16_to_byte(
        source,
        line_index,
        item_range.end.line,
        item_range.end.character,
    )?;

    let mut outgoing = Vec::new();
    for call_node in root.find_all_nodes(Kind::NodeCallExpr.into_syntax_kind()) {
        let tr = call_node.text_range();
        if tr.start < item_byte_start || tr.end > item_byte_end {
            continue;
        }
        let callee = call_node.child_nodes().next()?;
        let call_name = if callee.kind_as::<Kind>() == Some(Kind::NodeMemberExpr) {
            member_expr_member_name(&callee)
        } else {
            primary_expr_resolvable_name(&callee)
        }?;
        let scope_id = scope_at_offset(&state.scope_extents, tr.start);
        let sym = state.scope_store.resolve(scope_id, &call_name)?;
        let root_scope = state.scope_store.get(ScopeId(0))?;
        let (to_range, to_selection) = match &sym {
            ResolvedSymbol::Function(n, _) => {
                let span = root_scope.get_function_first_span(n)?;
                (
                    Range {
                        start: span_to_position(source, line_index, span.start),
                        end: span_to_position(source, line_index, span.end),
                    },
                    Range {
                        start: span_to_position(source, line_index, span.start),
                        end: span_to_position(source, line_index, span.end),
                    },
                )
            }
            ResolvedSymbol::Class(n) => {
                let span = root_scope.get_class_first_span(n)?;
                (
                    Range {
                        start: span_to_position(source, line_index, span.start),
                        end: span_to_position(source, line_index, span.end),
                    },
                    Range {
                        start: span_to_position(source, line_index, span.start),
                        end: span_to_position(source, line_index, span.end),
                    },
                )
            }
            _ => continue,
        };
        let to_item = CallHierarchyItem {
            name: call_name.clone(),
            kind: match &sym {
                ResolvedSymbol::Function(_, _) => SymbolKind::FUNCTION,
                ResolvedSymbol::Class(_) => SymbolKind::CLASS,
                _ => SymbolKind::FUNCTION,
            },
            tags: None,
            detail: None,
            uri: item.uri.clone(),
            range: to_range.clone(),
            selection_range: to_selection,
            data: None,
        };
        outgoing.push(CallHierarchyOutgoingCall {
            to: to_item,
            from_ranges: vec![Range {
                start: span_to_position(source, line_index, tr.start),
                end: span_to_position(source, line_index, tr.end),
            }],
        });
    }
    Some(outgoing)
}

/// Prepare type hierarchy: return the class item at the given position.
pub fn prepare_type_hierarchy_at(
    docs: &HashMap<String, DocumentState>,
    params: &TypeHierarchyPrepareParams,
) -> Option<Vec<TypeHierarchyItem>> {
    let uri = params.text_document_position_params.text_document.uri.to_string();
    let pos = params.text_document_position_params.position;
    let state = docs.get(&uri)?;
    let source = state.source.as_str();
    let line_index = &state.line_index;
    let root = state.root.as_ref()?;

    let byte_offset = line_col_utf16_to_byte(source, line_index, pos.line, pos.character)?;
    let node = root.node_at_offset(byte_offset)?;

    if let Some(Kind::NodeClassDecl) = node.kind_as::<Kind>() {
        let info = class_decl_info(&node)?;
        let range = node.text_range();
        let url = parse_uri(&uri)?;
        return Some(vec![TypeHierarchyItem {
            name: info.name.clone(),
            kind: SymbolKind::CLASS,
            tags: None,
            detail: info.super_class.as_ref().map(|s| format!("extends {}", s)),
            uri: url.clone(),
            range: Range {
                start: span_to_position(source, line_index, range.start),
                end: span_to_position(source, line_index, range.end),
            },
            selection_range: Range {
                start: span_to_position(source, line_index, info.name_span.start),
                end: span_to_position(source, line_index, info.name_span.end),
            },
            data: None,
        }]);
    }

    let token = root.token_at_offset(byte_offset)?;
    if token.kind_as::<Kind>() != Some(Kind::TokIdent) {
        return None;
    }
    let name = token.text().to_string();
    let scope_id = scope_at_offset(&state.scope_extents, byte_offset);
    let sym = state.scope_store.resolve(scope_id, &name)?;
    let root_scope = state.scope_store.get(ScopeId(0))?;

    if let ResolvedSymbol::Class(n) = &sym {
        let span = root_scope.get_class_first_span(n)?;
        let url = parse_uri(&uri)?;
        Some(vec![TypeHierarchyItem {
            name: name.clone(),
            kind: SymbolKind::CLASS,
            tags: None,
            detail: None,
            uri: url.clone(),
            range: Range {
                start: span_to_position(source, line_index, span.start),
                end: span_to_position(source, line_index, span.end),
            },
            selection_range: Range {
                start: span_to_position(source, line_index, span.start),
                end: span_to_position(source, line_index, span.end),
            },
            data: None,
        }])
    } else {
        None
    }
}

/// Return the supertype of the class item.
pub fn type_hierarchy_supertypes(
    docs: &HashMap<String, DocumentState>,
    params: &TypeHierarchySupertypesParams,
) -> Option<Vec<TypeHierarchyItem>> {
    let item = &params.item;
    let uri = item.uri.to_string();
    let state = docs.get(&uri)?;
    let source = state.source.as_str();
    let line_index = &state.line_index;

    let super_name = state.class_super.get(&item.name)?;
    if let (Some(ref tree), Some(ref main_path)) = (&state.include_tree, &state.main_path) {
        if let Some((ref def_path, start, end)) = state
            .definition_map
            .get(&(super_name.clone(), RootSymbolKind::Class))
        {
            if let Some(def_source) = tree.source_for_path(main_path, def_path) {
                let def_line_index = LineIndex::new(def_source.as_bytes());
                if let Some(def_uri) = path_to_uri(def_path) {
                    return Some(vec![TypeHierarchyItem {
                        name: super_name.clone(),
                        kind: SymbolKind::CLASS,
                        tags: None,
                        detail: None,
                        uri: def_uri,
                        range: Range {
                            start: span_to_position(&def_source, &def_line_index, *start),
                            end: span_to_position(&def_source, &def_line_index, *end),
                        },
                        selection_range: Range {
                            start: span_to_position(&def_source, &def_line_index, *start),
                            end: span_to_position(&def_source, &def_line_index, *end),
                        },
                        data: None,
                    }]);
                }
            }
        }
    }
    let root_scope = state.scope_store.get(ScopeId(0))?;
    let span = root_scope.get_class_first_span(super_name)?;
    let url = parse_uri(&uri)?;
    Some(vec![TypeHierarchyItem {
        name: super_name.clone(),
        kind: SymbolKind::CLASS,
        tags: None,
        detail: None,
        uri: url.clone(),
        range: Range {
            start: span_to_position(source, line_index, span.start),
            end: span_to_position(source, line_index, span.end),
        },
        selection_range: Range {
            start: span_to_position(source, line_index, span.start),
            end: span_to_position(source, line_index, span.end),
        },
        data: None,
    }])
}

/// Return the subtypes of the class item.
pub fn type_hierarchy_subtypes(
    docs: &HashMap<String, DocumentState>,
    params: &TypeHierarchySubtypesParams,
) -> Option<Vec<TypeHierarchyItem>> {
    let item = &params.item;
    let uri = item.uri.to_string();
    let state = docs.get(&uri)?;
    let source = state.source.as_str();
    let line_index = &state.line_index;

    let mut subtypes = Vec::new();
    for (sub_name, super_name) in &state.class_super {
        if super_name != &item.name {
            continue;
        }
        if let (Some(ref tree), Some(ref main_path)) = (&state.include_tree, &state.main_path) {
            if let Some((ref def_path, start, end)) = state
                .definition_map
                .get(&(sub_name.clone(), RootSymbolKind::Class))
            {
                if let Some(def_source) = tree.source_for_path(main_path, def_path) {
                    let def_line_index = LineIndex::new(def_source.as_bytes());
                    if let Some(def_uri) = path_to_uri(def_path) {
                        subtypes.push(TypeHierarchyItem {
                            name: sub_name.clone(),
                            kind: SymbolKind::CLASS,
                            tags: None,
                            detail: Some(format!("extends {}", item.name)),
                            uri: def_uri,
                            range: Range {
                                start: span_to_position(
                                    &def_source,
                                    &def_line_index,
                                    *start,
                                ),
                                end: span_to_position(
                                    &def_source,
                                    &def_line_index,
                                    *end,
                                ),
                            },
                            selection_range: Range {
                                start: span_to_position(
                                    &def_source,
                                    &def_line_index,
                                    *start,
                                ),
                                end: span_to_position(
                                    &def_source,
                                    &def_line_index,
                                    *end,
                                ),
                            },
                            data: None,
                        });
                        continue;
                    }
                }
            }
        }
        let root_scope = state.scope_store.get(ScopeId(0))?;
        let span = match root_scope.get_class_first_span(sub_name) {
            Some(s) => s,
            None => continue,
        };
        let url = parse_uri(&uri)?;
        subtypes.push(TypeHierarchyItem {
            name: sub_name.clone(),
            kind: SymbolKind::CLASS,
            tags: None,
            detail: Some(format!("extends {}", item.name)),
            uri: url.clone(),
            range: Range {
                start: span_to_position(source, line_index, span.start),
                end: span_to_position(source, line_index, span.end),
            },
            selection_range: Range {
                start: span_to_position(source, line_index, span.start),
                end: span_to_position(source, line_index, span.end),
            },
            data: None,
        });
    }
    Some(subtypes)
}
