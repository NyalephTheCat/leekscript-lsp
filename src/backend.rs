//! Open document text and init options for LSP requests.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

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
        if let Ok(md) = fs::metadata(p) {
            md.len().hash(&mut h);
            let modified = md.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            let since = modified
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            since.hash(&mut h);
        }
    }
    h.finish()
}

/// Options from the editor client (`initializationOptions`), e.g. VS Code `leekscript.*` settings.
#[derive(Clone, Debug)]
pub struct InitOptions {
    pub signature_files: Vec<PathBuf>,
    /// When false, do not advertise or serve `textDocument/inlayHint`.
    pub inlay_hints_enabled: bool,
    /// When true, do not show `: any` inferred type inlay hints.
    pub inlay_hints_hide_any: bool,
    /// When false, do not advertise or serve reference code lens.
    pub code_lens_references: bool,
}

impl Default for InitOptions {
    fn default() -> Self {
        Self {
            signature_files: Vec::new(),
            inlay_hints_enabled: true,
            inlay_hints_hide_any: false,
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
            .get("inlayHints")
            .and_then(|x| x.get("hideAny"))
            .and_then(serde_json::Value::as_bool)
        {
            o.inlay_hints_hide_any = b;
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
    pub documents: Arc<RwLock<HashMap<String, String>>>,
    /// Latest known version per open document URI string.
    pub document_versions: Arc<RwLock<HashMap<String, i32>>>,
    pub init: Arc<RwLock<InitOptions>>,
    /// Cached merged parse + analysis per `file://` URI (see [`Self::project_intel`]).
    analysis_cache: RwLock<HashMap<String, AnalysisCacheEntry>>,
    /// In-flight debounced diagnostics tasks per URI.
    diagnostics_tasks: RwLock<HashMap<String, tokio::task::JoinHandle<()>>>,
}

impl Backend {
    #[must_use]
    pub fn new(client: Client) -> Self {
        Self {
            client,
            documents: Arc::new(RwLock::new(HashMap::new())),
            document_versions: Arc::new(RwLock::new(HashMap::new())),
            init: Arc::new(RwLock::new(InitOptions::default())),
            analysis_cache: RwLock::new(HashMap::new()),
            diagnostics_tasks: RwLock::new(HashMap::new()),
        }
    }

    /// Drop all merged-analysis cache entries (e.g. after `initialize` options change).
    pub fn clear_intel_cache(&self) {
        self.analysis_cache.write().clear();
    }

    pub fn cancel_diagnostics_task(&self, uri_s: &str) {
        if let Some(h) = self.diagnostics_tasks.write().remove(uri_s) {
            h.abort();
        }
    }

    /// Debounced `publishDiagnostics` for a document URI.
    ///
    /// Cancels any in-flight task for that URI, then schedules a publish after a short delay.
    pub fn schedule_diagnostics_publish(&self, uri: Url, debounce_ms: u64) {
        let uri_s = uri.to_string();
        self.cancel_diagnostics_task(&uri_s);

        let this = self.clone_for_task();
        let h = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(debounce_ms)).await;
            let uri_s = uri.to_string();
            let (version, source) = {
                let docs = this.documents.read();
                let vers = this.document_versions.read();
                let Some(source) = docs.get(&uri_s).cloned() else {
                    return;
                };
                let Some(version) = vers.get(&uri_s).copied() else {
                    return;
                };
                (version, source)
            };
            this.publish_document_diagnostics(uri, version, &source).await;
        });
        self.diagnostics_tasks.write().insert(uri_s, h);
    }

    fn clone_for_task(&self) -> BackendTaskClone {
        BackendTaskClone {
            client: self.client.clone(),
            documents: Arc::clone(&self.documents),
            document_versions: Arc::clone(&self.document_versions),
            init: Arc::clone(&self.init),
        }
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

/// Minimal `Backend` state cloned into background tasks.
///
/// Avoids holding `&Backend` across `.await` boundaries in spawned tasks.
#[derive(Clone)]
struct BackendTaskClone {
    client: Client,
    documents: Arc<RwLock<HashMap<String, String>>>,
    document_versions: Arc<RwLock<HashMap<String, i32>>>,
    init: Arc<RwLock<InitOptions>>,
}

impl BackendTaskClone {
    async fn publish_document_diagnostics(&self, uri: Url, version: i32, source: &str) {
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
}
