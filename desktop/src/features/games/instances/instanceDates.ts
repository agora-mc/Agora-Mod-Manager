/**
 * Dates on an instance's card and header, in the form Minecraft's pages use: "Oct 30, 2026,
 * 12:30 PM" for when it was played, "Oct 1, 2026" for when it was made. No seconds.
 */

/** A backend time (SQLite UTC, no zone), or null when it does not parse. */
export function parseStoredTime(value: string | null | undefined): Date | null {
  if (value === null || value === undefined || value.trim() === '') return null;
  const parsed = new Date(`${value.trim().replace(' ', 'T')}Z`);
  return Number.isNaN(parsed.getTime()) ? null : parsed;
}

/** `Oct 30, 2026, 12:30 PM`; a value that does not parse is shown as it came. */
export function playedWhen(value: string): string {
  const parsed = parseStoredTime(value);
  if (parsed === null) return value;
  return new Intl.DateTimeFormat(undefined, { dateStyle: 'medium', timeStyle: 'short' }).format(parsed);
}

/** `Oct 1, 2026`, or null when there is no date or it does not parse. */
export function createdOn(value: string | null | undefined): string | null {
  const parsed = parseStoredTime(value);
  if (parsed === null) return null;
  return new Intl.DateTimeFormat(undefined, { month: 'short', day: 'numeric', year: 'numeric' }).format(parsed);
}
