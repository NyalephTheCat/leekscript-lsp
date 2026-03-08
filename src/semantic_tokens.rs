//! Semantic token types and computation for LSP-based syntax highlighting.
//!
//! Delegates to [`leekscript_rs`] for computation and legend.

pub use leekscript_rs::{
    compute_semantic_tokens, compute_semantic_tokens_fallback, compute_semantic_tokens_range,
    semantic_tokens_provider,
};
