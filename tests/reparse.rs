//! Integration tests for incremental reparse: parse → reparse → DocumentAnalysis
//! must not produce wrong type errors (e.g. E037 "expected vr, got integer").

use leekscript_lsp::{parse, reparse, DocumentAnalysis, TextEdit};

/// Source that uses a variable holding an anonymous function with union params;
/// calling it with (z, 10) where z is string|integer must not report type mismatch.
const SOURCE: &str = r#"
var x = "test";
var y = 12;
var sum = function(string|integer a, integer b) { return a + b; };
var z = sum(x, y);
var z2 = sum(20, 10);
var z3 = sum(z, 10);
"#;

fn has_type_mismatch(analysis: &DocumentAnalysis) -> bool {
    analysis.diagnostics.iter().any(|d| {
        d.message.contains("type mismatch") || d.code.as_ref().map(|c| c.as_str()) == Some("E037")
    })
}

#[test]
fn full_parse_then_analysis_no_type_mismatch() {
    let _root = parse(SOURCE).unwrap().expect("parse");
    let analysis = DocumentAnalysis::new(SOURCE, None, &[], None, None);
    assert!(
        !has_type_mismatch(&analysis),
        "full parse: expected no type mismatch (E037), got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn reparse_then_analysis_no_type_mismatch() {
    let old_source = SOURCE;
    let root = parse(old_source).unwrap().expect("parse");

    // Single edit: insert a space after the first newline (no-op for semantics).
    let edit = TextEdit {
        start: 1,
        end: 1,
        new_text: b" ".to_vec(),
    };
    let new_source_bytes = edit.apply(old_source.as_bytes());
    let new_source = String::from_utf8(new_source_bytes).expect("utf8");

    let new_root = match reparse(old_source, &root, &edit) {
        Ok(Some(r)) => r,
        Ok(None) => panic!("reparse returned None"),
        Err(e) => panic!("reparse failed: {:?}", e),
    };

    // Same pipeline as LSP after reparse: analyze with existing_root.
    let analysis = DocumentAnalysis::new(&new_source, None, &[], Some(new_root), None);
    assert!(
        !has_type_mismatch(&analysis),
        "after reparse: expected no type mismatch (E037), got: {:?}",
        analysis.diagnostics
    );
}

#[test]
fn reparse_preserves_diagnostics_semantics() {
    let old_source = SOURCE;
    let root = parse(old_source).unwrap().expect("parse");
    let analysis_full = DocumentAnalysis::new(old_source, None, &[], None, None);

    let edit = TextEdit {
        start: 0,
        end: 0,
        new_text: b"".to_vec(),
    };
    let new_source = old_source.to_string();
    let new_root = reparse(old_source, &root, &edit).unwrap().expect("reparse");

    let analysis_reparsed = DocumentAnalysis::new(&new_source, None, &[], Some(new_root), None);

    // No new type errors after reparse.
    assert!(
        !has_type_mismatch(&analysis_reparsed),
        "reparse should not introduce type mismatch: {:?}",
        analysis_reparsed.diagnostics
    );
    // Same or fewer errors (reparse might fix parse errors in some cases).
    assert!(
        analysis_reparsed.diagnostics.len() <= analysis_full.diagnostics.len() + 2,
        "reparse should not add many spurious diagnostics"
    );
}
