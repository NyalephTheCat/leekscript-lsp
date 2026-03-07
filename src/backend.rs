//! LSP backend: document analysis, hover, go-to-definition, completion, and other handlers.

use std::collections::HashMap;
use std::path::PathBuf;

use parking_lot::RwLock;

use leekscript_rs::doc_comment::DocComment;
use leekscript_rs::analysis::{
    call_argument_count, call_argument_node, class_decl_info, class_field_info, function_decl_info,
    member_expr_member_name, member_expr_receiver_name, primary_expr_resolvable_name, var_decl_info,
    MemberVisibility, ResolvedSymbol, VarDeclKind,
};
use leekscript_rs::formatter::FormatterOptions;
use leekscript_rs::syntax::Kind;
use leekscript_rs::{
    format, parse_signatures, scope_at_offset, DocumentAnalysis, LineIndex, ScopeId,
};
use sipha::red::{SyntaxElement, SyntaxNode};
use sipha::types::IntoSyntaxKind;
use tower_lsp::lsp_types::*;
use tower_lsp::Client;

use crate::config::LspSettings;
use crate::diagnostics::semantic_to_lsp;
use crate::document::{DocumentState, RootSymbolKind};
use crate::doc_comment::{format_class_hover_summary, format_doc_comment_markdown};
use crate::include::{include_path_at_offset, tree_file_contents};
use crate::resolve::{
    current_class_at_offset, identifier_prefix, is_same_or_subclass,
    iter_self_and_descendants, symbol_matches,
};
use crate::signature_help::{find_function_decl_for_signature_help, find_method_decl};
use crate::util::{
    canonical_path, line_col_utf16_to_byte, line_prefix_utf16,
    path_to_uri, span_to_position, uri_to_path,
};

/// Embedded standard library signature files (constants and functions).
const STDLIB_CONSTANTS_SIG: &str = include_str!("../signatures/stdlib_constants.sig");
const STDLIB_FUNCTIONS_SIG: &str = include_str!("../signatures/stdlib_functions.sig");

/// Runs parse + analysis on a blocking thread. Returns (uri, analysis, lsp_diagnostics).
/// Used by [`Backend::run_analysis_async`] so the async runtime is not blocked.
fn run_analysis_blocking(
    uri: String,
    source: String,
    main_path: Option<PathBuf>,
    signature_roots: Vec<SyntaxNode>,
    existing_root: Option<SyntaxNode>,
) -> (String, DocumentAnalysis, Vec<Diagnostic>) {
    let analysis = DocumentAnalysis::new(
        &source,
        main_path.as_deref(),
        &signature_roots,
        existing_root,
    );
    let lsp_diags: Vec<Diagnostic> = analysis
        .diagnostics
        .iter()
        .map(|d| semantic_to_lsp(d, &analysis.source, &analysis.line_index))
        .collect();
    (uri, analysis, lsp_diags)
}

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

pub struct Backend {
    pub client: Client,
    pub documents: RwLock<HashMap<String, DocumentState>>,
    pub settings: RwLock<LspSettings>,
}
impl Backend {
    /// Build signature roots from current settings (embedded stdlib + optional files).
    fn get_signature_roots(&self) -> Vec<SyntaxNode> {
        let settings = self.settings.read();
        let mut roots = Vec::new();
        if settings.load_stdlib_signatures {
            if let Ok(Some(node)) = parse_signatures(STDLIB_CONSTANTS_SIG) {
                roots.push(node);
            }
            if let Ok(Some(node)) = parse_signatures(STDLIB_FUNCTIONS_SIG) {
                roots.push(node);
            }
        }
        for path in &settings.signature_files {
            if let Ok(content) = std::fs::read_to_string(path) {
                if let Ok(Some(node)) = parse_signatures(&content) {
                    roots.push(node);
                }
            }
        }
        roots
    }

    /// Run analysis on a blocking thread, then store state and publish diagnostics.
    /// Keeps the async runtime responsive during heavy parse/analysis.
    /// When the client sends `$/cancelRequest`, tower-lsp aborts the request future so we stop
    /// waiting; the blocking task may still run to completion (there is no token to abort it).
    pub(crate) async fn run_analysis_async(
        &self,
        uri: String,
        source: String,
        existing_root: Option<SyntaxNode>,
    ) {
        let main_path = uri_to_path(&uri);
        let signature_roots = self.get_signature_roots();
        let result = tokio::task::spawn_blocking(move || {
            run_analysis_blocking(uri, source, main_path, signature_roots, existing_root)
        })
        .await;

        match result {
            Ok((uri, analysis, lsp_diags)) => {
                let n_diag = analysis.diagnostics.len();
                if let Ok(url) = uri.parse::<Url>() {
                    {
                        let mut docs = self.documents.write();
                        docs.insert(uri.clone(), analysis);
                    }
                    self.client
                        .log_message(
                            MessageType::LOG,
                            format!("leekscript-lsp: analysis done uri={uri} diagnostics={n_diag}"),
                        )
                        .await;
                    self.client.publish_diagnostics(url, lsp_diags, None).await;
                }
            }
            Err(e) => {
                let _ = self
                    .client
                    .log_message(
                        MessageType::ERROR,
                        format!("leekscript-lsp: analysis task failed: {e}"),
                    )
                    .await;
            }
        }
    }

    /// Send a LOG-level message to the client only when the "trace" setting is enabled.
    pub(crate) async fn log_trace(&self, msg: String) {
        if self.settings.read().trace {
            let _ = self.client.log_message(MessageType::LOG, msg).await;
        }
    }

    pub(crate) fn hover_at(&self, uri: &str, line: u32, character: u32) -> Option<Hover> {
        let docs = self.documents.read();
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
                    return Some(self.hover_response(source, line_index, contents, doc, name_span.start, name_span.end));
                }
            }
        } else if let Some(Kind::NodeFunctionDecl) = node.kind_as::<Kind>() {
            if let Some(info) = function_decl_info(&node) {
                let name_span = info.name_span;
                if byte_offset >= name_span.start && byte_offset <= name_span.end {
                    let decl_span = node.text_range();
                    let doc = state.doc_map.get(&(decl_span.start, decl_span.end)).cloned();
                    let type_str = self.function_decl_type_string(state, root, &node, &info);
                    let contents = format!("{}: {}", info.name, type_str);
                    return Some(self.hover_response(source, line_index, contents, doc, name_span.start, name_span.end));
                }
            }
        }

        let ty = state.type_at_offset(byte_offset)?;
        let contents_str = if node.kind_as::<Kind>() == Some(Kind::TokIdent) {
            let name = std::str::from_utf8(node.text(source.as_bytes()))
                .unwrap_or("")
                .to_string();
            if state.symbol_at_offset(byte_offset).is_some() {
                format!("{}: {}", name, ty.for_annotation())
            } else {
                ty.for_annotation().to_string()
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

        let hover_contents = if let Some(doc) = doc {
            let doc_md = format_doc_comment_markdown(&doc);
            HoverContents::Array(vec![
                MarkedString::String(contents_str),
                MarkedString::String(doc_md),
            ])
        } else {
            HoverContents::Scalar(MarkedString::String(contents_str))
        };

        let hover_range = Some(Range {
            start: span_to_position(source, line_index, range.start),
            end: span_to_position(source, line_index, range.end),
        });
        Some(Hover {
            contents: hover_contents,
            range: hover_range,
        })
    }

    /// Return the type signature string for a function/method declaration (e.g. `Function< => void>` or `Function<integer => Entity>`).
    fn function_decl_type_string(
        &self,
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

    /// Build a Hover for declaration-name hover (class/function) with optional Doxygen doc.
    fn hover_response(
        &self,
        source: &str,
        line_index: &LineIndex,
        contents_str: String,
        doc: Option<DocComment>,
        range_start: u32,
        range_end: u32,
    ) -> Hover {
        let hover_contents = if let Some(doc) = doc {
            let doc_md = format_doc_comment_markdown(&doc);
            HoverContents::Array(vec![
                MarkedString::String(contents_str),
                MarkedString::String(doc_md),
            ])
        } else {
            HoverContents::Scalar(MarkedString::String(contents_str))
        };
        let hover_range = Some(Range {
            start: span_to_position(source, line_index, range_start),
            end: span_to_position(source, line_index, range_end),
        });
        Hover {
            contents: hover_contents,
            range: hover_range,
        }
    }

    /// Compute inlay hints (e.g. `: type` after variable names) for the given document range.
    pub(crate) fn inlay_hints_at(
        &self,
        uri: &str,
        range_start_line: u32,
        range_start_character: u32,
        range_end_line: u32,
        range_end_character: u32,
    ) -> Vec<InlayHint> {
        let docs = self.documents.read();
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

        // Inlay hints for call sites: parameter names before arguments, return type after closing paren.
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
                    None => break (Vec::new(), leekscript_rs::types::Type::Any),
                };
                let scope = match state.scope_store.get(scope_id) {
                    Some(s) => s,
                    None => break (Vec::new(), leekscript_rs::types::Type::Any),
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

        // Scope-end inlay hints (e.g. "// end Cell" at closing brace).
        if self.settings.read().inlay_hints_scope_end {
            for (_scope_id, (start, end)) in state.scope_extents.iter().skip(1) {
                if *end >= byte_range_start && *end <= byte_range_end {
                    if let Some(node) =
                        find_scope_creating_node_by_range(root, *start, *end)
                    {
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

    pub(crate) fn goto_definition_at(
        &self,
        uri: &str,
        line: u32,
        character: u32,
    ) -> Option<Vec<Location>> {
        let docs = self.documents.read();
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
                    let url = uri.parse().ok()?;
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
        let url = uri.parse().ok()?;
        let range = Range {
            start: span_to_position(source, line_index, def_span.start),
            end: span_to_position(source, line_index, def_span.end),
        };
        Some(vec![Location { uri: url, range }])
    }

    pub(crate) fn references_at(
        &self,
        uri: &str,
        line: u32,
        character: u32,
        include_declaration: bool,
    ) -> Option<Vec<Location>> {
        let docs = self.documents.read();
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
                        if let Some(url) = uri.parse().ok() {
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
                        if let Some(url) = uri.parse().ok() {
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
                if let Some(url) = main_uri_str.parse().ok() {
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
            if let Some(url) = uri.parse().ok() {
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
            if let Some(url) = uri.parse().ok() {
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
            let other_url: Url = match other_uri.parse() {
                Ok(u) => u,
                Err(_) => continue,
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

    /// Compute a workspace edit that renames the symbol at the given position to `new_name`.
    pub(crate) fn rename_at(
        &self,
        uri: &str,
        line: u32,
        character: u32,
        new_name: &str,
    ) -> Option<WorkspaceEdit> {
        let locations = self.references_at(uri, line, character, true)?;
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
    pub(crate) fn prepare_rename_at(
        &self,
        uri: &str,
        line: u32,
        character: u32,
    ) -> Option<PrepareRenameResponse> {
        let docs = self.documents.read();
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

    #[allow(deprecated)]
    pub(crate) fn document_symbols_at(&self, uri: &str) -> Option<DocumentSymbolResponse> {
        let docs = self.documents.read();
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

    pub(crate) fn document_highlight_at(
        &self,
        uri: &str,
        line: u32,
        character: u32,
    ) -> Option<Vec<DocumentHighlight>> {
        let def_location = self.goto_definition_at(uri, line, character)?.first().cloned();
        let refs = self.references_at(uri, line, character, true)?;
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

    pub(crate) fn folding_ranges_at(&self, uri: &str) -> Option<Vec<FoldingRange>> {
        let docs = self.documents.read();
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

    pub(crate) fn selection_ranges_at(
        &self,
        uri: &str,
        positions: &[Position],
    ) -> Option<Vec<SelectionRange>> {
        let docs = self.documents.read();
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

    pub(crate) fn document_links_at(&self, uri: &str) -> Option<Vec<DocumentLink>> {
        let docs = self.documents.read();
        let state = docs.get(uri)?;
        let source = state.source.as_str();
        let line_index = &state.line_index;
        let root = state.root.as_ref()?;

        let base_dir: PathBuf = state
            .main_path
            .as_ref()
            .and_then(|p| p.parent().map(|x| x.to_path_buf()))
            .or_else(|| {
                uri_to_path(uri).and_then(|p| p.parent().map(|x| x.to_path_buf()))
            })?;

        let mut links = Vec::new();
        for node in root.find_all_nodes(Kind::NodeInclude.into_syntax_kind()) {
            let token = node
                .descendant_tokens()
                .into_iter()
                .find(|t| t.kind_as::<Kind>() == Some(Kind::TokString))?;
            let tr = token.text_range();
            let range = Range {
                start: span_to_position(source, line_index, tr.start),
                end: span_to_position(source, line_index, tr.end),
            };
            let path_str = token.text().trim_matches(|c| c == '"' || c == '\'').to_string();
            let resolved = base_dir.join(&path_str);
            if let Some(target_url) = path_to_uri(&resolved) {
                links.push(DocumentLink {
                    range,
                    target: Some(target_url),
                    tooltip: None,
                    data: None,
                });
            }
        }
        Some(links)
    }

    /// Format the given range by formatting the whole document and replacing the range with the corresponding slice of formatted output.
    pub(crate) fn format_range_at(
        &self,
        uri: &str,
        range: &Range,
        options: &FormatterOptions,
    ) -> Option<Vec<TextEdit>> {
        let docs = self.documents.read();
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
        if !new_text.is_empty() && end_line < lines.len().saturating_sub(1) {
            // Preserve trailing newline if the range included more content
        }
        Some(vec![TextEdit {
            range: range.clone(),
            new_text,
        }])
    }

    pub(crate) fn workspace_symbols_at(&self, query: &str) -> Vec<SymbolInformation> {
        let docs = self.documents.read();
        let query_lower = query.to_lowercase();
        let mut with_rank: Vec<(SymbolInformation, u8)> = Vec::new();
        for (uri, _state) in docs.iter() {
            let Some(symbols) = self.document_symbols_at(uri) else {
                continue;
            };
            let DocumentSymbolResponse::Nested(symbols) = symbols else {
                continue;
            };
            let uri_url: Url = match uri.parse() {
                Ok(u) => u,
                Err(_) => continue,
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

    pub(crate) fn completion_at(
        &self,
        uri: &str,
        line: u32,
        character: u32,
    ) -> Option<CompletionResponse> {
        let docs = self.documents.read();
        let state = docs.get(uri)?;
        let source = state.source.as_str();
        let line_index = &state.line_index;

        let byte_offset = line_col_utf16_to_byte(source, line_index, line, character).unwrap_or(0);
        let (line_exp, char_exp) = line_index.line_col_utf16(source, byte_offset);
        let prefix = line_prefix_utf16(source, line_index, line_exp, char_exp)?;
        let prefix = identifier_prefix(&prefix);

        // Collect (name, kind, detail) so we can show correct icons and optional detail.
        // Later entries overwrite earlier for same name (e.g. "foo" as variable then function -> show as function).
        let mut completion_entries: HashMap<String, (CompletionItemKind, Option<String>)> =
            HashMap::new();

        // Index/key access: after `.` offer member completion (fields/methods of receiver type).
        // In a postfix chain (primary.call().member), the receiver of MemberExpr is the *previous sibling* node in the parent, not a child of MemberExpr.
        // Cursor after "." may be in the next token (e.g. newline), so find MemberExpr that contains the offset rather than relying on node_at_offset's ancestor.
        let root = state.root.as_ref();
        let in_member_context: Option<leekscript_rs::Type> = root.and_then(|root| {
            let member_expr = root
                .find_all_nodes(Kind::NodeMemberExpr.into_syntax_kind())
                .into_iter()
                .find(|n| {
                    let r = n.text_range();
                    r.start <= byte_offset && byte_offset <= r.end
                })?;
            // Cursor must be at or after the dot (so we're in the member name part).
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
                // Receiver is the node immediately before this MemberExpr (postfix chain).
                let member_start = member_expr.text_range().start;
                let parent = member_expr.ancestors(root).into_iter().next()?;
                let mut receiver: Option<SyntaxNode> = None;
                for child in parent.children() {
                    if let SyntaxElement::Node(n) = child {
                        let r = n.text_range();
                        if r.start == member_start && r.end == member_expr.text_range().end {
                            break; // this is the MemberExpr itself
                        }
                        if r.end <= member_start {
                            receiver = Some(n); // candidate: ends before or at MemberExpr start
                        }
                    }
                }
                let receiver = receiver?;
                // Look up type: receiver node's span, or any descendant's span (type is recorded on the expression node that produced the value).
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
            // Scope-based completions: variables, functions, classes, globals, keywords.
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

    pub(crate) fn signature_help_at(
        &self,
        uri: &str,
        line: u32,
        character: u32,
    ) -> Option<SignatureHelp> {
        let docs = self.documents.read();
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
                documentation: param_doc.map(|s| Documentation::String(s)),
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

    // --- Call hierarchy ---

    pub(crate) fn prepare_call_hierarchy_at(
        &self,
        params: &CallHierarchyPrepareParams,
    ) -> Option<Vec<CallHierarchyItem>> {
        let uri = params.text_document_position_params.text_document.uri.to_string();
        let pos = params.text_document_position_params.position;
        let docs = self.documents.read();
        let state = docs.get(&uri)?;
        let source = state.source.as_str();
        let line_index = &state.line_index;
        let root = state.root.as_ref()?;

        let byte_offset = line_col_utf16_to_byte(source, line_index, pos.line, pos.character)?;
        let node = root.node_at_offset(byte_offset)?;

        if let Some(Kind::NodeFunctionDecl) = node.kind_as::<Kind>() {
            let info = function_decl_info(&node)?;
            let range = node.text_range();
            let url: Url = uri.parse().ok()?;
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
            let url: Url = uri.parse().ok()?;
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
                let url: Url = uri.parse().ok()?;
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
                let url: Url = uri.parse().ok()?;
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

    pub(crate) fn call_hierarchy_incoming(
        &self,
        params: &CallHierarchyIncomingCallsParams,
    ) -> Option<Vec<CallHierarchyIncomingCall>> {
        let item = &params.item;
        let def_uri = item.uri.to_string();
        let name = &item.name;

        let docs = self.documents.read();
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
                    .as_ref().map(|ref_sym| symbol_matches(target_sym, ref_sym))
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

    pub(crate) fn call_hierarchy_outgoing(
        &self,
        params: &CallHierarchyOutgoingCallsParams,
    ) -> Option<Vec<CallHierarchyOutgoingCall>> {
        let item = &params.item;
        let def_uri = item.uri.to_string();
        let item_range = &item.range;

        let docs = self.documents.read();
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

    // --- Type hierarchy ---

    pub(crate) fn prepare_type_hierarchy_at(
        &self,
        params: &TypeHierarchyPrepareParams,
    ) -> Option<Vec<TypeHierarchyItem>> {
        let uri = params.text_document_position_params.text_document.uri.to_string();
        let pos = params.text_document_position_params.position;
        let docs = self.documents.read();
        let state = docs.get(&uri)?;
        let source = state.source.as_str();
        let line_index = &state.line_index;
        let root = state.root.as_ref()?;

        let byte_offset = line_col_utf16_to_byte(source, line_index, pos.line, pos.character)?;
        let node = root.node_at_offset(byte_offset)?;

        if let Some(Kind::NodeClassDecl) = node.kind_as::<Kind>() {
            let info = class_decl_info(&node)?;
            let range = node.text_range();
            let url: Url = uri.parse().ok()?;
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
            let url: Url = uri.parse().ok()?;
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

    pub(crate) fn type_hierarchy_supertypes(
        &self,
        params: &TypeHierarchySupertypesParams,
    ) -> Option<Vec<TypeHierarchyItem>> {
        let item = &params.item;
        let uri = item.uri.to_string();
        let docs = self.documents.read();
        let state = docs.get(&uri)?;
        let source = state.source.as_str();
        let line_index = &state.line_index;

        let super_name = state.class_super.get(&item.name)?;
        // Use definition_map when the super class may be in an included file.
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
                                start: span_to_position(
                                    &def_source,
                                    &def_line_index,
                                    *start,
                                ),
                                end: span_to_position(&def_source, &def_line_index, *end),
                            },
                            selection_range: Range {
                                start: span_to_position(
                                    &def_source,
                                    &def_line_index,
                                    *start,
                                ),
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
        let url: Url = uri.parse().ok()?;
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

    pub(crate) fn type_hierarchy_subtypes(
        &self,
        params: &TypeHierarchySubtypesParams,
    ) -> Option<Vec<TypeHierarchyItem>> {
        let item = &params.item;
        let uri = item.uri.to_string();
        let docs = self.documents.read();
        let state = docs.get(&uri)?;
        let source = state.source.as_str();
        let line_index = &state.line_index;

        let mut subtypes = Vec::new();
        for (sub_name, super_name) in &state.class_super {
            if super_name != &item.name {
                continue;
            }
            // Use definition_map when the subtype may be in an included file.
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
            let url: Url = uri.parse().ok()?;
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
}

/// Find the scope-creating syntax node whose range equals (start, end).
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

/// Return the label for a scope-end inlay hint, or None to omit (e.g. plain blocks).
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

