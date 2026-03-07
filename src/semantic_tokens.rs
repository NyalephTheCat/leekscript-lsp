//! Semantic token types and computation for LSP-based syntax highlighting.
//!
//! Maps syntax tokens from the parse tree to LSP semantic token types (keyword, string,
//! number, operator, variable, etc.) so the editor can colorize via the LSP.

use leekscript_rs::syntax::Kind;
use leekscript_rs::LineIndex;
use sipha::red::SyntaxToken;
use tower_lsp::lsp_types::{
    SemanticToken, SemanticTokenModifier, SemanticTokenType, SemanticTokens, SemanticTokensFullOptions,
    SemanticTokensLegend, SemanticTokensOptions, SemanticTokensServerCapabilities,
};

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
/// TYPE_TYPE, TYPE_CLASS, etc. reserved for future semantic classification of identifiers.
#[allow(dead_code)]
const TYPE_KEYWORD: u32 = 0;
#[allow(dead_code)]
const TYPE_MODIFIER: u32 = 1;
const TYPE_STRING: u32 = 2;
const TYPE_NUMBER: u32 = 3;
const TYPE_OPERATOR: u32 = 4;
#[allow(dead_code)]
const TYPE_TYPE: u32 = 5;
#[allow(dead_code)]
const TYPE_CLASS: u32 = 6;
#[allow(dead_code)]
const TYPE_FUNCTION: u32 = 7;
#[allow(dead_code)]
const TYPE_METHOD: u32 = 8;
#[allow(dead_code)]
const TYPE_PARAMETER: u32 = 9;
const TYPE_VARIABLE: u32 = 10;
#[allow(dead_code)]
const TYPE_PROPERTY: u32 = 11;

/// Modifier bits (must match legend token_modifiers order). Reserved for future use.
#[allow(dead_code)]
const MOD_DECLARATION: u32 = 1 << 0;
#[allow(dead_code)]
const MOD_DEFINITION: u32 = 1 << 1;
#[allow(dead_code)]
const MOD_READONLY: u32 = 1 << 2;
#[allow(dead_code)]
const MOD_STATIC: u32 = 1 << 3;

fn kind_to_semantic(kind: Kind) -> (u32, u32) {
    let (token_type, modifiers) = match kind {
        Kind::TokNumber => (TYPE_NUMBER, 0u32),
        Kind::TokString => (TYPE_STRING, 0),
        Kind::TokIdent => (TYPE_VARIABLE, 0),
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
        | Kind::KwSwitch | Kind::KwCase | Kind::KwDefault | Kind::KwThrow
        | Kind::KwReserved => (TYPE_KEYWORD, 0),
        _ => (TYPE_VARIABLE, 0),
    };
    (token_type, modifiers)
}

/// Length of the token in UTF-16 code units (LSP semantic tokens use character = UTF-16).
fn token_length_utf16(token: &SyntaxToken) -> u32 {
    token.text().encode_utf16().count() as u32
}

/// Compute semantic tokens for tokens overlapping [byte_start, byte_end).
/// Returns delta-encoded LSP semantic tokens.
pub fn compute_semantic_tokens_range(
    source: &str,
    line_index: &LineIndex,
    root: &sipha::red::SyntaxNode,
    byte_start: u32,
    byte_end: u32,
) -> SemanticTokens {
    let tokens = root.descendant_semantic_tokens();
    let mut data = Vec::new();
    let mut prev_line = 0u32;
    let mut prev_char = 0u32;

    for token in tokens {
        let range = token.text_range();
        if range.end <= byte_start || range.start >= byte_end {
            continue;
        }
        let Some(kind) = token.kind_as::<Kind>() else {
            continue;
        };
        let (token_type, token_modifiers_bitset) = kind_to_semantic(kind);
        let (line, char) = line_index.line_col_utf16(source, range.start);
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

/// Compute full-document semantic tokens for the given root and source.
/// Returns delta-encoded LSP semantic tokens.
pub fn compute_semantic_tokens(
    source: &str,
    line_index: &LineIndex,
    root: &sipha::red::SyntaxNode,
) -> SemanticTokens {
    let tokens = root.descendant_semantic_tokens();
    let mut data = Vec::with_capacity(tokens.len());
    let mut prev_line = 0u32;
    let mut prev_char = 0u32;

    for token in tokens {
        let Some(kind) = token.kind_as::<Kind>() else {
            continue;
        };
        let (token_type, token_modifiers_bitset) = kind_to_semantic(kind);
        let (line, char) = line_index.line_col_utf16(source, token.text_range().start);
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
