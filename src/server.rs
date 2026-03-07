//! LSP server: `LanguageServer` trait implementation (request handlers).

use tower_lsp::jsonrpc::{Error, Result};
use tower_lsp::lsp_types::*;
use tower_lsp::LanguageServer;

use crate::backend::Backend;
use crate::config::apply_config_from_value;
use crate::semantic_tokens::{compute_semantic_tokens, compute_semantic_tokens_range, semantic_tokens_provider};
use crate::util::{
    apply_content_changes, formatter_options_from_lsp, line_col_utf16_to_byte,
    source_text_in_range,
};
use leekscript_rs::is_valid_identifier;
use leekscript_rs::{format, parse, reparse};

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
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                definition_provider: Some(OneOf::Left(true)),
                references_provider: Some(OneOf::Left(true)),
                rename_provider: Some(OneOf::Right(RenameOptions {
                    prepare_provider: Some(true),
                    work_done_progress_options: WorkDoneProgressOptions::default(),
                })),
                document_symbol_provider: Some(OneOf::Left(true)),
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec![
                        ".".to_string(),
                        ":".to_string(),
                        "[".to_string(),
                    ]),
                    ..Default::default()
                }),
                signature_help_provider: Some(SignatureHelpOptions {
                    trigger_characters: Some(vec!["(".to_string(), ",".to_string()]),
                    retrigger_characters: None,
                    work_done_progress_options: Default::default(),
                }),
                document_formatting_provider: Some(OneOf::Left(true)),
                inlay_hint_provider: Some(OneOf::Left(true)),
                semantic_tokens_provider: Some(semantic_tokens_provider()),
                code_action_provider: Some(CodeActionProviderCapability::Options(CodeActionOptions {
                    code_action_kinds: Some(vec![CodeActionKind::QUICKFIX]),
                    ..Default::default()
                })),
                document_highlight_provider: Some(OneOf::Left(true)),
                folding_range_provider: Some(FoldingRangeProviderCapability::Simple(true)),
                selection_range_provider: Some(SelectionRangeProviderCapability::Simple(true)),
                document_link_provider: Some(DocumentLinkOptions {
                    resolve_provider: Some(true),
                    work_done_progress_options: WorkDoneProgressOptions::default(),
                }),
                document_range_formatting_provider: Some(OneOf::Left(true)),
                workspace_symbol_provider: Some(OneOf::Left(true)),
                call_hierarchy_provider: Some(CallHierarchyServerCapability::Simple(true)),
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
            .log_message(MessageType::INFO, "leekscript-lsp initialized")
            .await;
        self.log_trace("leekscript-lsp: fetched workspace config".to_string())
            .await;
    }

    async fn did_change_configuration(&self, params: DidChangeConfigurationParams) {
        let _ = self
            .client
            .log_message(MessageType::INFO, "leekscript-lsp: configuration changed")
            .await;
        let config_obj = params.settings.get("leekscript").unwrap_or(&params.settings);
        {
            let mut settings = self.settings.write();
            apply_config_from_value(&mut settings, config_obj);
        }
        let uris_and_sources: Vec<_> = {
            let docs = self.documents.read();
            docs.iter()
                .map(|(uri, state)| (uri.clone(), state.source.clone()))
                .collect()
        };
        for (uri, source) in uris_and_sources {
            self.run_analysis_async(uri, source, None).await;
        }
    }

    async fn shutdown(&self) -> Result<()> {
        let _ = self
            .client
            .log_message(MessageType::INFO, "leekscript-lsp: shutdown requested")
            .await;
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri.to_string();
        let source = params.text_document.text;
        let _ = self
            .client
            .log_message(
                MessageType::INFO,
                format!("leekscript-lsp: document opened uri={uri} len={}", source.len()),
            )
            .await;
        self.run_analysis_async(uri, source, None).await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri.to_string();
        let content_changes = params.content_changes;
        self.log_trace(format!(
            "leekscript-lsp: document changed uri={uri} changes={}",
            content_changes.len()
        ))
        .await;
        let (new_source, single_edit, use_reparse) = {
            let docs = self.documents.read();
            match docs.get(&uri) {
                Some(state) => {
                    let (src, edit) = apply_content_changes(state, content_changes);
                    let use_reparse = state.include_tree.is_none();
                    (src, edit, use_reparse)
                }
                None => (
                    content_changes
                        .last()
                        .map(|c| c.text.clone())
                        .unwrap_or_default(),
                    None,
                    false,
                ),
            }
        };
        // When we have a single range-based edit and no include tree, use incremental reparse so
        // the new root is passed to run_analysis (DocumentAnalysis uses existing_root and skips full parse).
        let root = if use_reparse {
            if let Some(edit) = single_edit {
                let old_source_and_root = {
                    let docs = self.documents.read();
                    docs.get(&uri).map(|s| (s.source.clone(), s.root.clone()))
                };
                if let Some((old_source, old_root)) = old_source_and_root {
                    if let Some(old_root) = old_root {
                        match reparse(&old_source, &old_root, &edit) {
                            Ok(Some(r)) => Some(r),
                            _ => parse(&new_source).ok().and_then(|x| x),
                        }
                    } else {
                        parse(&new_source).ok().and_then(|x| x)
                    }
                } else {
                    parse(&new_source).ok().and_then(|x| x)
                }
            } else {
                None
            }
        } else {
            None
        };
        self.run_analysis_async(uri, new_source, root).await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri.to_string();
        let _ = self
            .client
            .log_message(MessageType::INFO, format!("leekscript-lsp: document closed uri={uri}"))
            .await;
        let url = params.text_document.uri;
        {
            let mut docs = self.documents.write();
            docs.remove(&uri);
        }
        self.client.publish_diagnostics(url, vec![], None).await;
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let uri = params.text_document_position_params.text_document.uri.to_string();
        let pos = params.text_document_position_params.position;
        self.log_trace(format!(
            "leekscript-lsp: hover uri={uri} L{}:{}",
            pos.line, pos.character
        ))
        .await;
        let hover = self.hover_at(&uri, pos.line, pos.character);
        Ok(hover)
    }

    async fn inlay_hint(
        &self,
        params: InlayHintParams,
    ) -> Result<Option<Vec<InlayHint>>> {
        if !self.settings.read().inlay_hints_enabled {
            return Ok(Some(vec![]));
        }
        let uri = params.text_document.uri.to_string();
        let r = params.range;
        self.log_trace(format!(
            "leekscript-lsp: inlay_hint uri={uri} range L{}:{} - L{}:{}",
            r.start.line, r.start.character, r.end.line, r.end.character
        ))
        .await;
        let hints = self.inlay_hints_at(
            &uri,
            r.start.line,
            r.start.character,
            r.end.line,
            r.end.character,
        );
        Ok(Some(hints))
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let uri = params.text_document_position_params.text_document.uri.to_string();
        let pos = params.text_document_position_params.position;
        self.log_trace(format!(
            "leekscript-lsp: goto_definition uri={uri} L{}:{}",
            pos.line, pos.character
        ))
        .await;
        let locations = self.goto_definition_at(&uri, pos.line, pos.character);
        Ok(locations.map(GotoDefinitionResponse::Array))
    }

    async fn references(&self, params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        let uri = params.text_document_position.text_document.uri.to_string();
        let pos = params.text_document_position.position;
        self.log_trace(format!(
            "leekscript-lsp: references uri={uri} L{}:{}",
            pos.line, pos.character
        ))
        .await;
        let include_declaration = params.context.include_declaration;
        let locations = self.references_at(&uri, pos.line, pos.character, include_declaration);
        Ok(locations)
    }

    async fn rename(&self, params: RenameParams) -> Result<Option<WorkspaceEdit>> {
        if !is_valid_identifier(&params.new_name) {
            return Err(Error::invalid_params(format!(
                "Invalid identifier: '{}' (must start with letter or underscore, then alphanumeric or underscore)",
                params.new_name
            )));
        }
        let uri = params.text_document_position.text_document.uri.to_string();
        let pos = params.text_document_position.position;
        self.log_trace(format!(
            "leekscript-lsp: rename uri={uri} L{}:{} new_name={}",
            pos.line, pos.character, params.new_name
        ))
        .await;
        let edit = self.rename_at(&uri, pos.line, pos.character, &params.new_name);
        Ok(edit)
    }

    async fn prepare_rename(
        &self,
        params: TextDocumentPositionParams,
    ) -> Result<Option<PrepareRenameResponse>> {
        let uri = params.text_document.uri.to_string();
        let pos = params.position;
        self.log_trace(format!(
            "leekscript-lsp: prepare_rename uri={uri} L{}:{}",
            pos.line, pos.character
        ))
        .await;
        let response = self.prepare_rename_at(&uri, pos.line, pos.character);
        Ok(response)
    }

    async fn document_highlight(
        &self,
        params: DocumentHighlightParams,
    ) -> Result<Option<Vec<DocumentHighlight>>> {
        let uri = params.text_document_position_params.text_document.uri.to_string();
        let pos = params.text_document_position_params.position;
        let highlights = self.document_highlight_at(&uri, pos.line, pos.character);
        Ok(highlights)
    }

    async fn folding_range(
        &self,
        params: FoldingRangeParams,
    ) -> Result<Option<Vec<FoldingRange>>> {
        let uri = params.text_document.uri.to_string();
        let ranges = self.folding_ranges_at(&uri);
        Ok(ranges)
    }

    async fn selection_range(
        &self,
        params: SelectionRangeParams,
    ) -> Result<Option<Vec<SelectionRange>>> {
        let uri = params.text_document.uri.to_string();
        let ranges = self.selection_ranges_at(&uri, &params.positions);
        Ok(ranges)
    }

    async fn document_link(
        &self,
        params: DocumentLinkParams,
    ) -> Result<Option<Vec<DocumentLink>>> {
        let uri = params.text_document.uri.to_string();
        let links = self.document_links_at(&uri);
        Ok(links)
    }

    async fn document_link_resolve(&self, params: DocumentLink) -> Result<DocumentLink> {
        let path_buf = params
            .target
            .as_ref()
            .and_then(|url| url.to_file_path().ok());
        let path_str = path_buf
            .as_ref()
            .map(|p| p.to_string_lossy().to_string());
        let tooltip = match path_str {
            Some(ref s) => {
                let mut tip = format!("Include: {}", s);
                if let Some(p) = path_buf {
                    let exists = tokio::task::spawn_blocking(move || std::fs::metadata(p).is_ok())
                        .await
                        .unwrap_or(false);
                    if !exists {
                        tip.push_str(" (file not found)");
                    }
                }
                Some(tip)
            }
            None => params.tooltip.clone(),
        };
        Ok(DocumentLink {
            range: params.range,
            target: params.target,
            tooltip,
            data: params.data,
        })
    }

    async fn range_formatting(
        &self,
        params: DocumentRangeFormattingParams,
    ) -> Result<Option<Vec<TextEdit>>> {
        let uri = params.text_document.uri.to_string();
        let range = params.range;
        let options = formatter_options_from_lsp(&params.options);
        let edits = self.format_range_at(&uri, &range, &options);
        Ok(edits)
    }

    async fn symbol(
        &self,
        params: WorkspaceSymbolParams,
    ) -> Result<Option<Vec<SymbolInformation>>> {
        let query = params.query;
        let symbols = self.workspace_symbols_at(&query);
        Ok(Some(symbols))
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        let uri = params.text_document.uri.to_string();
        self.log_trace(format!("leekscript-lsp: document_symbol uri={uri}"))
            .await;
        let response = self.document_symbols_at(&uri);
        Ok(response)
    }

    async fn completion(
        &self,
        params: CompletionParams,
    ) -> Result<Option<CompletionResponse>> {
        let uri = params.text_document_position.text_document.uri.to_string();
        let pos = params.text_document_position.position;
        self.log_trace(format!(
            "leekscript-lsp: completion uri={uri} L{}:{}",
            pos.line, pos.character
        ))
        .await;
        let response = self.completion_at(&uri, pos.line, pos.character);
        Ok(response)
    }

    async fn signature_help(
        &self,
        params: SignatureHelpParams,
    ) -> Result<Option<SignatureHelp>> {
        let uri = params.text_document_position_params.text_document.uri.to_string();
        let pos = params.text_document_position_params.position;
        self.log_trace(format!(
            "leekscript-lsp: signature_help uri={uri} L{}:{}",
            pos.line, pos.character
        ))
        .await;
        let help = self.signature_help_at(&uri, pos.line, pos.character);
        Ok(help)
    }

    async fn code_action(&self, params: CodeActionParams) -> Result<Option<CodeActionResponse>> {
        let uri = params.text_document.uri.clone();
        let diagnostics = &params.context.diagnostics;
        let mut actions: CodeActionResponse = Vec::new();
        let docs = self.documents.read();
        let uri_str = uri.to_string();
        let state = docs.get(&uri_str);
        for diag in diagnostics {
            // Deprecation quick fixes: replace === with ==, !== with !=
            if let Some(NumberOrString::String(code)) = &diag.code {
                if code == "deprecated_strict_eq" || code == "deprecated_strict_neq" {
                    let (replacement, title) = if code == "deprecated_strict_eq" {
                        ("==".to_string(), "Replace `===` with `==`".to_string())
                    } else {
                        ("!=".to_string(), "Replace `!==` with `!=`".to_string())
                    };
                    let edit = WorkspaceEdit {
                        changes: Some(
                            [(uri.clone(), vec![TextEdit {
                                range: diag.range,
                                new_text: replacement,
                            }])]
                            .into_iter()
                            .collect(),
                        ),
                        document_changes: None,
                        change_annotations: None,
                    };
                    actions.push(
                        CodeAction {
                            title,
                            kind: Some(CodeActionKind::QUICKFIX),
                            diagnostics: Some(vec![diag.clone()]),
                            edit: Some(edit),
                            command: None,
                            is_preferred: Some(true),
                            disabled: None,
                            data: None,
                        }
                        .into(),
                    );
                    continue;
                }
                // E033: unknown variable – offer "Add global declaration" (insert at line 0)
                if code == "E033" {
                    if let Some(s) = state {
                        let name = source_text_in_range(&s.source, &s.line_index, &diag.range);
                        if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                            let insert = format!("global {name};\n");
                            let start = Range {
                                start: Position { line: 0, character: 0 },
                                end: Position { line: 0, character: 0 },
                            };
                            let edit = WorkspaceEdit {
                                changes: Some(
                                    [(uri.clone(), vec![TextEdit { range: start, new_text: insert }])]
                                        .into_iter()
                                        .collect(),
                                ),
                                document_changes: None,
                                change_annotations: None,
                            };
                            actions.push(
                                CodeAction {
                                    title: format!("Add global declaration for '{name}'"),
                                    kind: Some(CodeActionKind::QUICKFIX),
                                    diagnostics: Some(vec![diag.clone()]),
                                    edit: Some(edit),
                                    command: None,
                                    is_preferred: Some(false),
                                    disabled: None,
                                    data: None,
                                }
                                .into(),
                            );
                        }
                    }
                }
            }
        }
        Ok(if actions.is_empty() {
            None
        } else {
            Some(actions)
        })
    }

    async fn formatting(&self, params: DocumentFormattingParams) -> Result<Option<Vec<TextEdit>>> {
        let uri = params.text_document.uri.to_string();
        self.log_trace(format!("leekscript-lsp: formatting uri={uri}"))
            .await;
        let docs = self.documents.read();
        let state = match docs.get(&uri) {
            Some(s) => s,
            None => return Ok(None),
        };
        let root = match state.root.as_ref() {
            Some(r) => r,
            None => return Ok(None),
        };
        let options = formatter_options_from_lsp(&params.options);
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
        self.log_trace(format!("leekscript-lsp: semantic_tokens_full uri={uri}"))
            .await;
        let docs = self.documents.read();
        let state = match docs.get(&uri) {
            Some(s) => s,
            None => return Ok(Some(SemanticTokensResult::Tokens(SemanticTokens::default()))),
        };
        let root = match &state.root {
            Some(r) => r,
            None => return Ok(Some(SemanticTokensResult::Tokens(SemanticTokens::default()))),
        };
        let tokens = compute_semantic_tokens(&state.source, &state.line_index, root);
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
        let state = match docs.get(&uri) {
            Some(s) => s,
            None => return Ok(Some(SemanticTokensRangeResult::Tokens(SemanticTokens::default()))),
        };
        let r = params.range;
        let source = state.source.as_str();
        let line_index = &state.line_index;
        let byte_end_default = state.source.len() as u32;
        let root = match &state.root {
            Some(rt) => rt.clone(),
            None => return Ok(Some(SemanticTokensRangeResult::Tokens(SemanticTokens::default()))),
        };
        let byte_start = line_col_utf16_to_byte(source, line_index, r.start.line, r.start.character)
            .unwrap_or(0);
        let byte_end = line_col_utf16_to_byte(source, line_index, r.end.line, r.end.character)
            .unwrap_or(byte_end_default);
        let tokens = compute_semantic_tokens_range(source, line_index, &root, byte_start, byte_end);
        Ok(Some(SemanticTokensRangeResult::Tokens(tokens)))
    }

    async fn prepare_call_hierarchy(
        &self,
        params: CallHierarchyPrepareParams,
    ) -> Result<Option<Vec<CallHierarchyItem>>> {
        let items = self.prepare_call_hierarchy_at(&params);
        Ok(items)
    }

    async fn incoming_calls(
        &self,
        params: CallHierarchyIncomingCallsParams,
    ) -> Result<Option<Vec<CallHierarchyIncomingCall>>> {
        let calls = self.call_hierarchy_incoming(&params);
        Ok(calls)
    }

    async fn outgoing_calls(
        &self,
        params: CallHierarchyOutgoingCallsParams,
    ) -> Result<Option<Vec<CallHierarchyOutgoingCall>>> {
        let calls = self.call_hierarchy_outgoing(&params);
        Ok(calls)
    }

    async fn prepare_type_hierarchy(
        &self,
        params: TypeHierarchyPrepareParams,
    ) -> Result<Option<Vec<TypeHierarchyItem>>> {
        let items = self.prepare_type_hierarchy_at(&params);
        Ok(items)
    }

    async fn supertypes(
        &self,
        params: TypeHierarchySupertypesParams,
    ) -> Result<Option<Vec<TypeHierarchyItem>>> {
        let items = self.type_hierarchy_supertypes(&params);
        Ok(items)
    }

    async fn subtypes(
        &self,
        params: TypeHierarchySubtypesParams,
    ) -> Result<Option<Vec<TypeHierarchyItem>>> {
        let items = self.type_hierarchy_subtypes(&params);
        Ok(items)
    }
}
