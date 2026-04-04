//! Library surface for tests: semantic tokens from in-tree [`leekscript`] parse.

mod semantic_tokens;

pub use semantic_tokens::{
    semantic_token_legend, semantic_tokens_for_document, semantic_tokens_for_source,
    signature_mode_for_uri,
};
