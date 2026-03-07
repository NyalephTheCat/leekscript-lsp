//! Symbol and class resolution helpers (visibility, current class).
//!
//! Class hierarchy is built by [`leekscript_rs::build_class_super`] and stored in document state.
//! Keywords for completion come from [`leekscript_rs::KEYWORDS`].

use leekscript_rs::analysis::{class_decl_info, ResolvedSymbol};
use sipha::red::SyntaxNode;
use sipha::types::IntoSyntaxKind;

/// Innermost class containing the given offset (if cursor is inside a class body). Used for visibility.
pub fn current_class_at_offset(root: &SyntaxNode, byte_offset: u32) -> Option<String> {
    let mut innermost: Option<(u32, String)> = None; // (span_len, name) — pick smallest containing range
    for node in root.find_all_nodes(leekscript_rs::syntax::Kind::NodeClassDecl.into_syntax_kind()) {
        let r = node.text_range();
        if r.start <= byte_offset && byte_offset <= r.end {
            if let Some(info) = class_decl_info(&node) {
                let span_len = r.end - r.start;
                if innermost.as_ref().map_or(true, |(len, _)| *len > span_len) {
                    innermost = Some((span_len, info.name.clone()));
                }
            }
        }
    }
    innermost.map(|(_, name)| name)
}

/// True if `sub` is the same as `base` or a (transitive) subclass of `base`.
pub fn is_same_or_subclass(
    class_super: &std::collections::HashMap<String, String>,
    sub: &str,
    base: &str,
) -> bool {
    let mut current = sub;
    loop {
        if current == base {
            return true;
        }
        match class_super.get(current) {
            Some(sup) => current = sup,
            None => return false,
        }
    }
}

/// Collect this node and all its descendants (for type_map lookup: type may be on a child expression).
pub fn iter_self_and_descendants(node: &SyntaxNode) -> Vec<SyntaxNode> {
    let mut out = vec![node.clone()];
    for child in node.child_nodes() {
        out.extend(iter_self_and_descendants(&child));
    }
    out
}

/// Identifier prefix (alphanumeric + underscore) for completion filtering.
pub fn identifier_prefix(s: &str) -> String {
    s.chars()
        .rev()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

/// True if candidate is the same symbol as target (for references).
pub fn symbol_matches(target: &ResolvedSymbol, candidate: &ResolvedSymbol) -> bool {
    match (target, candidate) {
        (ResolvedSymbol::Variable(a), ResolvedSymbol::Variable(b)) => {
            a.name == b.name && a.span.start == b.span.start && a.span.end == b.span.end
        }
        (ResolvedSymbol::Function(na, _), ResolvedSymbol::Function(nb, _)) => na == nb,
        (ResolvedSymbol::Class(na), ResolvedSymbol::Class(nb)) => na == nb,
        (ResolvedSymbol::Global(na), ResolvedSymbol::Global(nb)) => na == nb,
        _ => false,
    }
}
