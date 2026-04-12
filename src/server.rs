//! LSP server: full-document sync, diagnostics, semantic tokens, formatting, and language features.

use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::{
    CodeActionParams, CodeActionProviderCapability, CodeActionResponse, CodeLensOptions,
    CodeLensParams, CompletionOptions, CompletionParams, CompletionResponse,
    DidChangeConfigurationParams, DidChangeTextDocumentParams, DidCloseTextDocumentParams,
    DidOpenTextDocumentParams, DocumentFormattingParams, DocumentRangeFormattingParams,
    DocumentSymbolParams, DocumentSymbolResponse, FoldingRange, FoldingRangeParams,
    GotoDefinitionParams, GotoDefinitionResponse, Hover, HoverParams, InitializeParams,
    InitializeResult, InlayHint, InlayHintParams, Location, OneOf, ReferenceParams, RenameParams,
    SemanticTokensFullOptions, SemanticTokensOptions, SemanticTokensParams,
    SemanticTokensRangeParams, SemanticTokensRangeResult, SemanticTokensResult,
    SemanticTokensServerCapabilities, ServerCapabilities, TextDocumentContentChangeEvent,
    TextDocumentSyncCapability, TextDocumentSyncKind, TextDocumentSyncOptions, TextEdit,
    WorkDoneProgressOptions, WorkspaceEdit,
};
use tower_lsp::LanguageServer;

use crate::backend::{Backend, InitOptions};
use crate::folding;
use crate::formatting;
use crate::intel;
use crate::semantic_tokens::{semantic_token_legend, semantic_tokens_for_document};

use sipha::diagnostics::line_index::LineIndex;

fn apply_content_changes(
    source: &str,
    changes: Vec<TextDocumentContentChangeEvent>,
) -> Option<String> {
    let mut s = source.to_string();
    for c in changes {
        if c.range.is_none() {
            s = c.text;
            continue;
        }
        let r = c.range?;
        let idx = LineIndex::new(s.as_bytes());
        let start = idx.line_col_utf16_to_byte(&s, r.start.line, r.start.character)? as usize;
        let end = idx.line_col_utf16_to_byte(&s, r.end.line, r.end.character)? as usize;
        if start > end || end > s.len() {
            return None;
        }
        s.replace_range(start..end, &c.text);
    }
    Some(s)
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        let opts = InitOptions::from_initialization_json(params.initialization_options.as_ref());
        let inlay = opts.inlay_hints_enabled;
        let lens = opts.code_lens_references;
        *self.init.write() = opts;
        self.clear_intel_cache();

        let legend = semantic_token_legend();
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Options(
                    TextDocumentSyncOptions {
                        open_close: Some(true),
                        change: Some(TextDocumentSyncKind::INCREMENTAL),
                        will_save: None,
                        will_save_wait_until: None,
                        save: None,
                    },
                )),
                semantic_tokens_provider: Some(
                    SemanticTokensServerCapabilities::SemanticTokensOptions(
                        SemanticTokensOptions {
                            legend,
                            full: Some(SemanticTokensFullOptions::Bool(true)),
                            range: Some(true),
                            work_done_progress_options: WorkDoneProgressOptions::default(),
                        },
                    ),
                ),
                document_formatting_provider: Some(OneOf::Left(true)),
                document_range_formatting_provider: Some(OneOf::Left(true)),
                folding_range_provider: Some(true.into()),
                hover_provider: Some(tower_lsp::lsp_types::HoverProviderCapability::Simple(true)),
                definition_provider: Some(OneOf::Left(true)),
                references_provider: Some(OneOf::Left(true)),
                completion_provider: Some(CompletionOptions::default()),
                document_symbol_provider: Some(OneOf::Left(true)),
                rename_provider: Some(OneOf::Left(true)),
                code_action_provider: Some(CodeActionProviderCapability::Simple(true)),
                inlay_hint_provider: inlay.then(|| {
                    OneOf::Right(tower_lsp::lsp_types::InlayHintServerCapabilities::Options(
                        tower_lsp::lsp_types::InlayHintOptions::default(),
                    ))
                }),
                code_lens_provider: lens.then_some(CodeLensOptions {
                    resolve_provider: Some(false),
                }),
                ..Default::default()
            },
            ..Default::default()
        })
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    async fn did_change_configuration(&self, params: DidChangeConfigurationParams) {
        let opts = InitOptions::from_initialization_json(Some(&params.settings));
        *self.init.write() = opts;
        self.clear_intel_cache();

        let uris: Vec<tower_lsp::lsp_types::Url> = self
            .documents
            .read()
            .keys()
            .filter_map(|u| tower_lsp::lsp_types::Url::parse(u).ok())
            .collect();
        for uri in uris {
            self.schedule_diagnostics_publish(uri, 50);
        }
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri.clone();
        let uri_str = uri.to_string();
        let text = params.text_document.text.clone();
        let version = params.text_document.version;
        {
            let mut docs = self.documents.write();
            docs.insert(uri_str, text.clone());
        }
        {
            let mut vers = self.document_versions.write();
            vers.insert(uri.to_string(), version);
        }
        if let Ok(p) = uri.to_file_path() {
            self.invalidate_intel_cache_for_project_of(&p);
        }
        self.schedule_diagnostics_publish(uri, 200);
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri.clone();
        let uri_str = uri.to_string();
        let version = params.text_document.version;
        let new_source = {
            let old = self
                .documents
                .read()
                .get(&uri_str)
                .cloned()
                .unwrap_or_default();
            apply_content_changes(&old, params.content_changes).unwrap_or(old)
        };
        {
            let mut docs = self.documents.write();
            docs.insert(uri_str, new_source.clone());
        }
        {
            let mut vers = self.document_versions.write();
            vers.insert(uri.to_string(), version);
        }
        if let Ok(p) = uri.to_file_path() {
            self.invalidate_intel_cache_for_project_of(&p);
        }
        self.schedule_diagnostics_publish(uri, 200);
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri.clone();
        let uri_str = uri.to_string();
        if let Ok(p) = uri.to_file_path() {
            self.invalidate_intel_cache_for_project_of(&p);
        }
        self.cancel_diagnostics_task(&uri_str);
        {
            let mut docs = self.documents.write();
            docs.remove(&uri_str);
        }
        {
            let mut vers = self.document_versions.write();
            vers.remove(&uri_str);
        }
        self.clear_document_diagnostics(uri).await;
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let uri = params
            .text_document_position_params
            .text_document
            .uri
            .clone();
        if uri.scheme() != "file" {
            return Ok(None);
        }
        let uri_s = uri.to_string();
        let Some(source) = self.documents.read().get(&uri_s).cloned() else {
            return Ok(None);
        };
        let Some(path) = uri.to_file_path().ok() else {
            return Ok(None);
        };
        let Some(intel) = self.project_intel(&uri, &source).await else {
            return Ok(None);
        };
        Ok(intel::hover(intel.as_ref(), &path, &source, &params))
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let uri = params
            .text_document_position_params
            .text_document
            .uri
            .clone();
        if uri.scheme() != "file" {
            return Ok(None);
        }
        let uri_s = uri.to_string();
        let Some(source) = self.documents.read().get(&uri_s).cloned() else {
            return Ok(None);
        };
        let Some(path) = uri.to_file_path().ok() else {
            return Ok(None);
        };
        let Some(intel) = self.project_intel(&uri, &source).await else {
            return Ok(None);
        };
        Ok(intel::goto_definition(
            intel.as_ref(),
            &path,
            &source,
            &params,
        ))
    }

    async fn references(&self, params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        let uri = params.text_document_position.text_document.uri.clone();
        if uri.scheme() != "file" {
            return Ok(None);
        }
        let uri_s = uri.to_string();
        let Some(source) = self.documents.read().get(&uri_s).cloned() else {
            return Ok(None);
        };
        let Some(path) = uri.to_file_path().ok() else {
            return Ok(None);
        };
        let Some(intel) = self.project_intel(&uri, &source).await else {
            return Ok(None);
        };
        Ok(intel::references(intel.as_ref(), &path, &source, &params))
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let uri = params.text_document_position.text_document.uri.clone();
        if uri.scheme() != "file" {
            return Ok(None);
        }
        let uri_s = uri.to_string();
        let Some(source) = self.documents.read().get(&uri_s).cloned() else {
            return Ok(None);
        };
        let Some(path) = uri.to_file_path().ok() else {
            return Ok(None);
        };
        let Some(intel) = self.project_intel(&uri, &source).await else {
            return Ok(Some(CompletionResponse::Array(Vec::new())));
        };
        Ok(Some(intel::completion(
            intel.as_ref(),
            &path,
            &source,
            &params,
        )))
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        let uri = params.text_document.uri.clone();
        if uri.scheme() != "file" {
            return Ok(None);
        }
        let uri_s = uri.to_string();
        let Some(source) = self.documents.read().get(&uri_s).cloned() else {
            return Ok(None);
        };
        let Some(path) = uri.to_file_path().ok() else {
            return Ok(None);
        };
        let Some(intel) = self.project_intel(&uri, &source).await else {
            return Ok(Some(DocumentSymbolResponse::Nested(Vec::new())));
        };
        Ok(Some(intel::document_symbols(
            intel.as_ref(),
            &path,
            &source,
        )))
    }

    async fn inlay_hint(&self, params: InlayHintParams) -> Result<Option<Vec<InlayHint>>> {
        if !self.init.read().inlay_hints_enabled {
            return Ok(None);
        }
        let uri = params.text_document.uri.clone();
        if uri.scheme() != "file" {
            return Ok(None);
        }
        let uri_s = uri.to_string();
        let Some(source) = self.documents.read().get(&uri_s).cloned() else {
            return Ok(None);
        };
        let Some(path) = uri.to_file_path().ok() else {
            return Ok(None);
        };
        let Some(intel) = self.project_intel(&uri, &source).await else {
            return Ok(Some(Vec::new()));
        };
        let hide_any = self.init.read().inlay_hints_hide_any;
        Ok(Some(intel::inlay_hints(
            intel.as_ref(),
            &path,
            &source,
            Some(params.range),
            hide_any,
        )))
    }

    async fn code_lens(
        &self,
        params: CodeLensParams,
    ) -> Result<Option<Vec<tower_lsp::lsp_types::CodeLens>>> {
        if !self.init.read().code_lens_references {
            return Ok(None);
        }
        let uri = params.text_document.uri.clone();
        if uri.scheme() != "file" {
            return Ok(None);
        }
        let uri_s = uri.to_string();
        let Some(source) = self.documents.read().get(&uri_s).cloned() else {
            return Ok(None);
        };
        let Some(path) = uri.to_file_path().ok() else {
            return Ok(None);
        };
        let Some(intel) = self.project_intel(&uri, &source).await else {
            return Ok(Some(Vec::new()));
        };
        Ok(Some(intel::code_lenses(
            intel.as_ref(),
            &path,
            &source,
            &uri,
        )))
    }

    async fn rename(&self, params: RenameParams) -> Result<Option<WorkspaceEdit>> {
        let uri = params.text_document_position.text_document.uri.clone();
        if uri.scheme() != "file" {
            return Ok(None);
        }
        let uri_s = uri.to_string();
        let Some(source) = self.documents.read().get(&uri_s).cloned() else {
            return Ok(None);
        };
        let Some(path) = uri.to_file_path().ok() else {
            return Ok(None);
        };
        let Some(intel) = self.project_intel(&uri, &source).await else {
            return Ok(None);
        };
        Ok(intel::rename(intel.as_ref(), &path, &source, &params))
    }

    async fn code_action(&self, params: CodeActionParams) -> Result<Option<CodeActionResponse>> {
        let uri = params.text_document.uri.clone();
        if uri.scheme() != "file" {
            return Ok(None);
        }
        let uri_s = uri.to_string();
        let Some(source) = self.documents.read().get(&uri_s).cloned() else {
            return Ok(None);
        };
        let Some(path) = uri.to_file_path().ok() else {
            return Ok(None);
        };
        let Some(intel) = self.project_intel(&uri, &source).await else {
            return Ok(Some(Vec::new()));
        };
        let pos = params.range.start;
        Ok(Some(intel::code_actions(
            intel.as_ref(),
            &path,
            &source,
            &uri,
            pos,
            &params.context.diagnostics,
        )))
    }

    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> Result<Option<SemanticTokensResult>> {
        let uri = params.text_document.uri.to_string();
        let Some(source) = self.documents.read().get(&uri).cloned() else {
            return Ok(None);
        };
        let tokens = tokio::task::spawn_blocking(move || {
            semantic_tokens_for_document(&source, Some(uri.as_str()))
        })
        .await
        .unwrap_or_default();
        Ok(Some(SemanticTokensResult::Tokens(tokens)))
    }

    async fn semantic_tokens_range(
        &self,
        params: SemanticTokensRangeParams,
    ) -> Result<Option<SemanticTokensRangeResult>> {
        let uri = params.text_document.uri.to_string();
        let Some(source) = self.documents.read().get(&uri).cloned() else {
            return Ok(None);
        };
        let p = params.clone();
        let tokens =
            tokio::task::spawn_blocking(move || intel::semantic_tokens_range(&source, &uri, &p))
                .await
                .unwrap_or_default();
        Ok(Some(SemanticTokensRangeResult::Tokens(tokens)))
    }

    async fn formatting(&self, params: DocumentFormattingParams) -> Result<Option<Vec<TextEdit>>> {
        let uri = params.text_document.uri.to_string();
        let Some(source) = self.documents.read().get(&uri).cloned() else {
            return Ok(None);
        };
        let opts = params.options;
        let edits = tokio::task::spawn_blocking(move || {
            formatting::formatting_edits(&source, Some(uri.as_str()), &opts)
        })
        .await
        .ok()
        .flatten();
        Ok(edits)
    }

    async fn folding_range(&self, params: FoldingRangeParams) -> Result<Option<Vec<FoldingRange>>> {
        let uri = params.text_document.uri.to_string();
        let Some(source) = self.documents.read().get(&uri).cloned() else {
            return Ok(None);
        };
        let ranges = tokio::task::spawn_blocking(move || {
            folding::folding_ranges_for_document(&source, Some(uri.as_str()))
        })
        .await
        .unwrap_or_default();
        Ok(Some(ranges))
    }

    async fn range_formatting(
        &self,
        params: DocumentRangeFormattingParams,
    ) -> Result<Option<Vec<TextEdit>>> {
        let uri = params.text_document.uri.to_string();
        let Some(source) = self.documents.read().get(&uri).cloned() else {
            return Ok(None);
        };
        let opts = params.options;
        let range = params.range;
        let edits = tokio::task::spawn_blocking(move || {
            formatting::range_formatting_edits(&source, Some(uri.as_str()), &opts, &range)
        })
        .await
        .ok()
        .flatten();
        Ok(edits)
    }
}
