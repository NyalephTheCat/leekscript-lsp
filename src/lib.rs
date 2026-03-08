//! Library surface for leekscript-lsp (used by integration tests and potentially other tools).
//!
//! The main binary is in `main.rs`; this crate re-exports types needed for testing the
//! parse/reparse + analysis pipeline.

pub use leekscript_rs::{parse, reparse, reparse_or_parse, DocumentAnalysis, TextEdit};
