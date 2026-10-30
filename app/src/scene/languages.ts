import { Color, LinearSRGBColorSpace } from "three";

// Gem tints by main language, from the mockup. The mockup ran three.js without colour
// management, so its hex values went straight into the shaders as linear numbers; reading
// them as linear here keeps the same look.

const LANG: Record<string, number> = {
  TypeScript: 0x3178c6,
  Python: 0xf4c542,
  Rust: 0xe0763a,
  Go: 0x00add8,
  C: 0x9fb2c8,
  Swift: 0xf05138,
  Kotlin: 0xa97bff,
  JavaScript: 0xf1e05a,
  "C#": 0x9b4fd6,
  Shell: 0x89e051,
};

/** For repos with no detected language or one outside the table. */
const NEUTRAL = 0x9a9ea6;

export function languageColor(language: string | null): Color {
  const hex = language && Object.hasOwn(LANG, language) ? LANG[language] : NEUTRAL;
  return new Color().setHex(hex, LinearSRGBColorSpace);
}

/** Read a hex colour as linear numbers, as the mockup did. */
export const linearHex = (hex: number) => new Color().setHex(hex, LinearSRGBColorSpace);
