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

- **textDocument/didOpen**, **didChange**, **didClose** — incremental text sync; parses on open/change for semantic tokens.
- **textDocument/semanticTokens/full** — semantic tokens for the whole document (syntax highlighting).
- **textDocument/semanticTokens/range** — semantic tokens for a range (e.g. visible region).

## Configuration

Configure via workspace settings or `initializationOptions` under the `leekscript` key. Example:

```json
{
  "leekscript": {
    "trace": false
  }
}
```

| Option | Description | Default |
|--------|-------------|---------|
| `trace` | Send verbose (LOG) messages for each request to the LSP output. | `false` |

## Development

From the **repository root**:

```bash
cargo build -p leekscript-lsp
cargo test -p leekscript-lsp
cargo fmt --all -- --check
cargo clippy --workspace --all-features -- -D warnings
```

See the root [CONTRIBUTING.md](../../CONTRIBUTING.md) and [cursor.md](../../cursor.md) for conventions and workflow.
