//! `textDocument/formatting` and `textDocument/rangeFormatting` via [`leekscript::format`].

use leekscript::document::LeekDoc;
use leekscript::format::{format_leek_doc, format_leek_doc_range, FormatOptions};
use leekscript::parse::{
    language_options_with_source_directives, parse_doc_with_recovery,
    parse_signature_doc_with_recovery, LanguageOptions,
};
use lsp_types::{FormattingOptions, Range, TextEdit};
use sipha::diagnostics::line_index::LineIndex;
use sipha::types::Span;

use crate::diagnostics::{full_document_range, span_to_range_in_source};
use crate::semantic_tokens::signature_mode_for_uri;

fn leek_format_options_from_lsp(opts: &FormattingOptions) -> FormatOptions {
    let mut f = FormatOptions::default();
    if opts.tab_size > 0 {
        f.tab_width = opts.tab_size as usize;
        if opts.insert_spaces {
            f.indent_width = opts.tab_size as usize;
            f.use_tabs = false;
        } else {
            f.use_tabs = true;
        }
    }
    if let Some(v) = opts.insert_final_newline {
        f.trailing_newline = v;
    }
    f
}

/// Format the full buffer and return a single replacement edit, or `None` if unchanged or unparseable.
#[must_use]
pub(crate) fn formatting_edits(
    source: &str,
    document_uri: Option<&str>,
    lsp_opts: &FormattingOptions,
) -> Option<Vec<TextEdit>> {
    let base_opts =
        language_options_with_source_directives(source, LanguageOptions::v4_experimental_all());
    let parsed = if document_uri.is_some_and(signature_mode_for_uri) {
        parse_signature_doc_with_recovery(source, base_opts)
    } else {
        parse_doc_with_recovery(source, base_opts)
    };
    let Ok(pw) = parsed else {
        return None;
    };
    let doc = LeekDoc::from_parsed(&pw.doc);
    let fmt_opts = leek_format_options_from_lsp(lsp_opts);
    let formatted = format_leek_doc(&doc, &fmt_opts);
    if formatted == source {
        return None;
    }
    Some(vec![TextEdit {
        range: full_document_range(source),
        new_text: formatted,
    }])
}

/// LSP range (UTF-16 line/character) → UTF-8 half-open byte span.
#[must_use]
pub(crate) fn lsp_range_to_byte_span(source: &str, range: &Range) -> Option<Span> {
    let idx = LineIndex::new(source.as_bytes());
    let start = idx.line_col_utf16_to_byte(source, range.start.line, range.start.character)?;
    let end = idx.line_col_utf16_to_byte_clamped(source, range.end.line, range.end.character)?;
    let (lo, hi) = if start <= end {
        (start, end)
    } else {
        (end, start)
    };
    Some(Span::new(lo, hi))
}

/// Format only the smallest subtree covering the LSP range.
#[must_use]
pub(crate) fn range_formatting_edits(
    source: &str,
    document_uri: Option<&str>,
    lsp_opts: &FormattingOptions,
    range: &Range,
) -> Option<Vec<TextEdit>> {
    let sel = lsp_range_to_byte_span(source, range)?;
    if sel.is_empty() {
        return None;
    }
    let base_opts =
        language_options_with_source_directives(source, LanguageOptions::v4_experimental_all());
    let parsed = if document_uri.is_some_and(signature_mode_for_uri) {
        parse_signature_doc_with_recovery(source, base_opts)
    } else {
        parse_doc_with_recovery(source, base_opts)
    };
    let Ok(pw) = parsed else {
        return None;
    };
    let doc = LeekDoc::from_parsed(&pw.doc);
    let fmt_opts = leek_format_options_from_lsp(lsp_opts);
    let range_result = format_leek_doc_range(&doc, sel, &fmt_opts);
    let Some(parts) = range_result else {
        return Some(vec![]);
    };
    if parts.is_empty() {
        return Some(vec![]);
    }
    let mut edits = Vec::with_capacity(parts.len());
    for (span, text) in parts {
        edits.push(TextEdit {
            range: span_to_range_in_source(source, span),
            new_text: text,
        });
    }
    Some(edits)
}
