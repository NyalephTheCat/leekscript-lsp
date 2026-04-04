//! CST token → LSP semantic tokens for editor highlighting.

use leekscript::parse::{
    LanguageOptions, parse_doc_with_recovery, parse_signature_doc_with_recovery,
};
use leekscript::syntax::kinds::K;
use lsp_types::{
    SemanticToken, SemanticTokenModifier, SemanticTokenType, SemanticTokens, SemanticTokensLegend, Url,
};
use sipha::diagnostics::parsed_doc::ParsedDoc;
use sipha::diagnostics::utf16::{span_to_utf16_range, utf16_len};
use sipha::tree::red::SyntaxToken;
use sipha::types::{Pos, Span};

/// [`SemanticTokenType::COMMENT`] index in [`semantic_token_legend`].
const TY_COMMENT: u32 = 3;
/// [`SemanticTokenType::DECORATOR`] index — standard LSP name for `@` / annotation-style tokens (doc commands).
const TY_DECORATOR: u32 = 7;

/// Modifier bit for [`SemanticTokenModifier::DOCUMENTATION`] (legend index `0`).
const MOD_DOCUMENTATION: u32 = 1 << 0;

/// Legend indices must match [`leek_token_semantics`] and [`emit_documentation_line`].
pub fn semantic_token_legend() -> SemanticTokensLegend {
    SemanticTokensLegend {
        token_types: vec![
            SemanticTokenType::KEYWORD,
            SemanticTokenType::STRING,
            SemanticTokenType::NUMBER,
            SemanticTokenType::COMMENT,
            SemanticTokenType::OPERATOR,
            SemanticTokenType::TYPE,
            SemanticTokenType::VARIABLE,
            SemanticTokenType::DECORATOR,
        ],
        token_modifiers: vec![SemanticTokenModifier::DOCUMENTATION],
    }
}

fn documentation_modifier_bitset(bytes: &[u8]) -> u32 {
    // Line doc: `///` (Doxygen / LeekScript) or `//!` (file-level / directives).
    if bytes.starts_with(b"///") || bytes.starts_with(b"//!") {
        return MOD_DOCUMENTATION;
    }
    // Block doc: `/** … */` (not the empty `/**/` token).
    if bytes.starts_with(b"/**") && bytes != b"/**/" {
        return MOD_DOCUMENTATION;
    }
    0
}

fn span_utf16_len_first_line(doc: &ParsedDoc, span: Span) -> u32 {
    let bytes = doc.span_slice(span);
    let n = bytes
        .iter()
        .position(|&b| b == b'\n' || b == b'\r')
        .unwrap_or(bytes.len());
    let sub = &bytes[..n];
    std::str::from_utf8(sub).map(utf16_len).unwrap_or(0)
}

/// `(line_start_byte, line_end_byte)` per visual line inside `span` (`end` is exclusive).
fn span_visual_line_ranges(doc: &ParsedDoc, span: Span) -> Vec<(Pos, Pos)> {
    let bytes = doc.span_slice(span);
    let mut out = Vec::new();
    let Some(s) = std::str::from_utf8(bytes).ok() else {
        let n = bytes
            .iter()
            .position(|&b| b == b'\n' || b == b'\r')
            .unwrap_or(bytes.len());
        let end = span.start.saturating_add(u32::try_from(n).unwrap_or(u32::MAX));
        if end > span.start {
            out.push((span.start, end));
        }
        return out;
    };
    let base = span.start;
    let mut line_start = 0usize;
    for (i, _) in s.match_indices('\n') {
        let raw = &s[line_start..i];
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        let ls = base.saturating_add(u32::try_from(line_start).unwrap_or(u32::MAX));
        let le = ls.saturating_add(u32::try_from(line.len()).unwrap_or(u32::MAX));
        if le > ls {
            out.push((ls, le));
        }
        line_start = i + 1;
    }
    let raw = &s[line_start..];
    let line = raw.strip_suffix('\r').unwrap_or(raw);
    let ls = base.saturating_add(u32::try_from(line_start).unwrap_or(u32::MAX));
    let le = ls.saturating_add(u32::try_from(line.len()).unwrap_or(u32::MAX));
    if le > ls {
        out.push((ls, le));
    }
    out
}

fn push_span_comment(
    data: &mut Vec<SemanticToken>,
    doc: &ParsedDoc,
    state: &mut EncodeState,
    start: Pos,
    end: Pos,
    ty: u32,
    mods: u32,
) {
    if end <= start {
        return;
    }
    let src = doc.source_str();
    let (u0, u1) = span_to_utf16_range(Span::new(start, end), src);
    let len = u1.saturating_sub(u0);
    push_semantic_token(data, doc, state, start, len, ty, mods);
}

fn emit_documentation_line(
    data: &mut Vec<SemanticToken>,
    doc: &ParsedDoc,
    state: &mut EncodeState,
    line_start_byte: Pos,
    line: &str,
) {
    let tags = leekscript::syntax::doxygen_command_byte_ranges(line);
    let line_end_byte = line_start_byte.saturating_add(u32::try_from(line.len()).unwrap_or(u32::MAX));
    if tags.is_empty() {
        push_span_comment(
            data,
            doc,
            state,
            line_start_byte,
            line_end_byte,
            TY_COMMENT,
            MOD_DOCUMENTATION,
        );
        return;
    }
    let mut cursor = line_start_byte;
    for (ts, te) in tags {
        let abs_s = line_start_byte.saturating_add(u32::try_from(ts).unwrap_or(u32::MAX));
        let abs_e = line_start_byte.saturating_add(u32::try_from(te).unwrap_or(u32::MAX));
        if abs_s > cursor {
            push_span_comment(data, doc, state, cursor, abs_s, TY_COMMENT, MOD_DOCUMENTATION);
        }
        push_span_comment(
            data,
            doc,
            state,
            abs_s,
            abs_e,
            TY_DECORATOR,
            MOD_DOCUMENTATION,
        );
        cursor = abs_e;
    }
    if cursor < line_end_byte {
        push_span_comment(
            data,
            doc,
            state,
            cursor,
            line_end_byte,
            TY_COMMENT,
            MOD_DOCUMENTATION,
        );
    }
}

fn emit_comment_coverage(
    data: &mut Vec<SemanticToken>,
    doc: &ParsedDoc,
    state: &mut EncodeState,
    span: Span,
    mods: u32,
) {
    let bytes = doc.span_slice(span);
    let multiline = bytes.contains(&b'\n');

    if multiline {
        for (ls, le) in span_visual_line_ranges(doc, span) {
            let line_slice = doc.span_slice(Span::new(ls, le));
            let Ok(line_str) = std::str::from_utf8(line_slice) else {
                continue;
            };
            if mods & MOD_DOCUMENTATION != 0 {
                emit_documentation_line(data, doc, state, ls, line_str);
            } else {
                push_span_comment(data, doc, state, ls, le, TY_COMMENT, mods);
            }
        }
        return;
    }

    let line_slice = doc.span_slice(span);
    let Ok(line_str) = std::str::from_utf8(line_slice) else {
        let len = span_utf16_len_first_line(doc, span);
        push_semantic_token(data, doc, state, span.start, len, TY_COMMENT, mods);
        return;
    };
    if mods & MOD_DOCUMENTATION != 0 {
        emit_documentation_line(data, doc, state, span.start, line_str);
    } else {
        push_span_comment(data, doc, state, span.start, span.end, TY_COMMENT, mods);
    }
}

struct EncodeState {
    prev_line: u32,
    prev_start: u32,
    first: bool,
}

impl Default for EncodeState {
    fn default() -> Self {
        Self {
            prev_line: 0,
            prev_start: 0,
            first: true,
        }
    }
}

fn push_semantic_token(
    data: &mut Vec<SemanticToken>,
    doc: &ParsedDoc,
    state: &mut EncodeState,
    offset: Pos,
    len_utf16: u32,
    ty: u32,
    mods: u32,
) {
    if len_utf16 == 0 {
        return;
    }
    let (line, col) = doc.offset_to_line_col_utf16(offset);
    let (delta_line, delta_start) = if state.first {
        state.first = false;
        (line, col)
    } else {
        let dl = line.saturating_sub(state.prev_line);
        let ds = if dl == 0 {
            col.saturating_sub(state.prev_start)
        } else {
            col
        };
        (dl, ds)
    };
    state.prev_line = line;
    state.prev_start = col;
    data.push(SemanticToken {
        delta_line,
        delta_start,
        length: len_utf16,
        token_type: ty,
        token_modifiers_bitset: mods,
    });
}

fn is_type_keyword(k: K) -> bool {
    matches!(
        k,
        K::VoidKw
            | K::BooleanKw
            | K::AnyKw
            | K::IntegerKw
            | K::RealKw
            | K::StringTypeKw
            | K::ClassTypeKw
            | K::ObjectKw
            | K::ArrayKw
            | K::SetTypeKw
            | K::MapKw
            | K::FunctionTypeKw
            | K::IntervalKw
    )
}

fn is_control_keyword(k: K) -> bool {
    matches!(
        k,
        K::VarKw
            | K::LetKw
            | K::BreakKw
            | K::ContinueKw
            | K::DoKw
            | K::ReturnKw
            | K::FunctionKw
            | K::IfKw
            | K::ElseKw
            | K::ForKw
            | K::WhileKw
            | K::IncludeKw
            | K::MatchKw
            | K::InKw
            | K::AsKw
            | K::ClassKw
            | K::NewKw
            | K::ThisKw
            | K::SuperKw
            | K::SwitchKw
            | K::CaseKw
            | K::DefaultKw
            | K::GlobalKw
            | K::ExtendsKw
            | K::PublicKw
            | K::PrivateKw
            | K::ProtectedKw
            | K::StaticKw
            | K::FinalKw
            | K::ConstructorKw
            | K::IsKw
            | K::InstanceofKw
            | K::XorKw
            | K::NotKw
            | K::AbstractKw
            | K::AwaitKw
            | K::ByteKw
            | K::CatchKw
            | K::CharKw
            | K::ConstKw
            | K::DoubleKw
            | K::EnumKw
            | K::EvalKw
            | K::ExportKw
            | K::FinallyKw
            | K::FloatKw
            | K::GotoKw
            | K::ImplementsKw
            | K::ImportKw
            | K::IntKw
            | K::InterfaceKw
            | K::LongKw
            | K::NativeKw
            | K::PackageKw
            | K::ShortKw
            | K::SynchronizedKw
            | K::ThrowKw
            | K::ThrowsKw
            | K::TransientKw
            | K::TryKw
            | K::TypeofKw
            | K::VolatileKw
            | K::WithKw
            | K::YieldKw
    )
}

fn leek_token_semantics(token: &SyntaxToken) -> Option<(u32, u32)> {
    let k = token.kind_as::<K>()?;
    let text = token.text();
    let bytes = text.as_bytes();
    let ty = match k {
        K::Ws | K::Trivia => return None,
        K::LineComment | K::BlockComment => TY_COMMENT,
        K::String => 1,
        K::Number | K::Pi | K::Infinity | K::TrueKw | K::FalseKw | K::NullKw => 2,
        K::Ident => 6,
        k if is_type_keyword(k) => 5,
        k if is_control_keyword(k) => 0,
        K::Coalesce
        | K::CoalesceEq
        | K::StarStar
        | K::StarStarEq
        | K::Backslash
        | K::BackslashEq
        | K::Shl
        | K::Shr
        | K::UShr
        | K::ShlEq
        | K::ShrEq
        | K::UShrEq
        | K::TripleShl
        | K::TripleShlEq
        | K::BitAnd
        | K::BitOr
        | K::BitXor
        | K::BitAndEq
        | K::BitOrEq
        | K::BitXorEq
        | K::Question
        | K::Semi
        | K::Comma
        | K::Colon
        | K::Dot
        | K::DotDot
        | K::Arrow
        | K::Eq
        | K::Plus
        | K::Minus
        | K::Star
        | K::Slash
        | K::Percent
        | K::PlusEq
        | K::MinusEq
        | K::StarEq
        | K::SlashEq
        | K::PercentEq
        | K::EqEq
        | K::NotEq
        | K::EqEqEq
        | K::NotEqEq
        | K::Lt
        | K::Lte
        | K::Gt
        | K::Gte
        | K::AndAnd
        | K::OrOr
        | K::Bang
        | K::Tilde
        | K::PlusPlus
        | K::MinusMinus
        | K::LParen
        | K::RParen
        | K::LBracket
        | K::RBracket
        | K::LBrace
        | K::RBrace
        | K::Operator => 4,
        _ => return None,
    };
    let mods = match k {
        K::LineComment | K::BlockComment => documentation_modifier_bitset(bytes),
        _ => 0,
    };
    Some((ty, mods))
}

/// `true` when the document URI names a signature / stub file (`.sig.leek` or `.sig.` in the filename).
#[must_use]
pub fn signature_mode_for_uri(uri_str: &str) -> bool {
    let Ok(url) = Url::parse(uri_str) else {
        return false;
    };
    if let Ok(path) = url.to_file_path() {
        if leekscript::is_signature_stub_path(&path) {
            return true;
        }
    }
    let path = url.path();
    path.contains(".sig.") || path.ends_with(".sig.leek")
}

fn semantic_tokens_from_parsed_doc(doc: &ParsedDoc) -> SemanticTokens {
    let mut data: Vec<SemanticToken> = Vec::new();
    let mut state = EncodeState::default();

    for t in doc.root().descendant_tokens() {
        let Some((ty, mods)) = leek_token_semantics(&t) else {
            continue;
        };
        let span = t.text_range();

        if ty == TY_COMMENT {
            emit_comment_coverage(&mut data, doc, &mut state, span, mods);
            continue;
        }

        let len = span_utf16_len_first_line(doc, span);
        push_semantic_token(&mut data, doc, &mut state, span.start, len, ty, mods);
    }

    SemanticTokens {
        result_id: None,
        data,
    }
}

/// Full-document semantic tokens for `source` (UTF-8). Uses signature parse mode when `document_uri`
/// points at a signature stub file (see [`leekscript::is_signature_stub_path`]).
#[must_use]
pub fn semantic_tokens_for_document(source: &str, document_uri: Option<&str>) -> SemanticTokens {
    let opts = LanguageOptions::v4_experimental_all();
    let parsed = if document_uri.is_some_and(signature_mode_for_uri) {
        parse_signature_doc_with_recovery(source, opts)
    } else {
        parse_doc_with_recovery(source, opts)
    };
    let Ok(parsed) = parsed else {
        return SemanticTokens::default();
    };
    semantic_tokens_from_parsed_doc(&parsed.doc)
}

/// Like [`semantic_tokens_for_document`] with `document_uri: None` (normal `.leek` modules).
#[must_use]
#[allow(dead_code)] // Public crate API; the `leekscript-lsp` binary only calls `semantic_tokens_for_document`.
pub fn semantic_tokens_for_source(source: &str) -> SemanticTokens {
    semantic_tokens_for_document(source, None)
}
