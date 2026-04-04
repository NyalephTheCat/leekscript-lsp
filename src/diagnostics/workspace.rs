//! Merged project + include overlay: map diagnostics back to original `file://` documents.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use leekscript::include::{infer_include_project_root, prepare_merged_check_unit, LoadedProject, MergedSourceMapping};
use leekscript::parse::{parse_doc_with_recovery, parse_signature_doc_with_recovery, LanguageOptions, ParseError, ParseErrorInner};
use leekscript::{run_semantic_analysis, SemanticDiagnostic, SemanticSeverity};
use lsp_types::{
    Diagnostic, DiagnosticRelatedInformation, DiagnosticSeverity, Location, NumberOrString, Range, Url,
};
use sipha::parse::engine::ParseError as EngineParseError;
use sipha::types::Span;

use super::convert::{
    leek_parse_error_to_lsp, semantic_code_str, sipha_diagnostic_message, span_to_range_in_source,
};

fn path_overlay_from_open_documents(open: &HashMap<String, String>) -> HashMap<PathBuf, String> {
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

fn source_for_mapped_path(project: &LoadedProject, path: &Path) -> Option<String> {
    for f in &project.files {
        if let (Ok(a), Ok(b)) = (fs::canonicalize(&f.path), fs::canonicalize(path)) {
            if a == b {
                return Some(f.source.clone());
            }
        }
    }
    fs::read_to_string(path).ok()
}

fn merged_span_to_file_span(mapping: &MergedSourceMapping, span: Span) -> Option<(PathBuf, Span)> {
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

fn clamp_span_to_source(src: &str, span: Span) -> Span {
    let len = u32::try_from(src.len()).unwrap_or(u32::MAX);
    let end = span.end.min(len);
    let start = span.start.min(end);
    Span::new(start, end)
}

fn location_for_merged_span(
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

fn related_for_merged_span(
    mapping: &MergedSourceMapping,
    project: &LoadedProject,
    combined: &str,
    entry_uri: &Url,
    related: Span,
) -> DiagnosticRelatedInformation {
    let (uri, range) = location_for_merged_span(mapping, project, combined, entry_uri, related);
    DiagnosticRelatedInformation {
        location: Location { uri, range },
        message: "Related location".to_string(),
    }
}

fn parse_error_diagnostic_merged(
    mapping: &MergedSourceMapping,
    project: &LoadedProject,
    combined: &str,
    entry_uri: &Url,
    err: &ParseError,
) -> (Url, Diagnostic) {
    if let ParseError::Sipha(
        ParseErrorInner::NoMatch(d) | ParseErrorInner::Other(EngineParseError::NoMatch(d)),
    ) = err
    {
        let merged_span = d.primary_span(combined.len());
        let message = sipha_diagnostic_message(d, combined);
        let (uri, range) = location_for_merged_span(mapping, project, combined, entry_uri, merged_span);
        return (
            uri,
            Diagnostic {
                range,
                severity: Some(DiagnosticSeverity::ERROR),
                code: Some(NumberOrString::String("parse".to_string())),
                code_description: None,
                source: Some("leekscript".to_string()),
                message,
                related_information: None,
                tags: None,
                data: None,
            },
        );
    }
    let (uri, range) = location_for_merged_span(mapping, project, combined, entry_uri, Span::new(0, 0));
    let mut diag = leek_parse_error_to_lsp(None, combined, err);
    diag.range = range;
    (uri, diag)
}

fn semantic_diagnostic_merged(
    mapping: &MergedSourceMapping,
    project: &LoadedProject,
    combined: &str,
    entry_uri: &Url,
    d: &SemanticDiagnostic,
) -> (Url, Diagnostic) {
    let severity = match d.severity {
        SemanticSeverity::Error => DiagnosticSeverity::ERROR,
        SemanticSeverity::Warning => DiagnosticSeverity::WARNING,
    };
    let (uri, range) = location_for_merged_span(mapping, project, combined, entry_uri, d.span);
    let related_information = d.related_span.map(|r| {
        vec![related_for_merged_span(mapping, project, combined, entry_uri, r)]
    });
    (
        uri,
        Diagnostic {
            range,
            severity: Some(severity),
            code: Some(NumberOrString::String(semantic_code_str(d.code).to_string())),
            code_description: None,
            source: Some("leekscript".to_string()),
            message: d.message.clone(),
            related_information,
            tags: None,
            data: None,
        },
    )
}

pub(crate) fn try_file_project_diagnostics(
    source: &str,
    entry_path: &Path,
    entry_uri: &Url,
    signature_files: &[PathBuf],
    open_documents: &HashMap<String, String>,
) -> Result<Vec<(Url, Diagnostic)>, String> {
    let root = infer_include_project_root(entry_path);
    let lang = LanguageOptions::v4_experimental_all();

    let mut overlay = path_overlay_from_open_documents(open_documents);
    if let Ok(entry_canon) = fs::canonicalize(entry_path) {
        overlay.insert(entry_canon, source.to_string());
    } else if let Ok(p) = entry_uri.to_file_path() {
        overlay.insert(p, source.to_string());
    }

    let prep = prepare_merged_check_unit(&root, entry_path, lang, signature_files, Some(&overlay))
        .map_err(|e| e.to_string())?;

    let parsed = if prep.use_signature_grammar {
        parse_signature_doc_with_recovery(&prep.combined, prep.resolved)
    } else {
        parse_doc_with_recovery(&prep.combined, prep.resolved)
    };

    let mut pairs: Vec<(Url, Diagnostic)> = Vec::new();

    match parsed {
        Err(e) => {
            pairs.push(parse_error_diagnostic_merged(
                &prep.mapping,
                &prep.project,
                &prep.combined,
                entry_uri,
                &e,
            ));
        }
        Ok(pw) => {
            for err in &pw.errors {
                pairs.push(parse_error_diagnostic_merged(
                    &prep.mapping,
                    &prep.project,
                    &prep.combined,
                    entry_uri,
                    err,
                ));
            }
            let analysis = run_semantic_analysis(pw.doc.root(), prep.resolved.version);
            for d in &analysis.diagnostics {
                pairs.push(semantic_diagnostic_merged(
                    &prep.mapping,
                    &prep.project,
                    &prep.combined,
                    entry_uri,
                    d,
                ));
            }
        }
    }

    Ok(pairs)
}
