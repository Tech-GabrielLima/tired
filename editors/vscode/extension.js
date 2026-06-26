// Minimal VS Code client: launches `hale lsp` and wires it up as a language server.
const { workspace } = require("vscode");
const { LanguageClient, TransportKind } = require("vscode-languageclient/node");

let client;

function activate(context) {
  const serverPath = workspace.getConfiguration("hale").get("serverPath", "hale");

  // The server is the `hale` binary in stdio LSP mode.
  const serverOptions = {
    run: { command: serverPath, args: ["lsp"], transport: TransportKind.stdio },
    debug: { command: serverPath, args: ["lsp"], transport: TransportKind.stdio },
  };

  const clientOptions = {
    documentSelector: [{ scheme: "file", language: "hale" }],
    synchronize: {
      fileEvents: workspace.createFileSystemWatcher("**/*.hale"),
    },
  };

  client = new LanguageClient("hale", "hale Language Server", serverOptions, clientOptions);
  client.start();
}

function deactivate() {
  return client ? client.stop() : undefined;
}

module.exports = { activate, deactivate };
