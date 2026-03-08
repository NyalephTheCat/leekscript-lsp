//! `LeekScript` language server: syntax highlighting only (semantic tokens).
//!
//! Run with: `leekscript-lsp` (stdio). Configure your editor to use this binary
//! as the language server for `.leek` files.

#![warn(clippy::pedantic)]

mod backend;
mod config;
mod document;
mod semantic_tokens;
mod server;
mod util;

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;
use tower_lsp::{LspService, Server};

use crate::backend::Backend;
use crate::config::LspSettings;
use leekscript_rs::signatures::default_signature_roots_with_locations;

#[tokio::main]
async fn main() {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let initial_sig_state = default_signature_roots_with_locations();
    let signature_state = Arc::new(RwLock::new(initial_sig_state));
    let (service, socket) = LspService::new(|client| Backend {
        client,
        documents: RwLock::new(HashMap::new()),
        settings: RwLock::new(LspSettings::default()),
        signature_state,
    });
    Server::new(stdin, stdout, socket).serve(service).await;
}
