//! Markdown for `textDocument/hover` from structured Doxygen ([`leekscript::syntax::ParsedDoxygen`]).

use std::fmt::Write as _;

use leekscript::syntax::{DoxygenParam, DoxygenRetval, DoxygenThrows, ParsedDoxygen};
use leekscript::{Symbol, SymbolKind};

#[inline]
fn opt_string_trim(opt: Option<&String>) -> Option<&str> {
    opt.map(|s| s.as_str().trim()).filter(|t| !t.is_empty())
}

fn has_structured_sections(doc: &ParsedDoxygen) -> bool {
    doc.brief.is_some()
        || doc.details.is_some()
        || !doc.params.is_empty()
        || !doc.template_params.is_empty()
        || doc.returns.is_some()
        || !doc.retvals.is_empty()
        || !doc.throws.is_empty()
        || !doc.see_also.is_empty()
        || doc.deprecated.is_some()
        || doc.note.is_some()
        || doc.warning.is_some()
        || doc.attention.is_some()
        || doc.preconditions.is_some()
        || doc.postconditions.is_some()
        || doc.invariant.is_some()
        || doc.remark.is_some()
        || doc.since.is_some()
        || !doc.authors.is_empty()
        || doc.version.is_some()
        || doc.copyright.is_some()
        || !doc.bugs.is_empty()
        || !doc.todos.is_empty()
        || !doc.tests.is_empty()
        || doc.internal
        || doc.overload
        || !doc.unknown.is_empty()
}

fn format_param(p: &DoxygenParam) -> String {
    let name = p.name.trim();
    let desc = p.description.trim();
    let dir = match p.direction.as_deref().map(str::trim).filter(|d| !d.is_empty()) {
        Some(d) => format!(" *({d})*"),
        None => String::new(),
    };
    if desc.is_empty() {
        format!("- **`{name}`**{dir}\n")
    } else {
        format!("- **`{name}`**{dir} — {desc}\n")
    }
}

fn format_retval(r: &DoxygenRetval) -> String {
    let v = r.value.trim();
    let d = r.description.trim();
    if d.is_empty() {
        format!("- **`{v}`**\n")
    } else {
        format!("- **`{v}`** — {d}\n")
    }
}

fn format_throw(t: &DoxygenThrows) -> String {
    let desc = t.description.trim();
    match t.type_name.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
        Some(tn) if !desc.is_empty() => format!("- **`{tn}`** — {desc}\n"),
        Some(tn) => format!("- **`{tn}`**\n"),
        None if !desc.is_empty() => format!("- {desc}\n"),
        None => String::new(),
    }
}

fn push_blockquote_heading(out: &mut String, heading: &str, body: &str) {
    let t = body.trim();
    if t.is_empty() {
        return;
    }
    out.push_str("> **");
    out.push_str(heading);
    out.push_str(":** ");
    out.push_str(t);
    out.push_str("\n\n");
}

/// Render parsed Doxygen as Markdown for hovers. Returns `None` if there is no doc text.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn parsed_doxygen_markdown(doc: &ParsedDoxygen) -> Option<String> {
    if doc.raw.trim().is_empty() {
        return None;
    }

    if !has_structured_sections(doc) {
        return Some(doc.raw.trim().to_string());
    }

    let mut out = String::new();

    if let Some(b) = opt_string_trim(doc.brief.as_ref()) {
        out.push_str(b);
        out.push_str("\n\n");
    }

    if let Some(d) = opt_string_trim(doc.details.as_ref()) {
        if opt_string_trim(doc.brief.as_ref()) != Some(d) {
            out.push_str(d);
            out.push_str("\n\n");
        }
    }

    if !doc.template_params.is_empty() {
        out.push_str("#### Template parameters\n\n");
        for p in &doc.template_params {
            out.push_str(&format_param(p));
        }
        out.push('\n');
    }

    if !doc.params.is_empty() {
        out.push_str("#### Parameters\n\n");
        for p in &doc.params {
            out.push_str(&format_param(p));
        }
        out.push('\n');
    }

    if let Some(r) = opt_string_trim(doc.returns.as_ref()) {
        out.push_str("#### Returns\n\n");
        out.push_str(r);
        out.push_str("\n\n");
    }

    if !doc.retvals.is_empty() {
        out.push_str("#### Return values\n\n");
        for rv in &doc.retvals {
            out.push_str(&format_retval(rv));
        }
        out.push('\n');
    }

    if !doc.throws.is_empty() {
        out.push_str("#### Throws\n\n");
        for t in &doc.throws {
            let s = format_throw(t);
            if !s.is_empty() {
                out.push_str(&s);
            }
        }
        out.push('\n');
    }

    if let Some(p) = opt_string_trim(doc.preconditions.as_ref()) {
        push_blockquote_heading(&mut out, "Precondition", p);
    }
    if let Some(p) = opt_string_trim(doc.postconditions.as_ref()) {
        push_blockquote_heading(&mut out, "Postcondition", p);
    }
    if let Some(p) = opt_string_trim(doc.invariant.as_ref()) {
        push_blockquote_heading(&mut out, "Invariant", p);
    }

    if let Some(t) = opt_string_trim(doc.note.as_ref()) {
        push_blockquote_heading(&mut out, "Note", t);
    }
    if let Some(t) = opt_string_trim(doc.warning.as_ref()) {
        push_blockquote_heading(&mut out, "Warning", t);
    }
    if let Some(t) = opt_string_trim(doc.attention.as_ref()) {
        push_blockquote_heading(&mut out, "Attention", t);
    }
    if let Some(t) = opt_string_trim(doc.remark.as_ref()) {
        push_blockquote_heading(&mut out, "Remark", t);
    }
    if let Some(t) = opt_string_trim(doc.deprecated.as_ref()) {
        push_blockquote_heading(&mut out, "Deprecated", t);
    }

    if !doc.see_also.is_empty() {
        out.push_str("#### See also\n\n");
        for s in &doc.see_also {
            let x = s.trim();
            if !x.is_empty() {
                out.push_str("- ");
                out.push_str(x);
                out.push('\n');
            }
        }
        out.push('\n');
    }

    let mut meta: Vec<String> = Vec::new();
    if let Some(s) = opt_string_trim(doc.since.as_ref()) {
        meta.push(format!("*Since {s}*"));
    }
    if !doc.authors.is_empty() {
        let a: Vec<_> = doc.authors.iter().map(|x| x.trim()).filter(|x| !x.is_empty()).collect();
        if !a.is_empty() {
            meta.push(format!("*Authors: {}*", a.join(", ")));
        }
    }
    if let Some(v) = opt_string_trim(doc.version.as_ref()) {
        meta.push(format!("*Version {v}*"));
    }
    if let Some(c) = opt_string_trim(doc.copyright.as_ref()) {
        meta.push(format!("*{c}*"));
    }
    if !meta.is_empty() {
        out.push_str(&meta.join(" · "));
        out.push_str("\n\n");
    }

    if !doc.bugs.is_empty() {
        out.push_str("#### Bugs\n\n");
        for b in &doc.bugs {
            let x = b.trim();
            if !x.is_empty() {
                out.push_str("- ");
                out.push_str(x);
                out.push('\n');
            }
        }
        out.push('\n');
    }
    if !doc.todos.is_empty() {
        out.push_str("#### Todo\n\n");
        for b in &doc.todos {
            let x = b.trim();
            if !x.is_empty() {
                out.push_str("- ");
                out.push_str(x);
                out.push('\n');
            }
        }
        out.push('\n');
    }
    if !doc.tests.is_empty() {
        out.push_str("#### Tests\n\n");
        for b in &doc.tests {
            let x = b.trim();
            if !x.is_empty() {
                out.push_str("- ");
                out.push_str(x);
                out.push('\n');
            }
        }
        out.push('\n');
    }

    if !doc.unknown.is_empty() {
        out.push_str("#### Documentation tags\n\n");
        for (name, body) in &doc.unknown {
            let b = body.trim();
            if b.is_empty() {
                let _ = writeln!(out, "- `\\{name}`");
            } else {
                let _ = writeln!(out, "- **`\\{name}`** — {b}");
            }
        }
        out.push('\n');
    }

    let mut badges: Vec<&str> = Vec::new();
    if doc.internal {
        badges.push("*internal*");
    }
    if doc.overload {
        badges.push("*overload*");
    }
    if !badges.is_empty() {
        out.push_str(&badges.join(" "));
        out.push_str("\n\n");
    }

    let s = out.trim_end().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

fn symbol_heading(sym: &Symbol) -> Option<String> {
    match sym.kind {
        SymbolKind::Function | SymbolKind::Method | SymbolKind::Constructor => {
            Some(format!("### `{}`\n\n", sym.name))
        }
        SymbolKind::Class => Some(format!("### class `{}`\n\n", sym.name)),
        _ => None,
    }
}

/// Hover body: optional title, Leek type, then formatted doc.
#[must_use]
pub fn symbol_markdown(sym: &Symbol) -> String {
    let mut s = String::new();
    if let Some(h) = symbol_heading(sym) {
        s.push_str(&h);
    }
    s.push_str("```leekscript\n");
    s.push_str(&sym.effective_ty().to_string());
    s.push_str("\n```");

    if let Some(doc) = &sym.doc {
        if let Some(body) = parsed_doxygen_markdown(doc) {
            s.push_str("\n\n---\n\n");
            s.push_str(&body);
        }
    }

    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use leekscript::syntax::parse_doxygen;

    #[test]
    fn brief_params_return_markdown() {
        let doc = parse_doxygen(
            r"\brief Adds two integers.
\param a the first operand
\param[in] b the second operand
\return a + b",
        );
        let md = parsed_doxygen_markdown(&doc).unwrap();
        assert!(md.contains("Adds two integers"));
        assert!(md.contains("#### Parameters"));
        assert!(md.contains("**`a`**"));
        assert!(md.contains("*(in)*"));
        assert!(md.contains("#### Returns"));
        assert!(md.contains("a + b"));
    }

    #[test]
    fn plain_comment_falls_back_to_raw() {
        let doc = parse_doxygen("Only prose, no commands.");
        let md = parsed_doxygen_markdown(&doc).unwrap();
        assert_eq!(md, "Only prose, no commands.");
    }
}
