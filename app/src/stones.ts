// Which stone each language is drawn as (banknote spec §3). Kept free of three.js so the
// overlays can name a stone without pulling the scene chunk in.

export interface Stone {
  /** Shown in the focus label, e.g. "sapphire". */
  name: string;
  /** sRGB colour of the stone's wash. */
  hex: number;
}

const STONES: Record<string, Stone> = {
  TypeScript: { name: "sapphire", hex: 0x3d63c9 },
  Python: { name: "citrine", hex: 0xe0a93a },
  Rust: { name: "mandarin garnet", hex: 0xd9672e },
  Go: { name: "aquamarine", hex: 0x8fd3d8 },
  C: { name: "white topaz", hex: 0xe8ecf0 },
  Kotlin: { name: "amethyst", hex: 0x8a62c8 },
  Swift: { name: "rhodolite", hex: 0xc23a5a },
  Shell: { name: "peridot", hex: 0x9cc650 },
  JavaScript: { name: "yellow sapphire", hex: 0xe3c34a },
  "C#": { name: "tanzanite", hex: 0x5a54c4 },
};

/** For repos with no detected language or one outside the table. */
export const QUARTZ: Stone = { name: "quartz", hex: 0xb9b4a8 };

export function stoneFor(language: string | null): Stone {
  return language && Object.hasOwn(STONES, language) ? STONES[language] : QUARTZ;
}
