/** The name a store id shows as ("steam" -> "Steam"). An unknown id is shown as it is. */

const STORE_NAMES: Record<string, string> = {
  steam: 'Steam',
  gog: 'GOG',
  epic: 'Epic Games Store',
  'microsoft-store': 'Microsoft Store',
  direct: 'a folder you added',
};

export function storeName(store: string): string {
  return STORE_NAMES[store] ?? store;
}
