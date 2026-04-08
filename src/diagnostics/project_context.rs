//! Shared merged-project preparation and merged-span → LSP location mapping.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use leekscript::include::{
    infer_include_project_root, prepare_merged_check_unit, LoadedProject, MergedCheckUnit, MergedSourceMapping,
};
use leekscript::parse::{
    parse_doc_with_recovery, parse_signature_doc_with_recovery, LanguageOptions, ParsedWithRecovery, ParseError,
    Version,
};
use leekscript::{run_semantic_analysis, AnalysisResult};
use lsp_types::{Location, Range, Url};
use sipha::types::Span;

use super::convert::span_to_range_in_source;

pub(crate) fn path_overlay_from_open_documents(open: &HashMap<String, String>) -> HashMap<PathBuf, String> {
    let mut m = HashMap::new();
    for (uri_s, text) in open {
        let Ok(uri) = Url::parse(uri_s) else {
            continue;
        };
        if !uri.scheme().eq_ignore_ascii_case("file") {
            continue;
        }
        let Ok(path) = uri.to_file_path() else {
            continue;
        };
        if let Ok(canon) = fs::canonicalize(&path) {
            m.insert(canon, text.clone());
        } else {
            m.insert(path, text.clone());
        }
    }
    m
}

pub(crate) fn source_for_mapped_path(project: &LoadedProject, path: &Path) -> Option<String> {
    for f in &project.files {
        if let (Ok(a), Ok(b)) = (fs::canonicalize(&f.path), fs::canonicalize(path)) {
            if a == b {
                return Some(f.source.clone());
            }
        }
    }
    fs::read_to_string(path).ok()
}

pub(crate) fn merged_span_to_file_span(mapping: &MergedSourceMapping, span: Span) -> Option<(PathBuf, Span)> {
    let sm = mapping.span_at_merged_offset(span.start)?;
    let rel = span.start.saturating_sub(sm.merged_start);
    let file_start = sm.file_offset.saturating_add(rel);
    let len = span.end.saturating_sub(span.start);
    let file_end = file_start.saturating_add(len);
    let chunk_len = sm.merged_end.saturating_sub(sm.merged_start);
    let chunk_end = sm.file_offset.saturating_add(chunk_len);
    let file_end = file_end.min(chunk_end);
    Some((sm.path.clone(), Span::new(file_start, file_end)))
}

/// Clamp a byte span to `src` length (for safe LSP ranges).
pub(crate) fn clamp_span_to_source(src: &str, span: Span) -> Span {
    let len = u32::try_from(src.len()).unwrap_or(u32::MAX);
    let end = span.end.min(len);
    let start = span.start.min(end);
    Span::new(start, end)
}

/// Map a merged-buffer span to a `file://` location and UTF-16 range in that file’s source.
pub(crate) fn location_for_merged_span(
    mapping: &MergedSourceMapping,
    project: &LoadedProject,
    combined: &str,
    entry_uri: &Url,
    merged_span: Span,
) -> (Url, Range) {
    let Some((path, fspan)) = merged_span_to_file_span(mapping, merged_span) else {
        return (
            entry_uri.clone(),
            span_to_range_in_source(combined, clamp_span_to_source(combined, merged_span)),
        );
    };
    let Some(src) = source_for_mapped_path(project, &path) else {
        return (
            entry_uri.clone(),
            span_to_range_in_source(combined, clamp_span_to_source(combined, merged_span)),
        );
    };
    let span = clamp_span_to_source(&src, fspan);
    let uri = Url::from_file_path(&path).unwrap_or_else(|()| entry_uri.clone());
    (uri, span_to_range_in_source(&src, span))
}

pub(crate) fn merged_location_to_lsp(
    mapping: &MergedSourceMapping,
    project: &LoadedProject,
    combined: &str,
    entry_uri: &Url,
    merged_span: Span,
) -> Location {
    let (uri, range) = location_for_merged_span(mapping, project, combined, entry_uri, merged_span);
    Location { uri, range }
}

/// Prepare the same merged check unit as diagnostics (`include` expansion + signatures + overlay).
pub(crate) fn prepare_open_file_merged_unit(
    source: &str,
    entry_path: &Path,
    entry_uri: &Url,
    signature_files: &[PathBuf],
    open_documents: &HashMap<String, String>,
) -> Result<MergedCheckUnit, String> {
    let root = infer_include_project_root(entry_path);
    let lang = LanguageOptions::v4_experimental_all();

    let mut overlay = path_overlay_from_open_documents(open_documents);
    if let Ok(entry_canon) = fs::canonicalize(entry_path) {
        overlay.insert(entry_canon, source.to_string());
    } else if let Ok(p) = entry_uri.to_file_path() {
        overlay.insert(p, source.to_string());
    }

    prepare_merged_check_unit(&root, entry_path, lang, signature_files, Some(&overlay)).map_err(|e| e.to_string())
}

pub(crate) fn parse_merged_check_unit(prep: &MergedCheckUnit) -> Result<ParsedWithRecovery, ParseError> {
    if prep.use_signature_grammar {
        parse_signature_doc_with_recovery(&prep.combined, prep.resolved)
    } else {
        parse_doc_with_recovery(&prep.combined, prep.resolved)
    }
}

pub(crate) fn analyze_parsed(pw: &ParsedWithRecovery, version: Version) -> AnalysisResult {
    run_semantic_analysis(pw.doc.root(), version)
}
