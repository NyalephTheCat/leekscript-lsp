//! Parse and semantic diagnostics → LSP `Diagnostic` (UTF-16 ranges).
//!
//! For `file://` documents, diagnostics follow the same model as `leekscript check`: expand
//! top-level `include(...)`, prepend configured signature bundles, parse the combined buffer, then
//! map spans back to the original files. The open editor buffer replaces the entry file on disk
//! before merging.

mod convert;
mod project_context;
mod workspace;

pub(crate) use convert::{full_document_range, span_to_range_in_source};
pub(crate) use project_context::{
    analyze_parsed, clamp_span_to_source, merged_location_to_lsp, merged_span_to_file_span,
    parse_merged_check_unit, prepare_open_file_merged_unit,
};

use std::collections::HashMap;
use std::path::PathBuf;

use leekscript::include::{prepend_signatures_to_merged, MergedSourceMapping};
use leekscript::parse::LanguageOptions;
use lsp_types::{Diagnostic, Url};

use convert::{compute_diagnostics_single_buffer, simple_error_at_start};
use workspace::try_file_project_diagnostics;

/// One `textDocument/publishDiagnostics` the client should apply.
pub struct DiagnosticPublish {
    pub uri: Url,
    pub diagnostics: Vec<Diagnostic>,
    pub version: Option<i32>,
}

/// Re-run merged diagnostics for other open `file://` documents that share the same inferred
/// include project root as the document that just changed. Uses `version: None` so the client does
/// not drop updates when we do not know their document versions.
pub(crate) fn cascade_publishes_same_project(
    open: &HashMap<String, String>,
    skip_uri: &str,
    project_root: &std::path::Path,
    signature_files: &[PathBuf],
) -> Vec<DiagnosticPublish> {
    use leekscript::include::infer_include_project_root;

    let mut out = Vec::new();
    for (uri_s, text) in open {
        if uri_s == skip_uri {
            continue;
        }
        let Ok(u) = Url::parse(uri_s) else {
            continue;
        };
        if !u.scheme().eq_ignore_ascii_case("file") {
            continue;
        }
        let Ok(p) = u.to_file_path() else {
            continue;
        };
        if infer_include_project_root(&p) != project_root {
            continue;
        }
        let mut batch = compute_diagnostic_publishes(text, uri_s, 0, signature_files, open);
        for b in &mut batch {
            b.version = None;
        }
        out.extend(batch);
    }
    out
}

fn group_by_uri(pairs: Vec<(Url, Diagnostic)>) -> HashMap<String, Vec<Diagnostic>> {
    let mut m: HashMap<String, Vec<Diagnostic>> = HashMap::new();
    for (uri, d) in pairs {
        m.entry(uri.to_string()).or_default().push(d);
    }
    m
}

/// Compute diagnostic publishes for one open document (`source` is the editor buffer).
#[must_use]
pub fn compute_diagnostic_publishes(
    source: &str,
    document_uri_str: &str,
    document_version: i32,
    signature_files: &[PathBuf],
    open_documents: &HashMap<String, String>,
) -> Vec<DiagnosticPublish> {
    let Ok(entry_uri) = Url::parse(document_uri_str) else {
        return Vec::new();
    };

    if entry_uri.scheme() != "file" {
        let use_sig = !signature_files.is_empty();
        if use_sig {
            let merged_default = MergedSourceMapping::default();
            let lang = LanguageOptions::v4_experimental_all();
            let combined =
                match prepend_signatures_to_merged(lang, signature_files, source, merged_default) {
                    Ok((c, _)) => c,
                    Err(e) => {
                        return vec![DiagnosticPublish {
                            uri: entry_uri,
                            diagnostics: vec![simple_error_at_start(e.to_string())],
                            version: Some(document_version),
                        }];
                    }
                };
            return vec![DiagnosticPublish {
                uri: entry_uri,
                diagnostics: compute_diagnostics_single_buffer(
                    &combined,
                    Some(document_uri_str),
                    true,
                ),
                version: Some(document_version),
            }];
        }
        return vec![DiagnosticPublish {
            uri: entry_uri,
            diagnostics: compute_diagnostics_single_buffer(source, Some(document_uri_str), false),
            version: Some(document_version),
        }];
    }

    let Ok(path) = entry_uri.to_file_path() else {
        return vec![DiagnosticPublish {
            uri: entry_uri,
            diagnostics: vec![simple_error_at_start("non-local file URI".to_string())],
            version: Some(document_version),
        }];
    };

    match try_file_project_diagnostics(source, &path, &entry_uri, signature_files, open_documents) {
        Ok(pairs) => {
            let mut grouped = group_by_uri(pairs);
            let entry_key = entry_uri.to_string();
            let entry_diags = grouped.remove(&entry_key).unwrap_or_default();
            let mut out = vec![DiagnosticPublish {
                uri: entry_uri.clone(),
                diagnostics: entry_diags,
                version: Some(document_version),
            }];
            for (uri_s, diags) in grouped {
                let Ok(u) = Url::parse(&uri_s) else {
                    continue;
                };
                out.push(DiagnosticPublish {
                    uri: u,
                    diagnostics: diags,
                    version: None,
                });
            }
            out.sort_by(|a, b| a.uri.as_str().cmp(b.uri.as_str()));
            out
        }
        Err(msg) => vec![DiagnosticPublish {
            uri: entry_uri,
            diagnostics: vec![simple_error_at_start(msg)],
            version: Some(document_version),
        }],
    }
}
