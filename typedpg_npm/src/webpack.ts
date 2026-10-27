// Moved to `@cubos/typedpg-cli/webpack` in 0.6.0; this subpath stays so the
// loader says where it went (see moved.ts). A loader's module is imported
// and its default export called, so the throw is there: webpack reports the
// move at the rule that used it.

import { moved } from "./moved.js";

export default function typedpgLoader(): never {
  throw moved("webpack");
}
