//! `leekscript` language server (stdio JSON-RPC): diagnostics, semantic highlighting.

#![warn(clippy::pedantic)]

mod backend;
mod diagnostics;
mod folding;
mod formatting;
mod hover_markdown;
mod intel;
mod semantic_tokens;
mod server;
mod token_context;

use tower_lsp::{LspService, Server};

use crate::backend::Backend;

#[tokio::main]
async fn main() {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, socket) = LspService::new(Backend::new);
    Server::new(stdin, stdout, socket).serve(service).await;
}
