// The injection grammars, tokenized by VS Code's own TextMate engine: the
// SQL of a `sql(…)` call (TypeScript) or a `sql!(…)` (Rust) must stay
// embedded up to the string's real end. The host grammars are stubs — the injection is what is tested:
// `source.sql` knows only quoted identifiers and strings, the constructs a
// string's escaped quote could open or close.

const assert = require("node:assert/strict");
const { readFileSync } = require("node:fs");
const { join } = require("node:path");
const { test } = require("node:test");

const oniguruma = require("vscode-oniguruma");
const textmate = require("vscode-textmate");

const STUBS = {
  "source.ts": { scopeName: "source.ts", patterns: [] },
  "source.rust": { scopeName: "source.rust", patterns: [] },
  "source.sql": {
    scopeName: "source.sql",
    patterns: [
      { name: "string.quoted.double.sql", begin: '"', end: '"' },
      { name: "string.quoted.single.sql", begin: "'", end: "'" },
    ],
  },
};

async function registry() {
  const wasm = readFileSync(require.resolve("vscode-oniguruma/release/onig.wasm"));
  await oniguruma.loadWASM(wasm.buffer.slice(wasm.byteOffset, wasm.byteOffset + wasm.byteLength));
  return new textmate.Registry({
    onigLib: Promise.resolve({
      createOnigScanner: (patterns) => new oniguruma.OnigScanner(patterns),
      createOnigString: (s) => new oniguruma.OnigString(s),
    }),
    loadGrammar: async (scopeName) => {
      const file = { "inline.sql.typedpg.ts": "typedpg-ts", "inline.sql.typedpg": "typedpg" }[scopeName];
      if (file) {
        const path = join(__dirname, "syntaxes", `${file}.tmLanguage.json`);
        return textmate.parseRawGrammar(readFileSync(path, "utf8"), path);
      }
      const stub = STUBS[scopeName];
      return stub ? textmate.parseRawGrammar(JSON.stringify(stub), `${scopeName}.json`) : null;
    },
    getInjections: (scopeName) =>
      ({ "source.ts": ["inline.sql.typedpg.ts"], "source.rust": ["inline.sql.typedpg"] })[scopeName] ?? [],
  });
}

/** Each token of `line` (in `host`) with whether it is embedded SQL. */
async function tokens(line, host = "source.ts") {
  const grammar = await (await registry()).loadGrammar(host);
  const { tokens } = grammar.tokenizeLine(line, textmate.INITIAL);
  return tokens.map((t) => ({
    text: line.slice(t.startIndex, t.endIndex),
    sql: t.scopes.includes("meta.embedded.sql"),
  }));
}

/** Whether every character of `part` in `line` is embedded SQL. */
async function embedded(line, part, host = "source.ts") {
  const start = line.indexOf(part);
  let at = 0;
  for (const token of await tokens(line, host)) {
    const end = at + token.text.length;
    if (end > start && at < start + part.length && !token.sql) return false;
    at = end;
  }
  return true;
}

for (const [quote, line] of [
  ['"', String.raw`sql("SELECT \"name\" FROM users WHERE id = $id")`],
  ["'", String.raw`sql('SELECT name FROM users WHERE kind = \'a\' AND id = $id')`],
  ["`", "sql(`SELECT name AS \\`x\\` FROM users WHERE id = $id`)"],
]) {
  test(`an escaped ${quote} stays inside the SQL`, async () => {
    // Everything between the string's own quotes.
    const contents = line.slice("sql(".length + 1, -2);
    assert.ok(await embedded(line, contents), JSON.stringify(await tokens(line)));
  });
}

for (const [what, line, contents] of [
  ["an escaped quote", String.raw`sql!(pool, "SELECT \"name\" FROM users WHERE id = $id")`, String.raw`SELECT \"name\" FROM users WHERE id = $id`],
  ["a raw string's quote", 'sql!(pool, r#"SELECT "name" FROM users WHERE id = $id"#)', 'SELECT "name" FROM users WHERE id = $id'],
  ["a raw string's quote and #", 'sql!(pool, r##"SELECT "a"# FROM users WHERE id = $id"##)', 'SELECT "a"# FROM users WHERE id = $id'],
]) {
  test(`Rust: ${what} stays inside the SQL`, async () => {
    assert.ok(await embedded(line, contents, "source.rust"), JSON.stringify(await tokens(line, "source.rust")));
  });
}

test("Rust: a parameter's ! annotation is part of it", async () => {
  const line = 'sql!(pool, "UPDATE users SET age = $age! WHERE id = $id")';
  const grammar = await (await registry()).loadGrammar("source.rust");
  const { tokens } = grammar.tokenizeLine(line, textmate.INITIAL);
  const param = tokens.find((t) => t.scopes.includes("variable.parameter.typedpg"));
  assert.equal(line.slice(param.startIndex, param.endIndex), "$age!");
});
