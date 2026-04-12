//! Hover, go-to-definition, find references, completion, document symbols, inlay hints, code lens.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

use leekscript::{AnalysisResult, LeekTy, MergedCheckUnit, Reference, Symbol, SymbolKind};
use lsp_types::{
    CodeAction, CodeActionKind, CodeActionOrCommand, Command, CompletionItem, CompletionItemKind,
    CompletionParams, CompletionResponse, Diagnostic, DocumentSymbol, DocumentSymbolResponse,
    GotoDefinitionParams, GotoDefinitionResponse, Hover, HoverContents, HoverParams, InlayHint,
    InlayHintKind, InlayHintLabel, InlayHintTooltip, Location, MarkupContent, MarkupKind,
    NumberOrString, Position, Range, ReferenceParams, RenameParams, SemanticTokensRangeParams,
    SymbolKind as LspSymbolKind, TextEdit, Url, WorkspaceEdit,
};
use sipha::diagnostics::line_index::LineIndex;
use sipha::types::Span;

use crate::diagnostics::span_to_range_in_source;
use crate::diagnostics::{
    analyze_parsed, clamp_span_to_source, full_document_range, merged_location_to_lsp,
    merged_span_to_file_span, parse_merged_check_unit, prepare_open_file_merged_unit,
};
use crate::hover_markdown::symbol_markdown;
use crate::semantic_tokens::semantic_tokens_for_document_in_range;

const KEYWORDS: &[&str] = &[
    "any",
    "as",
    "boolean",
    "break",
    "case",
    "catch",
    "class",
    "continue",
    "default",
    "do",
    "else",
    "extends",
    "false",
    "final",
    "for",
    "function",
    "global",
    "if",
    "in",
    "include",
    "instanceof",
    "integer",
    "is",
    "let",
    "match",
    "new",
    "null",
    "private",
    "protected",
    "public",
    "real",
    "return",
    "static",
    "string",
    "super",
    "switch",
    "this",
    "throw",
    "true",
    "try",
    "var",
    "void",
    "while",
];

fn is_ident_continue(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn identifier_prefix_at(source: &str, pos: Position) -> String {
    let idx = LineIndex::new(source.as_bytes());
    let Some(b) = idx.line_col_utf16_to_byte(source, pos.line, pos.character) else {
        return String::new();
    };
    let mut i = b as usize;
    if i > source.len() {
        i = source.len();
    }
    let bytes = source.as_bytes();
    while i > 0 && is_ident_continue(bytes[i - 1]) {
        i -= 1;
    }
    source[i..(b as usize).min(source.len())].to_string()
}

fn completion_rank(k: SymbolKind) -> u8 {
    // Lower is better. Prefer "local-ish" names over structural names.
    match k {
        SymbolKind::Parameter => 0,
        SymbolKind::Variable => 1,
        SymbolKind::Field => 2,
        SymbolKind::Method | SymbolKind::Constructor => 3,
        SymbolKind::Function => 4,
        SymbolKind::Class => 5,
        SymbolKind::Global => 6,
        SymbolKind::TypeParam => 7,
    }
}

pub(crate) struct ProjectIntel {
    pub prep: MergedCheckUnit,
    pub analysis: AnalysisResult,
    pub entry_uri: Url,
}

pub(crate) fn load_intel_file(
    source: &str,
    entry_path: &Path,
    entry_uri: &Url,
    signature_files: &[std::path::PathBuf],
    open: &HashMap<String, String>,
) -> Option<ProjectIntel> {
    let prep =
        prepare_open_file_merged_unit(source, entry_path, entry_uri, signature_files, open).ok()?;
    let parsed = parse_merged_check_unit(&prep).ok()?;
    let analysis = analyze_parsed(&parsed, prep.resolved.version);
    Some(ProjectIntel {
        prep,
        analysis,
        entry_uri: entry_uri.clone(),
    })
}

fn path_same_file(a: &Path, b: &Path) -> bool {
    let ka = fs::canonicalize(a).unwrap_or_else(|_| a.to_path_buf());
    let kb = fs::canonicalize(b).unwrap_or_else(|_| b.to_path_buf());
    ka == kb
}

/// LSP range in the **entry** buffer for a merged-span, if that span maps to `entry_path`.
fn range_in_entry_source(
    intel: &ProjectIntel,
    entry_path: &Path,
    entry_source: &str,
    merged_span: Span,
) -> Option<Range> {
    let (path, fspan) = merged_span_to_file_span(&intel.prep.mapping, merged_span)?;
    if !path_same_file(&path, entry_path) {
        return None;
    }
    let span = clamp_span_to_source(entry_source, fspan);
    Some(span_to_range_in_source(entry_source, span))
}

fn file_byte_at_position(source: &str, pos: Position) -> Option<u32> {
    let idx = LineIndex::new(source.as_bytes());
    idx.line_col_utf16_to_byte(source, pos.line, pos.character)
}

fn merged_offset_for_cursor(
    intel: &ProjectIntel,
    entry_path: &Path,
    source: &str,
    pos: Position,
) -> Option<u32> {
    let fb = file_byte_at_position(source, pos)?;
    intel
        .prep
        .mapping
        .merged_offset_for_file_byte(entry_path, fb)
}

fn narrowest_ref_at(analysis: &AnalysisResult, off: u32) -> Option<&Reference> {
    analysis
        .references
        .iter()
        .filter(|r| r.span.start <= off && off < r.span.end)
        .min_by_key(|r| r.span.end - r.span.start)
}

fn narrowest_symbol_name_at(analysis: &AnalysisResult, off: u32) -> Option<&Symbol> {
    analysis
        .symbols
        .iter()
        .filter(|s| s.name_span.start <= off && off < s.name_span.end)
        .min_by_key(|s| s.name_span.end - s.name_span.start)
}

fn narrowest_expr_ty_at(analysis: &AnalysisResult, off: u32) -> Option<&LeekTy> {
    analysis
        .expr_types
        .iter()
        .filter(|(k, _)| k.start <= off && off < k.end)
        .min_by_key(|(k, _)| k.end - k.start)
        .map(|(_, t)| t)
}

fn position_leq(a: Position, b: Position) -> bool {
    a.line < b.line || (a.line == b.line && a.character <= b.character)
}

fn position_in_range(pos: Position, r: &Range) -> bool {
    position_leq(r.start, pos) && position_leq(pos, r.end)
}

fn lsp_diag_is_undefined_name(d: &Diagnostic) -> bool {
    let Some(code) = d.code.as_ref() else {
        return false;
    };
    match code {
        NumberOrString::String(s) => s == "undefined-name",
        NumberOrString::Number(n) => *n == 1, // not used by us, but keep a fallback
    }
}

fn skip_ws_fwd(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    i
}

fn skip_ws_back(bytes: &[u8], mut i: usize) -> usize {
    while i > 0 && bytes[i - 1].is_ascii_whitespace() {
        i -= 1;
    }
    i
}

fn is_word_boundary(bytes: &[u8], i: usize) -> bool {
    if i >= bytes.len() {
        return true;
    }
    !bytes[i].is_ascii_alphanumeric() && bytes[i] != b'_'
}

fn looks_like_new_class(source: &str, cursor_byte: usize) -> bool {
    let bytes = source.as_bytes();
    let i = skip_ws_back(bytes, cursor_byte);
    // Look for "... new <Ident>"
    let new_kw = b"new";
    if i < new_kw.len() + 1 {
        return false;
    }
    // Find start of previous token
    let mut j = i;
    while j > 0 && is_ident_continue(bytes[j - 1]) {
        j -= 1;
    }
    // If we are currently on an identifier, `j` is its start; move left of it.
    let left = skip_ws_back(bytes, j);
    if left < new_kw.len() {
        return false;
    }
    let start = left - new_kw.len();
    if &bytes[start..left] != new_kw {
        return false;
    }
    // Ensure token boundary.
    (start == 0 || is_word_boundary(bytes, start - 1)) && is_word_boundary(bytes, left)
}

fn looks_like_call(source: &str, cursor_byte: usize) -> bool {
    let bytes = source.as_bytes();
    let i = skip_ws_fwd(bytes, cursor_byte);
    i < bytes.len() && bytes[i] == b'('
}

fn looks_like_member_access(source: &str, cursor_byte: usize) -> bool {
    let bytes = source.as_bytes();
    let i = skip_ws_fwd(bytes, cursor_byte);
    i < bytes.len() && bytes[i] == b'.'
}

pub(crate) fn hover(
    intel: &ProjectIntel,
    entry_path: &Path,
    source: &str,
    params: &HoverParams,
) -> Option<Hover> {
    let pos = params.text_document_position_params.position;
    let off = merged_offset_for_cursor(intel, entry_path, source, pos)?;

    let md = if let Some(sym) = narrowest_symbol_name_at(&intel.analysis, off) {
        symbol_markdown(sym)
    } else if let Some(r) = narrowest_ref_at(&intel.analysis, off) {
        if let Some(res) = r.resolved {
            intel.analysis.symbol(res).map(symbol_markdown)?
        } else {
            format!("`{}` (unresolved)", r.name)
        }
    } else if let Some(ty) = narrowest_expr_ty_at(&intel.analysis, off) {
        format!("```leekscript\n{ty}\n```")
    } else {
        return None;
    };

    Some(Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: md,
        }),
        range: None,
    })
}

pub(crate) fn goto_definition(
    intel: &ProjectIntel,
    entry_path: &Path,
    source: &str,
    params: &GotoDefinitionParams,
) -> Option<GotoDefinitionResponse> {
    let pos = params.text_document_position_params.position;
    let off = merged_offset_for_cursor(intel, entry_path, source, pos)?;

    let target_span = if let Some(sym) = narrowest_symbol_name_at(&intel.analysis, off) {
        Some(sym.name_span)
    } else if let Some(r) = narrowest_ref_at(&intel.analysis, off) {
        r.resolved
            .and_then(|id| intel.analysis.symbol(id))
            .map(|s| s.name_span)
    } else {
        None
    }?;

    let loc = merged_location_to_lsp(
        &intel.prep.mapping,
        &intel.prep.project,
        &intel.prep.combined,
        &intel.entry_uri,
        target_span,
    );
    Some(GotoDefinitionResponse::Scalar(loc))
}

pub(crate) fn references(
    intel: &ProjectIntel,
    entry_path: &Path,
    source: &str,
    params: &ReferenceParams,
) -> Option<Vec<Location>> {
    let pos = params.text_document_position.position;
    let off = merged_offset_for_cursor(intel, entry_path, source, pos)?;

    let target_id = if let Some(sym) = narrowest_symbol_name_at(&intel.analysis, off) {
        Some(sym.id)
    } else if let Some(r) = narrowest_ref_at(&intel.analysis, off) {
        r.resolved
    } else {
        None
    }?;

    let mut locs: Vec<Location> = intel
        .analysis
        .references
        .iter()
        .filter(|r| r.resolved == Some(target_id))
        .map(|r| {
            merged_location_to_lsp(
                &intel.prep.mapping,
                &intel.prep.project,
                &intel.prep.combined,
                &intel.entry_uri,
                r.span,
            )
        })
        .collect();

    if params.context.include_declaration {
        if let Some(sym) = intel.analysis.symbol(target_id) {
            locs.insert(
                0,
                merged_location_to_lsp(
                    &intel.prep.mapping,
                    &intel.prep.project,
                    &intel.prep.combined,
                    &intel.entry_uri,
                    sym.name_span,
                ),
            );
        }
    }

    Some(locs)
}

pub(crate) fn completion(
    intel: &ProjectIntel,
    entry_path: &Path,
    source: &str,
    params: &CompletionParams,
) -> CompletionResponse {
    let pos = params.text_document_position.position;
    let Some(off) = merged_offset_for_cursor(intel, entry_path, source, pos) else {
        return CompletionResponse::Array(Vec::new());
    };
    let prefix = identifier_prefix_at(source, pos);
    let prefix_lc = prefix.to_ascii_lowercase();

    let mut seen = HashSet::<String>::new();
    let mut items: Vec<CompletionItem> = Vec::new();

    let want = |label: &str| {
        if prefix_lc.is_empty() {
            return true;
        }
        label.to_ascii_lowercase().starts_with(&prefix_lc)
    };

    for kw in KEYWORDS {
        if !want(kw) {
            continue;
        }
        if seen.insert((*kw).to_string()) {
            items.push(CompletionItem {
                label: (*kw).to_string(),
                kind: Some(CompletionItemKind::KEYWORD),
                ..Default::default()
            });
        }
    }

    let mut sym_items: Vec<(u8, String, CompletionItem)> = Vec::new();
    for sym in &intel.analysis.symbols {
        if sym.name_span.end > off {
            continue;
        }
        if !want(&sym.name) {
            continue;
        }
        if !seen.insert(sym.name.clone()) {
            continue;
        }
        let kind = match sym.kind {
            SymbolKind::Function => Some(CompletionItemKind::FUNCTION),
            SymbolKind::Class => Some(CompletionItemKind::CLASS),
            SymbolKind::Method | SymbolKind::Constructor => Some(CompletionItemKind::METHOD),
            SymbolKind::Field => Some(CompletionItemKind::FIELD),
            SymbolKind::Variable | SymbolKind::Parameter | SymbolKind::Global => {
                Some(CompletionItemKind::VARIABLE)
            }
            SymbolKind::TypeParam => Some(CompletionItemKind::TYPE_PARAMETER),
        };
        let label = sym.name.clone();
        let rank = completion_rank(sym.kind.clone());
        let sort = format!("{rank:02}-{label}");
        sym_items.push((
            rank,
            label.clone(),
            CompletionItem {
                label,
                kind,
                detail: Some(sym.effective_ty().to_string()),
                sort_text: Some(sort),
                ..Default::default()
            },
        ));
    }

    sym_items.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    for (_, _, it) in sym_items {
        items.push(it);
    }

    // A few high-value snippets (shown only when they match prefix).
    // Kept intentionally minimal to avoid clutter.
    let push_snippet = |items: &mut Vec<CompletionItem>, label: &str, snippet: &str, kind| {
        items.push(CompletionItem {
            label: label.to_string(),
            kind: Some(kind),
            insert_text: Some(snippet.to_string()),
            insert_text_format: Some(lsp_types::InsertTextFormat::SNIPPET),
            ..Default::default()
        });
    };

    if want("if") {
        push_snippet(
            &mut items,
            "if",
            "if (${1:cond}) {\n\t$0\n}",
            CompletionItemKind::SNIPPET,
        );
    }
    if want("for") {
        push_snippet(
            &mut items,
            "for",
            "for (let ${1:i} = 0; ${1:i} < ${2:n}; ${1:i}++) {\n\t$0\n}",
            CompletionItemKind::SNIPPET,
        );
    }
    if want("function") {
        push_snippet(
            &mut items,
            "function",
            "function ${1:name}(${2:params}) {\n\t$0\n}",
            CompletionItemKind::SNIPPET,
        );
    }

    CompletionResponse::Array(items)
}

fn lsp_symbol_kind(k: &SymbolKind) -> LspSymbolKind {
    match k {
        SymbolKind::Class => LspSymbolKind::CLASS,
        SymbolKind::Function | SymbolKind::Method | SymbolKind::Constructor => {
            LspSymbolKind::FUNCTION
        }
        SymbolKind::Field => LspSymbolKind::FIELD,
        SymbolKind::Variable | SymbolKind::Parameter | SymbolKind::Global => {
            LspSymbolKind::VARIABLE
        }
        SymbolKind::TypeParam => LspSymbolKind::TYPE_PARAMETER,
    }
}

pub(crate) fn document_symbols(
    intel: &ProjectIntel,
    entry_path: &Path,
    entry_source: &str,
) -> DocumentSymbolResponse {
    let mut syms: Vec<DocumentSymbol> = Vec::new();
    for s in &intel.analysis.symbols {
        if !matches!(
            s.kind,
            SymbolKind::Function
                | SymbolKind::Class
                | SymbolKind::Method
                | SymbolKind::Field
                | SymbolKind::Constructor
        ) {
            continue;
        }
        let Some(range) = range_in_entry_source(intel, entry_path, entry_source, s.name_span)
        else {
            continue;
        };
        #[allow(deprecated)]
        syms.push(DocumentSymbol {
            name: s.name.clone(),
            detail: Some(s.effective_ty().to_string()),
            kind: lsp_symbol_kind(&s.kind),
            tags: None,
            deprecated: None,
            range,
            selection_range: range,
            children: None,
        });
    }

    syms.sort_by(|a, b| {
        a.range
            .start
            .line
            .cmp(&b.range.start.line)
            .then_with(|| a.range.start.character.cmp(&b.range.start.character))
    });

    DocumentSymbolResponse::Nested(syms)
}

pub(crate) fn inlay_hints(
    intel: &ProjectIntel,
    entry_path: &Path,
    entry_source: &str,
    only_range: Option<Range>,
    hide_any: bool,
) -> Vec<InlayHint> {
    let mut out = Vec::new();
    for sym in &intel.analysis.symbols {
        if sym.kind != SymbolKind::Variable && sym.kind != SymbolKind::Parameter {
            continue;
        }
        let Some(inf) = &sym.inferred_ty else {
            continue;
        };
        if hide_any && inf.to_string() == "any" {
            continue;
        }
        if sym.declared_ty.is_some() {
            continue;
        }
        let Some(range) = range_in_entry_source(intel, entry_path, entry_source, sym.name_span)
        else {
            continue;
        };
        if let Some(r) = only_range.as_ref() {
            if !position_in_range(range.end, r) {
                continue;
            }
        }
        out.push(InlayHint {
            position: range.end,
            label: InlayHintLabel::String(format!(": {inf}")),
            kind: Some(InlayHintKind::TYPE),
            text_edits: None,
            tooltip: Some(InlayHintTooltip::String(inf.to_string())),
            padding_left: Some(true),
            padding_right: None,
            data: None,
        });
    }
    out.sort_by(|a, b| {
        a.position
            .line
            .cmp(&b.position.line)
            .then_with(|| a.position.character.cmp(&b.position.character))
    });
    out
}

pub(crate) fn code_lenses(
    intel: &ProjectIntel,
    entry_path: &Path,
    entry_source: &str,
    uri: &Url,
) -> Vec<lsp_types::CodeLens> {
    let mut lenses = Vec::new();
    for sym in &intel.analysis.symbols {
        if !matches!(
            sym.kind,
            SymbolKind::Function | SymbolKind::Class | SymbolKind::Method | SymbolKind::Constructor
        ) {
            continue;
        }
        let Some(range) = range_in_entry_source(intel, entry_path, entry_source, sym.name_span)
        else {
            continue;
        };
        let n = intel
            .analysis
            .references
            .iter()
            .filter(|r| r.resolved == Some(sym.id))
            .count();
        let start = range.start;
        lenses.push(lsp_types::CodeLens {
            range,
            command: Some(Command {
                title: format!("{n} references"),
                command: "leekscript.showReferences".to_string(),
                arguments: Some(vec![
                    serde_json::to_value(uri.as_str()).unwrap_or_default(),
                    serde_json::json!({ "line": start.line, "character": start.character }),
                ]),
            }),
            data: None,
        });
    }
    lenses.sort_by(|a, b| {
        a.range
            .start
            .line
            .cmp(&b.range.start.line)
            .then_with(|| a.range.start.character.cmp(&b.range.start.character))
    });
    lenses
}

pub(crate) fn semantic_tokens_range(
    source: &str,
    uri_str: &str,
    params: &SemanticTokensRangeParams,
) -> lsp_types::SemanticTokens {
    let range = Range {
        start: params.range.start,
        end: params.range.end,
    };
    semantic_tokens_for_document_in_range(source, Some(uri_str), range)
}

pub(crate) fn code_actions(
    intel: &ProjectIntel,
    entry_path: &Path,
    entry_source: &str,
    entry_uri: &Url,
    pos: Position,
    lsp_context_diagnostics: &[Diagnostic],
) -> Vec<CodeActionOrCommand> {
    let Some(off) = merged_offset_for_cursor(intel, entry_path, entry_source, pos) else {
        return Vec::new();
    };
    let mut out: Vec<CodeActionOrCommand> = Vec::new();

    // Quick fix: create a stub for an unresolved reference at the cursor.
    if let Some(r) = narrowest_ref_at(&intel.analysis, off) {
        if r.resolved.is_none() && !r.name.is_empty() {
            let has_undefined_name_diag_here = lsp_context_diagnostics
                .iter()
                .filter(|d| lsp_diag_is_undefined_name(d))
                .any(|d| position_in_range(pos, &d.range));
            if !has_undefined_name_diag_here {
                // Avoid noisy actions while typing: only show when the client reports an actual
                // undefined-name diagnostic for this position.
                return out;
            }
            let already_defined = intel.analysis.symbols.iter().any(|s| s.name == r.name);
            if !already_defined {
                let cursor_byte = file_byte_at_position(entry_source, pos).unwrap_or(0) as usize;
                let prefer_fn = looks_like_call(entry_source, cursor_byte);
                let prefer_class = looks_like_new_class(entry_source, cursor_byte)
                    || looks_like_member_access(entry_source, cursor_byte)
                    || r.name
                        .chars()
                        .next()
                        .is_some_and(|c| c.is_ascii_uppercase());

                let end = full_document_range(entry_source).end;
                let insert_at = Range { start: end, end };

                let mut changes_fn: HashMap<Url, Vec<TextEdit>> = HashMap::new();
                changes_fn.insert(
                    entry_uri.clone(),
                    vec![TextEdit {
                        range: insert_at,
                        new_text: format!("\n\nfunction {}() {{\n\t\n}}\n", r.name),
                    }],
                );
                out.push(CodeActionOrCommand::CodeAction(CodeAction {
                    title: format!("Create function `{}`", r.name),
                    kind: Some(CodeActionKind::QUICKFIX),
                    diagnostics: None,
                    is_preferred: Some(prefer_fn && !prefer_class),
                    disabled: None,
                    edit: Some(WorkspaceEdit {
                        changes: Some(changes_fn),
                        document_changes: None,
                        change_annotations: None,
                    }),
                    command: None,
                    data: None,
                }));

                let mut changes_var: HashMap<Url, Vec<TextEdit>> = HashMap::new();
                changes_var.insert(
                    entry_uri.clone(),
                    vec![TextEdit {
                        range: insert_at,
                        new_text: format!("\n\nvar {};\n", r.name),
                    }],
                );
                out.push(CodeActionOrCommand::CodeAction(CodeAction {
                    title: format!("Declare variable `{}`", r.name),
                    kind: Some(CodeActionKind::QUICKFIX),
                    diagnostics: None,
                    is_preferred: Some(!prefer_fn && !prefer_class),
                    disabled: None,
                    edit: Some(WorkspaceEdit {
                        changes: Some(changes_var),
                        document_changes: None,
                        change_annotations: None,
                    }),
                    command: None,
                    data: None,
                }));

                let mut changes_class: HashMap<Url, Vec<TextEdit>> = HashMap::new();
                changes_class.insert(
                    entry_uri.clone(),
                    vec![TextEdit {
                        range: insert_at,
                        new_text: format!(
                            "\n\nclass {} {{\n\tconstructor() {{\n\t\t\n\t}}\n}}\n",
                            r.name
                        ),
                    }],
                );
                out.push(CodeActionOrCommand::CodeAction(CodeAction {
                    title: format!("Create class `{}`", r.name),
                    kind: Some(CodeActionKind::QUICKFIX),
                    diagnostics: None,
                    is_preferred: Some(prefer_class),
                    disabled: None,
                    edit: Some(WorkspaceEdit {
                        changes: Some(changes_class),
                        document_changes: None,
                        change_annotations: None,
                    }),
                    command: None,
                    data: None,
                }));
            }
        }
    }

    // Quick fix: insert inferred type annotation for variables/parameters.
    if let Some(sym) = narrowest_symbol_name_at(&intel.analysis, off) {
        if (sym.kind == SymbolKind::Variable || sym.kind == SymbolKind::Parameter)
            && sym.declared_ty.is_none()
        {
            if let Some(inf) = &sym.inferred_ty {
                if let Some(name_range) =
                    range_in_entry_source(intel, entry_path, entry_source, sym.name_span)
                {
                    let insert_at = Range {
                        start: name_range.end,
                        end: name_range.end,
                    };

                    let mut changes: HashMap<Url, Vec<TextEdit>> = HashMap::new();
                    changes.insert(
                        entry_uri.clone(),
                        vec![TextEdit {
                            range: insert_at,
                            new_text: format!(": {inf}"),
                        }],
                    );

                    out.push(CodeActionOrCommand::CodeAction(CodeAction {
                        title: "Insert inferred type annotation".to_string(),
                        kind: Some(CodeActionKind::QUICKFIX),
                        diagnostics: None,
                        is_preferred: Some(true),
                        disabled: None,
                        edit: Some(WorkspaceEdit {
                            changes: Some(changes),
                            document_changes: None,
                            change_annotations: None,
                        }),
                        command: None,
                        data: None,
                    }));
                }
            }
        }
    }

    out
}

pub(crate) fn rename(
    intel: &ProjectIntel,
    entry_path: &Path,
    source: &str,
    params: &RenameParams,
) -> Option<WorkspaceEdit> {
    let off = merged_offset_for_cursor(
        intel,
        entry_path,
        source,
        params.text_document_position.position,
    )?;

    let target_id = if let Some(sym) = narrowest_symbol_name_at(&intel.analysis, off) {
        Some(sym.id)
    } else if let Some(r) = narrowest_ref_at(&intel.analysis, off) {
        r.resolved
    } else {
        None
    }?;

    let old_len = intel.analysis.symbol(target_id).map(|s| s.name.len())?;

    let mut edits: HashMap<String, Vec<TextEdit>> = HashMap::new();
    let mut seen: HashSet<(String, u32, u32, u32, u32)> = HashSet::new();

    let push_edit = |m: &mut HashMap<String, Vec<TextEdit>>,
                     seen: &mut HashSet<(String, u32, u32, u32, u32)>,
                     loc: Location,
                     new_text: String| {
        let key = (
            loc.uri.to_string(),
            loc.range.start.line,
            loc.range.start.character,
            loc.range.end.line,
            loc.range.end.character,
        );
        if !seen.insert(key) {
            return;
        }
        m.entry(loc.uri.to_string()).or_default().push(TextEdit {
            range: loc.range,
            new_text,
        });
    };

    let def_sym = intel.analysis.symbol(target_id)?;
    let def_loc = merged_location_to_lsp(
        &intel.prep.mapping,
        &intel.prep.project,
        &intel.prep.combined,
        &intel.entry_uri,
        def_sym.name_span,
    );
    push_edit(&mut edits, &mut seen, def_loc, params.new_name.clone());

    for r in &intel.analysis.references {
        if r.resolved != Some(target_id) {
            continue;
        }
        let span_len = r.span.end.saturating_sub(r.span.start);
        if r.name.len() != old_len || span_len != u32::try_from(old_len).unwrap_or(u32::MAX) {
            continue;
        }
        let loc = merged_location_to_lsp(
            &intel.prep.mapping,
            &intel.prep.project,
            &intel.prep.combined,
            &intel.entry_uri,
            r.span,
        );
        push_edit(&mut edits, &mut seen, loc, params.new_name.clone());
    }

    let changes: HashMap<Url, Vec<TextEdit>> = edits
        .into_iter()
        .filter_map(|(k, v)| Url::parse(&k).ok().map(|u| (u, v)))
        .collect();

    Some(WorkspaceEdit {
        changes: Some(changes),
        document_changes: None,
        change_annotations: None,
    })
}
