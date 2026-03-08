//! LSP server: `LanguageServer` trait implementation (syntax highlighting and inlay hints).

use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::{
    CodeActionKind, CodeActionOptions, CodeActionOrCommand, CodeActionParams,
    CodeActionProviderCapability, CodeActionResponse, CompletionItem, CompletionOptions,
    CompletionParams, CompletionResponse, ConfigurationItem, DidChangeConfigurationParams,
    DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    DocumentLink, DocumentLinkOptions, DocumentLinkParams, DocumentSymbolParams,
    DocumentSymbolResponse, GotoDefinitionParams, GotoDefinitionResponse, Hover, HoverParams,
    HoverProviderCapability, InitializeParams, InitializeResult, InitializedParams, InlayHint,
    InlayHintParams, Location, MessageType, OneOf, PrepareRenameResponse, ReferenceParams,
    RenameOptions, RenameParams, SemanticTokens, SemanticTokensParams, SemanticTokensRangeParams,
    SemanticTokensRangeResult, SemanticTokensResult, ServerCapabilities, SymbolInformation,
    TextDocumentPositionParams, TextDocumentSyncCapability, TextDocumentSyncKind,
    WorkDoneProgressOptions, WorkspaceEdit, WorkspaceSymbolParams,
};
use tower_lsp::LanguageServer;

use leekscript_rs::lsp::{
    compute_code_actions, compute_completion, compute_definition, compute_document_links,
    compute_document_symbols, compute_hover, compute_inlay_hints, compute_rename,
    compute_workspace_symbols, find_references, prepare_rename, resolve_completion_item,
    DocumentAnalysisLspExt, InlayHintOptions, RenameError, DATA_KEY_URI,
};

use crate::backend::Backend;
use crate::config::apply_config_from_value;
use crate::document::DocumentAnalysis;
use crate::semantic_tokens::{
    compute_semantic_tokens, compute_semantic_tokens_fallback, compute_semantic_tokens_range,
    semantic_tokens_provider,
};
use crate::util::{apply_content_changes, line_col_utf16_to_byte, parse_uri};

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        self.log_trace("leekscript-lsp: initialize requested".to_string())
            .await;
        if let Some(opts) = params.initialization_options {
            {
                let mut settings = self.settings.write();
                let config_obj = opts.get("leekscript").unwrap_or(&opts);
                apply_config_from_value(&mut settings, config_obj);
            }
            self.log_trace("leekscript-lsp: applied initialization options".to_string())
                .await;
        }
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::INCREMENTAL,
                )),
                definition_provider: Some(OneOf::Left(true)),
                references_provider: Some(OneOf::Left(true)),
                rename_provider: Some(OneOf::Right(RenameOptions {
                    prepare_provider: Some(true),
                    work_done_progress_options: WorkDoneProgressOptions::default(),
                })),
                semantic_tokens_provider: Some(semantic_tokens_provider()),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                inlay_hint_provider: Some(OneOf::Left(true)),
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec![".".to_string()]),
                    resolve_provider: Some(true),
                    ..Default::default()
                }),
                document_link_provider: Some(DocumentLinkOptions {
                    resolve_provider: Some(false),
                    work_done_progress_options: WorkDoneProgressOptions::default(),
                }),
                document_symbol_provider: Some(OneOf::Left(true)),
                workspace_symbol_provider: Some(OneOf::Left(true)),
                code_action_provider: Some(CodeActionProviderCapability::Options(
                    CodeActionOptions {
                        code_action_kinds: Some(vec![CodeActionKind::QUICKFIX]),
                        work_done_progress_options: WorkDoneProgressOptions::default(),
                        ..Default::default()
                    },
                )),
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
                let mut settings = self.settings.write();
                let config_obj = value.get("leekscript").unwrap_or(&value);
                apply_config_from_value(&mut settings, config_obj);
            }
        }
        self.client
            .log_message(
                MessageType::INFO,
                "leekscript-lsp initialized (semantic/syntax highlighting including comments, hover, inlay hints, completion, goto definition, references, rename)",
            )
            .await;
        self.log_trace("leekscript-lsp: fetched workspace config".to_string())
            .await;
    }

    async fn did_change_configuration(&self, params: DidChangeConfigurationParams) {
        let () = self
            .client
            .log_message(MessageType::INFO, "leekscript-lsp: configuration changed")
            .await;
        let config_obj = params
            .settings
            .get("leekscript")
            .unwrap_or(&params.settings);
        let mut settings = self.settings.write();
        apply_config_from_value(&mut settings, config_obj);
    }

    async fn shutdown(&self) -> Result<()> {
        let () = self
            .client
            .log_message(MessageType::INFO, "leekscript-lsp: shutdown requested")
            .await;
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri.to_string();
        let source = params.text_document.text;
        let () = self
            .client
            .log_message(
                MessageType::INFO,
                format!(
                    "leekscript-lsp: document opened uri={uri} len={}",
                    source.len()
                ),
            )
            .await;
        self.run_analysis_async(uri, source, None, Some(params.text_document.version))
            .await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri.to_string();
        let content_changes = params.content_changes;
        self.log_trace(format!(
            "leekscript-lsp: document changed uri={uri} changes={}",
            content_changes.len()
        ))
        .await;

        let result = {
            let mut docs = self.documents.write();
            match docs.get(&uri) {
                None => None,
                Some(state) => {
                    let use_reparse = state.include_tree.is_none();
                    let (new_source, root) = if use_reparse {
                        state.apply_changes(content_changes)
                    } else {
                        let (src, _) = apply_content_changes(state, content_changes);
                        (src, None)
                    };
                    let new_state = match &root {
                        Some(r) => {
                            DocumentAnalysis::minimal_with_root(new_source.clone(), r.clone())
                        }
                        None => DocumentAnalysis::minimal(new_source.clone()),
                    };
                    docs.insert(uri.clone(), new_state);
                    Some((new_source, root))
                }
            }
        };

        let (new_source, root) = match result {
            None => {
                self.log_trace(format!(
                    "leekscript-lsp: did_change for unknown document uri={uri}, skipping"
                ))
                .await;
                return;
            }
            Some(r) => r,
        };
        self.log_trace(format!(
            "leekscript-lsp: did_change uri={uri} applied (incremental reparse when possible)"
        ))
        .await;
        if root.is_some() {
            self.client
                .log_message(
                    MessageType::LOG,
                    format!("leekscript-lsp: reparse succeeded uri={uri}"),
                )
                .await;
        } else {
            let () = self
                .client
                .log_message(
                    MessageType::LOG,
                    format!("leekscript-lsp: reparse failed uri={uri}, using full parse"),
                )
                .await;
        }
        self.run_analysis_async(uri, new_source, root, Some(params.text_document.version))
            .await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri.to_string();
        let () = self
            .client
            .log_message(
                MessageType::INFO,
                format!("leekscript-lsp: document closed uri={uri}"),
            )
            .await;
        {
            let mut docs = self.documents.write();
            docs.remove(&uri);
        }
        if let Some(url) = parse_uri(&uri) {
            self.client.publish_diagnostics(url, vec![], None).await;
        }
    }

    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> Result<Option<SemanticTokensResult>> {
        let uri = params.text_document.uri.to_string();
        self.log_trace(format!("leekscript-lsp: semantic_tokens_full uri={uri}"))
            .await;
        let docs = self.documents.read();
        let Some(state) = docs.get(&uri) else {
            return Ok(Some(
                SemanticTokensResult::Tokens(SemanticTokens::default()),
            ));
        };
        let tokens = match &state.root {
            Some(root) => compute_semantic_tokens(&state.source, &state.line_index, root),
            None => compute_semantic_tokens_fallback(&state.source, &state.line_index, None),
        };
        Ok(Some(SemanticTokensResult::Tokens(tokens)))
    }

    async fn semantic_tokens_range(
        &self,
        params: SemanticTokensRangeParams,
    ) -> Result<Option<SemanticTokensRangeResult>> {
        let uri = params.text_document.uri.to_string();
        self.log_trace(format!("leekscript-lsp: semantic_tokens_range uri={uri}"))
            .await;
        let docs = self.documents.read();
        let Some(state) = docs.get(&uri) else {
            return Ok(Some(SemanticTokensRangeResult::Tokens(
                SemanticTokens::default(),
            )));
        };
        let r = params.range;
        let source = state.source.as_str();
        let line_index = &state.line_index;
        let byte_end_default = state.source.len() as u32;
        let byte_start =
            line_col_utf16_to_byte(source, line_index, r.start.line, r.start.character)
                .unwrap_or(0);
        let byte_end = line_col_utf16_to_byte(source, line_index, r.end.line, r.end.character)
            .unwrap_or(byte_end_default);
        let range = (byte_start, byte_end);
        let tokens = match &state.root {
            Some(root) => {
                compute_semantic_tokens_range(source, line_index, root, byte_start, byte_end)
            }
            None => compute_semantic_tokens_fallback(source, line_index, Some(range)),
        };
        Ok(Some(SemanticTokensRangeResult::Tokens(tokens)))
    }

    async fn inlay_hint(&self, params: InlayHintParams) -> Result<Option<Vec<InlayHint>>> {
        let uri = params.text_document.uri.to_string();
        self.log_trace(format!("leekscript-lsp: inlay_hint uri={uri}"))
            .await;
        let docs = self.documents.read();
        let Some(state) = docs.get(&uri) else {
            return Ok(Some(Vec::new()));
        };
        let r = params.range;
        let source = state.source.as_str();
        let line_index = &state.line_index;
        let byte_end_default = state.source.len() as u32;
        let byte_start =
            line_col_utf16_to_byte(source, line_index, r.start.line, r.start.character)
                .unwrap_or(0);
        let byte_end = line_col_utf16_to_byte(source, line_index, r.end.line, r.end.character)
            .unwrap_or(byte_end_default);
        let range = Some((byte_start, byte_end));
        // Only show type hints for var/global declarations when inferred type is not `any`.
        let options = InlayHintOptions {
            expression_types: false,
            variable_types: true,
            parenthesis: false,
        };
        let hints = compute_inlay_hints(state, &options, range);
        Ok(Some(hints))
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let uri = params
            .text_document_position_params
            .text_document
            .uri
            .to_string();
        self.log_trace(format!("leekscript-lsp: hover uri={uri}"))
            .await;
        let docs = self.documents.read();
        let Some(state) = docs.get(&uri) else {
            return Ok(None);
        };
        let position = params.text_document_position_params.position;
        Ok(compute_hover(state, position, Some(&uri)))
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let uri = params
            .text_document_position_params
            .text_document
            .uri
            .to_string();
        self.log_trace(format!("leekscript-lsp: goto_definition uri={uri}"))
            .await;
        let docs = self.documents.read();
        let Some(state) = docs.get(&uri) else {
            return Ok(None);
        };
        let position = params.text_document_position_params.position;
        let current_uri = Some(uri.as_str());
        Ok(compute_definition(state, position, current_uri).map(GotoDefinitionResponse::Scalar))
    }

    async fn references(&self, params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        let uri = params.text_document_position.text_document.uri.to_string();
        self.log_trace(format!("leekscript-lsp: references uri={uri}"))
            .await;
        let docs = self.documents.read();
        let Some(state) = docs.get(&uri) else {
            return Ok(Some(Vec::new()));
        };
        let position = params.text_document_position.position;
        let include_declaration = params.context.include_declaration;
        let current_uri = Some(uri.as_str());
        let locations = find_references(state, position, current_uri, include_declaration);
        Ok(Some(locations))
    }

    async fn prepare_rename(
        &self,
        params: TextDocumentPositionParams,
    ) -> Result<Option<PrepareRenameResponse>> {
        let uri = params.text_document.uri.to_string();
        self.log_trace(format!("leekscript-lsp: prepare_rename uri={uri}"))
            .await;
        let docs = self.documents.read();
        let Some(state) = docs.get(&uri) else {
            return Ok(None);
        };
        let position = params.position;
        let current_uri = Some(uri.as_str());
        Ok(prepare_rename(state, position, current_uri).map(PrepareRenameResponse::Range))
    }

    async fn rename(&self, params: RenameParams) -> Result<Option<WorkspaceEdit>> {
        let uri = params.text_document_position.text_document.uri.to_string();
        self.log_trace(format!("leekscript-lsp: rename uri={uri}"))
            .await;
        let docs = self.documents.read();
        let state = match docs.get(&uri) {
            Some(s) => s,
            None => return Ok(None),
        };
        let position = params.text_document_position.position;
        let new_name = params.new_name;
        let current_uri = Some(uri.as_str());
        match compute_rename(state, position, current_uri, &new_name) {
            Ok(edit) => Ok(Some(edit)),
            Err(RenameError::InvalidName(_)) => Err(tower_lsp::jsonrpc::Error::invalid_params(
                "Invalid new name for rename",
            )),
            Err(RenameError::NotRenamable) => Ok(None),
        }
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let uri = params.text_document_position.text_document.uri.to_string();
        self.log_trace(format!("leekscript-lsp: completion uri={uri}"))
            .await;
        let docs = self.documents.read();
        let Some(state) = docs.get(&uri) else {
            return Ok(Some(CompletionResponse::Array(Vec::new())));
        };
        Ok(compute_completion(state, &params, &uri))
    }

    async fn completion_resolve(&self, item: CompletionItem) -> Result<CompletionItem> {
        let doc_uri = item
            .data
            .as_ref()
            .and_then(|d| d.get(DATA_KEY_URI))
            .and_then(|v| v.as_str());
        let resolved = if let Some(uri) = doc_uri {
            let docs = self.documents.read();
            let scope_store = docs.get(uri).map(|s| &s.scope_store);
            resolve_completion_item(item, scope_store)
        } else {
            resolve_completion_item(item, None)
        };
        Ok(resolved)
    }

    async fn document_link(&self, params: DocumentLinkParams) -> Result<Option<Vec<DocumentLink>>> {
        let uri = params.text_document.uri.to_string();
        self.log_trace(format!("leekscript-lsp: document_link uri={uri}"))
            .await;
        let docs = self.documents.read();
        let Some(state) = docs.get(&uri) else {
            return Ok(Some(Vec::new()));
        };
        Ok(Some(compute_document_links(state, Some(&uri))))
    }

    async fn symbol(
        &self,
        params: WorkspaceSymbolParams,
    ) -> Result<Option<Vec<SymbolInformation>>> {
        self.log_trace("leekscript-lsp: workspace_symbol".to_string())
            .await;
        let docs = self.documents.read();
        let analyses: Vec<&DocumentAnalysis> = docs.values().collect();
        let symbols = compute_workspace_symbols(&analyses, &params.query);
        Ok(Some(symbols))
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        let uri = params.text_document.uri.to_string();
        self.log_trace(format!("leekscript-lsp: document_symbol uri={uri}"))
            .await;
        let docs = self.documents.read();
        let Some(state) = docs.get(&uri) else {
            return Ok(Some(DocumentSymbolResponse::Nested(Vec::new())));
        };
        Ok(Some(compute_document_symbols(state)))
    }

    async fn code_action(&self, params: CodeActionParams) -> Result<Option<CodeActionResponse>> {
        let uri = params.text_document.uri.clone();
        self.log_trace(format!("leekscript-lsp: code_action uri={uri}"))
            .await;
        let diagnostics = params.context.diagnostics;
        let actions = compute_code_actions(&uri, &diagnostics);
        if actions.is_empty() {
            return Ok(None);
        }
        Ok(Some(
            actions
                .into_iter()
                .map(CodeActionOrCommand::CodeAction)
                .collect(),
        ))
    }
}
