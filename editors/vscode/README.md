# hale — VS Code extension

Syntax highlighting and a language client for [hale](../../). The client launches the
compiler's own language server (`hale lsp`), so you get the **exact same diagnostics** as
the CLI — live, as you type — plus completion and hover.

## What you get

- Syntax highlighting for `.hale` files (TextMate grammar).
- **Live diagnostics**: unknown endpoints, "did you mean?", unhandled `Result`, non-exhaustive
  `match`, dead/duplicate requests — underlined in the editor.
- Completion (keywords + declared endpoints/flows/types) and hover.

## Install (development)

```bash
# 1. build the compiler so the `hale` binary is on your PATH
cargo install --path ../../crates/hale-cli      # provides `hale`

# 2. install the client deps and open the extension in VS Code
cd editors/vscode
npm install
code .                                           # then press F5 ("Run Extension")
```

Open any `.hale` file in the launched Extension Development Host. If `hale` is not on your
`PATH`, set `hale.serverPath` in settings to its absolute path.

## Package

```bash
npm install -g @vscode/vsce
vsce package        # produces hale-lang-0.1.0.vsix
```

> Note: this extension is a thin client over `hale lsp` (which is fully tested in the Rust
> workspace). The packaging/run step requires VS Code and is not part of the Rust test suite.
