import { test, expect } from '@playwright/test';

/**
 * "Share mod files between instances" is the one setting that can rewrite files an instance
 * already has. The toggle must explain both modes, and converting must be offered with the count
 * and space it involves rather than done quietly.
 */
test('the sharing toggle offers and runs the one-time conversion', async ({ page }) => {
  await page.addInitScript(() => {
    const calls: Array<{ command: string; args: Record<string, unknown> }> = [];
    Object.assign(window as unknown as Record<string, unknown>, {
      __sharingCalls: calls,
      __TAURI_INTERNALS__: {
        transformCallback() { return 1; },
        unregisterCallback() {},
        invoke(command: string, args: Record<string, unknown> = {}) {
          if (command.startsWith('plugin:')) return Promise.resolve(1);
          calls.push({ command, args });
          if (command === 'get_setting') return Promise.resolve(args.key === 'onboarding_complete');
          if (command === 'get_windows_accent_color') return Promise.resolve(null);
          if (command === 'list_instances') return Promise.resolve([]);
          if (command === 'list_snapshots') return Promise.resolve([]);
          if (command === 'get_registry_status') {
            return Promise.resolve({ has_cached_db: false, checked: true, message: 'Missing' });
          }
          if (command === 'get_content_sharing') return Promise.resolve('link');
          if (command === 'preview_content_sharing_conversion') {
            return Promise.resolve({
              instances: 2,
              summary: {
                filesConverted: 3,
                filesAlready: 1,
                bytes: 3 * 1024 * 1024,
                skipped: [],
              },
              warnings: [],
            });
          }
          if (command === 'set_content_sharing') {
            return Promise.resolve({
              instances: 2,
              summary: {
                filesConverted: 3,
                filesAlready: 1,
                bytes: 3 * 1024 * 1024,
                skipped: [],
              },
              failures: [],
              warnings: [],
            });
          }
          return Promise.resolve(null);
        },
      },
      __TAURI_EVENT_PLUGIN_INTERNALS__: { unregisterListener() {} },
    });
  });

  await page.goto('/');
  await page.getByRole('button', { name: 'Settings', exact: true }).click();
  await page.getByRole('navigation', { name: 'Settings sections' })
    .getByRole('tab', { name: 'General', exact: true }).click();
  await page.getByRole('tablist', { name: 'General pages' })
    .getByRole('tab', { name: 'Storage', exact: true }).click();

  const toggle = page.getByRole('switch', { name: 'Share mod files between instances' });
  await expect(toggle).toBeChecked();
  await expect(page.getByText(/saves space, but a mod file cannot be edited in place/)).toBeVisible();
  await expect(page.getByText(/each instance keeps its own editable copy and uses more space/)).toBeVisible();

  await toggle.click();
  await expect(page.getByText('Also convert my existing instances?')).toBeVisible();
  const body = page.getByText(/can be converted/);
  await expect(body).toContainText('3 files');
  await expect(body).toContainText('2 instances');
  await expect(body).toContainText('uses about 3.0 MB');

  await page.getByRole('button', { name: 'Convert', exact: true }).click();
  await expect(page.getByText('Separate copies (off)')).toBeVisible();

  const call = await page.evaluate(() => {
    const calls = (window as unknown as { __sharingCalls: Array<{ command: string; args: Record<string, unknown> }> }).__sharingCalls;
    return calls.find((entry) => entry.command === 'set_content_sharing') ?? null;
  });
  expect(call?.args).toEqual({ share: false, convert: true });
});
