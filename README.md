# leekscript-lsp

Language server for [LeekScript](https://leekscript.com). Provides diagnostics, hover, go-to-definition, find references, document symbols, completion, signature help, formatting, and semantic tokens.

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

- **textDocument/didOpen**, **didChange**, **didClose** — re-parses and re-analyzes on change; publishes diagnostics.
- **textDocument/hover** — returns the inferred type for the expression under the cursor (when available).
- **textDocument/definition** — go to definition for variables, functions, and classes.
- **textDocument/references** — find all references to the symbol at the position (with optional include declaration).
- **textDocument/documentSymbol** — outline: top-level functions, classes, and global variables.
- **textDocument/completion** — keywords and in-scope symbols (variables, functions, classes, globals).
- **textDocument/signatureHelp** — parameter info when inside a function call (triggered by `(` and `,`).
- **textDocument/formatting** — format the whole document (round-trip with optional canonical options).
- **textDocument/semanticTokens/full** — semantic tokens for syntax highlighting.
- **textDocument/semanticTokens/range** — semantic tokens for a range (e.g. visible region).
