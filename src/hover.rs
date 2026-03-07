//! Hover provider: type and doc tooltips at cursor position.

use std::collections::HashMap;

use leekscript_rs::doc_comment::DocComment;
use leekscript_rs::analysis::{
    call_argument_count, class_decl_info, function_decl_info,
    member_expr_member_name, member_expr_receiver_name, primary_expr_resolvable_name,
    ResolvedSymbol,
};
use leekscript_rs::syntax::Kind;
use leekscript_rs::{scope_at_offset, LineIndex, ScopeId};
use sipha::red::SyntaxNode;
use sipha::types::IntoSyntaxKind;
use tower_lsp::lsp_types::{MarkupContent, MarkupKind, *};

use crate::document::{DocumentState, RootSymbolKind};
use crate::doc_comment::{
    format_class_hover_summary, format_doc_comment_markdown, hover_markdown,
};
use crate::resolve::current_class_at_offset;
use crate::signature_help::{find_function_decl_for_signature_help, find_method_decl};
use crate::util::{line_col_utf16_to_byte, span_to_position};

/// Compute hover content at the given position.
pub fn hover_at(
    docs: &HashMap<String, DocumentState>,
    uri: &str,
    line: u32,
    character: u32,
) -> Option<Hover> {
    let state = docs.get(uri)?;
    let source = state.source.as_str();
    let line_index = &state.line_index;
    let root = state.root.as_ref()?;

    let byte_offset = line_col_utf16_to_byte(source, line_index, line, character)?;
    let node = root.node_at_offset(byte_offset)?;
    let range = node.text_range();

    // When hovering on a class/function declaration name, node_at_offset returns the decl node (not the ident token).
    // Detect that and show Doxygen from doc_map keyed by the decl span, and use name span for hover range.
    if let Some(Kind::NodeClassDecl) = node.kind_as::<Kind>() {
        if let Some(info) = class_decl_info(&node) {
            let name_span = info.name_span;
            if byte_offset >= name_span.start && byte_offset <= name_span.end {
                let decl_span = node.text_range();
                let doc = state.doc_map.get(&(decl_span.start, decl_span.end)).cloned();
                let method_type_strings = state
                    .scope_store
                    .get_class_members(&info.name)
                    .map(|members| {
                        let mut map = std::collections::HashMap::new();
                        for (name, (params, ret, _)) in &members.methods {
                            map.insert(
                                name.clone(),
                                leekscript_rs::Type::function(params.clone(), ret.clone()).for_annotation(),
                            );
                        }
                        for (name, (params, ret, _)) in &members.static_methods {
                            map.insert(
                                name.clone(),
                                leekscript_rs::Type::function(params.clone(), ret.clone()).for_annotation(),
                            );
                        }
                        map
                    });
                let contents = format_class_hover_summary(
                    &node,
                    root,
                    &info.name,
                    info.super_class.as_deref(),
                    method_type_strings.as_ref(),
                );
                return Some(hover_response(source, line_index, contents, doc, name_span.start, name_span.end));
            }
        }
    } else if let Some(Kind::NodeFunctionDecl) = node.kind_as::<Kind>() {
        if let Some(info) = function_decl_info(&node) {
            let name_span = info.name_span;
            if byte_offset >= name_span.start && byte_offset <= name_span.end {
                let decl_span = node.text_range();
                let doc = state.doc_map.get(&(decl_span.start, decl_span.end)).cloned();
                let type_str = function_decl_type_string(state, root, &node, &info);
                let contents = format!("{}: {}", info.name, type_str);
                return Some(hover_response(source, line_index, contents, doc, name_span.start, name_span.end));
            }
        }
    }

    // Hover on call expression or its callee: show function/method signature and doc.
    let call_ancestor = node.find_ancestor(root, Kind::NodeCallExpr.into_syntax_kind());
    let call_node = if node.kind_as::<Kind>() == Some(Kind::NodeCallExpr) {
        Some(&node)
    } else {
        call_ancestor.as_ref()
    };
    if let Some(call_node) = call_node {
        let call_range = call_node.text_range();
        if byte_offset >= call_range.start && byte_offset <= call_range.end {
            if let Some((signature, doc, callee_start, callee_end)) =
                hover_for_call(state, root, call_node)
            {
                return Some(hover_response(
                    source,
                    line_index,
                    signature,
                    doc,
                    callee_start,
                    callee_end,
                ));
            }
        }
    }

    let ty = match state.type_at_offset(byte_offset) {
        Some(t) => t,
        None => {
            if let Some((content, r_start, r_end)) =
                hover_for_literal_or_keyword(&node, source)
            {
                let hover_contents = HoverContents::Markup(MarkupContent {
                    kind: MarkupKind::Markdown,
                    value: content,
                });
                let hover_range = Some(Range {
                    start: span_to_position(source, line_index, r_start),
                    end: span_to_position(source, line_index, r_end),
                });
                return Some(Hover {
                    contents: hover_contents,
                    range: hover_range,
                });
            }
            return None;
        }
    };
    let contents_str = if node.kind_as::<Kind>() == Some(Kind::TokIdent) {
        let name = std::str::from_utf8(node.text(source.as_bytes()))
            .unwrap_or("")
            .to_string();
        match state.symbol_at_offset(byte_offset) {
            Some(ResolvedSymbol::Global(_)) => {
                format!("global `{}`: {}", name, ty.for_annotation())
            }
            Some(ResolvedSymbol::Variable(_)) => {
                format!("variable `{}`: {}", name, ty.for_annotation())
            }
            Some(_) => format!("{}: {}", name, ty.for_annotation()),
            None => ty.for_annotation().to_string(),
        }
    } else {
        ty.for_annotation().to_string()
    };

    // Attach Doxygen doc when hovering on an identifier that resolves to a class or function (same file or from includes).
    let doc = (node.kind_as::<Kind>() == Some(Kind::TokIdent)).then(|| ()).and_then(|_| {
        let name = std::str::from_utf8(node.text(source.as_bytes()))
            .unwrap_or("")
            .to_string();
        let sym = state.symbol_at_offset(byte_offset)?;
        let root_scope = state.scope_store.get(ScopeId(0))?;
        let (name_start, name_end) = match &sym {
            ResolvedSymbol::Class(n) => {
                let span = root_scope.get_class_first_span(n)?;
                (span.start, span.end)
            }
            ResolvedSymbol::Function(n, _) => {
                let span = root_scope.get_function_first_span(n)?;
                (span.start, span.end)
            }
            _ => return None,
        };
        // Prefer definition_map when available (correct path for includes).
        if let (Some(ref tree), Some(ref main_path)) = (&state.include_tree, &state.main_path) {
            let kind = match &sym {
                ResolvedSymbol::Class(_) => RootSymbolKind::Class,
                ResolvedSymbol::Function(_, _) => RootSymbolKind::Function,
                _ => return None,
            };
            if let Some((ref path, start, end)) = state.definition_map.get(&(name.clone(), kind)) {
                let root = tree.root_for_path(main_path, path)?;
                let (decl_start, decl_end) = leekscript_rs::decl_span_for_name_span(root, *start, *end)?;
                let doc_map = if state.main_path.as_ref() == Some(path) {
                    Some(&state.doc_map)
                } else {
                    state.include_doc_maps.as_ref().and_then(|m| m.get(path))
                };
                return doc_map.and_then(|dm| dm.get(&(decl_start, decl_end)).cloned());
            }
        }
        // Same-file: resolve name span to declaration span (doc_map is keyed by decl span).
        let root = state.root.as_ref()?;
        let (decl_start, decl_end) = leekscript_rs::decl_span_for_name_span(root, name_start, name_end)?;
        state.doc_map.get(&(decl_start, decl_end)).cloned()
    });

    let doc_str = doc.as_ref().map(|d| format_doc_comment_markdown(d));
    let value = hover_markdown(
        &contents_str,
        doc_str.as_deref().filter(|s| !s.is_empty()),
    );
    let hover_contents = HoverContents::Markup(MarkupContent {
        kind: MarkupKind::Markdown,
        value,
    });

    let hover_range = Some(Range {
        start: span_to_position(source, line_index, range.start),
        end: span_to_position(source, line_index, range.end),
    });
    Some(Hover {
        contents: hover_contents,
        range: hover_range,
    })
}

/// Return the type signature string for a function/method declaration.
fn function_decl_type_string(
    state: &DocumentState,
    root: &SyntaxNode,
    _decl_node: &SyntaxNode,
    info: &leekscript_rs::analysis::FunctionDeclInfo,
) -> String {
    let decl_start = _decl_node.text_range().start;
    let class_name = current_class_at_offset(root, decl_start);
    if let Some(ref cname) = class_name {
        if let Some(members) = state.scope_store.get_class_members(cname) {
            if let Some((params, ret, _)) = members.methods.get(&info.name) {
                return leekscript_rs::Type::function(params.clone(), ret.clone()).for_annotation();
            }
            if let Some((params, ret, _)) = members.static_methods.get(&info.name) {
                return leekscript_rs::Type::function(params.clone(), ret.clone()).for_annotation();
            }
        }
    }
    state
        .scope_store
        .get(ScopeId(0))
        .and_then(|scope| scope.get_function_type_as_value(&info.name))
        .map(|ty| ty.for_annotation())
        .unwrap_or_else(|| "Function<... => any>".to_string())
}

/// Resolve a call expression to a signature string and optional doc for hover.
fn hover_for_call(
    state: &DocumentState,
    root: &SyntaxNode,
    call_node: &SyntaxNode,
) -> Option<(String, Option<DocComment>, u32, u32)> {
    let callee = call_node.child_nodes().next()?;
    let callee_range = callee.text_range();
    let name = if callee.kind_as::<Kind>() == Some(Kind::NodeMemberExpr) {
        member_expr_member_name(&callee)
    } else {
        primary_expr_resolvable_name(&callee)
    }?;
    let arity = call_argument_count(call_node);
    let scope_id = scope_at_offset(&state.scope_extents, call_node.text_range().start);
    let mut id = Some(scope_id);
    let (param_types, return_type) = loop {
        let scope_id = match id {
            Some(s) => s,
            None => break (Vec::new(), leekscript_rs::types::Type::any()),
        };
        let scope = state.scope_store.get(scope_id)?;
        if let Some((p, r)) = scope.get_function_type(&name, arity) {
            break (p, r);
        }
        id = scope.parent;
    };
    let (param_types, return_type, doc_opt) =
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
                        let doc_opt = find_method_decl(root, cname, &name, arity)
                            .and_then(|(decl_node, _)| {
                                let r = decl_node.text_range();
                                state.doc_map.get(&(r.start, r.end)).cloned()
                            });
                        (p.clone(), r.clone(), doc_opt)
                    } else {
                        let doc_opt = find_function_decl_for_signature_help(root, &name, arity)
                            .and_then(|(decl_node, _)| {
                                let r = decl_node.text_range();
                                state.doc_map.get(&(r.start, r.end)).cloned()
                            });
                        (param_types, return_type, doc_opt)
                    }
                } else {
                    let doc_opt = find_function_decl_for_signature_help(root, &name, arity)
                        .and_then(|(decl_node, _)| {
                            let r = decl_node.text_range();
                            state.doc_map.get(&(r.start, r.end)).cloned()
                        });
                    (param_types, return_type, doc_opt)
                }
            } else {
                let doc_opt = find_function_decl_for_signature_help(root, &name, arity)
                    .and_then(|(decl_node, _)| {
                        let r = decl_node.text_range();
                        state.doc_map.get(&(r.start, r.end)).cloned()
                    });
                (param_types, return_type, doc_opt)
            }
        } else {
            let doc_opt = find_function_decl_for_signature_help(root, &name, arity)
                .and_then(|(decl_node, _)| {
                    let r = decl_node.text_range();
                    state.doc_map.get(&(r.start, r.end)).cloned()
                });
            (param_types, return_type, doc_opt)
        };
    if param_types.is_empty() && return_type.for_annotation() == "any" {
        return None;
    }
    let param_list = param_types
        .iter()
        .map(|t| t.for_annotation())
        .collect::<Vec<_>>()
        .join(", ");
    let signature = format!("{}({}): {}", name, param_list, return_type.for_annotation());
    Some((signature, doc_opt, callee_range.start, callee_range.end))
}

/// Return hover content for a literal or keyword token when type is not available.
fn hover_for_literal_or_keyword(
    node: &SyntaxNode,
    source: &str,
) -> Option<(String, u32, u32)> {
    let range = node.text_range();
    let content = match node.kind_as::<Kind>()? {
        Kind::TokString => {
            let text = std::str::from_utf8(node.text(source.as_bytes())).unwrap_or("");
            let len = text.chars().count();
            if len <= 1 {
                "string".to_string()
            } else {
                format!("string ({} chars)", len)
            }
        }
        Kind::TokNumber => "number".to_string(),
        Kind::KwVar => "`var` — local variable declaration".to_string(),
        Kind::KwLet => "`let` — immutable local binding".to_string(),
        Kind::KwGlobal => "`global` — global variable declaration".to_string(),
        Kind::KwConst => "`const` — constant declaration".to_string(),
        Kind::KwFunction => "`function` — function declaration".to_string(),
        Kind::KwClass => "`class` — class declaration".to_string(),
        Kind::KwIf => "`if` — conditional branch".to_string(),
        Kind::KwElse => "`else` — else branch".to_string(),
        Kind::KwWhile => "`while` — while loop".to_string(),
        Kind::KwFor => "`for` — for loop".to_string(),
        Kind::KwDo => "`do` — do-while loop".to_string(),
        Kind::KwReturn => "`return` — return from function".to_string(),
        Kind::KwBreak => "`break` — break out of loop".to_string(),
        Kind::KwContinue => "`continue` — continue to next iteration".to_string(),
        Kind::KwInclude => "`include` — include another file".to_string(),
        Kind::KwNew => "`new` — construct instance".to_string(),
        Kind::KwTrue => "`true` — boolean literal".to_string(),
        Kind::KwFalse => "`false` — boolean literal".to_string(),
        Kind::KwNull => "`null` — null literal".to_string(),
        Kind::KwThis => "`this` — current instance".to_string(),
        Kind::KwSuper => "`super` — parent class".to_string(),
        Kind::KwIn => "`in` — for-in loop / membership".to_string(),
        Kind::KwInstanceof => "`instanceof` — type check".to_string(),
        Kind::KwAnd => "`and` — logical and".to_string(),
        Kind::KwOr => "`or` — logical or".to_string(),
        Kind::KwXor => "`xor` — logical xor".to_string(),
        Kind::KwNot => "`not` — logical not".to_string(),
        Kind::KwAs => "`as` — type cast".to_string(),
        Kind::KwPublic => "`public` — public visibility".to_string(),
        Kind::KwPrivate => "`private` — private visibility".to_string(),
        Kind::KwProtected => "`protected` — protected visibility".to_string(),
        Kind::KwStatic => "`static` — static member".to_string(),
        Kind::KwFinal => "`final` — final member".to_string(),
        Kind::KwAbstract => "`abstract` — abstract class".to_string(),
        Kind::KwExtends => "`extends` — class inheritance".to_string(),
        Kind::KwConstructor => "`constructor` — constructor declaration".to_string(),
        Kind::KwTry => "`try` — try block".to_string(),
        Kind::KwCatch => "`catch` — catch block".to_string(),
        Kind::KwThrow => "`throw` — throw exception".to_string(),
        Kind::KwSwitch => "`switch` — switch statement".to_string(),
        Kind::KwCase => "`case` — case label".to_string(),
        Kind::KwDefault => "`default` — default case".to_string(),
        _ => return None,
    };
    Some((content, range.start, range.end))
}

/// Build a Hover for declaration-name hover (class/function) with optional Doxygen doc.
fn hover_response(
    source: &str,
    line_index: &LineIndex,
    contents_str: String,
    doc: Option<DocComment>,
    range_start: u32,
    range_end: u32,
) -> Hover {
    let doc_str = doc.as_ref().map(|d| format_doc_comment_markdown(d));
    let value = hover_markdown(
        &contents_str,
        doc_str.as_deref().filter(|s| !s.is_empty()),
    );
    let hover_contents = HoverContents::Markup(MarkupContent {
        kind: MarkupKind::Markdown,
        value,
    });
    let hover_range = Some(Range {
        start: span_to_position(source, line_index, range_start),
        end: span_to_position(source, line_index, range_end),
    });
    Hover {
        contents: hover_contents,
        range: hover_range,
    }
}
