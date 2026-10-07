export interface UserPrefs {
  theme: "dark" | "light";
  fontSize?: number;
}

/** The `point2` composite, mapped to a type of our own. */
export interface Point2 {
  x: number | null;
  y: number | null;
}
