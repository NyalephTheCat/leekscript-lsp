//! LeekScript language server: diagnostics, hover, go-to-definition, formatting, and more.
//!
//! Run with: `leekscript-lsp` (stdio). Configure your editor to use this binary
//! as the language server for `.leek` files.

mod backend;
mod config;
mod diagnostics;
mod document;
mod doc_comment;
mod include;
mod resolve;
mod semantic_tokens;
mod server;
mod signature_help;
mod util;

use std::collections::HashMap;

use parking_lot::RwLock;
use tower_lsp::{LspService, Server};

use crate::backend::Backend;
use crate::config::LspSettings;

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
