//! Formatting of doc comments and class hover summaries for LSP hover.

use leekscript_rs::doc_comment::DocComment;
use leekscript_rs::syntax::Kind;
use sipha::red::SyntaxNode;
use sipha::types::IntoSyntaxKind;

use leekscript_rs::analysis::{class_field_info, function_decl_info};

/// Format a parsed doc comment as Markdown for hover display.
pub fn format_doc_comment_markdown(doc: &DocComment) -> String {
    let mut parts = Vec::new();
    if let Some(ref brief) = doc.brief {
        if !brief.is_empty() {
            parts.push(brief.clone());
        }
    }
    if !doc.description.is_empty() {
        parts.push(doc.description.clone());
    }
    if !doc.params.is_empty() {
        parts.push(
            doc.params
                .iter()
                .map(|(name, desc)| {
                    if desc.is_empty() {
                        format!("- **{name}**")
                    } else {
                        format!("- **{name}** — {desc}")
                    }
                })
                .collect::<Vec<_>>()
                .join("\n"),
        );
    }
    if let Some(ref ret) = doc.returns {
        if !ret.is_empty() {
            parts.push(format!("**Returns:** {ret}"));
        }
    }
    if let Some(ref dep) = doc.deprecated {
        if !dep.is_empty() {
            parts.push(format!("*Deprecated:* {dep}"));
        }
    }
    if !doc.see.is_empty() {
        parts.push(format!("**See:** {}", doc.see.join(", ")));
    }
    if let Some(ref since) = doc.since {
        if !since.is_empty() {
            parts.push(format!("**Since:** {since}"));
        }
    }
    parts.retain(|s| !s.is_empty());
    parts.join("\n\n")
}

/// Build a rich class summary for hover: signature, fields, constructors, and methods.
/// When `method_type_strings` is provided, methods are shown as `name: Function<params => return_type>`.
pub fn format_class_hover_summary(
    class_node: &SyntaxNode,
    root: &SyntaxNode,
    class_name: &str,
    super_class: Option<&str>,
    method_type_strings: Option<&std::collections::HashMap<String, String>>,
) -> String {
    let class_range = class_node.text_range();
    let mut fields: Vec<String> = Vec::new();
    let mut constructors: Vec<String> = Vec::new();
    let mut methods: Vec<String> = Vec::new();

    let direct_member = |d: &SyntaxNode| {
        let containing = d
            .ancestors(root)
            .into_iter()
            .find(|a| a.kind_as::<Kind>() == Some(Kind::NodeClassDecl));
        containing.as_ref().map(|a| a.text_range()) == Some(class_range)
    };

    for d in class_node.find_all_nodes(Kind::NodeClassField.into_syntax_kind()) {
        if !direct_member(&d) {
            continue;
        }
        if let Some((name, ty_opt, is_static)) = class_field_info(&d) {
            let ty_str = ty_opt
                .as_ref()
                .map(|t| t.for_annotation())
                .unwrap_or_else(|| "any".to_string());
            let prefix = if is_static { "static " } else { "" };
            fields.push(format!("{prefix}{name}: {ty_str}"));
        }
    }
    for d in class_node.find_all_nodes(Kind::NodeFunctionDecl.into_syntax_kind()) {
        if !direct_member(&d) {
            continue;
        }
        if let Some(info) = function_decl_info(&d) {
            // Constructor or method: show as name: Function<params => return_type> when type is available.
            let sig = method_type_strings
                .and_then(|m| m.get(&info.name))
                .map(|ty_str| format!("{}: {}", info.name, ty_str))
                .unwrap_or_else(|| {
                    if info.min_arity == info.max_arity {
                        format!(
                            "{}({} param{})",
                            info.name,
                            info.min_arity,
                            if info.min_arity == 1 { "" } else { "s" }
                        )
                    } else {
                        format!("{}({}..{} params)", info.name, info.min_arity, info.max_arity)
                    }
                });
            if info.name == class_name {
                constructors.push(sig);
            } else {
                methods.push(sig);
            }
        }
    }

    let mut lines: Vec<String> = Vec::new();
    let header = match super_class {
        Some(s) => format!("class {} extends {}", class_name, s),
        None => format!("class {}", class_name),
    };
    lines.push(header);
    if !fields.is_empty() {
        lines.push(format!("**Fields:** {}", fields.join(", ")));
    }
    if !constructors.is_empty() {
        lines.push(format!("**Constructors:** {}", constructors.join(", ")));
    }
    if !methods.is_empty() {
        lines.push(format!("**Methods:** {}", methods.join(", ")));
    }
    lines.join("\n\n")
}
