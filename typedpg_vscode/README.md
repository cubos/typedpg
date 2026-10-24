# typedpg for VS Code

- **SQL highlighting** inside typedpg's `sql!` macros (Rust) and `sql()` /
  `copyIn()` calls (TypeScript).
- **Errors of `.sql` files** in the editor, at their place in the file,
  through TypeScript 7.1's content mapper (see
  [Queries in .sql files](../typedpg_npm/README.md#the-content-mapper-typescript-71)).
  It needs the TypeScript 7 extension (`TypeScriptTeam.native-preview`) with
  TypeScript 7.1 or later in the workspace, `contentMappers` in the
  project's `tsconfig.json`, and a trusted workspace (TypeScript runs the
  mapper only in one). The extension only tells TypeScript to take `.sql`
  files: VS Code hands its server the documents of the languages it
  registered, and the server learns a project's content mappers only once
  it has opened it.
