//! Include directive parsing: resolve path at offset for go-to-definition on include("...").

use std::collections::HashMap;
use std::path::PathBuf;

use leekscript_rs::IncludeTree;

/// If `byte_offset` in `source` is inside an `include("...")` or `include('...')`, return the
/// resolved path of the included file. Only returns paths that exist in `included_paths`.
pub fn include_path_at_offset(
    source: &str,
    main_path: &std::path::Path,
    byte_offset: usize,
    included_paths: &HashMap<PathBuf, String>,
) -> Option<PathBuf> {
    let bytes = source.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if !word_match(bytes, i, b"include") {
            i = next_char_boundary(bytes, i);
            continue;
        }
        let include_keyword_start = i;
        i += 7;
        i = skip_whitespace_and_comments(bytes, i);
        if i >= bytes.len() || bytes[i] != b'(' {
            i = next_char_boundary(bytes, include_keyword_start);
            continue;
        }
        i += 1;
        i = skip_whitespace_and_comments(bytes, i);
        if i >= bytes.len() {
            break;
        }
        let quote = bytes[i];
        if quote != b'"' && quote != b'\'' {
            i = next_char_boundary(bytes, i);
            continue;
        }
        let string_start = i;
        if let Some((path_str, end)) = parse_include_string_bytes(bytes, i) {
            let string_end = end;
            let in_keyword = byte_offset >= include_keyword_start && byte_offset < include_keyword_start + 7;
            let in_string = byte_offset >= string_start && byte_offset <= string_end;
            if in_keyword || in_string {
                let base_dir = main_path.parent().unwrap_or(std::path::Path::new("."));
                let resolved = base_dir.join(path_str);
                if included_paths.contains_key(&resolved) {
                    return Some(resolved);
                }
            }
            i = end;
        } else {
            i = next_char_boundary(bytes, i);
        }
    }
    None
}

pub fn word_match(bytes: &[u8], i: usize, word: &[u8]) -> bool {
    if i + word.len() > bytes.len() {
        return false;
    }
    if bytes[i..i + word.len()] != *word {
        return false;
    }
    let after = i + word.len();
    if after < bytes.len() {
        let c = bytes[after];
        if c.is_ascii_alphanumeric() || c == b'_' {
            return false;
        }
    }
    let before_ok = i == 0 || {
        let c = bytes[i - 1];
        !c.is_ascii_alphanumeric() && c != b'_'
    };
    before_ok
}

pub fn skip_whitespace_and_comments(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() {
        if bytes[i].is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'/' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i = next_char_boundary(bytes, i);
            }
            continue;
        }
        if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i = next_char_boundary(bytes, i);
            }
            if i + 1 < bytes.len() {
                i += 2;
            }
            continue;
        }
        break;
    }
    i
}

/// Parse a double- or single-quoted string starting at i; return (unescaped content, index after string).
pub fn parse_include_string_bytes(bytes: &[u8], i: usize) -> Option<(String, usize)> {
    if i >= bytes.len() {
        return None;
    }
    let quote = bytes[i];
    if quote != b'"' && quote != b'\'' {
        return None;
    }
    let mut out = String::new();
    let mut j = i + 1;
    while j < bytes.len() {
        if bytes[j] == b'\\' && j + 1 < bytes.len() {
            match bytes[j + 1] {
                b'n' => out.push('\n'),
                b't' => out.push('\t'),
                b'r' => out.push('\r'),
                b'"' => out.push('"'),
                b'\'' => out.push('\''),
                b'\\' => out.push('\\'),
                b'u' if j + 5 < bytes.len() => {
                    let hex = std::str::from_utf8(&bytes[j + 2..j + 6]).ok()?;
                    let code = u32::from_str_radix(hex, 16).ok()?;
                    out.push(char::from_u32(code)?);
                    j += 4;
                }
                _ => out.push(bytes[j + 1] as char),
            }
            j += 2;
            continue;
        }
        if bytes[j] == quote {
            return Some((out, j + 1));
        }
        out.push(bytes[j] as char);
        j = next_char_boundary(bytes, j);
    }
    None
}

pub fn next_char_boundary(bytes: &[u8], i: usize) -> usize {
    if i >= bytes.len() {
        return bytes.len();
    }
    let b = bytes[i];
    if b < 128 {
        return i + 1;
    }
    let mut j = i + 1;
    while j < bytes.len() && (bytes[j] & 0xC0) == 0x80 {
        j += 1;
    }
    j
}

/// Build path -> source map from an include tree (for include_path_at_offset).
pub fn tree_file_contents(tree: &IncludeTree) -> HashMap<PathBuf, String> {
    leekscript_rs::all_files(tree)
        .into_iter()
        .map(|(p, s)| (p, s.to_string()))
        .collect()
}
