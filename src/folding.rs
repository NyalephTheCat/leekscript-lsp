//! `textDocument/foldingRange`: brace blocks from the recovered parse tree.

use leekscript::parse::{
    language_options_with_source_directives, parse_doc_with_recovery,
    parse_signature_doc_with_recovery, LanguageOptions,
};
use leekscript::syntax::kinds::Node;
use lsp_types::{FoldingRange, FoldingRangeKind};
use sipha::diagnostics::parsed_doc::ParsedDoc;
use sipha::tree::red::SyntaxNode;
use sipha::types::IntoSyntaxKind;

use crate::semantic_tokens::signature_mode_for_uri;

/// Folding ranges for `source`, using the same parse mode as semantic tokens / formatting.
#[must_use]
pub fn folding_ranges_for_document(source: &str, document_uri: Option<&str>) -> Vec<FoldingRange> {
    let base_opts =
        language_options_with_source_directives(source, LanguageOptions::v4_experimental_all());
    let parsed = if document_uri.is_some_and(signature_mode_for_uri) {
        parse_signature_doc_with_recovery(source, base_opts)
    } else {
        parse_doc_with_recovery(source, base_opts)
    };
    let Ok(pw) = parsed else {
        return Vec::new();
    };
    folding_ranges_from_parsed(&pw.doc)
}

fn folding_ranges_from_parsed(doc: &ParsedDoc) -> Vec<FoldingRange> {
    let kind_block = Node::Block.into_syntax_kind();
    let mut out = Vec::new();
    for node in doc.root().descendant_nodes() {
        if node.kind() == kind_block {
            push_block_fold(doc, &node, &mut out);
        }
    }
    out
}

fn push_block_fold(doc: &ParsedDoc, node: &SyntaxNode, out: &mut Vec<FoldingRange>) {
    let span = node.text_range();
    if span.is_empty() || span.end <= span.start {
        return;
    }
    let last = span.end.saturating_sub(1);
    let (start_line, _) = doc.offset_to_line_col(span.start);
    let (end_line, _) = doc.offset_to_line_col(last);
    if end_line <= start_line {
        return;
    }
    out.push(FoldingRange {
        start_line,
        end_line,
        kind: Some(FoldingRangeKind::Region),
        ..Default::default()
    });
}
