// Hands .sql files to TypeScript's language server (the TypeScript 7
// extension, TypeScriptTeam.native-preview): VS Code gives it only the
// documents of the languages it registered, and TypeScript knows a
// project's `contentMappers` only once it opened that project. The
// mapping itself is the project's: `contentMappers` in its tsconfig.json
// names @cubos/typedpg-cli, whose `typedpg serve` TypeScript runs.

const vscode = require("vscode");

const TYPESCRIPT = "TypeScriptTeam.native-preview";

/** @param {vscode.ExtensionContext} context */
async function activate(context) {
  const typescript = vscode.extensions.getExtension(TYPESCRIPT);
  if (!typescript) return;
  const api = await typescript.activate();
  if (typeof api?.registerContentMappers !== "function") return;
  context.subscriptions.push(api.registerContentMappers(context.extension.id, [{ extensions: [".sql"] }]));
}

module.exports = { activate };
