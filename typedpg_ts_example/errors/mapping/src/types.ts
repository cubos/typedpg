// Types the config maps PostgreSQL types to. Those that fit what their PG
// type is read as compile; the generated module reports the others, as
// ../expected.tsc pins.

/** Fits `readonly number[]`. */
export type Embedding = number[];
/** Doesn't: a halfvec is read as numbers, not as its text. */
export type EmbeddingText = string;
/** A string enum fits `string`. */
export enum Mood {
  Happy = "happy",
  Sad = "sad",
}
/** Fits: its fields are narrower than the composite's. */
export interface Address {
  street: string;
  number: number;
}
/** Doesn't: `number` is missing. */
export interface ShortAddress {
  street: string;
}
/** Doesn't: an int8 is read as a bigint. */
export type UserId = number & { readonly __brand: "UserId" };
/** Any type fits JSON. */
export interface Prefs {
  theme: "dark" | "light";
}
/** Doesn't: a timestamptz is read as a Date. */
export type Instant = string;
export type JsonPath = string;
