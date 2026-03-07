//! Inlay hints: type annotations and parameter names at call sites.

use std::collections::HashMap;

use leekscript_rs::analysis::{
    call_argument_count, call_argument_node, class_decl_info, function_decl_info,
    member_expr_member_name, member_expr_receiver_name, primary_expr_resolvable_name,
    var_decl_info, VarDeclKind,
};
use leekscript_rs::syntax::Kind;
use leekscript_rs::{scope_at_offset, ScopeId};
use sipha::red::SyntaxNode;
use sipha::types::IntoSyntaxKind;
use tower_lsp::lsp_types::*;

use crate::config::LspSettings;
use crate::document::DocumentState;
use crate::resolve::current_class_at_offset;
use crate::signature_help::{find_function_decl_for_signature_help, find_method_decl};
use crate::util::{line_col_utf16_to_byte, span_to_position};

/// Compute inlay hints for the given document range.
pub fn inlay_hints_at(
    docs: &HashMap<String, DocumentState>,
    settings: &LspSettings,
    uri: &str,
    range_start_line: u32,
    range_start_character: u32,
    range_end_line: u32,
    range_end_character: u32,
) -> Vec<InlayHint> {
    let state = match docs.get(uri) {
        Some(s) => s,
        None => return vec![],
    };
    let source = state.source.as_str();
    let line_index = &state.line_index;
    let root = match state.root.as_ref() {
        Some(r) => r,
        None => return vec![],
    };

    let byte_range_start = line_col_utf16_to_byte(
        source,
        line_index,
        range_start_line,
        range_start_character,
    )
    .unwrap_or(0);
    let byte_range_end = line_col_utf16_to_byte(
        source,
        line_index,
        range_end_line,
        range_end_character,
    )
    .unwrap_or(source.len() as u32);

    let mut hints = Vec::new();
    for node in root.find_all_nodes(Kind::NodeVarDecl.into_syntax_kind()) {
        let info = match var_decl_info(&node) {
            Some(i) => i,
            None => continue,
        };
        if matches!(info.kind, VarDeclKind::Typed) {
            continue;
        }
        let decl_range = node.text_range();
        let ty = match state.type_map.get(&(decl_range.start, decl_range.end)) {
            Some(t) => t,
            None => continue,
        };
        if ty.for_annotation() == "any" {
            continue;
        }
        let pos_byte = info.name_span.end;
        if pos_byte < byte_range_start || pos_byte > byte_range_end {
            continue;
        }
        let position = span_to_position(source, line_index, pos_byte);
        let label = format!(": {}", ty.for_annotation());
        hints.push(InlayHint {
            position,
            label: InlayHintLabel::String(label),
            kind: Some(InlayHintKind::TYPE),
            text_edits: None,
            tooltip: None,
            padding_left: Some(false),
            padding_right: Some(false),
            data: None,
        });
    }

    for call_node in root.find_all_nodes(Kind::NodeCallExpr.into_syntax_kind()) {
        let call_range = call_node.text_range();
        if call_range.end <= byte_range_start || call_range.start >= byte_range_end {
            continue;
        }
        let callee = match call_node.child_nodes().next() {
            Some(c) => c,
            None => continue,
        };
        let name = match callee.kind_as::<Kind>() {
            Some(Kind::NodeMemberExpr) => member_expr_member_name(&callee),
            _ => primary_expr_resolvable_name(&callee),
        };
        let name = match name {
            Some(n) => n,
            None => continue,
        };
        let arity = call_argument_count(&call_node);
        let scope_id = scope_at_offset(&state.scope_extents, call_range.start);
        let mut id = Some(scope_id);
        let (_param_types, return_type) = loop {
            let scope_id = match id {
                Some(s) => s,
                None => break (Vec::new(), leekscript_rs::types::Type::any()),
            };
            let scope = match state.scope_store.get(scope_id) {
                Some(s) => s,
                None => break (Vec::new(), leekscript_rs::types::Type::any()),
            };
            if let Some((p, r)) = scope.get_function_type(&name, arity) {
                break (p, r);
            }
            id = scope.parent;
        };
        let (param_names, return_type) = {
            let mut param_names = find_function_decl_for_signature_help(root, &name, arity)
                .map(|(_, names)| names)
                .unwrap_or_default();
            let mut return_type = return_type;
            if return_type.for_annotation() == "any"
                && callee.kind_as::<Kind>() == Some(Kind::NodeMemberExpr)
            {
                let receiver = member_expr_receiver_name(&callee).unwrap_or_default();
                let class_name = if receiver == "this" {
                    current_class_at_offset(root, call_range.start)
                } else {
                    state
                        .scope_store
                        .get(ScopeId(0))
                        .filter(|s| s.has_class(&receiver))
                        .map(|_| receiver)
                };
                if let Some(ref cname) = class_name {
                    if let Some(members) = state.scope_store.get_class_members(cname) {
                        let sig = members
                            .static_methods
                            .get(&name)
                            .or_else(|| members.methods.get(&name));
                        if let Some((_, ret, _)) = sig {
                            return_type = ret.clone();
                            param_names = find_method_decl(root, cname, &name, arity)
                                .map(|(_, names)| names)
                                .unwrap_or_default();
                        }
                    }
                }
            }
            (param_names, return_type)
        };

        for i in 0..arity {
            let arg_node = match call_argument_node(&call_node, i) {
                Some(n) => n,
                None => continue,
            };
            let arg_start = arg_node.text_range().start;
            if arg_start < byte_range_start || arg_start > byte_range_end {
                continue;
            }
            if let Some(pname) = param_names.get(i) {
                let position = span_to_position(source, line_index, arg_start);
                let label = format!("{}: ", pname);
                hints.push(InlayHint {
                    position,
                    label: InlayHintLabel::String(label),
                    kind: Some(InlayHintKind::PARAMETER),
                    text_edits: None,
                    tooltip: None,
                    padding_left: Some(false),
                    padding_right: Some(true),
                    data: None,
                });
            }
        }

        if return_type.for_annotation() != "any" {
            let rparen = match call_node
                .descendant_tokens()
                .into_iter()
                .find(|t| t.text() == ")")
            {
                Some(t) => t,
                None => continue,
            };
            let after_paren = rparen.text_range().end;
            if after_paren >= byte_range_start && after_paren <= byte_range_end {
                let position = span_to_position(source, line_index, after_paren);
                let label = format!(" -> {}", return_type.for_annotation());
                hints.push(InlayHint {
                    position,
                    label: InlayHintLabel::String(label),
                    kind: Some(InlayHintKind::TYPE),
                    text_edits: None,
                    tooltip: None,
                    padding_left: Some(true),
                    padding_right: Some(false),
                    data: None,
                });
            }
        }
    }

    if settings.inlay_hints_scope_end {
        for (_scope_id, (start, end)) in state.scope_extents.iter().skip(1) {
            if *end >= byte_range_start && *end <= byte_range_end {
                if let Some(node) = find_scope_creating_node_by_range(root, *start, *end) {
                    if let Some(label) = scope_end_label(&node) {
                        let position = span_to_position(source, line_index, *end);
                        hints.push(InlayHint {
                            position,
                            label: InlayHintLabel::String(label),
                            kind: None,
                            text_edits: None,
                            tooltip: None,
                            padding_left: Some(true),
                            padding_right: Some(false),
                            data: None,
                        });
                    }
                }
            }
        }
    }

    hints
}

fn find_scope_creating_node_by_range(
    root: &SyntaxNode,
    start: u32,
    end: u32,
) -> Option<SyntaxNode> {
    let kinds = [
        Kind::NodeBlock,
        Kind::NodeFunctionDecl,
        Kind::NodeClassDecl,
        Kind::NodeConstructorDecl,
        Kind::NodeWhileStmt,
        Kind::NodeForStmt,
        Kind::NodeForInStmt,
        Kind::NodeDoWhileStmt,
    ];
    for kind in kinds {
        for node in root.find_all_nodes(kind.into_syntax_kind()) {
            let r = node.text_range();
            if r.start == start && r.end == end {
                return Some(node);
            }
        }
    }
    None
}

fn scope_end_label(node: &SyntaxNode) -> Option<String> {
    match node.kind_as::<Kind>() {
        Some(Kind::NodeClassDecl) => {
            class_decl_info(node).map(|info| format!("// end {}", info.name))
        }
        Some(Kind::NodeFunctionDecl) => {
            function_decl_info(node).map(|info| format!("// end {}", info.name))
        }
        Some(Kind::NodeConstructorDecl) => Some("// end constructor".to_string()),
        Some(Kind::NodeBlock) => None,
        Some(Kind::NodeWhileStmt) => Some("// end while".to_string()),
        Some(Kind::NodeForStmt) => Some("// end for".to_string()),
        Some(Kind::NodeForInStmt) => Some("// end for…in".to_string()),
        Some(Kind::NodeDoWhileStmt) => Some("// end do…while".to_string()),
        _ => None,
    }
}
