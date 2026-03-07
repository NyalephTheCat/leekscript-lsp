//! Formatting of doc comments and class hover summaries for LSP hover.

use leekscript_rs::doc_comment::DocComment;
use leekscript_rs::syntax::Kind;
use sipha::red::SyntaxNode;
use sipha::types::IntoSyntaxKind;

use leekscript_rs::analysis::{class_field_info, function_decl_info};

/// Escape Markdown metacharacters in user content to avoid accidental emphasis (e.g. _ and *).
fn escape_markdown(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '_' => out.push_str("\\_"),
            '*' => out.push_str("\\*"),
            '`' => out.push_str("\\`"),
            '#' => out.push_str("\\#"),
            '[' => out.push_str("\\["),
            ']' => out.push_str("\\]"),
            '<' => out.push_str("\\<"),
            '>' => out.push_str("\\>"),
            _ => out.push(c),
        }
    }
    out
}

/// Format a parsed doc comment as Markdown for hover display.
pub fn format_doc_comment_markdown(doc: &DocComment) -> String {
    let mut parts = Vec::new();
    if let Some(ref brief) = doc.brief {
        if !brief.is_empty() {
            parts.push(escape_markdown(brief));
        }
    }
    if !doc.description.is_empty() {
        parts.push(escape_markdown(&doc.description));
    }
    if !doc.params.is_empty() {
        parts.push(
            doc.params
                .iter()
                .map(|(name, desc)| {
                    let name_esc = escape_markdown(name);
                    if desc.is_empty() {
                        format!("- **{name_esc}**")
                    } else {
                        format!("- **{name_esc}** — {}", escape_markdown(desc))
                    }
                })
                .collect::<Vec<_>>()
                .join("\n"),
        );
    }
    if let Some(ref ret) = doc.returns {
        if !ret.is_empty() {
            parts.push(format!("**Returns:** {}", escape_markdown(ret)));
        }
    }
    if let Some(ref dep) = doc.deprecated {
        if !dep.is_empty() {
            parts.push(format!("*Deprecated:* {}", escape_markdown(dep)));
        }
    }
    if !doc.see.is_empty() {
        parts.push(format!(
            "**See:** {}",
            doc.see
                .iter()
                .map(|s| escape_markdown(s))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if let Some(ref since) = doc.since {
        if !since.is_empty() {
            parts.push(format!("**Since:** {}", escape_markdown(since)));
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
        Some(s) => format!("class `{}` extends `{}`", escape_markdown(class_name), escape_markdown(s)),
        None => format!("class `{}`", escape_markdown(class_name)),
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

/// Build a single Markdown string for hover: optional code block with signature/type, then optional doc block.
pub fn hover_markdown(signature_or_type: &str, doc: Option<&str>) -> String {
    let mut parts = Vec::new();
    if !signature_or_type.is_empty() {
        parts.push(format!("```leek\n{}\n```", signature_or_type));
    }
    if let Some(d) = doc {
        if !d.is_empty() {
            parts.push(d.to_string());
        }
    }
    parts.join("\n\n")
}
