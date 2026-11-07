import { beforeEach, describe, expect, it, vi } from 'vitest';

const invokeMock = vi.hoisted(() => vi.fn());
vi.mock('./api', async (importOriginal) => {
  const actual = await importOriginal<typeof import('./api')>();
  return {
    ...actual,
    launchGameInstance: invokeMock,
  };
});

import { clearGameLaunches, gameLaunchState, startGameLaunch } from './launchController';
import type { LaunchReply } from './api';

function reply(overrides: Partial<LaunchReply>): LaunchReply {
  return {
    status: 'launched',
    pid: 1,
    deployment: null,
    notice: null,
    findings: [],
    reason: null,
    next: null,
    beforeLaunch: [],
    beforeLaunchFailure: null,
    ...overrides,
  };
}

describe('before-launch progress', () => {
  beforeEach(() => {
    invokeMock.mockReset();
    clearGameLaunches();
  });

  it('runs several run-mode tools without guessing which one is running', async () => {
    let finish: (value: LaunchReply) => void = () => undefined;
    const second = new Promise<LaunchReply>((resolve) => { finish = resolve; });
    invokeMock
      .mockResolvedValueOnce(reply({
        status: 'before_launch_needed',
        pid: null,
        beforeLaunch: [
          { tool: 'a', name: 'Deploy', mode: 'run', reason: 'it has never run here' },
          { tool: 'b', name: 'Pack', mode: 'run', reason: 'its output is out of date' },
        ],
      }))
      .mockReturnValueOnce(second);

    const done = startGameLaunch('inst-1');
    await vi.waitFor(() => expect(invokeMock).toHaveBeenCalledTimes(2));
    expect(gameLaunchState('inst-1').runningTools).toBe(2);
    expect(gameLaunchState('inst-1').runningTool).toBeNull();

    finish(reply({}));
    await done;
    expect(invokeMock).toHaveBeenLastCalledWith('inst-1', expect.objectContaining({
      beforeLaunch: { a: 'run', b: 'run' },
    }));
  });

  it('names the one run-mode tool it is running', async () => {
    let finish: (value: LaunchReply) => void = () => undefined;
    const second = new Promise<LaunchReply>((resolve) => { finish = resolve; });
    invokeMock
      .mockResolvedValueOnce(reply({
        status: 'before_launch_needed',
        pid: null,
        beforeLaunch: [{ tool: 'a', name: 'Deploy', mode: 'run', reason: 'it has never run here' }],
      }))
      .mockReturnValueOnce(second);

    const done = startGameLaunch('inst-1');
    await vi.waitFor(() => expect(invokeMock).toHaveBeenCalledTimes(2));
    expect(gameLaunchState('inst-1').runningTool).toBe('Deploy');
    finish(reply({}));
    await done;
  });
});
