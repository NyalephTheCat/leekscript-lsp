//! LSP server: full-document sync and semantic highlighting.

use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::{
    DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    InitializeParams, InitializeResult, SemanticTokensFullOptions, SemanticTokensOptions,
    SemanticTokensParams, SemanticTokensResult, SemanticTokensServerCapabilities,
    ServerCapabilities, TextDocumentSyncCapability, TextDocumentSyncKind,
    WorkDoneProgressOptions,
};
use tower_lsp::LanguageServer;

use crate::backend::Backend;
use crate::semantic_tokens::{semantic_token_legend, semantic_tokens_for_document};

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, _: InitializeParams) -> Result<InitializeResult> {
        let legend = semantic_token_legend();
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                semantic_tokens_provider: Some(SemanticTokensServerCapabilities::SemanticTokensOptions(
                    SemanticTokensOptions {
                        legend,
                        full: Some(SemanticTokensFullOptions::Bool(true)),
                        range: None,
                        work_done_progress_options: WorkDoneProgressOptions::default(),
                    },
                )),
                ..Default::default()
            },
            ..Default::default()
        })
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri.to_string();
        let mut docs = self.documents.write();
        docs.insert(uri, params.text_document.text);
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri.to_string();
        let Some(new_source) = params.content_changes.into_iter().last().map(|c| c.text) else {
            return;
        };
        let mut docs = self.documents.write();
        docs.insert(uri, new_source);
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri.to_string();
        let mut docs = self.documents.write();
        docs.remove(&uri);
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
}
