//! LSP signature help: parameter hints at call sites.

use std::collections::HashMap;

use leekscript_rs::analysis::{
    call_argument_count,
    member_expr_member_name, member_expr_receiver_name, primary_expr_resolvable_name,
};
use leekscript_rs::syntax::Kind;
use leekscript_rs::{scope_at_offset, ScopeId};
use sipha::types::IntoSyntaxKind;
use tower_lsp::lsp_types::*;

use crate::document::DocumentState;
use crate::resolve::current_class_at_offset;
use crate::signature_help::{find_function_decl_for_signature_help, find_method_decl};
use crate::util::line_col_utf16_to_byte;

/// Compute signature help (parameter hints) at the given position.
pub fn signature_help_at(
    docs: &HashMap<String, DocumentState>,
    uri: &str,
    line: u32,
    character: u32,
) -> Option<SignatureHelp> {
    let state = docs.get(uri)?;
    let source = state.source.as_str();
    let line_index = &state.line_index;
    let root = state.root.as_ref()?;

    let byte_offset = line_col_utf16_to_byte(source, line_index, line, character)?;
    let node = root.node_at_offset(byte_offset)?;
    let call_node = node.find_ancestor(root, Kind::NodeCallExpr.into_syntax_kind())?;
    let callee = call_node.child_nodes().next()?;
    let name = if callee.kind_as::<Kind>() == Some(Kind::NodeMemberExpr) {
        member_expr_member_name(&callee)
    } else {
        primary_expr_resolvable_name(&callee)
    }?;
    let arity = call_argument_count(&call_node);
    let scope_id = scope_at_offset(&state.scope_extents, call_node.text_range().start);
    let mut id = Some(scope_id);
    let (param_types, return_type) = loop {
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
    let (param_types, return_type, param_names_opt, doc_opt) =
        if param_types.is_empty() && return_type.for_annotation() == "any"
            && callee.kind_as::<Kind>() == Some(Kind::NodeMemberExpr)
        {
            let receiver = member_expr_receiver_name(&callee).unwrap_or_default();
            let class_name = if receiver == "this" {
                current_class_at_offset(root, call_node.text_range().start)
            } else {
                state
                    .scope_store
                    .get(ScopeId(0))
                    .filter(|s| s.has_class(&receiver))
                    .map(|_| receiver.clone())
            };
            if let Some(ref cname) = class_name {
                if let Some(members) = state.scope_store.get_class_members(cname) {
                    let sig = members
                        .static_methods
                        .get(&name)
                        .or_else(|| members.methods.get(&name));
                    if let Some((p, r, _)) = sig {
                        let (param_names_opt, doc_opt) =
                            find_method_decl(root, cname, &name, arity)
                                .map(|(decl_node, names)| {
                                    let r = decl_node.text_range();
                                    let doc = state.doc_map.get(&(r.start, r.end)).cloned();
                                    (Some(names), doc)
                                })
                                .unwrap_or((None, None));
                        (p.clone(), r.clone(), param_names_opt, doc_opt)
                    } else {
                        let fd = find_function_decl_for_signature_help(root, &name, arity)
                            .map(|(decl_node, param_names)| {
                                let r = decl_node.text_range();
                                let doc = state.doc_map.get(&(r.start, r.end)).cloned();
                                (Some(param_names), doc)
                            })
                            .unwrap_or((None, None));
                        (param_types, return_type, fd.0, fd.1)
                    }
                } else {
                    let fd = find_function_decl_for_signature_help(root, &name, arity)
                        .map(|(decl_node, param_names)| {
                            let r = decl_node.text_range();
                            let doc = state.doc_map.get(&(r.start, r.end)).cloned();
                            (Some(param_names), doc)
                        })
                        .unwrap_or((None, None));
                    (param_types, return_type, fd.0, fd.1)
                }
            } else {
                let fd = find_function_decl_for_signature_help(root, &name, arity)
                    .map(|(decl_node, param_names)| {
                        let r = decl_node.text_range();
                        let doc = state.doc_map.get(&(r.start, r.end)).cloned();
                        (Some(param_names), doc)
                    })
                    .unwrap_or((None, None));
                (param_types, return_type, fd.0, fd.1)
            }
        } else {
            let fd = find_function_decl_for_signature_help(root, &name, arity)
                .map(|(decl_node, param_names)| {
                    let r = decl_node.text_range();
                    let doc = state.doc_map.get(&(r.start, r.end)).cloned();
                    (Some(param_names), doc)
                })
                .unwrap_or((None, None));
            (param_types, return_type, fd.0, fd.1)
        };
    if param_types.is_empty() && return_type.for_annotation() == "any" {
        return None;
    }

    let mut param_labels = Vec::with_capacity(param_types.len());
    for (i, ty) in param_types.iter().enumerate() {
        let param_label = param_names_opt
            .as_ref()
            .and_then(|names| names.get(i).cloned())
            .map(|pname| format!("{}: {}", pname, ty.for_annotation()))
            .unwrap_or_else(|| format!("{}: {}", i, ty.for_annotation()));
        let param_doc = doc_opt.as_ref().and_then(|d| {
            let pname = param_names_opt.as_ref().and_then(|n| n.get(i))?;
            d.params
                .iter()
                .find(|(name, _)| name == pname)
                .map(|(_, desc)| desc.clone())
        });
        param_labels.push(ParameterInformation {
            label: ParameterLabel::Simple(param_label),
            documentation: param_doc.map(Documentation::String),
        });
    }
    let label = format!(
        "{}({}) -> {}",
        name,
        param_types
            .iter()
            .enumerate()
            .map(|(i, t)| {
                param_names_opt
                    .as_ref()
                    .and_then(|names| names.get(i).cloned())
                    .map(|pname| format!("{}: {}", pname, t.for_annotation()))
                    .unwrap_or_else(|| format!("{}: {}", i, t.for_annotation()))
            })
            .collect::<Vec<_>>()
            .join(", "),
        return_type.for_annotation()
    );
    let sig_doc = doc_opt.as_ref().map(|d| {
        let mut s = String::new();
        if let Some(ref b) = d.brief {
            s.push_str(b);
            s.push_str("\n\n");
        }
        if !d.description.is_empty() {
            s.push_str(&d.description);
        }
        Documentation::String(s.trim().to_string())
    });
    let sig = SignatureInformation {
        label: label.clone(),
        documentation: sig_doc,
        parameters: Some(param_labels),
        active_parameter: None,
    };
    let lparen = call_node
        .descendant_tokens()
        .into_iter()
        .find(|t| t.text() == "(")?;
    let args_start = lparen.text_range().end;
    let args_end = byte_offset.min(call_node.text_range().end);
    let arg_span = &source[args_start as usize..args_end as usize];
    let active_parameter = arg_span.matches(',').count();
    Some(SignatureHelp {
        signatures: vec![sig],
        active_signature: Some(0),
        active_parameter: Some(active_parameter as u32),
    })
}
