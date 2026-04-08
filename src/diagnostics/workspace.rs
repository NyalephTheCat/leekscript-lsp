//! Merged project + include overlay: map diagnostics back to original `file://` documents.

use std::collections::HashMap;
use std::path::PathBuf;

use leekscript::parse::{ParseError, ParseErrorInner};
use leekscript::{LoadedProject, MergedSourceMapping, SemanticDiagnostic, SemanticSeverity};
use lsp_types::{
    Diagnostic, DiagnosticRelatedInformation, DiagnosticSeverity, Location, NumberOrString, Url,
};
use sipha::parse::engine::ParseError as EngineParseError;
use sipha::types::Span;

use super::convert::{leek_parse_error_to_lsp, semantic_code_str, sipha_diagnostic_message};
use super::project_context::{
    analyze_parsed, location_for_merged_span, parse_merged_check_unit, prepare_open_file_merged_unit,
};

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
    entry_path: &std::path::Path,
    entry_uri: &Url,
    signature_files: &[PathBuf],
    open_documents: &HashMap<String, String>,
) -> Result<Vec<(Url, Diagnostic)>, String> {
    let prep = prepare_open_file_merged_unit(source, entry_path, entry_uri, signature_files, open_documents)?;

    let parsed = parse_merged_check_unit(&prep);

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
            let analysis = analyze_parsed(&pw, prep.resolved.version);
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
