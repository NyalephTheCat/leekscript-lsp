//! Open document text and init options for LSP requests.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use leekscript::include::infer_include_project_root;
use parking_lot::RwLock;
use tower_lsp::lsp_types::Url;
use tower_lsp::Client;

use crate::diagnostics;
use crate::intel::{self, ProjectIntel};

struct AnalysisCacheEntry {
    source_fp: u64,
    sig_fp: u64,
    intel: Arc<ProjectIntel>,
}

fn fingerprint_source(source: &str) -> u64 {
    let mut h = DefaultHasher::new();
    source.hash(&mut h);
    h.finish()
}

fn fingerprint_signature_files(paths: &[PathBuf]) -> u64 {
    let mut h = DefaultHasher::new();
    paths.len().hash(&mut h);
    for p in paths {
        p.hash(&mut h);
    }
    h.finish()
}

/// Options from the editor client (`initializationOptions`), e.g. VS Code `leekscript.*` settings.
#[derive(Clone, Debug)]
pub struct InitOptions {
    pub signature_files: Vec<PathBuf>,
    /// When false, do not advertise or serve `textDocument/inlayHint`.
    pub inlay_hints_enabled: bool,
    /// When false, do not advertise or serve reference code lens.
    pub code_lens_references: bool,
}

impl Default for InitOptions {
    fn default() -> Self {
        Self {
            signature_files: Vec::new(),
            inlay_hints_enabled: true,
            code_lens_references: true,
        }
    }
}

impl InitOptions {
    /// Parse VS Code / JSON `initializationOptions` (see `leekscript-code` extension).
    #[must_use]
    pub fn from_initialization_json(value: Option<&serde_json::Value>) -> Self {
        let mut o = Self::default();
        let Some(v) = value else {
            return o;
        };
        if let Some(arr) = v.get("signatureFiles").and_then(|x| x.as_array()) {
            o.signature_files = arr
                .iter()
                .filter_map(|x| x.as_str().map(PathBuf::from))
                .collect();
        }
        if let Some(b) = v
            .get("inlayHints")
            .and_then(|x| x.get("enabled"))
            .and_then(serde_json::Value::as_bool)
        {
            o.inlay_hints_enabled = b;
        }
        if let Some(b) = v
            .get("codeLens")
            .and_then(|x| x.get("references"))
            .and_then(serde_json::Value::as_bool)
        {
            o.code_lens_references = b;
        }
        o
    }
}

pub struct Backend {
    pub client: Client,
    pub documents: RwLock<HashMap<String, String>>,
    pub init: RwLock<InitOptions>,
    /// Cached merged parse + analysis per `file://` URI (see [`Self::project_intel`]).
    analysis_cache: RwLock<HashMap<String, AnalysisCacheEntry>>,
}

impl Backend {
    #[must_use]
    pub fn new(client: Client) -> Self {
        Self {
            client,
            documents: RwLock::new(HashMap::new()),
            init: RwLock::new(InitOptions::default()),
            analysis_cache: RwLock::new(HashMap::new()),
        }
    }

    /// Drop all merged-analysis cache entries (e.g. after `initialize` options change).
    pub fn clear_intel_cache(&self) {
        self.analysis_cache.write().clear();
    }

    /// Invalidate cache for every open `file://` document in the same include project as `changed_path`.
    ///
    /// Changing one buffer can change the merged unit for siblings (includes + overlay).
    pub fn invalidate_intel_cache_for_project_of(&self, changed_path: &Path) {
        let root = infer_include_project_root(changed_path);
        self.analysis_cache.write().retain(|uri_s, _| {
            let Ok(u) = Url::parse(uri_s) else {
                return true;
            };
            let Ok(p) = u.to_file_path() else {
                return true;
            };
            infer_include_project_root(&p) != root
        });
    }

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

    /// Merged parse + semantic analysis for `file://` buffers (same pipeline as diagnostics).
    ///
    /// Results are cached per URI until the buffer text, signature list, or another file in the
    /// same include project changes ([`Self::invalidate_intel_cache_for_project_of`]).
    pub async fn project_intel(&self, uri: &Url, source: &str) -> Option<Arc<ProjectIntel>> {
        let path = uri.to_file_path().ok()?;
        let uri_s = uri.to_string();
        let source_fp = fingerprint_source(source);
        let (sig_fp, sigs) = {
            let i = self.init.read();
            (fingerprint_signature_files(&i.signature_files), i.signature_files.clone())
        };

        {
            let cache = self.analysis_cache.read();
            if let Some(e) = cache.get(&uri_s) {
                if e.source_fp == source_fp && e.sig_fp == sig_fp {
                    return Some(Arc::clone(&e.intel));
                }
            }
        }

        let open = self.documents.read().clone();
        let uri = uri.clone();
        let source = source.to_string();
        let built = tokio::task::spawn_blocking(move || {
            intel::load_intel_file(&source, &path, &uri, &sigs, &open)
        })
        .await
        .ok()??;
        let arc = Arc::new(built);
        self.analysis_cache.write().insert(
            uri_s,
            AnalysisCacheEntry {
                source_fp,
                sig_fp,
                intel: Arc::clone(&arc),
            },
        );
        Some(arc)
    }
}
