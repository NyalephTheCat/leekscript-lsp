//! Single-buffer parse/semantic diagnostics → LSP types (UTF-16 ranges).

use leekscript::grammar;
use leekscript::parse::{
    language_options_with_source_directives, parse_doc_with_recovery, parse_signature_doc_with_recovery,
    LanguageOptions, ParseError, ParseErrorInner,
};
use leekscript::{run_semantic_analysis, SemanticCode, SemanticSeverity};
use lsp_types::{
    Diagnostic, DiagnosticRelatedInformation, DiagnosticSeverity, Location, NumberOrString, Position,
    Range, Url,
};
use sipha::diagnostics::error::Diagnostic as SiphaDiagnostic;
use sipha::diagnostics::line_index::LineIndex;
use sipha::diagnostics::parsed_doc::ParsedDoc;
use sipha::parse::engine::ParseError as EngineParseError;
use sipha::types::Span;

use crate::semantic_tokens::signature_mode_for_uri;

pub(crate) fn span_to_lsp_range(doc: &ParsedDoc, span: Span) -> Range {
    let (sl, sc) = doc.offset_to_line_col_utf16(span.start);
    let (el, ec) = doc.offset_to_line_col_utf16(span.end);
    Range {
        start: Position {
            line: sl,
            character: sc,
        },
        end: Position {
            line: el,
            character: ec,
        },
    }
}

pub(crate) fn span_to_range_in_source(source: &str, span: Span) -> Range {
    let idx = LineIndex::new(source.as_bytes());
    let (sl, sc) = idx.line_col_utf16(source, span.start);
    let (el, ec) = idx.line_col_utf16(source, span.end);
    Range {
        start: Position {
            line: sl,
            character: sc,
        },
        end: Position {
            line: el,
            character: ec,
        },
    }
}

/// LSP range covering the entire UTF-8 buffer (byte offsets → UTF-16 line/character).
#[must_use]
pub(crate) fn full_document_range(source: &str) -> Range {
    let end = u32::try_from(source.len()).unwrap_or(u32::MAX);
    span_to_range_in_source(source, Span::new(0, end))
}

pub(crate) fn sipha_diagnostic_message(d: &SiphaDiagnostic, source: &str) -> String {
    let idx = LineIndex::new(source.as_bytes());
    let graph = grammar::built_graph().as_graph();
    d.format_with_source_deduped_expected(
        source.as_bytes(),
        &idx,
        Some(&graph.literals),
        Some(&graph),
    )
}

pub(crate) fn semantic_code_str(code: SemanticCode) -> &'static str {
    match code {
        SemanticCode::UndefinedName => "undefined-name",
        SemanticCode::IncompatibleInitializer => "incompatible-initializer",
        SemanticCode::DeprecatedStrictEquality => "deprecated-strict-equality",
        SemanticCode::DeprecatedCallable => "deprecated-callable",
        SemanticCode::NullableChainAccess => "nullable-chain-access",
    }
}

fn diagnostic_parse_no_doc(source: &str, d: &SiphaDiagnostic) -> Diagnostic {
    let span = d.primary_span(source.len());
    Diagnostic {
        range: span_to_range_in_source(source, span),
        severity: Some(DiagnosticSeverity::ERROR),
        code: Some(NumberOrString::String("parse".to_string())),
        code_description: None,
        source: Some("leekscript".to_string()),
        message: sipha_diagnostic_message(d, source),
        related_information: None,
        tags: None,
        data: None,
    }
}

fn diagnostic_parse(doc: &ParsedDoc, source: &str, d: &SiphaDiagnostic) -> Diagnostic {
    let span = d.primary_span(source.len());
    Diagnostic {
        range: span_to_lsp_range(doc, span),
        severity: Some(DiagnosticSeverity::ERROR),
        code: Some(NumberOrString::String("parse".to_string())),
        code_description: None,
        source: Some("leekscript".to_string()),
        message: sipha_diagnostic_message(d, source),
        related_information: None,
        tags: None,
        data: None,
    }
}

pub(crate) fn leek_parse_error_to_lsp(doc: Option<&ParsedDoc>, source: &str, err: &ParseError) -> Diagnostic {
    match err {
        ParseError::Sipha(
            ParseErrorInner::NoMatch(d) | ParseErrorInner::Other(EngineParseError::NoMatch(d)),
        ) => match doc {
            Some(pd) => diagnostic_parse(pd, source, d),
            None => diagnostic_parse_no_doc(source, d),
        },
        ParseError::Sipha(ParseErrorInner::Other(other)) => Diagnostic {
            range: span_to_range_in_source(source, Span::new(0, 0)),
            severity: Some(DiagnosticSeverity::ERROR),
            code: Some(NumberOrString::String("parse".to_string())),
            code_description: None,
            source: Some("leekscript".to_string()),
            message: other.to_string(),
            related_information: None,
            tags: None,
            data: None,
        },
        ParseError::NoSyntaxRoot => Diagnostic {
            range: span_to_range_in_source(source, Span::new(0, 0)),
            severity: Some(DiagnosticSeverity::ERROR),
            code: Some(NumberOrString::String("parse".to_string())),
            code_description: None,
            source: Some("leekscript".to_string()),
            message: "failed to build syntax tree".to_string(),
            related_information: None,
            tags: None,
            data: None,
        },
    }
}

pub(crate) fn simple_error_at_start(message: String) -> Diagnostic {
    Diagnostic {
        range: Range {
            start: Position {
                line: 0,
                character: 0,
            },
            end: Position {
                line: 0,
                character: 0,
            },
        },
        severity: Some(DiagnosticSeverity::ERROR),
        code: Some(NumberOrString::String("project".to_string())),
        code_description: None,
        source: Some("leekscript".to_string()),
        message,
        related_information: None,
        tags: None,
        data: None,
    }
}

pub(crate) fn compute_diagnostics_single_buffer(
    source: &str,
    document_uri: Option<&str>,
    use_signature_grammar: bool,
) -> Vec<Diagnostic> {
    let opts = language_options_with_source_directives(source, LanguageOptions::v4_experimental_all());
    let base_uri = document_uri.and_then(|s| Url::parse(s).ok());

    let parsed = if use_signature_grammar || document_uri.is_some_and(signature_mode_for_uri) {
        parse_signature_doc_with_recovery(source, opts)
    } else {
        parse_doc_with_recovery(source, opts)
    };

    match parsed {
        Err(e) => vec![leek_parse_error_to_lsp(None, source, &e)],
        Ok(pw) => {
            let mut out: Vec<Diagnostic> = pw
                .errors
                .iter()
                .map(|e| leek_parse_error_to_lsp(Some(&pw.doc), source, e))
                .collect();
            let analysis = run_semantic_analysis(pw.doc.root(), opts.version);
            for d in &analysis.diagnostics {
                let related_information = base_uri
                    .as_ref()
                    .zip(d.related_span)
                    .map(|(u, span)| {
                        vec![DiagnosticRelatedInformation {
                            location: Location {
                                uri: u.clone(),
                                range: span_to_lsp_range(&pw.doc, span),
                            },
                            message: "Related location".to_string(),
                        }]
                    });
                let severity = match d.severity {
                    SemanticSeverity::Error => DiagnosticSeverity::ERROR,
                    SemanticSeverity::Warning => DiagnosticSeverity::WARNING,
                };
                out.push(Diagnostic {
                    range: span_to_lsp_range(&pw.doc, d.span),
                    severity: Some(severity),
                    code: Some(NumberOrString::String(semantic_code_str(d.code).to_string())),
                    code_description: None,
                    source: Some("leekscript".to_string()),
                    message: d.message.clone(),
                    related_information,
                    tags: None,
                    data: None,
                });
            }
            out
        }
    }
}
