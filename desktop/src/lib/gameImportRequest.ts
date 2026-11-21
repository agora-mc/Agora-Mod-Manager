/**
 * Opening a game's import dialog from elsewhere: an old link to the retired Import page, today.
 *
 * The dialog belongs to the game's Instances page, so a request is held here until that page
 * mounts (or is already open and hears the event) and takes it. One request opens the dialog once.
 */

export const GAME_IMPORT_REQUEST_EVENT = 'agora-game-import-request';

let pending: string | null = null;

/** Ask the Instances page of `gameId` to open its import dialog. */
export function requestGameImport(gameId: string): void {
  pending = gameId;
  window.dispatchEvent(new CustomEvent(GAME_IMPORT_REQUEST_EVENT));
}

/** Whether a request for `gameId` is waiting, without taking it (a mount's first render). */
export function peekGameImportRequest(gameId: string): boolean {
  return pending === gameId;
}

/** Take the request for `gameId`, if there is one. A request for another game is left alone. */
export function takeGameImportRequest(gameId: string): boolean {
  if (pending !== gameId) return false;
  pending = null;
  return true;
}
