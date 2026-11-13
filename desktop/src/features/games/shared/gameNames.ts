/**
 * A game's name short enough for the sidebar switcher and the Settings rail.
 *
 * Store and discovery names are long ("The Elder Scrolls V: Skyrim Special Edition") and were cut
 * to "The Elder Scr…" in the switcher. The short name drops a series prefix before a colon and
 * writes the usual edition names the way players do ("Skyrim SE"). Where a name is still long,
 * the full name stays available as the accessible name and tooltip.
 */

const EDITION_ABBREVIATIONS: [RegExp, string][] = [
  [/\bSpecial Edition\b/i, 'SE'],
  [/\bAnniversary Edition\b/i, 'AE'],
  [/\bGame of the Year Edition\b/i, 'GOTY'],
  [/\bLegendary Edition\b/i, 'LE'],
  [/\bVirtual Reality\b/i, 'VR'],
];

const ABBREVIATIONS = new Set(EDITION_ABBREVIATIONS.map(([, abbreviation]) => abbreviation));

export function gameShortName(name: string): string {
  const full = name.trim();
  if (!full) return name;
  let short = full;
  for (const [pattern, abbreviation] of EDITION_ABBREVIATIONS) {
    short = short.replace(pattern, abbreviation);
  }
  // "The Elder Scrolls V: Skyrim SE" -> "Skyrim SE", but "Fallout 4: GOTY" -> "Fallout 4 GOTY":
  // the part after the colon is the game unless it is only the edition. A name that is only a
  // prefix and a colon keeps its full form.
  const colon = short.indexOf(':');
  if (colon > 0 && colon < short.length - 1) {
    const before = short.slice(0, colon).trim();
    const after = short.slice(colon + 1).trim();
    short = ABBREVIATIONS.has(after) ? `${before} ${after}` : after;
  }
  return short.replace(/\s+/g, ' ').trim() || full;
}
