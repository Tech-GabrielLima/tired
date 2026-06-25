# TIRED — VS Code extension

Syntax highlighting and a language client for [TIRED](../../). The client launches the
compiler's own language server (`tired lsp`), so you get the **exact same diagnostics** as
the CLI — live, as you type — plus completion and hover.

## What you get

- Syntax highlighting for `.tired` files (TextMate grammar).
- **Live diagnostics**: unknown endpoints, "did you mean?", unhandled `Result`, non-exhaustive
  `match`, dead/duplicate requests — underlined in the editor.
- Completion (keywords + declared endpoints/flows/types) and hover.

## Install (development)

```bash
# 1. build the compiler so the `tired` binary is on your PATH
cargo install --path ../../crates/tired-cli      # provides `tired`

# 2. install the client deps and open the extension in VS Code
cd editors/vscode
npm install
code .                                           # then press F5 ("Run Extension")
```

Open any `.tired` file in the launched Extension Development Host. If `tired` is not on your
`PATH`, set `tired.serverPath` in settings to its absolute path.

## Package

```bash
npm install -g @vscode/vsce
vsce package        # produces tired-lang-0.1.0.vsix
```

> Note: this extension is a thin client over `tired lsp` (which is fully tested in the Rust
> workspace). The packaging/run step requires VS Code and is not part of the Rust test suite.
