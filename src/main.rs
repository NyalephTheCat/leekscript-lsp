//! `leekscript` language server (stdio JSON-RPC): semantic highlighting only.

#![warn(clippy::pedantic)]

mod backend;
mod semantic_tokens;
mod server;

use std::collections::HashMap;

use parking_lot::RwLock;
use tower_lsp::{LspService, Server};

use crate::backend::Backend;

#[tokio::main]
async fn main() {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, socket) = LspService::new(|client| Backend {
        client,
        documents: RwLock::new(HashMap::new()),
    });
    Server::new(stdin, stdout, socket).serve(service).await;
}
