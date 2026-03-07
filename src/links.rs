//! Document links: include paths and other link targets.

use std::collections::HashMap;
use std::path::PathBuf;

use leekscript_rs::syntax::Kind;
use sipha::types::IntoSyntaxKind;
use tower_lsp::lsp_types::*;

use crate::document::DocumentState;
use crate::util::{path_to_uri, span_to_position, uri_to_path};

/// Compute document links (e.g. include paths) for the document.
pub fn document_links_at(
    docs: &HashMap<String, DocumentState>,
    uri: &str,
) -> Option<Vec<DocumentLink>> {
    let state = docs.get(uri)?;
    let source = state.source.as_str();
    let line_index = &state.line_index;
    let root = state.root.as_ref()?;

    let base_dir: PathBuf = state
        .main_path
        .as_ref()
        .and_then(|p| p.parent().map(|x| x.to_path_buf()))
        .or_else(|| uri_to_path(uri).and_then(|p| p.parent().map(|x| x.to_path_buf())))?;

    let mut links = Vec::new();
    for node in root.find_all_nodes(Kind::NodeInclude.into_syntax_kind()) {
        let token = node
            .descendant_tokens()
            .into_iter()
            .find(|t| t.kind_as::<Kind>() == Some(Kind::TokString))?;
        let tr = token.text_range();
        let range = Range {
            start: span_to_position(source, line_index, tr.start),
            end: span_to_position(source, line_index, tr.end),
        };
        let path_str = token.text().trim_matches(|c| c == '"' || c == '\'').to_string();
        let resolved = base_dir.join(&path_str);
        if let Some(target_url) = path_to_uri(&resolved) {
            links.push(DocumentLink {
                range,
                target: Some(target_url),
                tooltip: None,
                data: None,
            });
        }
    }
    Some(links)
}
