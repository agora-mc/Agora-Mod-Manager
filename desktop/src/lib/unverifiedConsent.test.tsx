import { describe, expect, it, vi, beforeEach } from 'vitest';

const setSetting = vi.fn();
vi.mock('./tauri', () => ({
  setSetting: (...args: unknown[]) => setSetting(...args),
  formatError: (e: unknown) => (e as { message: string }).message,
  hasErrorCode: (e: unknown, code: string) => (e as { code?: string }).code === code,
}));

import { LOW_SECURITY_CODE, withUnverifiedConsent } from './unverifiedConsent';

const lowSecurity = { code: LOW_SECURITY_CODE, message: 'This install includes a file with no integrity information' };

describe('withUnverifiedConsent', () => {
  beforeEach(() => setSetting.mockReset());

  it('runs once without asking when core does not ask', async () => {
    const confirm = vi.fn();
    const run = vi.fn().mockResolvedValue('done');
    await expect(withUnverifiedConsent(confirm, run)).resolves.toBe('done');
    expect(run).toHaveBeenCalledWith(false);
    expect(confirm).not.toHaveBeenCalled();
  });

  it('asks, then runs again with the answer', async () => {
    const confirm = vi.fn().mockResolvedValue(true);
    const run = vi.fn().mockRejectedValueOnce(lowSecurity).mockResolvedValueOnce('done');
    await expect(withUnverifiedConsent(confirm, run)).resolves.toBe('done');
    expect(run.mock.calls).toEqual([[false], [true]]);
    expect(confirm).toHaveBeenCalledWith(expect.objectContaining({ confirmLabel: 'Install anyway' }));
    expect(setSetting).not.toHaveBeenCalled();
  });

  it('installs nothing when the user declines', async () => {
    const confirm = vi.fn().mockResolvedValue(false);
    const run = vi.fn().mockRejectedValueOnce(lowSecurity);
    await expect(withUnverifiedConsent(confirm, run)).resolves.toBeNull();
    expect(run).toHaveBeenCalledTimes(1);
  });

  it('passes other errors through untouched', async () => {
    const confirm = vi.fn();
    const other = { code: 'ERR_NETWORK', message: 'offline' };
    await expect(withUnverifiedConsent(confirm, vi.fn().mockRejectedValue(other))).rejects.toBe(other);
    expect(confirm).not.toHaveBeenCalled();
  });
});
