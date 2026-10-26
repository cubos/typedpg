// A query of broken.sql with an error fails to type-check where it is used,
// with the error; one without is typed as any other.
import pg from "pg";
import { pgtypedParams, positional, typo } from "./broken.ts";

const pool = new pg.Pool();

export async function cases() {
  // @ts-expect-error: column "nmae" does not exist
  await typo.fetchAll(pool);
  // @ts-expect-error: positional placeholder `$1` is not supported
  await positional.fetchOne(pool, { 1: 1 });
  // @ts-expect-error: `ids` is an int4 array
  await pgtypedParams.fetchAll(pool, { ids: ["1"] });
  return pgtypedParams.fetchAll(pool, { ids: [1] });
}
