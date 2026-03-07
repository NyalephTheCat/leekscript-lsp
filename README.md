# leekscript-lsp

Language server for [LeekScript](https://leekscript.com). Provides diagnostics, hover, go-to-definition, find references, rename, document symbols, completion, signature help, formatting, semantic tokens, inlay hints, code actions, and more.

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

**Text sync**

- **textDocument/didOpen**, **didChange**, **didClose** — re-parses and re-analyzes on change; publishes diagnostics. Analysis runs on a blocking thread to keep the editor responsive.

**Navigation & symbols**

- **textDocument/definition** — go to definition for variables, functions, and classes (including in included files).
- **textDocument/references** — find all references to the symbol at the position (with optional include declaration).
- **textDocument/documentSymbol** — outline: top-level functions, classes, and global variables (nested methods/fields).
- **workspace/symbol** — workspace-wide symbol search (including nested methods/fields); results sorted by relevance (prefix then substring).

**Rename**

- **textDocument/rename** — rename symbol at position (with **prepareRename** for validation). Rejects invalid identifiers.

**Hover & completion**

- **textDocument/hover** — inferred type and optional doc comment for the expression under the cursor.
- **textDocument/completion** — keywords and in-scope symbols (variables, functions, classes, globals); member completion after `.`.
- **textDocument/signatureHelp** — parameter info when inside a function call (triggered by `(` and `,`).

**Formatting & highlighting**

- **textDocument/formatting** — format the whole document (respects client `tabSize` / `insertSpaces`).
- **textDocument/rangeFormatting** — format the selected range.
- **textDocument/semanticTokens/full** — semantic tokens for syntax highlighting.
- **textDocument/semanticTokens/range** — semantic tokens for a range (e.g. visible region).

**Other**

- **textDocument/inlayHint** — optional type hints (variable types, parameter names, return types, scope-end labels).
- **textDocument/codeAction** — quickfixes (e.g. deprecation `===`→`==`, add global declaration for unknown variable).
- **textDocument/documentHighlight** — highlight all references to the symbol at the cursor.
- **textDocument/foldingRange** — folding ranges for classes, functions, blocks.
- **textDocument/selectionRange** — expand selection by AST node.
- **textDocument/documentLink** — clickable links for `include("...")` paths; **documentLink/resolve** for descriptive tooltip (validates target file and shows "file not found" when missing).
- **callHierarchy/prepare**, **incomingCalls**, **outgoingCalls** — call hierarchy.
- **typeHierarchy/prepare**, **supertypes**, **subtypes** — type hierarchy (including across included files).

## Configuration

Configure via workspace settings or `initializationOptions` under the `leekscript` key. Example:

```json
{
  "leekscript": {
    "loadStdlibSignatures": true,
    "signatureFiles": [],
    "inlayHints": {
      "enabled": true,
      "scopeEnd": true
    },
    "trace": false
  }
}
```

| Option | Description | Default |
|--------|-------------|---------|
| `loadStdlibSignatures` | Load embedded stdlib `.sig` files (constants and functions). | `true` |
| `signatureFiles` | Additional `.sig` file paths for API signatures. | `[]` |
| `inlayHints.enabled` | Show inlay hints (e.g. variable types, parameter names). | `true` |
| `inlayHints.scopeEnd` | Show scope-end labels (e.g. `// end Cell`). | `true` |
| `trace` | Send verbose (LOG) messages for each request to the LSP output. | `false` |
