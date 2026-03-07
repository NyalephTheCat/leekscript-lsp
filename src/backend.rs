//! LSP backend: document analysis, hover, go-to-definition, completion, and other handlers.

use std::collections::HashMap;
use std::path::PathBuf;

use parking_lot::RwLock;

use leekscript_rs::formatter::FormatterOptions;
use leekscript_rs::{parse_signatures, DocumentAnalysis};
use sipha::red::SyntaxNode;
use tower_lsp::lsp_types::*;
use tower_lsp::Client;

use crate::config::LspSettings;
use crate::diagnostics::semantic_to_lsp;
use crate::document::DocumentState;
use crate::util::{parse_uri, uri_to_path};

use crate::completion;
use crate::folding;
use crate::formatting;
use crate::hierarchy;
use crate::hover;
use crate::inlay_hints;
use crate::links;
use crate::navigation;
use crate::signature_help_handler;
use crate::symbols;

/// Embedded standard library signature files (constants and functions).
const STDLIB_CONSTANTS_SIG: &str = include_str!("../signatures/stdlib_constants.sig");
const STDLIB_FUNCTIONS_SIG: &str = include_str!("../signatures/stdlib_functions.sig");

/// Runs parse + analysis on a blocking thread. Returns (uri, analysis, lsp_diagnostics).
/// Used by [`Backend::run_analysis_async`] so the async runtime is not blocked.
fn run_analysis_blocking(
    uri: String,
    source: String,
    main_path: Option<PathBuf>,
    signature_roots: Vec<SyntaxNode>,
    existing_root: Option<SyntaxNode>,
) -> (String, DocumentAnalysis, Vec<Diagnostic>) {
    let analysis = DocumentAnalysis::new(
        &source,
        main_path.as_deref(),
        &signature_roots,
        existing_root,
    );
    let lsp_diags: Vec<Diagnostic> = analysis
        .diagnostics
        .iter()
        .map(|d| semantic_to_lsp(d, &analysis.source, &analysis.line_index))
        .collect();
    (uri, analysis, lsp_diags)
}

pub struct Backend {
    pub client: Client,
    pub documents: RwLock<HashMap<String, DocumentState>>,
    pub settings: RwLock<LspSettings>,
}
impl Backend {
    /// Build signature roots from current settings (embedded stdlib + optional files).
    fn get_signature_roots(&self) -> Vec<SyntaxNode> {
        let settings = self.settings.read();
        let mut roots = Vec::new();
        if settings.load_stdlib_signatures {
            if let Ok(Some(node)) = parse_signatures(STDLIB_CONSTANTS_SIG) {
                roots.push(node);
            }
            if let Ok(Some(node)) = parse_signatures(STDLIB_FUNCTIONS_SIG) {
                roots.push(node);
            }
        }
        for path in &settings.signature_files {
            if let Ok(content) = std::fs::read_to_string(path) {
                if let Ok(Some(node)) = parse_signatures(&content) {
                    roots.push(node);
                }
            }
        }
        roots
    }

    /// Run analysis on a blocking thread, then store state and publish diagnostics.
    /// Keeps the async runtime responsive during heavy parse/analysis.
    /// When the client sends `$/cancelRequest`, tower-lsp aborts the request future so we stop
    /// waiting; the blocking task may still run to completion (there is no token to abort it).
    pub(crate) async fn run_analysis_async(
        &self,
        uri: String,
        source: String,
        existing_root: Option<SyntaxNode>,
    ) {
        let main_path = uri_to_path(&uri);
        let signature_roots = self.get_signature_roots();
        let result = tokio::task::spawn_blocking(move || {
            run_analysis_blocking(uri, source, main_path, signature_roots, existing_root)
        })
        .await;

        match result {
            Ok((uri, analysis, lsp_diags)) => {
                let n_diag = analysis.diagnostics.len();
                if let Some(url) = parse_uri(&uri) {
                    {
                        let mut docs = self.documents.write();
                        docs.insert(uri.clone(), analysis);
                    }
                    self.client
                        .log_message(
                            MessageType::LOG,
                            format!("leekscript-lsp: analysis done uri={uri} diagnostics={n_diag}"),
                        )
                        .await;
                    self.client.publish_diagnostics(url, lsp_diags, None).await;
                }
            }
            Err(e) => {
                let _ = self
                    .client
                    .log_message(
                        MessageType::ERROR,
                        format!("leekscript-lsp: analysis task failed: {e}"),
                    )
                    .await;
            }
        }
    }

    /// Send a LOG-level message to the client only when the "trace" setting is enabled.
    pub(crate) async fn log_trace(&self, msg: String) {
        if self.settings.read().trace {
            let _ = self.client.log_message(MessageType::LOG, msg).await;
        }
    }

    pub(crate) fn hover_at(&self, uri: &str, line: u32, character: u32) -> Option<Hover> {
        let docs = self.documents.read();
        hover::hover_at(&docs, uri, line, character)
    }

    /// Compute inlay hints (e.g. `: type` after variable names) for the given document range.
    pub(crate) fn inlay_hints_at(
        &self,
        uri: &str,
        range_start_line: u32,
        range_start_character: u32,
        range_end_line: u32,
        range_end_character: u32,
    ) -> Vec<InlayHint> {
        let docs = self.documents.read();
        let settings = self.settings.read();
        inlay_hints::inlay_hints_at(
            &docs,
            &settings,
            uri,
            range_start_line,
            range_start_character,
            range_end_line,
            range_end_character,
        )
    }

    pub(crate) fn goto_definition_at(
        &self,
        uri: &str,
        line: u32,
        character: u32,
    ) -> Option<Vec<Location>> {
        let docs = self.documents.read();
        navigation::goto_definition_at(&docs, uri, line, character)
    }

    pub(crate) fn references_at(
        &self,
        uri: &str,
        line: u32,
        character: u32,
        include_declaration: bool,
    ) -> Option<Vec<Location>> {
        let docs = self.documents.read();
        navigation::references_at(&docs, uri, line, character, include_declaration)
    }

    pub(crate) fn rename_at(
        &self,
        uri: &str,
        line: u32,
        character: u32,
        new_name: &str,
    ) -> Option<WorkspaceEdit> {
        let docs = self.documents.read();
        navigation::rename_at(&docs, uri, line, character, new_name)
    }

    pub(crate) fn prepare_rename_at(
        &self,
        uri: &str,
        line: u32,
        character: u32,
    ) -> Option<PrepareRenameResponse> {
        let docs = self.documents.read();
        navigation::prepare_rename_at(&docs, uri, line, character)
    }

    pub(crate) fn document_highlight_at(
        &self,
        uri: &str,
        line: u32,
        character: u32,
    ) -> Option<Vec<DocumentHighlight>> {
        let docs = self.documents.read();
        navigation::document_highlight_at(&docs, uri, line, character)
    }

    #[allow(deprecated)]
    pub(crate) fn document_symbols_at(&self, uri: &str) -> Option<DocumentSymbolResponse> {
        let docs = self.documents.read();
        symbols::document_symbols_at(&docs, uri)
    }

    pub(crate) fn folding_ranges_at(&self, uri: &str) -> Option<Vec<FoldingRange>> {
        let docs = self.documents.read();
        folding::folding_ranges_at(&docs, uri)
    }

    pub(crate) fn selection_ranges_at(
        &self,
        uri: &str,
        positions: &[Position],
    ) -> Option<Vec<SelectionRange>> {
        let docs = self.documents.read();
        folding::selection_ranges_at(&docs, uri, positions)
    }

    pub(crate) fn document_links_at(&self, uri: &str) -> Option<Vec<DocumentLink>> {
        let docs = self.documents.read();
        links::document_links_at(&docs, uri)
    }

    /// Format the given range by formatting the whole document and replacing the range with the corresponding slice of formatted output.
    pub(crate) fn format_range_at(
        &self,
        uri: &str,
        range: &Range,
        options: &FormatterOptions,
    ) -> Option<Vec<TextEdit>> {
        let docs = self.documents.read();
        formatting::format_range_at(&docs, uri, range, options)
    }

    pub(crate) fn workspace_symbols_at(&self, query: &str) -> Vec<SymbolInformation> {
        let docs = self.documents.read();
        symbols::workspace_symbols_at(&docs, query)
    }

    pub(crate) fn completion_at(
        &self,
        uri: &str,
        line: u32,
        character: u32,
    ) -> Option<CompletionResponse> {
        let docs = self.documents.read();
        completion::completion_at(&docs, uri, line, character)
    }

    pub(crate) fn signature_help_at(
        &self,
        uri: &str,
        line: u32,
        character: u32,
    ) -> Option<SignatureHelp> {
        let docs = self.documents.read();
        signature_help_handler::signature_help_at(&docs, uri, line, character)
    }

    // --- Call hierarchy ---

    pub(crate) fn prepare_call_hierarchy_at(
        &self,
        params: &CallHierarchyPrepareParams,
    ) -> Option<Vec<CallHierarchyItem>> {
        let docs = self.documents.read();
        hierarchy::prepare_call_hierarchy_at(&docs, params)
    }

    pub(crate) fn call_hierarchy_incoming(
        &self,
        params: &CallHierarchyIncomingCallsParams,
    ) -> Option<Vec<CallHierarchyIncomingCall>> {
        let docs = self.documents.read();
        hierarchy::call_hierarchy_incoming(&docs, params)
    }

    pub(crate) fn call_hierarchy_outgoing(
        &self,
        params: &CallHierarchyOutgoingCallsParams,
    ) -> Option<Vec<CallHierarchyOutgoingCall>> {
        let docs = self.documents.read();
        hierarchy::call_hierarchy_outgoing(&docs, params)
    }

    // --- Type hierarchy ---

    pub(crate) fn prepare_type_hierarchy_at(
        &self,
        params: &TypeHierarchyPrepareParams,
    ) -> Option<Vec<TypeHierarchyItem>> {
        let docs = self.documents.read();
        hierarchy::prepare_type_hierarchy_at(&docs, params)
    }

    pub(crate) fn type_hierarchy_supertypes(
        &self,
        params: &TypeHierarchySupertypesParams,
    ) -> Option<Vec<TypeHierarchyItem>> {
        let docs = self.documents.read();
        hierarchy::type_hierarchy_supertypes(&docs, params)
    }

    pub(crate) fn type_hierarchy_subtypes(
        &self,
        params: &TypeHierarchySubtypesParams,
    ) -> Option<Vec<TypeHierarchyItem>> {
        let docs = self.documents.read();
        hierarchy::type_hierarchy_subtypes(&docs, params)
    }
}

