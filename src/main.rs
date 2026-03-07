//! LeekScript language server: diagnostics, hover, go-to-definition, formatting, and more.
//!
//! Run with: `leekscript-lsp` (stdio). Configure your editor to use this binary
//! as the language server for `.leek` files.

mod semantic_tokens;

use std::collections::HashMap;
use std::sync::RwLock;

use leekscript_rs::analysis::{
    call_argument_count, class_decl_info, function_decl_info, member_expr_member_name,
    primary_expr_resolvable_name, var_decl_info, ResolvedSymbol, VarDeclKind,
};
use leekscript_rs::formatter::FormatterOptions;
use leekscript_rs::syntax::Kind;
use leekscript_rs::{
    analyze, analyze_with_signatures, build_scope_extents, format, parse, parse_signatures,
    scope_at_offset, LineIndex, ScopeId, ScopeStore, Severity,
};
use sipha::engine::ParseError;
use sipha::red::SyntaxNode;
use sipha::types::{IntoSyntaxKind, Span};
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};

use semantic_tokens::{
    compute_semantic_tokens, compute_semantic_tokens_range, semantic_tokens_provider,
};

/// Embedded standard library signature files (constants and functions).
const STDLIB_CONSTANTS_SIG: &str = include_str!("../signatures/stdlib_constants.sig");
const STDLIB_FUNCTIONS_SIG: &str = include_str!("../signatures/stdlib_functions.sig");

#[derive(Debug, Clone)]
struct LspSettings {
    /// Load embedded stdlib .sig files (constants + functions). Default true.
    load_stdlib_signatures: bool,
    /// Additional .sig file paths (resolved by the client / workspace).
    signature_files: Vec<String>,
}

impl Default for LspSettings {
    fn default() -> Self {
        Self {
            load_stdlib_signatures: true,
            signature_files: Vec::new(),
        }
    }
}

fn apply_config_from_value(settings: &mut LspSettings, value: &serde_json::Value) {
    if let Some(b) = value.get("loadStdlibSignatures").and_then(|v| v.as_bool()) {
        settings.load_stdlib_signatures = b;
    }
    if let Some(arr) = value.get("signatureFiles").and_then(|v| v.as_array()) {
        settings.signature_files = arr
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect();
    }
}

struct Backend {
    client: Client,
    documents: RwLock<HashMap<String, DocumentState>>,
    settings: RwLock<LspSettings>,
}

struct DocumentState {
    source: String,
    root: Option<SyntaxNode>,
    line_index: LineIndex,
    #[allow(dead_code)]
    diagnostics: Vec<leekscript_rs::SemanticDiagnostic>,
    type_map: HashMap<(u32, u32), leekscript_rs::Type>,
    scope_store: ScopeStore,
    scope_extents: Vec<(ScopeId, (u32, u32))>,
}

impl Backend {
    /// Build signature roots from current settings (embedded stdlib + optional files).
    fn get_signature_roots(&self) -> Vec<SyntaxNode> {
        let settings = self.settings.read().unwrap();
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

    fn run_analysis(&self, uri: &str, source: String) {
        let line_index = LineIndex::new(source.as_bytes());
        let mut diagnostics = Vec::new();
        let mut type_map = HashMap::new();
        let mut scope_store = ScopeStore::new();
        let mut scope_extents = vec![];

        let signature_roots = self.get_signature_roots();

        let root = match parse(&source) {
            Ok(Some(ref root)) => {
                let result = if signature_roots.is_empty() {
                    analyze(root)
                } else {
                    analyze_with_signatures(root, &signature_roots)
                };
                diagnostics = result.diagnostics.clone();
                type_map = result.type_map.clone();
                scope_store = result.scope_store;
                scope_extents =
                    build_scope_extents(root, &result.scope_id_sequence, source.len());
                Some(root.clone())
            }
            Ok(None) => {
                scope_extents = vec![(ScopeId(0), (0, source.len() as u32))];
                None
            }
            Err(ParseError::NoMatch(diag)) => {
                diagnostics.push(leekscript_rs::SemanticDiagnostic {
                    span: Span::new(diag.furthest, diag.furthest),
                    message: diag.message(None, None, None),
                    severity: Severity::Error,
                    code: Some("parse_error".to_string()),
                    file_id: None,
                });
                None
            }
            Err(ParseError::BadGraph) => {
                diagnostics.push(leekscript_rs::SemanticDiagnostic {
                    span: Span::new(0, 0),
                    message: "internal parse error".to_string(),
                    severity: Severity::Error,
                    code: Some("parse_error".to_string()),
                    file_id: None,
                });
                scope_extents = vec![(ScopeId(0), (0, source.len() as u32))];
                None
            }
        };

        let state = DocumentState {
            source: source.clone(),
            root,
            line_index: line_index.clone(),
            diagnostics: diagnostics.clone(),
            type_map,
            scope_store,
            scope_extents,
        };
        {
            let mut docs = self.documents.write().unwrap();
            docs.insert(uri.to_string(), state);
        }

        self.publish_diagnostics(uri, &source, &line_index, &diagnostics);
    }

    fn publish_diagnostics(
        &self,
        uri: &str,
        source: &str,
        line_index: &LineIndex,
        diagnostics: &[leekscript_rs::SemanticDiagnostic],
    ) {
        let url = uri.parse().unwrap_or_else(|_| {
            tower_lsp::lsp_types::Url::parse("file:///").unwrap()
        });
        let lsp_diags: Vec<Diagnostic> = diagnostics
            .iter()
            .map(|d| semantic_to_lsp(d, source, line_index))
            .collect();
        let _ = self.client.publish_diagnostics(url, lsp_diags, None);
    }

    fn hover_at(&self, uri: &str, line: u32, character: u32) -> Option<Hover> {
        let docs = self.documents.read().unwrap();
        let state = docs.get(uri)?;
        let source = state.source.as_str();
        let line_index = &state.line_index;
        let root = state.root.as_ref()?;

        let byte_offset = line_col_utf16_to_byte(source, line_index, line, character)?;
        let node = root.node_at_offset(byte_offset)?;
        let range = node.text_range();
        let key = (range.start, range.end);
        let ty = state.type_map.get(&key).cloned().or_else(|| {
            for anc in node.ancestors(root) {
                let r = anc.text_range();
                if let Some(t) = state.type_map.get(&(r.start, r.end)) {
                    return Some(t.clone());
                }
            }
            None
        })?;
        Some(Hover {
            contents: HoverContents::Scalar(MarkedString::String(ty.for_annotation().to_string())),
            range: Some(Range {
                start: span_to_position(source, line_index, range.start),
                end: span_to_position(source, line_index, range.end),
            }),
        })
    }

    fn goto_definition_at(
        &self,
        uri: &str,
        line: u32,
        character: u32,
    ) -> Option<Vec<Location>> {
        let docs = self.documents.read().unwrap();
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
        let sym = state.scope_store.resolve(scope_id, &name)?;
        let root_scope = state.scope_store.get(ScopeId(0))?;
        let def_span = match &sym {
            ResolvedSymbol::Variable(v) => v.span,
            ResolvedSymbol::Function(n, _) => root_scope.get_function_first_span(n)?,
            ResolvedSymbol::Class(n) => root_scope.get_class_first_span(n)?,
            ResolvedSymbol::Global(_) => return None,
        };
        let url = uri.parse().ok()?;
        Some(vec![Location {
            uri: url,
            range: Range {
                start: span_to_position(source, line_index, def_span.start),
                end: span_to_position(source, line_index, def_span.end),
            },
        }])
    }

    fn references_at(
        &self,
        uri: &str,
        line: u32,
        character: u32,
        include_declaration: bool,
    ) -> Option<Vec<Location>> {
        let docs = self.documents.read().unwrap();
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

        let url: tower_lsp::lsp_types::Url = uri.parse().ok()?;
        let mut locations = Vec::new();
        if include_declaration {
            locations.push(Location {
                uri: url.clone(),
                range: Range {
                    start: span_to_position(source, line_index, def_span.start),
                    end: span_to_position(source, line_index, def_span.end),
                },
            });
        }
        for tok in root.descendant_tokens() {
            if tok.kind_as::<Kind>() != Some(Kind::TokIdent) || tok.text() != name {
                continue;
            }
            let off = tok.text_range().start;
            let ref_scope = scope_at_offset(&state.scope_extents, off);
            let Some(ref_sym) = state.scope_store.resolve(ref_scope, &name) else {
                continue;
            };
            if !symbol_matches(&target_sym, &ref_sym) {
                continue;
            }
            locations.push(Location {
                uri: url.clone(),
                range: Range {
                    start: span_to_position(source, line_index, tok.text_range().start),
                    end: span_to_position(source, line_index, tok.text_range().end),
                },
            });
        }
        Some(locations)
    }

    #[allow(deprecated)]
    fn document_symbols_at(&self, uri: &str) -> Option<DocumentSymbolResponse> {
        let docs = self.documents.read().unwrap();
        let state = docs.get(uri)?;
        let source = state.source.as_str();
        let line_index = &state.line_index;
        let root = state.root.as_ref()?;

        let mut symbols = Vec::new();

        for node in root.find_all_nodes(Kind::NodeFunctionDecl.into_syntax_kind()) {
            let info = function_decl_info(&node)?;
            let range = node.text_range();
            symbols.push(DocumentSymbol {
                name: info.name,
                detail: None,
                kind: SymbolKind::FUNCTION,
                tags: None,
                deprecated: None,
                range: Range {
                    start: span_to_position(source, line_index, range.start),
                    end: span_to_position(source, line_index, range.end),
                },
                selection_range: Range {
                    start: span_to_position(source, line_index, info.name_span.start),
                    end: span_to_position(source, line_index, info.name_span.end),
                },
                children: None,
            });
        }
        for node in root.find_all_nodes(Kind::NodeClassDecl.into_syntax_kind()) {
            let info = class_decl_info(&node)?;
            let range = node.text_range();
            symbols.push(DocumentSymbol {
                name: info.name,
                detail: None,
                kind: SymbolKind::CLASS,
                tags: None,
                deprecated: None,
                range: Range {
                    start: span_to_position(source, line_index, range.start),
                    end: span_to_position(source, line_index, range.end),
                },
                selection_range: Range {
                    start: span_to_position(source, line_index, info.name_span.start),
                    end: span_to_position(source, line_index, info.name_span.end),
                },
                children: None,
            });
        }
        for node in root.find_all_nodes(Kind::NodeVarDecl.into_syntax_kind()) {
            let info = var_decl_info(&node)?;
            if info.kind != VarDeclKind::Global {
                continue;
            }
            let range = node.text_range();
            symbols.push(DocumentSymbol {
                name: info.name,
                detail: None,
                kind: SymbolKind::VARIABLE,
                tags: None,
                deprecated: None,
                range: Range {
                    start: span_to_position(source, line_index, range.start),
                    end: span_to_position(source, line_index, range.end),
                },
                selection_range: Range {
                    start: span_to_position(source, line_index, info.name_span.start),
                    end: span_to_position(source, line_index, info.name_span.end),
                },
                children: None,
            });
        }

        Some(DocumentSymbolResponse::Nested(symbols))
    }

    fn completion_at(
        &self,
        uri: &str,
        line: u32,
        character: u32,
    ) -> Option<CompletionResponse> {
        let docs = self.documents.read().unwrap();
        let state = docs.get(uri)?;
        let source = state.source.as_str();
        let line_index = &state.line_index;

        let prefix = line_prefix_utf16(source, line_index, line, character)?;
        let prefix = identifier_prefix(&prefix);

        let mut names = std::collections::HashSet::<String>::new();
        let byte_offset = line_col_utf16_to_byte(source, line_index, line, character)
            .unwrap_or(0);
        let scope_id = scope_at_offset(&state.scope_extents, byte_offset);
        let mut id = Some(scope_id);
        while let Some(scope_id) = id {
            let scope = match state.scope_store.get(scope_id) {
                Some(s) => s,
                None => break,
            };
            for name in scope.variable_names() {
                names.insert(name);
            }
            if scope_id.0 == 0 {
                for name in scope.function_names() {
                    names.insert(name);
                }
                for name in scope.class_names() {
                    names.insert(name);
                }
                for name in scope.global_names() {
                    names.insert(name);
                }
            }
            id = scope.parent;
        }
        for kw in LEEKSCRIPT_KEYWORDS {
            names.insert((*kw).to_string());
        }

        let mut items: Vec<CompletionItem> = names
            .into_iter()
            .filter(|name| name.starts_with(&prefix))
            .map(|name| CompletionItem {
                label: name.clone(),
                kind: Some(completion_kind(&name)),
                filter_text: Some(name),
                ..Default::default()
            })
            .collect();
        items.sort_by(|a, b| a.label.cmp(&b.label));
        Some(CompletionResponse::Array(items))
    }

    fn signature_help_at(
        &self,
        uri: &str,
        line: u32,
        character: u32,
    ) -> Option<SignatureHelp> {
        let docs = self.documents.read().unwrap();
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
                None => return None,
            };
            let scope = state.scope_store.get(scope_id)?;
            if let Some((p, r)) = scope.get_function_type(&name, arity) {
                break (p, r);
            }
            id = scope.parent;
        };

        let mut param_labels = Vec::with_capacity(param_types.len());
        for (i, ty) in param_types.iter().enumerate() {
            param_labels.push(ParameterInformation {
                label: ParameterLabel::Simple(format!("{}: {}", i, ty.for_annotation())),
                documentation: None,
            });
        }
        let label = format!(
            "{}({}) -> {}",
            name,
            param_types
                .iter()
                .enumerate()
                .map(|(i, t)| format!("{}: {}", i, t.for_annotation()))
                .collect::<Vec<_>>()
                .join(", "),
            return_type.for_annotation()
        );
        let sig = SignatureInformation {
            label: label.clone(),
            documentation: None,
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
}

fn line_prefix_utf16(
    source: &str,
    line_index: &LineIndex,
    line: u32,
    character: u32,
) -> Option<String> {
    let line_start = line_index.line_start(line) as usize;
    let line_span = line_index.line_range(line, source.len());
    let line_end = line_span.end as usize;
    let line_src = source.get(line_start..line_end)?;
    let mut utf16_count = 0u32;
    let mut end = line_src.len();
    for (i, c) in line_src.char_indices() {
        if utf16_count >= character {
            end = i;
            break;
        }
        utf16_count += c.len_utf16() as u32;
    }
    Some(line_src[..end].to_string())
}

fn identifier_prefix(s: &str) -> String {
    s.chars()
        .rev()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

fn completion_kind(name: &str) -> CompletionItemKind {
    if LEEKSCRIPT_KEYWORDS.contains(&name) {
        CompletionItemKind::KEYWORD
    } else {
        CompletionItemKind::VARIABLE
    }
}

const LEEKSCRIPT_KEYWORDS: &[&str] = &[
    "abstract", "and", "as", "break", "case", "catch", "class", "const", "constructor",
    "continue", "default", "do", "else", "extends", "false", "final", "for", "function",
    "global", "if", "include", "in", "instanceof", "let", "new", "not", "null", "or",
    "private", "protected", "public", "reserved", "return", "static", "super", "switch",
    "this", "throw", "true", "try", "var", "while", "xor",
];

fn symbol_matches(target: &ResolvedSymbol, candidate: &ResolvedSymbol) -> bool {
    match (target, candidate) {
        (ResolvedSymbol::Variable(a), ResolvedSymbol::Variable(b)) => {
            a.name == b.name && a.span.start == b.span.start && a.span.end == b.span.end
        }
        (ResolvedSymbol::Function(na, _), ResolvedSymbol::Function(nb, _)) => na == nb,
        (ResolvedSymbol::Class(na), ResolvedSymbol::Class(nb)) => na == nb,
        (ResolvedSymbol::Global(na), ResolvedSymbol::Global(nb)) => na == nb,
        _ => false,
    }
}

fn span_to_position(source: &str, line_index: &LineIndex, byte_offset: u32) -> Position {
    let (line_0, col_utf16) = line_index.line_col_utf16(source, byte_offset);
    Position {
        line: line_0,
        character: col_utf16,
    }
}

fn line_col_utf16_to_byte(
    source: &str,
    line_index: &LineIndex,
    line: u32,
    character: u32,
) -> Option<u32> {
    let line_start = line_index.line_start(line) as usize;
    let source_len = source.len();
    let line_span = line_index.line_range(line, source_len);
    let line_end = line_span.end as usize;
    let line_src = source.get(line_start..line_end)?;
    let mut utf16_col = 0u32;
    for (i, c) in line_src.char_indices() {
        if utf16_col >= character {
            return Some((line_start + i) as u32);
        }
        utf16_col += c.len_utf16() as u32;
    }
    Some((line_start + line_src.len()) as u32)
}

fn semantic_to_lsp(
    d: &leekscript_rs::SemanticDiagnostic,
    source: &str,
    line_index: &LineIndex,
) -> Diagnostic {
    let (line_start, col_start) = line_index.line_col_utf16(source, d.span.start);
    let (line_end, col_end) = line_index.line_col_utf16(source, d.span.end);
    let severity = match d.severity {
        Severity::Error => Some(DiagnosticSeverity::ERROR),
        Severity::Warning => Some(DiagnosticSeverity::WARNING),
        Severity::Deprecation => Some(DiagnosticSeverity::WARNING),
        Severity::Note => Some(DiagnosticSeverity::INFORMATION),
    };
    Diagnostic {
        range: Range {
            start: Position {
                line: line_start,
                character: col_start,
            },
            end: Position {
                line: line_end,
                character: col_end,
            },
        },
        severity,
        code: d.code.clone().map(NumberOrString::String),
        code_description: None,
        source: Some("leekscript".to_string()),
        message: d.message.clone(),
        related_information: None,
        tags: None,
        data: None,
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        if let Some(opts) = params.initialization_options {
            let mut settings = self.settings.write().unwrap();
            let config_obj = opts.get("leekscript").unwrap_or(&opts);
            apply_config_from_value(&mut settings, config_obj);
        }
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::INCREMENTAL,
                )),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                definition_provider: Some(OneOf::Left(true)),
                references_provider: Some(OneOf::Left(true)),
                document_symbol_provider: Some(OneOf::Left(true)),
                completion_provider: Some(CompletionOptions::default()),
                signature_help_provider: Some(SignatureHelpOptions {
                    trigger_characters: Some(vec!["(".to_string(), ",".to_string()]),
                    retrigger_characters: None,
                    work_done_progress_options: Default::default(),
                }),
                document_formatting_provider: Some(OneOf::Left(true)),
                semantic_tokens_provider: Some(semantic_tokens_provider()),
                ..Default::default()
            },
            ..Default::default()
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        if let Ok(config) = self
            .client
            .configuration(vec![ConfigurationItem {
                scope_uri: None,
                section: Some("leekscript".to_string()),
            }])
            .await
        {
            if let Some(value) = config.into_iter().next() {
                let mut settings = self.settings.write().unwrap();
                let config_obj = value.get("leekscript").unwrap_or(&value);
                apply_config_from_value(&mut settings, config_obj);
            }
        }
        self.client
            .log_message(MessageType::INFO, "leekscript-lsp initialized")
            .await;
    }

    async fn did_change_configuration(&self, params: DidChangeConfigurationParams) {
        let config_obj = params.settings.get("leekscript").unwrap_or(&params.settings);
        let mut settings = self.settings.write().unwrap();
        apply_config_from_value(&mut settings, config_obj);
        drop(settings);
        let docs = self.documents.read().unwrap();
        let uris_and_sources: Vec<_> = docs
            .iter()
            .map(|(uri, state)| (uri.clone(), state.source.clone()))
            .collect();
        drop(docs);
        for (uri, source) in uris_and_sources {
            self.run_analysis(&uri, source);
        }
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri.to_string();
        let source = params.text_document.text;
        self.run_analysis(&uri, source);
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri.to_string();
        let source = params
            .content_changes
            .into_iter()
            .last()
            .map(|c| c.text)
            .unwrap_or_default();
        self.run_analysis(&uri, source);
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let mut docs = self.documents.write().unwrap();
        docs.remove(&params.text_document.uri.to_string());
        let url = params.text_document.uri;
        let _ = self.client.publish_diagnostics(url, vec![], None);
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let uri = params.text_document_position_params.text_document.uri.to_string();
        let pos = params.text_document_position_params.position;
        let hover = self.hover_at(&uri, pos.line, pos.character);
        Ok(hover)
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let uri = params.text_document_position_params.text_document.uri.to_string();
        let pos = params.text_document_position_params.position;
        let locations = self.goto_definition_at(&uri, pos.line, pos.character);
        Ok(locations.map(GotoDefinitionResponse::Array))
    }

    async fn references(&self, params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        let uri = params.text_document_position.text_document.uri.to_string();
        let pos = params.text_document_position.position;
        let include_declaration = params.context.include_declaration;
        let locations = self.references_at(&uri, pos.line, pos.character, include_declaration);
        Ok(locations)
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        let uri = params.text_document.uri.to_string();
        let response = self.document_symbols_at(&uri);
        Ok(response)
    }

    async fn completion(
        &self,
        params: CompletionParams,
    ) -> Result<Option<CompletionResponse>> {
        let uri = params.text_document_position.text_document.uri.to_string();
        let pos = params.text_document_position.position;
        let response = self.completion_at(&uri, pos.line, pos.character);
        Ok(response)
    }

    async fn signature_help(
        &self,
        params: SignatureHelpParams,
    ) -> Result<Option<SignatureHelp>> {
        let uri = params.text_document_position_params.text_document.uri.to_string();
        let pos = params.text_document_position_params.position;
        let help = self.signature_help_at(&uri, pos.line, pos.character);
        Ok(help)
    }

    async fn formatting(&self, params: DocumentFormattingParams) -> Result<Option<Vec<TextEdit>>> {
        let uri = params.text_document.uri.to_string();
        let docs = self.documents.read().unwrap();
        let state = match docs.get(&uri) {
            Some(s) => s,
            None => return Ok(None),
        };
        let root = match state.root.as_ref() {
            Some(r) => r,
            None => return Ok(None),
        };
        let options = FormatterOptions::default();
        let formatted = format(root, &options);
        if formatted == state.source {
            return Ok(None);
        }
        let start = Position { line: 0, character: 0 };
        let end_line = state.source.lines().count().saturating_sub(1) as u32;
        let last_line_len = state.source.lines().last().map(str::len).unwrap_or(0) as u32;
        let end = Position {
            line: end_line,
            character: last_line_len,
        };
        Ok(Some(vec![TextEdit {
            range: Range { start, end },
            new_text: formatted,
        }]))
    }

    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> Result<Option<SemanticTokensResult>> {
        let uri = params.text_document.uri.to_string();
        let docs = self.documents.read().unwrap();
        let state = match docs.get(&uri) {
            Some(s) => s,
            None => return Ok(Some(SemanticTokensResult::Tokens(SemanticTokens::default()))),
        };
        let root = match &state.root {
            Some(r) => r,
            None => {
                return Ok(Some(SemanticTokensResult::Tokens(SemanticTokens::default())));
            }
        };
        let tokens = compute_semantic_tokens(&state.source, &state.line_index, root);
        Ok(Some(SemanticTokensResult::Tokens(tokens)))
    }

    async fn semantic_tokens_range(
        &self,
        params: SemanticTokensRangeParams,
    ) -> Result<Option<SemanticTokensRangeResult>> {
        let uri = params.text_document.uri.to_string();
        let docs = self.documents.read().unwrap();
        let state = match docs.get(&uri) {
            Some(s) => s,
            None => return Ok(Some(SemanticTokensRangeResult::Tokens(SemanticTokens::default()))),
        };
        let root = match &state.root {
            Some(r) => r,
            None => {
                return Ok(Some(SemanticTokensRangeResult::Tokens(SemanticTokens::default())));
            }
        };
        let r = params.range;
        let byte_start = line_col_utf16_to_byte(
            &state.source,
            &state.line_index,
            r.start.line,
            r.start.character,
        )
        .unwrap_or(0);
        let byte_end = line_col_utf16_to_byte(
            &state.source,
            &state.line_index,
            r.end.line,
            r.end.character,
        )
        .unwrap_or(state.source.len() as u32);
        let tokens = compute_semantic_tokens_range(
            &state.source,
            &state.line_index,
            root,
            byte_start,
            byte_end,
        );
        Ok(Some(SemanticTokensRangeResult::Tokens(tokens)))
    }
}

#[tokio::main]
async fn main() {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, socket) = LspService::new(|client| Backend {
        client,
        documents: RwLock::new(HashMap::new()),
        settings: RwLock::new(LspSettings::default()),
    });
    Server::new(stdin, stdout, socket).serve(service).await;
}
