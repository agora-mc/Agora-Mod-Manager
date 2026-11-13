import { test, expect, type Page } from '@playwright/test';

/**
 * The Crash Doctor is the engine answer screen for Minecraft plus its test-it flow. Every command
 * the page calls is answered from the table below; anything else returns null and would crash a
 * page that reads it. The names are invented, as are the logs and files.
 */

async function installMock(page: Page) {
  await page.addInitScript(() => {
    const callbacks = new Map<number, (...a: unknown[]) => void>();
    let callbackId = 0;
    const eventListeners = new Map<string, number>();
    const commandCalls: Record<string, number> = {};
    const lastArgs: Record<string, Record<string, unknown>> = {};

    const instanceRow = {
      instance_id: 'crash-test-instance',
      name: 'Crash Test',
      loader: 'fabric',
      loader_version: '0.16',
      minecraft_version: '1.21',
      is_locked: false,
      last_launched_at: null,
    };

    const internals = {
      transformCallback(cb: (...a: unknown[]) => void) {
        const id = ++callbackId;
        callbacks.set(id, cb);
        return id;
      },
      unregisterCallback(id: number) { callbacks.delete(id); },
      invoke(command: string, args: Record<string, unknown> = {}) {
        commandCalls[command] = (commandCalls[command] ?? 0) + 1;
        lastArgs[command] = { ...args };

        if (command === 'get_setting') {
          const key = args.key as string;
          if (key === 'onboarding_complete') return Promise.resolve(true);
          if (key === 'launch_mode') return Promise.resolve('direct');
          if (key === 'modrinth_enabled') return Promise.resolve(true);
          if (key === 'last_home_visit') return Promise.resolve(null);
          return Promise.resolve(false);
        }
        if (command === 'set_setting') return Promise.resolve(null);
        if (command === 'get_windows_accent_color') return Promise.resolve(null);
        if (command === 'get_registry_status') return Promise.resolve({ has_cached_db: true, cached_tag: 'test', cached_schema_version: 5, latest_tag: 'test', update_available: false, checked: true, message: 'Catalog ready.' });
        if (command === 'check_registry_update') return Promise.resolve(null);
        if (command === 'list_categories') return Promise.resolve([]);
        if (command === 'list_manifest_loaders') return Promise.resolve([]);
        if (command === 'list_manifest_mc_versions') return Promise.resolve([]);
        if (command === 'for_you_items') return Promise.resolve([]);
        if (command === 'browse_search') return Promise.resolve({ items: [], total: 0, page: 0, hasMore: false });
        if (command === 'query_launch_state') return Promise.resolve([]);
        if (command === 'check_all_instance_health') return Promise.resolve([]);

        if (command === 'plugin:event|listen') {
          eventListeners.set(args.event as string, args.handler as number);
          return Promise.resolve(1);
        }
        if (command === 'plugin:event|unlisten') return Promise.resolve(1);
        if (command.startsWith('plugin:event|')) return Promise.resolve(1);
        if (command.startsWith('plugin:sql|')) return Promise.resolve(null);
        if (command.startsWith('plugin:dialog|')) return Promise.resolve(null);

        if (command === 'list_instances') return Promise.resolve([instanceRow]);
        if (command === 'check_instance_updates') return Promise.resolve([]);
        if (command === 'get_lkg_marker') return Promise.resolve(null);
        if (command === 'list_snapshots') return Promise.resolve([]);
        if (command === 'detect_drift') return Promise.resolve({ added: [], removed: [], modified: [] });

        if (command === 'list_crash_answers') return Promise.resolve(listReplyFromScript());
        if (command === 'get_crash_answer') return Promise.resolve(detailReplyFromScript());
        if (command === 'analyze_crash_text') {
          return Promise.resolve({ status: 'analyzed', answer: pastedAnswerFromScript(), bucket: 'pasted-bucket' });
        }
        if (command === 'analyze_crash_file') {
          return Promise.resolve({ status: 'analyzed', answer: pastedAnswerFromScript(), bucket: 'file-bucket' });
        }
        if (command === 'reveal_crash_artifact') return Promise.resolve(null);
        if (command === 'read_crash_artifact') return Promise.resolve('invented crash log');

        if (command === 'investigate_crash') {
          return Promise.resolve({
            suspects: [
              { mod_id: 'item:mods/example.jar:abc', filename: 'example.jar', total_score: 9, breakdown: {}, is_dependent_of: null, under_review: false },
              { mod_id: 'item:mods/second.jar:def', filename: 'second.jar', total_score: 4, breakdown: {}, is_dependent_of: null, under_review: false },
            ],
            suggested_action: {
              kind: 'GuidedDisable',
              next_suspect: { mod_id: 'item:mods/example.jar:abc', filename: 'example.jar', total_score: 9, breakdown: {}, is_dependent_of: null, under_review: false },
            },
            ruled_out: [],
          });
        }
        if (command === 'resolve_crash_suspect_file') return Promise.resolve('example.jar');
        if (command === 'get_disable_plan') return Promise.resolve({ dependents: [] });
        if (command === 'disable_mod_for_test') return Promise.resolve(null);
        if (command === 'enable_mod_for_test') return Promise.resolve(null);
        if (command === 'confirm_crash_fix') return Promise.resolve(null);
        if (command === 'report_still_crashing') {
          return Promise.resolve({
            suspects: [
              { mod_id: 'item:mods/second.jar:def', filename: 'second.jar', total_score: 4, breakdown: {}, is_dependent_of: null, under_review: false },
            ],
            suggested_action: {
              kind: 'GuidedDisable',
              next_suspect: { mod_id: 'item:mods/second.jar:def', filename: 'second.jar', total_score: 4, breakdown: {}, is_dependent_of: null, under_review: false },
            },
            ruled_out: ['item:mods/example.jar:abc'],
          });
        }

        if (command === 'create_snapshot') {
          return Promise.resolve({ id: 'snap-1', label: 'Recovery snapshot', created_at: '2026-11-12T21:00:00Z', file_count: 1, size_estimate: 1024, is_lkg: false, is_current_lkg: false, is_pre_restore: false });
        }
        if (command === 'delete_snapshot') return Promise.resolve(null);
        if (command === 'restore_snapshot') return Promise.resolve(null);

        if (command === 'check_instance_health') {
          return Promise.resolve({ score: 'green', blockers: [], warnings: [], recommendations: [], scan_token: 'scan' });
        }
        if (command === 'launch_instance_direct') return Promise.resolve(4242);
        if (command === 'launch_instance') return Promise.resolve(null);

        if (command === 'export_crash_report') return Promise.resolve('# Minecraft crash report\n## Agora\'s Crash Answer');

        return Promise.resolve(null);
      },
    };

    // The constants above are serialized through the page's own copy; these small builders keep
    // the shapes readable without relying on closures from the test process.
    function listReplyFromScript() {
      return {
        answers: [{
          sessionId: 'session-42',
          instanceId: 'crash-test-instance',
          verdict: { kind: 'crashed', how: 'pid' },
          endedAt: '2026-11-12T21:14:00Z',
          createdAt: '2026-11-12T21:15:00Z',
          status: 'analyzed',
          headline: 'The game crashed, and Example Mod is likely the cause.',
          level: 'likely',
          cause: 'mod',
        }],
        latestVerdict: { kind: 'crashed', how: 'pid' },
        retainedCrashes: 40,
      };
    }

    function detailReplyFromScript() {
      return {
        sessionId: 'session-42',
        instanceId: 'crash-test-instance',
        verdict: { kind: 'crashed', how: 'pid' },
        endedAt: '2026-11-12T21:14:00Z',
        createdAt: '2026-11-12T21:15:00Z',
        status: 'analyzed',
        answer: answerFromScript(),
        bucket: 'bucket-1',
        changes: [],
      };
    }

    function answerFromScript() {
      return {
        contract: 1,
        verdict: { kind: 'crashed', how: 'pid' },
        headline: { en: 'The game crashed, and Example Mod is likely the cause.' },
        level: 'likely',
        cause: 'mod',
        candidates: [
          { owner: { kind: 'item', key: 'mods/example.jar:abc', name: 'Example Mod' }, level: 'likely', score: 9, cause: 'mod', families: ['execution'], sources: [sourceFromScript()], save_risk: 'unknown' },
          { owner: { kind: 'item', key: 'mods/second.jar:def', name: 'Second Mod' }, level: 'possible', score: 4, cause: 'mod', families: ['known_signature'], sources: [sourceFromScript()] },
          { owner: { kind: 'framework', name: 'Invented Engine' }, level: 'possible', score: 3, cause: 'mod', role: 'framework', families: ['execution'], sources: [sourceFromScript()] },
        ],
        findings: [],
        inspections: [],
        could_not_check: [],
        trust_notes: [],
        next: { kind: 'confirm_experiment', keys: ['mods/example.jar:abc'] },
        repairs: [],
        actions: [{ action: { kind: 'open_crash_log' }, source: sourceFromScript() }],
        artifacts: [{ id: 'log-1', name: 'crash-2026-11-12-21-14.log', size: 48_320, privacy: 'personal', retained: { kind: 'whole' } }],
      };
    }

    function sourceFromScript() {
      return {
        plugin_id: 'agora.minecraft',
        plugin_name: 'Minecraft crash rules',
        plugin_version: '0.1.0',
        rule_id: 'invented_rule',
        rule_version: 1,
      };
    }

    function pastedAnswerFromScript() {
      const value = answerFromScript();
      value.headline = { en: 'The pasted log names Example Mod.' };
      return value;
    }

    Object.assign(window as unknown as Record<string, unknown>, {
      __TAURI_INTERNALS__: internals,
      __TAURI_EVENT_PLUGIN_INTERNALS__: { unregisterListener() {} },
      __commandCalls: commandCalls,
      __lastCommandArgs: lastArgs,
      __tauriEventListeners: eventListeners,
      __callbacks: callbacks,
    });
  });
}

async function openDoctor(page: Page) {
  await page.goto('/');
  await page.getByRole('button', { name: 'My Instances' }).click();
  await expect(page.getByRole('button', { name: 'Troubleshoot' })).toBeVisible({ timeout: 10_000 });
  await page.getByRole('button', { name: 'Troubleshoot' }).click();
  await expect(page.getByRole('heading', { name: 'Crash Doctor' })).toBeVisible({ timeout: 5_000 });
}

async function finishSuccessfulLaunch(page: Page) {
  const waiting = page.getByText(/Game is running without/);
  await expect(waiting).toBeVisible({ timeout: 8_000 });
  await page.evaluate(() => {
    const listeners = (window as unknown as { __tauriEventListeners: Map<string, number> }).__tauriEventListeners;
    const callbacks = (window as unknown as { __callbacks: Map<number, (...args: unknown[]) => void> }).__callbacks;
    const handlerId = listeners.get('game-exited');
    const callback = handlerId == null ? undefined : callbacks.get(handlerId);
    callback?.({
      payload: {
        instance_id: 'crash-test-instance',
        exit_code: 0,
        outcome: 'success',
        snapshot_id: 'launch-snapshot',
      },
    });
  });
}

async function calls(page: Page) {
  return page.evaluate(() => (window as unknown as { __commandCalls: Record<string, number> }).__commandCalls);
}

async function lastArgs(page: Page) {
  return page.evaluate(() => (window as unknown as { __lastCommandArgs: Record<string, Record<string, unknown>> }).__lastCommandArgs);
}

test.describe('CrashInvestigator', () => {
  test.beforeEach(async ({ page }) => {
    await installMock(page);
  });

  test('shows the stored engine answer for the newest crashed session', async ({ page }) => {
    await openDoctor(page);
    await expect(page.getByTestId('crash-headline')).toContainText('Example Mod is likely the cause');
    await expect(page.getByTestId('crash-save-warning')).toContainText(
      'Removing a mod partway through a playthrough can damage your save.',
    );
    const suspects = page.getByTestId('crash-suspect');
    await expect(suspects).toHaveCount(3);
    await expect(suspects.first()).toContainText('Example Mod');
    await expect(page.getByText('Invented Engine')).toBeVisible();
  });

  test('a pasted log runs through analyze_crash_text and shows its answer', async ({ page }) => {
    await openDoctor(page);
    await page.getByRole('button', { name: 'Check another crash log' }).click();
    await page.getByPlaceholder('Paste the crash log here').fill('invented pasted crash text');
    await page.getByRole('button', { name: 'Check this text' }).click();
    await expect(page.getByTestId('crash-headline')).toContainText('The pasted log names Example Mod.');
    expect((await calls(page)).analyze_crash_text).toBe(1);
  });

  test('an action button calls the existing handler', async ({ page }) => {
    await openDoctor(page);
    await page.getByRole('button', { name: 'Open crash log' }).click();
    const args = await lastArgs(page);
    expect(args.reveal_crash_artifact).toMatchObject({ sessionId: 'session-42', artifactId: 'log-1' });
  });

  test('Test without it disables the top installable suspect, never a framework', async ({ page }) => {
    await openDoctor(page);
    await page.getByRole('button', { name: 'Test without Example Mod' }).click();
    await finishSuccessfulLaunch(page);

    const recorded = await calls(page);
    expect(recorded.disable_mod_for_test).toBe(1);
    const args = await lastArgs(page);
    expect(args.disable_mod_for_test.filename).toBe('example.jar');
    // The framework candidate is never the one that gets turned off.
    expect(args.disable_mod_for_test.filename).not.toContain('Invented Engine');
  });

  test('a confirmed fix records the crash bucket and the owner key', async ({ page }) => {
    await openDoctor(page);
    await page.getByRole('button', { name: 'Test without Example Mod' }).click();
    await finishSuccessfulLaunch(page);
    await page.getByRole('button', { name: 'Yes, it worked' }).click();

    const args = await lastArgs(page);
    expect(args.confirm_crash_fix).toMatchObject({ bucket: 'bucket-1', ownerKey: 'item:mods/example.jar:abc' });
    await expect(page.getByText(/Got it — keeping/)).toBeVisible({ timeout: 5_000 });
  });

  test('still crashing restores the setup and moves past the ruled-out suspect', async ({ page }) => {
    await openDoctor(page);
    await page.getByRole('button', { name: 'Test without Example Mod' }).click();
    await finishSuccessfulLaunch(page);
    await page.getByRole('button', { name: 'No, still crashing' }).click();

    await expect(page.getByText(/It still crashed without/)).toBeVisible({ timeout: 8_000 });
    await expect(page.getByTestId('crash-suspect').filter({ hasText: 'Second Mod' })).toBeVisible();
    await expect(page.getByTestId('crash-suspect').filter({ hasText: 'Example Mod' })).toHaveCount(0);
    expect((await calls(page)).report_still_crashing).toBe(1);
    expect((await calls(page)).restore_snapshot).toBeGreaterThanOrEqual(1);
  });
});
