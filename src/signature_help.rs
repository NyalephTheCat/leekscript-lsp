//! Signature help: find function declaration by name and arity for parameter names and doc.

use leekscript_rs::analysis::{class_decl_info, function_decl_info, param_name};
use leekscript_rs::syntax::Kind;
use sipha::red::SyntaxNode;
use sipha::types::IntoSyntaxKind;

/// Find a method declaration inside a class (name + arity match) to get param names.
/// Returns (decl_node, param_names).
pub fn find_method_decl(
    root: &SyntaxNode,
    class_name: &str,
    method_name: &str,
    arity: usize,
) -> Option<(SyntaxNode, Vec<String>)> {
    let class_node = root
        .find_all_nodes(Kind::NodeClassDecl.into_syntax_kind())
        .into_iter()
        .find(|n| class_decl_info(n).map(|i| i.name == class_name).unwrap_or(false))?;
    let class_range = class_node.text_range();
    for node in root.find_all_nodes(Kind::NodeFunctionDecl.into_syntax_kind()) {
        let info = function_decl_info(&node)?;
        if info.name != method_name {
            continue;
        }
        if arity < info.min_arity || arity > info.max_arity {
            continue;
        }
        let r = node.text_range();
        if r.start >= class_range.start && r.end <= class_range.end {
            let param_nodes: Vec<SyntaxNode> = node
                .child_nodes()
                .filter(|n| n.kind_as::<Kind>() == Some(Kind::NodeParam))
                .collect();
            let param_names: Vec<String> = param_nodes
                .iter()
                .filter_map(|p| param_name(p).map(|(s, _)| s))
                .collect();
            return Some((node, param_names));
        }
    }
    None
}

/// Find a top-level function declaration (name + arity match) to get param names and doc.
/// Returns (decl_node, param_names).
pub fn find_function_decl_for_signature_help(
    root: &SyntaxNode,
    name: &str,
    arity: usize,
) -> Option<(SyntaxNode, Vec<String>)> {
    let class_ranges: Vec<(u32, u32)> = root
        .find_all_nodes(Kind::NodeClassDecl.into_syntax_kind())
        .into_iter()
        .map(|n| {
            let r = n.text_range();
            (r.start, r.end)
        })
        .collect();
    for node in root.find_all_nodes(Kind::NodeFunctionDecl.into_syntax_kind()) {
        let info = function_decl_info(&node)?;
        if info.name != name {
            continue;
        }
        if arity < info.min_arity || arity > info.max_arity {
            continue;
        }
        let r = node.text_range();
        let inside_class = class_ranges
            .iter()
            .any(|&(start, end)| start <= r.start && r.end <= end);
        if inside_class {
            continue;
        }
        let param_nodes: Vec<SyntaxNode> = node
            .child_nodes()
            .filter(|n| n.kind_as::<Kind>() == Some(Kind::NodeParam))
            .collect();
        let param_names: Vec<String> = param_nodes
            .iter()
            .filter_map(|p| param_name(p).map(|(s, _)| s))
            .collect();
        return Some((node, param_names));
    }
    None
}
