//! Semantic token types and computation for LSP-based syntax highlighting.
//!
//! Maps syntax tokens from the parse tree to LSP semantic token types (keyword, string,
//! number, operator, variable, etc.) so the editor can colorize via the LSP.
//! Declaration names (class, function, variable, parameter) are classified using the
//! syntax tree. Doxygen-style comment tags (@param, @return, etc.) are emitted as KEYWORD within comments.

use leekscript_rs::analysis::{
    class_decl_info, function_decl_info, param_name, var_decl_info, VarDeclKind,
};
use leekscript_rs::syntax::Kind;
use leekscript_rs::LineIndex;
use sipha::red::{SyntaxNode, SyntaxToken};
use sipha::types::IntoSyntaxKind;
use tower_lsp::lsp_types::{
    SemanticToken, SemanticTokenModifier, SemanticTokenType, SemanticTokens, SemanticTokensFullOptions,
    SemanticTokensLegend, SemanticTokensOptions, SemanticTokensServerCapabilities,
};

/// Doxygen tag names we highlight (after @ or \).
const DOXYGEN_TAGS: &[&str] = &[
    "param", "return", "returns", "brief", "deprecated", "see", "since", "throws", "throw",
    "author", "version", "note", "warning", "todo", "code", "endcode", "pre", "post", "invariant",
];

/// Segment of a comment: either a Doxygen tag (keyword) or plain comment text.
#[derive(Clone, Copy)]
struct CommentSegment {
    start: usize,
    end: usize,
    is_tag: bool,
}

/// Find Doxygen tag and comment segments in comment text (byte offsets). Merges adjacent same-type segments.
/// Advances by UTF-8 character boundaries so multi-byte characters (e.g. é, 中文) do not cause panics.
fn doxygen_segments(text: &str) -> Vec<CommentSegment> {
    let mut raw = Vec::new();
    let mut i = 0;
    while i < text.len() {
        let rest = &text[i..];
        let (is_tag, len) = if rest.starts_with('@') || rest.starts_with('\\') {
            let prefix_len = 1;
            let after = &rest[prefix_len..];
            let tag_match = DOXYGEN_TAGS.iter().find(|tag| {
                after.starts_with(*tag)
                    && (after.len() == tag.len()
                        || !after[tag.len()..].starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_'))
            });
            if let Some(tag) = tag_match {
                (true, prefix_len + tag.len())
            } else {
                let char_len = rest.chars().next().map(|c| c.len_utf8()).unwrap_or(1);
                (false, char_len)
            }
        } else {
            let char_len = rest.chars().next().map(|c| c.len_utf8()).unwrap_or(1);
            (false, char_len)
        };
        raw.push(CommentSegment {
            start: i,
            end: i + len,
            is_tag,
        });
        i += len;
    }
    // Merge adjacent segments of the same type.
    let mut merged: Vec<CommentSegment> = Vec::new();
    for seg in raw {
        if let Some(last) = merged.last_mut() {
            if last.is_tag == seg.is_tag && last.end == seg.start {
                last.end = seg.end;
                continue;
            }
        }
        merged.push(seg);
    }
    merged
}

/// Legend of token types and modifiers this server uses. The client uses this to map
/// token type/modifier indices to theme colors.
pub fn semantic_tokens_legend() -> SemanticTokensLegend {
    SemanticTokensLegend {
        token_types: vec![
            SemanticTokenType::KEYWORD,
            SemanticTokenType::MODIFIER,
            SemanticTokenType::STRING,
            SemanticTokenType::NUMBER,
            SemanticTokenType::OPERATOR,
            SemanticTokenType::COMMENT,
            SemanticTokenType::TYPE,
            SemanticTokenType::CLASS,
            SemanticTokenType::FUNCTION,
            SemanticTokenType::METHOD,
            SemanticTokenType::PARAMETER,
            SemanticTokenType::VARIABLE,
            SemanticTokenType::PROPERTY,
        ],
        token_modifiers: vec![
            SemanticTokenModifier::DECLARATION,
            SemanticTokenModifier::DEFINITION,
            SemanticTokenModifier::READONLY,
            SemanticTokenModifier::STATIC,
        ],
    }
}

/// Server capability for semantic tokens (full document and range).
pub fn semantic_tokens_provider() -> SemanticTokensServerCapabilities {
    SemanticTokensOptions {
        work_done_progress_options: Default::default(),
        legend: semantic_tokens_legend(),
        range: Some(true),
        full: Some(SemanticTokensFullOptions::Bool(true)),
    }
    .into()
}

/// Indices into the legend's token_types (must match `semantic_tokens_legend()`).
const TYPE_KEYWORD: u32 = 0;
const TYPE_MODIFIER: u32 = 1;
const TYPE_STRING: u32 = 2;
const TYPE_NUMBER: u32 = 3;
const TYPE_OPERATOR: u32 = 4;
const TYPE_COMMENT: u32 = 5;
#[allow(dead_code)]
const TYPE_TYPE: u32 = 6;
const TYPE_CLASS: u32 = 7;
const TYPE_FUNCTION: u32 = 8;
#[allow(dead_code)]
const TYPE_METHOD: u32 = 9;
const TYPE_PARAMETER: u32 = 10;
const TYPE_VARIABLE: u32 = 11;
#[allow(dead_code)]
const TYPE_PROPERTY: u32 = 12;

/// Modifier bits (must match legend token_modifiers order).
const MOD_DECLARATION: u32 = 1 << 0;
#[allow(dead_code)]
const MOD_DEFINITION: u32 = 1 << 1;
const MOD_READONLY: u32 = 1 << 2;
#[allow(dead_code)]
const MOD_STATIC: u32 = 1 << 3;

/// Maps declaration name spans to (token_type, modifier_bitset) for semantic classification.
struct DeclMap(Vec<(u32, u32, u32, u32)>);

impl DeclMap {
    fn new() -> Self {
        Self(Vec::new())
    }

    fn insert(&mut self, start: u32, end: u32, token_type: u32, modifiers: u32) {
        self.0.push((start, end, token_type, modifiers));
    }

    fn get(&self, start: u32, end: u32) -> Option<(u32, u32)> {
        self.0
            .iter()
            .find(|(s, e, _, _)| *s == start && *e == end)
            .map(|(_, _, ty, mod_)| (*ty, *mod_))
    }
}

/// Build a map of declaration name spans to semantic token type and modifiers from the syntax tree.
fn build_decl_map(root: &SyntaxNode) -> DeclMap {
    let mut map = DeclMap::new();
    for node in root.find_all_nodes(Kind::NodeClassDecl.into_syntax_kind()) {
        if let Some(info) = class_decl_info(&node) {
            let s = info.name_span.start;
            let e = info.name_span.end;
            map.insert(s, e, TYPE_CLASS, MOD_DECLARATION);
        }
    }
    for node in root.find_all_nodes(Kind::NodeFunctionDecl.into_syntax_kind()) {
        if let Some(info) = function_decl_info(&node) {
            let s = info.name_span.start;
            let e = info.name_span.end;
            map.insert(s, e, TYPE_FUNCTION, MOD_DECLARATION);
        }
    }
    for node in root.find_all_nodes(Kind::NodeVarDecl.into_syntax_kind()) {
        if let Some(info) = var_decl_info(&node) {
            let s = info.name_span.start;
            let e = info.name_span.end;
            let readonly = matches!(info.kind, VarDeclKind::Const | VarDeclKind::Let);
            let mods = MOD_DECLARATION | if readonly { MOD_READONLY } else { 0 };
            map.insert(s, e, TYPE_VARIABLE, mods);
        }
    }
    for node in root.find_all_nodes(Kind::NodeParam.into_syntax_kind()) {
        if let Some((_, span)) = param_name(&node) {
            map.insert(span.start, span.end, TYPE_PARAMETER, MOD_DECLARATION);
        }
    }
    map
}

/// Maps a token kind to (token_type_index, modifier_bitset) for the semantic token legend.
/// Comment trivia is handled separately in the token loop; other trivia (e.g. whitespace) is skipped.
/// For `TokIdent`, the default (TYPE_VARIABLE, 0) can be overridden via the declaration map.
fn kind_to_semantic(kind: Kind) -> Option<(u32, u32)> {
    let (token_type, modifiers) = match kind {
        Kind::TokNumber => (TYPE_NUMBER, 0u32),
        Kind::TokString => (TYPE_STRING, 0),
        Kind::TokIdent => (TYPE_VARIABLE, 0), // overridden by decl_map when at declaration span
        Kind::TokOp | Kind::TokArrow | Kind::TokDotDot | Kind::TokDot | Kind::TokColon
        | Kind::TokComma | Kind::TokSemi | Kind::TokParenL | Kind::TokParenR
        | Kind::TokBracketL | Kind::TokBracketR | Kind::TokBraceL | Kind::TokBraceR
        | Kind::TokLemnisate | Kind::TokPi => (TYPE_OPERATOR, 0),
        Kind::KwAbstract | Kind::KwPublic | Kind::KwPrivate | Kind::KwProtected
        | Kind::KwFinal | Kind::KwStatic => (TYPE_MODIFIER, 0),
        Kind::KwClass | Kind::KwFunction | Kind::KwVar | Kind::KwGlobal
        | Kind::KwConst | Kind::KwLet | Kind::KwIf | Kind::KwElse | Kind::KwWhile
        | Kind::KwFor | Kind::KwDo | Kind::KwReturn | Kind::KwBreak | Kind::KwContinue
        | Kind::KwAnd | Kind::KwOr | Kind::KwNot | Kind::KwXor | Kind::KwIn
        | Kind::KwTrue | Kind::KwFalse | Kind::KwNull | Kind::KwNew | Kind::KwAs
        | Kind::KwInclude | Kind::KwExtends | Kind::KwConstructor | Kind::KwThis
        | Kind::KwSuper | Kind::KwInstanceof | Kind::KwTry | Kind::KwCatch
        |         Kind::KwSwitch | Kind::KwCase | Kind::KwDefault | Kind::KwThrow
        |         Kind::KwReserved => (TYPE_KEYWORD, 0),
        Kind::TriviaLineComment | Kind::TriviaBlockComment => (TYPE_COMMENT, 0),
        Kind::TriviaWs => return None,
        _ => (TYPE_VARIABLE, 0),
    };
    Some((token_type, modifiers))
}

/// Length of the token in UTF-16 code units (LSP semantic tokens use character = UTF-16).
fn token_length_utf16(token: &SyntaxToken) -> u32 {
    token.text().encode_utf16().count() as u32
}

/// Emit semantic tokens for a comment token, splitting out Doxygen tags as KEYWORD. Appends to `data`
/// and updates `prev_line` and `prev_char`.
fn emit_comment_tokens(
    token: &SyntaxToken,
    source: &str,
    line_index: &LineIndex,
    data: &mut Vec<SemanticToken>,
    prev_line: &mut u32,
    prev_char: &mut u32,
) {
    let token_range = token.text_range();
    let token_start = token_range.start;
    let text = token.text();
    let segments = doxygen_segments(text);
    for seg in segments {
        if seg.start >= seg.end {
            continue;
        }
        let byte_start = token_start + seg.start as u32;
        let seg_text = &text[seg.start..seg.end];
        let length_utf16 = seg_text.encode_utf16().count() as u32;
        let (line, char) = line_index.line_col_utf16(source, byte_start);
        let token_type = if seg.is_tag {
            TYPE_KEYWORD
        } else {
            TYPE_COMMENT
        };
        let delta_line = line.saturating_sub(*prev_line);
        let delta_start = if delta_line == 0 {
            char.saturating_sub(*prev_char)
        } else {
            char
        };
        data.push(SemanticToken {
            delta_line,
            delta_start,
            length: length_utf16,
            token_type,
            token_modifiers_bitset: 0,
        });
        *prev_line = line;
        *prev_char = char + length_utf16;
    }
}

/// Optional byte range to restrict emitted tokens. None = full document.
fn compute_semantic_tokens_impl(
    source: &str,
    line_index: &LineIndex,
    root: &SyntaxNode,
    range: Option<(u32, u32)>,
) -> SemanticTokens {
    let decl_map = build_decl_map(root);
    let tokens = root.descendant_tokens();
    let mut data = Vec::new();
    let mut prev_line = 0u32;
    let mut prev_char = 0u32;

    for token in tokens {
        let r = token.text_range();
        if let Some((byte_start, byte_end)) = range {
            if r.end <= byte_start || r.start >= byte_end {
                continue;
            }
        }
        let Some(kind) = token.kind_as::<Kind>() else {
            continue;
        };
        if kind == Kind::TriviaLineComment || kind == Kind::TriviaBlockComment {
            emit_comment_tokens(&token, source, line_index, &mut data, &mut prev_line, &mut prev_char);
            continue;
        }
        let (mut token_type, mut token_modifiers_bitset) = match kind_to_semantic(kind) {
            Some(t) => t,
            None => continue,
        };
        if kind == Kind::TokIdent {
            if let Some((ty, mods)) = decl_map.get(r.start, r.end) {
                token_type = ty;
                token_modifiers_bitset = mods;
            }
        }
        let (line, char) = line_index.line_col_utf16(source, r.start);
        let length = token_length_utf16(&token);

        let delta_line = line.saturating_sub(prev_line);
        let delta_start = if delta_line == 0 {
            char.saturating_sub(prev_char)
        } else {
            char
        };

        data.push(SemanticToken {
            delta_line,
            delta_start,
            length,
            token_type,
            token_modifiers_bitset,
        });

        prev_line = line;
        prev_char = char;
    }

    SemanticTokens {
        result_id: None,
        data,
    }
}

/// Compute semantic tokens for tokens overlapping [byte_start, byte_end).
/// Uses all tokens (including trivia) so comments are emitted as COMMENT type.
/// Returns delta-encoded LSP semantic tokens.
pub fn compute_semantic_tokens_range(
    source: &str,
    line_index: &LineIndex,
    root: &SyntaxNode,
    byte_start: u32,
    byte_end: u32,
) -> SemanticTokens {
    compute_semantic_tokens_impl(source, line_index, root, Some((byte_start, byte_end)))
}

/// Compute full-document semantic tokens for the given root and source.
/// Uses all tokens (including trivia) so comments are emitted as COMMENT type.
/// Returns delta-encoded LSP semantic tokens.
pub fn compute_semantic_tokens(
    source: &str,
    line_index: &LineIndex,
    root: &SyntaxNode,
) -> SemanticTokens {
    compute_semantic_tokens_impl(source, line_index, root, None)
}

