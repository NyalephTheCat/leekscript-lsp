# leekscript-lsp

Language server for [LeekScript](https://leekscript.com). Uses the same merged-project pipeline as `leekscript check` (includes, signature stubs, open-buffer overlay) for diagnostics and for navigation features.

## Build

From the workspace root:

```bash
cargo build -p leekscript-lsp
```

## Run

The server talks LSP over stdio. Configure your editor to run the `leekscript-lsp` binary as the language server for `.leek` files.

### VS Code

1. Install the "LeekScript" extension or use a generic LSP client.
2. Set the server command to the path of `leekscript-lsp` (e.g. `target/debug/leekscript-lsp`).

The extension passes `initializationOptions` (see `leekscript-code`):

- `signatureFiles` — extra `.sig` bundle paths
- `inlayHints.enabled` — enable/disable inlay hints (server capability)
- `codeLens.references` — enable/disable reference count code lens (server capability)

### Other editors

Point your LSP client at the `leekscript-lsp` executable.

## Supported LSP features

- **Document sync** — `textDocument/didOpen`, `didChange`, `didClose` (full sync).
- **Diagnostics** — `textDocument/publishDiagnostics` (parse + semantic analysis, mapped across `include` and signature prelude).
- **Semantic tokens** — `textDocument/semanticTokens/full` and `textDocument/semanticTokens/range`.
- **Formatting** — `textDocument/formatting`, `textDocument/rangeFormatting`.
- **Folding** — `textDocument/foldingRange`.
- **Hover** — types, symbol docs (Doxygen), unresolved name hints.
- **Go to definition** — `textDocument/definition` (resolved references).
- **Find references** — `textDocument/references` (optional declaration).
- **Completion** — keywords plus symbol names declared before the cursor (merged analysis).
- **Document symbols** — outline for symbols that lie in the **current file** (ranges mapped from the merged tree).
- **Rename** — `textDocument/rename` when spans match identifier length (same constraint as simple LSP rename).
- **Inlay hints** — inferred types for unannotated `var` / parameters (when enabled in init options).
- **Code lens** — “N references” on functions/classes/methods (when enabled; uses `leekscript.showReferences` in VS Code).
- **Code actions** — advertised; currently returns an empty list (placeholder for future quick fixes).

`file://` URIs use the merged include graph; non-file buffers fall back to single-buffer behaviour for diagnostics only.

**Caching:** merged parse + semantic analysis for hovers, navigation, completion, etc. is cached per open `file://` URI (keyed by buffer text and signature file list). The cache is cleared when `initialize` runs, and entries for the whole include project are dropped when any open file in that project is opened, edited, or closed (so includes and overlay stay consistent).

### Semantic highlighting

Highlighting uses standard LSP semantic token kinds (`keyword`, `string`, `number`, `comment`, `operator`, `type`, `variable`, etc.).

- **Comments:** Any multiline `/* … */` or `// …` trivia that contains newlines is split into **one semantic token per visual line** (plain comments keep the `comment` kind only).
- **Docstrings:** `///`, `//!`, and `/** … */` (except empty `/**/`) use `comment` + the `documentation` modifier.
- **Doxygen:** Inside doc lines, `\` / `@` command tokens use `leekscript::syntax::doxygen_command_byte_ranges` (same scanner as `parse_doxygen`). Those spans use the standard LSP token type **`decorator`** plus the **`documentation`** modifier; prose in the same line stays **`comment`** + `documentation`.

Use `editor.semanticTokenColorCustomizations` / `semanticTokenScopes` in VS Code to tune `decorator.documentation` vs `comment.documentation` if needed.

Signature stub files are detected from the document URI (same rules as `leekscript::is_signature_stub_path`: names ending in `.sig.leek` or containing `.sig.` before `.leek`). Those buffers are parsed in **signature mode** so `function … => T;` stubs tokenize like normal LeekScript.

## Development

From the **repository root**:

```bash
cargo build -p leekscript-lsp
cargo test -p leekscript-lsp
cargo fmt --all -- --check
cargo clippy --no-deps -p leekscript-lsp -- -D warnings
```

See the root [CONTRIBUTING.md](../../CONTRIBUTING.md) and [cursor.md](../../cursor.md) for conventions and workflow.
