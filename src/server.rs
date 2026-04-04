//! LSP server: full-document sync, diagnostics, and semantic tokens.

use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::{
    DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams, InitializeParams, InitializeResult,
    SemanticTokensFullOptions, SemanticTokensOptions, SemanticTokensParams, SemanticTokensResult,
    SemanticTokensServerCapabilities, ServerCapabilities, TextDocumentSyncCapability,
    TextDocumentSyncKind, WorkDoneProgressOptions,
};
use tower_lsp::LanguageServer;

use crate::backend::{Backend, InitOptions};
use crate::diagnostics;
use crate::semantic_tokens::{semantic_token_legend, semantic_tokens_for_document};

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        let sigs = diagnostics::signature_files_from_init(params.initialization_options.as_ref());
        *self.init.write() = InitOptions {
            signature_files: sigs,
        };

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
        let uri = params.text_document.uri.clone();
        let uri_str = uri.to_string();
        let text = params.text_document.text.clone();
        let version = params.text_document.version;
        {
            let mut docs = self.documents.write();
            docs.insert(uri_str, text.clone());
        }
        self.publish_document_diagnostics(uri, version, &text).await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri.clone();
        let uri_str = uri.to_string();
        let version = params.text_document.version;
        let Some(new_source) = params.content_changes.into_iter().last().map(|c| c.text) else {
            return;
        };
        {
            let mut docs = self.documents.write();
            docs.insert(uri_str, new_source.clone());
        }
        self.publish_document_diagnostics(uri, version, &new_source)
            .await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri.clone();
        let uri_str = uri.to_string();
        {
            let mut docs = self.documents.write();
            docs.remove(&uri_str);
        }
        self.clear_document_diagnostics(uri).await;
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
