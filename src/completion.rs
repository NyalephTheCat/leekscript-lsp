//! Completion provider: identifier and member completions.

use std::collections::HashMap;

use leekscript_rs::analysis::MemberVisibility;
use leekscript_rs::syntax::Kind;
use leekscript_rs::{scope_at_offset, ScopeId};
use sipha::red::{SyntaxElement, SyntaxNode};
use sipha::types::IntoSyntaxKind;
use tower_lsp::lsp_types::*;

use crate::document::DocumentState;
use crate::resolve::{
    current_class_at_offset, identifier_prefix, is_same_or_subclass, iter_self_and_descendants,
};
use crate::util::{line_col_utf16_to_byte, line_prefix_utf16};

/// Compute completion items at the given position.
pub fn completion_at(
    docs: &HashMap<String, DocumentState>,
    uri: &str,
    line: u32,
    character: u32,
) -> Option<CompletionResponse> {
    let state = docs.get(uri)?;
    let source = state.source.as_str();
    let line_index = &state.line_index;

    let byte_offset = line_col_utf16_to_byte(source, line_index, line, character).unwrap_or(0);
    let (line_exp, char_exp) = line_index.line_col_utf16(source, byte_offset);
    let prefix = line_prefix_utf16(source, line_index, line_exp, char_exp)?;
    let prefix = identifier_prefix(&prefix);

    // Collect (name, kind, detail) so we can show correct icons and optional detail.
    let mut completion_entries: HashMap<String, (CompletionItemKind, Option<String>)> =
        HashMap::new();

    // Index/key access: after `.` offer member completion (fields/methods of receiver type).
    let root = state.root.as_ref();
    let in_member_context: Option<leekscript_rs::Type> = root.and_then(|root| {
        let member_expr = root
            .find_all_nodes(Kind::NodeMemberExpr.into_syntax_kind())
            .into_iter()
            .find(|n| {
                let r = n.text_range();
                r.start <= byte_offset && byte_offset <= r.end
            })?;
        let dot_end = member_expr
            .children()
            .find(|e| {
                if let SyntaxElement::Token(t) = e {
                    !t.is_trivia() && t.text() == "."
                } else {
                    false
                }
            })
            .map(|e| e.text_range().end)?;
        if byte_offset >= dot_end {
            let member_start = member_expr.text_range().start;
            let parent = member_expr.ancestors(root).into_iter().next()?;
            let mut receiver: Option<SyntaxNode> = None;
            for child in parent.children() {
                if let SyntaxElement::Node(n) = child {
                    let r = n.text_range();
                    if r.start == member_start && r.end == member_expr.text_range().end {
                        break;
                    }
                    if r.end <= member_start {
                        receiver = Some(n);
                    }
                }
            }
            let receiver = receiver?;
            let ty = state
                .type_map
                .get(&(receiver.text_range().start, receiver.text_range().end))
                .cloned()
                .or_else(|| {
                    for desc in iter_self_and_descendants(&receiver) {
                        let r = desc.text_range();
                        if let Some(t) = state.type_map.get(&(r.start, r.end)) {
                            return Some(t.clone());
                        }
                    }
                    None
                })?;
            Some(ty)
        } else {
            None
        }
    });

    if let Some(ref receiver_ty) = in_member_context {
        let receiver_class = match receiver_ty {
            leekscript_rs::Type::Instance(c) => c.as_str(),
            leekscript_rs::Type::Class(Some(c)) => c.as_str(),
            _ => "",
        };
        if !receiver_class.is_empty() {
            let current_class = root.and_then(|r| current_class_at_offset(r, byte_offset));
            let visible = |vis: &MemberVisibility| -> bool {
                match vis {
                    MemberVisibility::Public => true,
                    MemberVisibility::Protected => {
                        current_class.as_deref().map_or(false, |cur| {
                            is_same_or_subclass(&state.class_super, cur, receiver_class)
                        })
                    }
                    MemberVisibility::Private => current_class.as_deref() == Some(receiver_class),
                }
            };
            if let Some(members) = state.scope_store.get_class_members(receiver_class) {
                for (name, (ty, vis)) in &members.fields {
                    if visible(vis) {
                        completion_entries.insert(
                            name.clone(),
                            (
                                CompletionItemKind::FIELD,
                                Some(ty.for_annotation().to_string()),
                            ),
                        );
                    }
                }
                for (name, (_, ret, vis)) in &members.methods {
                    if visible(vis) {
                        completion_entries.insert(
                            name.clone(),
                            (
                                CompletionItemKind::METHOD,
                                Some(ret.for_annotation().to_string()),
                            ),
                        );
                    }
                }
                if matches!(receiver_ty, leekscript_rs::Type::Class(Some(_))) {
                    for (name, (ty, vis)) in &members.static_fields {
                        if visible(vis) {
                            completion_entries.insert(
                                name.clone(),
                                (
                                    CompletionItemKind::FIELD,
                                    Some(ty.for_annotation().to_string()),
                                ),
                            );
                        }
                    }
                    for (name, (_, ret, vis)) in &members.static_methods {
                        if visible(vis) {
                            completion_entries.insert(
                                name.clone(),
                                (
                                    CompletionItemKind::METHOD,
                                    Some(ret.for_annotation().to_string()),
                                ),
                            );
                        }
                    }
                }
            }
        }
    }

    if in_member_context.is_none() {
        let scope_id = scope_at_offset(&state.scope_extents, byte_offset);
        let root_scope_id = ScopeId(0);
        let mut id = Some(scope_id);
        while let Some(scope_id) = id {
            let scope = match state.scope_store.get(scope_id) {
                Some(s) => s,
                None => break,
            };
            for name in scope.variable_names() {
                completion_entries.insert(name, (CompletionItemKind::VARIABLE, None));
            }
            if scope_id == root_scope_id {
                for name in scope.function_names() {
                    completion_entries.insert(
                        name,
                        (CompletionItemKind::FUNCTION, Some("function".to_string())),
                    );
                }
                for name in scope.class_names() {
                    completion_entries.insert(
                        name,
                        (CompletionItemKind::CLASS, Some("class".to_string())),
                    );
                }
                for name in scope.global_names() {
                    completion_entries.insert(
                        name,
                        (CompletionItemKind::CONSTANT, Some("global".to_string())),
                    );
                }
            }
            id = scope.parent;
        }
        for kw in leekscript_rs::KEYWORDS {
            completion_entries.insert(
                (*kw).to_string(),
                (CompletionItemKind::KEYWORD, None),
            );
        }
    }

    let mut items: Vec<CompletionItem> = completion_entries
        .into_iter()
        .filter(|(name, _)| name.starts_with(&prefix))
        .map(|(name, (kind, detail))| CompletionItem {
            label: name.clone(),
            kind: Some(kind),
            detail,
            filter_text: Some(name.clone()),
            ..Default::default()
        })
        .collect();
    items.sort_by(|a, b| a.label.cmp(&b.label));
    Some(CompletionResponse::Array(items))
}
