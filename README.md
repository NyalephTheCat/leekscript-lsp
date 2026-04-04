# leekscript-lsp

Language server for [LeekScript](https://leekscript.com). Provides **syntax highlighting only** via LSP semantic tokens.

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

### Other editors

Point your LSP client at the `leekscript-lsp` executable. The server supports:

- **textDocument/didOpen**, **didChange**, **didClose** — full-document sync (buffer kept in memory).
- **textDocument/semanticTokens/full** — semantic tokens for highlighting (parses the buffer on each request).

Highlighting uses standard LSP semantic token kinds (`keyword`, `string`, `number`, `comment`, `operator`, `type`, `variable`).

- **Comments:** Any multiline `/* … */` or `// …` trivia that contains newlines is split into **one semantic token per visual line** (plain comments keep the `comment` kind only).
- **Docstrings:** `///`, `//!`, and `/** … */` (except empty `/**/`) use `comment` + the `documentation` modifier.
- **Doxygen:** Inside doc lines, `\` / `@` command tokens use `leekscript::syntax::doxygen_command_byte_ranges` (same scanner as `parse_doxygen`). Those spans use the standard LSP token type **`decorator`** (like TypeScript `@` annotations) plus the **`documentation`** modifier; prose in the same line stays **`comment`** + `documentation`.

Use `editor.semanticTokenColorCustomizations` / `semanticTokenScopes` in VS Code to tune `decorator.documentation` vs `comment.documentation` if needed.

Signature stub files are detected from the document URI (same rules as `leekscript::is_signature_stub_path`: names ending in `.sig.leek` or containing `.sig.` before `.leek`). Those buffers are parsed in **signature mode** so `function … => T;` stubs tokenize like normal LeekScript.

## Development

From the **repository root**:

```bash
cargo build -p leekscript-lsp
cargo test -p leekscript-lsp
cargo fmt --all -- --check
cargo clippy --workspace --all-features -- -D warnings
```

See the root [CONTRIBUTING.md](../../CONTRIBUTING.md) and [cursor.md](../../cursor.md) for conventions and workflow.
