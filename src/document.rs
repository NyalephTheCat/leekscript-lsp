//! Document state for the LSP.
//!
//! Uses [`leekscript_rs::DocumentAnalysis`] as the single source of truth; no separate state struct.

pub use leekscript_rs::DocumentAnalysis;

/// Per-document state: alias for the analysis result from leekscript-rs.
pub type DocumentState = DocumentAnalysis;
