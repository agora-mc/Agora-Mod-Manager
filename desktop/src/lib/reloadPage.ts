/**
 * Reload the app window. Its own module so a test can stand in for it: jsdom
 * does not let `window.location.reload` be replaced.
 */
export function reloadPage(): void {
  window.location.reload();
}
