//! LSP backend: document storage and full analysis pipeline (parse + type checking) for syntax highlighting and inlay hints.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::RwLock;
use sipha::red::SyntaxNode;
use tower_lsp::lsp_types::MessageType;
use tower_lsp::Client;

use crate::config::LspSettings;
use crate::document::DocumentState;
use crate::util::{parse_uri, uri_to_path};
use leekscript_rs::lsp::DocumentAnalysisLspExt;
use leekscript_rs::DocumentAnalysis;

/// Runs full analysis (parse + scope + type checking) on a blocking thread. Returns (uri, analysis).
/// Uses `main_path` from URI when available; `signature_roots` seed the scope with stdlib/API definitions when provided.
fn run_analysis_blocking(
    uri: String,
    source: String,
    existing_root: Option<SyntaxNode>,
    signature_roots: &[SyntaxNode],
    sig_definition_locations: &HashMap<String, (PathBuf, u32)>,
) -> (String, DocumentState) {
    let main_path = uri_to_path(&uri);
    let analysis = DocumentAnalysis::new(
        &source,
        main_path.as_deref(),
        signature_roots,
        existing_root,
        Some(sig_definition_locations.clone()),
    );
    (uri, analysis)
}

pub struct Backend {
    pub client: Client,
    pub documents: RwLock<HashMap<String, DocumentState>>,
    pub settings: RwLock<LspSettings>,
    /// Pre-loaded stdlib/API signature roots (e.g. from `default_signature_roots()`). Shared across analysis runs.
    pub signature_roots: Arc<Vec<SyntaxNode>>,
    /// Function/global name -> (path, line) from .sig files for hover links (e.g. getCellX, getCellY).
    pub sig_definition_locations: Arc<HashMap<String, (PathBuf, u32)>>,
}

impl Backend {
    /// Run parse on a blocking thread, then store state and publish diagnostics.
    pub(crate) async fn run_analysis_async(
        &self,
        uri: String,
        source: String,
        existing_root: Option<SyntaxNode>,
        version: Option<i32>,
    ) {
        let parse_mode = if existing_root.is_some() {
            "incremental"
        } else {
            "full"
        };
        self.log_trace(format!(
            "leekscript-lsp: run_analysis_async uri={uri} parse={parse_mode} source_len={}",
            source.len()
        ))
        .await;

        let signature_roots = Arc::clone(&self.signature_roots);
        let sig_definition_locations = Arc::clone(&self.sig_definition_locations);
        let result = tokio::task::spawn_blocking(move || {
            let uri_for_fail = uri.clone();
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run_analysis_blocking(
                    uri,
                    source,
                    existing_root,
                    &signature_roots,
                    &sig_definition_locations,
                )
            })) {
                Ok(ok) => Ok(ok),
                Err(_) => Err(uri_for_fail),
            }
        })
        .await;

        match result {
            Ok(Ok((uri, analysis))) => {
                if parse_uri(&uri).is_some() {
                    self.client
                        .log_message(
                            MessageType::LOG,
                            format!("leekscript-lsp: parse done uri={uri} parse={parse_mode}"),
                        )
                        .await;
                    let diags = analysis.lsp_diagnostics(Some(&uri));
                    {
                        let mut docs = self.documents.write();
                        docs.insert(uri.clone(), analysis);
                    }
                    if let Some(url) = parse_uri(&uri) {
                        self.client.publish_diagnostics(url, diags, version).await;
                    }
                }
            }
            Ok(Err(uri)) => {
                let () = self
                    .client
                    .log_message(
                        MessageType::ERROR,
                        format!("leekscript-lsp: parse panicked for uri={uri}, using parse-only recovery"),
                    )
                    .await;
                if parse_uri(&uri).is_some() {
                    if let Some(path) = uri_to_path(&uri) {
                        if let Ok(source) = std::fs::read_to_string(&path) {
                            let recovery = DocumentAnalysis::new(
                                &source,
                                None,
                                &self.signature_roots,
                                None,
                                Some((*self.sig_definition_locations).clone()),
                            );
                            let diags = recovery.lsp_diagnostics(Some(&uri));
                            {
                                let mut docs = self.documents.write();
                                docs.insert(uri.clone(), recovery);
                            }
                            if let Some(url) = parse_uri(&uri) {
                                self.client.publish_diagnostics(url, diags, version).await;
                            }
                        }
                    }
                }
            }
            Err(e) => {
                let () = self
                    .client
                    .log_message(
                        MessageType::ERROR,
                        format!("leekscript-lsp: parse task failed: {e}"),
                    )
                    .await;
            }
        }
    }

    /// Send a LOG-level message to the client only when the "trace" setting is enabled.
    pub(crate) async fn log_trace(&self, msg: String) {
        if self.settings.read().trace {
            let () = self.client.log_message(MessageType::LOG, msg).await;
        }
    }
}
