/**
 * The launch/kill error branches in useProcessController must react to the
 * codes the Rust backend actually emits, never to a made-up code or to a
 * substring of a message that any unrelated failure might contain.
 */
import { describe, expect, it } from 'vitest';
import { isStaleProcessError, releasesHealthDialog } from './useProcessController';

describe('launch error classification', () => {
  it('releases the health dialog only on real Microsoft sign-in codes', () => {
    expect(releasesHealthDialog('ERR_MSA_AUTH_REQUIRED')).toBe(true);
    // ERR_MSA_LOGIN_EXPIRED is the device-code sign-in timing out; a launch never reports it.
    expect(releasesHealthDialog('ERR_MSA_LOGIN_EXPIRED')).toBe(false);
    // There is no ERR_MSA_AUTH_EXPIRED in the backend; relying on it would
    // leave a stale health dialog on a genuine expired-sign-in failure.
    expect(releasesHealthDialog('ERR_MSA_AUTH_EXPIRED')).toBe(false);
    expect(releasesHealthDialog('ERR_LAUNCH_FAILED')).toBe(false);
    expect(releasesHealthDialog(null)).toBe(false);
  });

  it('treats only the structured stale-process code as a stale kill', () => {
    const stale = {
      code: 'ERR_PROCESS_STALE',
      message: 'Process identity changed',
      details: null,
      suggested_action: null,
    };
    expect(isStaleProcessError(stale)).toBe(true);

    // A message that merely says "stale" is not the backend's stale-process
    // code: a failed launch whose message mentions a stale cache must not
    // silently reset the controller to idle.
    expect(isStaleProcessError(new Error('the cached profile is stale'))).toBe(false);
    expect(isStaleProcessError({
      code: 'ERR_LAUNCH_FAILED',
      message: 'stale process detected during kill',
    })).toBe(false);
  });
});
