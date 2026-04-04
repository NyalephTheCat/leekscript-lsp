//! Open document text and init options for LSP requests.

use std::collections::HashMap;
use std::path::PathBuf;

use leekscript::include::infer_include_project_root;
use parking_lot::RwLock;
use tower_lsp::lsp_types::Url;
use tower_lsp::Client;

use crate::diagnostics;

/// Options from the editor client (`initializationOptions`), e.g. VS Code `leekscript.signatureFiles`.
#[derive(Clone, Default)]
pub struct InitOptions {
    pub signature_files: Vec<PathBuf>,
}

pub struct Backend {
    pub client: Client,
    pub documents: RwLock<HashMap<String, String>>,
    pub init: RwLock<InitOptions>,
}

impl Backend {
    pub async fn publish_document_diagnostics(&self, uri: Url, version: i32, source: &str) {
        let sigs = self.init.read().signature_files.clone();
        let open = self.documents.read().clone();
        let uri_string = uri.to_string();
        let owned = source.to_string();
        let project_root = uri
            .to_file_path()
            .ok()
            .map(|p| infer_include_project_root(&p));
        let client = self.client.clone();
        let (publishes, cascade) = tokio::task::spawn_blocking(move || {
            let publishes = diagnostics::compute_diagnostic_publishes(
                &owned,
                uri_string.as_str(),
                version,
                &sigs,
                &open,
            );
            let cascade = project_root
                .as_ref()
                .map(|root| {
                    diagnostics::cascade_publishes_same_project(&open, uri_string.as_str(), root, &sigs)
                })
                .unwrap_or_default();
            (publishes, cascade)
        })
        .await
        .unwrap_or_default();
        for p in publishes {
            client
                .publish_diagnostics(p.uri, p.diagnostics, p.version)
                .await;
        }
        for p in cascade {
            client
                .publish_diagnostics(p.uri, p.diagnostics, p.version)
                .await;
        }
    }

    pub async fn clear_document_diagnostics(&self, uri: Url) {
        self.client.publish_diagnostics(uri, vec![], None).await;
    }
}
