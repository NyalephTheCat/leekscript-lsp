//! Semantic token classification from the LeekScript CST.

use leekscript_lsp::{
    semantic_token_legend, semantic_tokens_for_document, semantic_tokens_for_source,
    signature_mode_for_uri,
};

/// `keyword` index in [`semantic_token_legend`].
fn keyword_type_index() -> u32 {
    semantic_token_legend()
        .token_types
        .iter()
        .position(|t| *t == lsp_types::SemanticTokenType::KEYWORD)
        .expect("legend includes keyword") as u32
}

#[test]
fn highlights_var_as_keyword() {
    let src = "var x = 1;";
    let tokens = semantic_tokens_for_source(src);
    let kw = keyword_type_index();
    let var_tok = tokens
        .data
        .iter()
        .find(|t| t.delta_line == 0 && t.delta_start == 0 && t.token_type == kw);
    assert!(var_tok.is_some(), "expected leading `var` as keyword, got {:?}", tokens.data);
}

#[test]
fn highlights_string_literal() {
    let legend = semantic_token_legend();
    let str_idx = legend
        .token_types
        .iter()
        .position(|t| *t == lsp_types::SemanticTokenType::STRING)
        .expect("legend includes string") as u32;
    let src = r#"var s = "hi";"#;
    let tokens = semantic_tokens_for_source(src);
    assert!(
        tokens.data.iter().any(|t| t.token_type == str_idx),
        "expected a string token, got {:?}",
        tokens.data
    );
}

#[test]
fn signature_uri_detection() {
    assert!(signature_mode_for_uri("file:///proj/sig/std.sig.functions.leek"));
    assert!(signature_mode_for_uri("file:///proj/std.sig.leek"));
    assert!(!signature_mode_for_uri("file:///proj/main.leek"));
}

#[test]
fn doc_comment_uses_documentation_modifier() {
    let legend = semantic_token_legend();
    let comment_idx = legend
        .token_types
        .iter()
        .position(|t| *t == lsp_types::SemanticTokenType::COMMENT)
        .expect("legend includes comment") as u32;
    assert_eq!(
        legend.token_modifiers.first(),
        Some(&lsp_types::SemanticTokenModifier::DOCUMENTATION),
        "documentation modifier must be legend index 0"
    );
    let doc_bit: u32 = 1;

    let src = "/// One liner\nfunction f() {}\n";
    let tokens = semantic_tokens_for_document(src, None);
    let doc_tok = tokens.data.iter().find(|t| {
        t.token_type == comment_idx && t.token_modifiers_bitset == doc_bit
    });
    assert!(
        doc_tok.is_some(),
        "expected /// line with comment+documentation, got {:?}",
        tokens.data
    );

    let block = "/**\n * Line two\n */\nfunction g() {}\n";
    let tokens_b = semantic_tokens_for_document(block, None);
    let doc_lines = tokens_b
        .data
        .iter()
        .filter(|t| t.token_type == comment_idx && t.token_modifiers_bitset == doc_bit)
        .count();
    assert!(
        doc_lines >= 2,
        "expected multiline /** */ split into >=2 doc comment tokens, got {doc_lines}: {:?}",
        tokens_b.data
    );
}

#[test]
fn doxygen_commands_use_decorator_token_type() {
    let legend = semantic_token_legend();
    let decorator_idx = legend
        .token_types
        .iter()
        .position(|t| *t == lsp_types::SemanticTokenType::DECORATOR)
        .expect("legend includes decorator") as u32;
    assert_eq!(legend.token_modifiers.len(), 1);
    let doc_bit: u32 = 1;
    let src = "/// @brief Summary\nfunction f() {}\n";
    let tokens = semantic_tokens_for_document(src, None);
    assert!(
        tokens.data.iter().any(|t| {
            t.token_type == decorator_idx && t.token_modifiers_bitset == doc_bit
        }),
        "expected @brief as decorator+documentation, got {:?}",
        tokens.data
    );
    let src_bs = "/// \\brief Alt\nfunction g() {}\n";
    let tokens_b = semantic_tokens_for_document(src_bs, None);
    assert!(
        tokens_b.data.iter().any(|t| {
            t.token_type == decorator_idx && t.token_modifiers_bitset == doc_bit
        }),
        "expected \\brief as decorator+documentation, got {:?}",
        tokens_b.data
    );
}

#[test]
fn multiline_plain_block_comment_split_per_line() {
    let legend = semantic_token_legend();
    let comment_idx = legend
        .token_types
        .iter()
        .position(|t| *t == lsp_types::SemanticTokenType::COMMENT)
        .expect("legend includes comment") as u32;
    let src = "/* first\n second */\nvar x = 1;\n";
    let tokens = semantic_tokens_for_document(src, None);
    let plain = tokens
        .data
        .iter()
        .filter(|t| t.token_type == comment_idx && t.token_modifiers_bitset == 0)
        .count();
    assert!(
        plain >= 2,
        "expected >=2 plain comment lines, got {plain}: {:?}",
        tokens.data
    );
}

#[test]
fn highlights_function_stub_with_signature_uri() {
    let stub = "function abs(integer|real a) => integer|real;\n";
    let sig_uri = "file:///workspace/sig/std.sig.functions.leek";
    let tokens = semantic_tokens_for_document(stub, Some(sig_uri));
    let kw = keyword_type_index();
    let function_keywords = tokens
        .data
        .iter()
        .filter(|t| t.token_type == kw)
        .count();
    assert!(
        function_keywords >= 1,
        "expected `function` keyword token in signature parse, got {:?}",
        tokens.data
    );
}
