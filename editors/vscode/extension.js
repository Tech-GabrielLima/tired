// Minimal VS Code client: launches `tired lsp` and wires it up as a language server.
const { workspace } = require("vscode");
const { LanguageClient, TransportKind } = require("vscode-languageclient/node");

let client;

function activate(context) {
  const serverPath = workspace.getConfiguration("tired").get("serverPath", "tired");

  // The server is the `tired` binary in stdio LSP mode.
  const serverOptions = {
    run: { command: serverPath, args: ["lsp"], transport: TransportKind.stdio },
    debug: { command: serverPath, args: ["lsp"], transport: TransportKind.stdio },
  };

  const clientOptions = {
    documentSelector: [{ scheme: "file", language: "tired" }],
    synchronize: {
      fileEvents: workspace.createFileSystemWatcher("**/*.tired"),
    },
  };

  client = new LanguageClient("tired", "TIRED Language Server", serverOptions, clientOptions);
  client.start();
}

function deactivate() {
  return client ? client.stop() : undefined;
}

module.exports = { activate, deactivate };
