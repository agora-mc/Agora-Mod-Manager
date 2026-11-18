import { describe, expect, it } from 'vitest';
import { applyLaunchProgressEvent, STARTING_PROGRESS } from './launchProgress';

describe('applyLaunchProgressEvent', () => {
  it('records the offline note and keeps it through later stages', () => {
    const notice =
      'Playing offline: online servers and Realms will not accept this session. Lockdown Mode is on, so Agora is using no network.';
    const afterOffline = applyLaunchProgressEvent(
      STARTING_PROGRESS(1),
      { instance_id: 'inst', phase: 'offline', message: notice },
      2,
    );
    expect(afterOffline?.notice).toBe(notice);

    const afterRunning = applyLaunchProgressEvent(
      afterOffline,
      { instance_id: 'inst', phase: 'running' },
      3,
    );
    expect(afterRunning?.stage).toBe('loading');
    expect(afterRunning?.notice).toBe(notice);
  });
});
