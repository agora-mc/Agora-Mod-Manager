import { test, expect, type Page } from '@playwright/test';

async function tokenColor(page: Page, token: string): Promise<string> {
  return page.evaluate((name) => {
    const probe = document.createElement('span');
    probe.style.color = `hsl(var(${name}))`;
    probe.style.position = 'absolute';
    document.body.appendChild(probe);
    const color = getComputedStyle(probe).color;
    probe.remove();
    return color;
  }, token);
}

async function selectStyle(page: Page, label: string) {
  return page.getByLabel(label).evaluate((element) => {
    const style = getComputedStyle(element);
    return {
      appearance: style.appearance,
      webkitAppearance: style.getPropertyValue('-webkit-appearance'),
      backgroundColor: style.backgroundColor,
      color: style.color,
      backgroundImage: style.backgroundImage,
      paddingLeft: Number.parseFloat(style.paddingLeft),
      paddingRight: Number.parseFloat(style.paddingRight),
      colorScheme: getComputedStyle(document.documentElement).colorScheme,
    };
  });
}

test.beforeEach(async ({ page }) => {
  await page.addInitScript(() => {
    Object.assign(window as unknown as Record<string, unknown>, {
      __TAURI_INTERNALS__: {
        transformCallback() { return 1; },
        unregisterCallback() {},
        invoke(command: string, args: Record<string, unknown> = {}) {
          if (command === 'get_setting') return Promise.resolve(args.key === 'onboarding_complete');
          if (command === 'get_windows_accent_color') return Promise.resolve(null);
          if (command.startsWith('plugin:event|')) return Promise.resolve(1);
          if (command === 'get_registry_status') return Promise.resolve({ has_cached_db: false, checked: true, message: 'Missing' });
          if (command === 'list_instances' || command === 'list_snapshots' || command === 'for_you_items') return Promise.resolve([]);
          return Promise.resolve(null);
        },
      },
      __TAURI_EVENT_PLUGIN_INTERNALS__: { unregisterListener() {} },
    });
  });
  await page.goto('/');
  await page.getByRole('button', { name: 'Settings', exact: true }).click();
  await page.getByRole('navigation', { name: 'Settings sections' })
    .getByRole('tab', { name: 'Appearance' }).click();
  await page.getByRole('tab', { name: 'Theme', exact: true }).click();
});

test('a native select follows the theme in dark and light mode', async ({ page }) => {
  const dark = await selectStyle(page, 'Appearance preset');
  expect(dark.backgroundColor).toBe(await tokenColor(page, '--background'));
  expect(dark.color).toBe(await tokenColor(page, '--foreground'));
  expect(dark.colorScheme).toBe('dark');
  expect(dark.appearance).toBe('none');
  expect(dark.webkitAppearance).toBe('none');
  expect(dark.backgroundImage).toContain('linear-gradient');
  expect(dark.paddingRight).toBeGreaterThan(dark.paddingLeft);

  await page.getByLabel('Appearance preset').screenshot({ path: 'test-results/gtk-select-dark.png' });

  await page.getByLabel('Appearance preset').selectOption('agora');
  await page.getByLabel('Color mode').selectOption('light');

  const light = await selectStyle(page, 'Appearance preset');
  expect(light.backgroundColor).toBe(await tokenColor(page, '--background'));
  expect(light.color).toBe(await tokenColor(page, '--foreground'));
  expect(light.colorScheme).toBe('light');
  expect(light.appearance).toBe('none');
  expect(light.webkitAppearance).toBe('none');
  expect(light.backgroundImage).toContain('linear-gradient');
  expect(light.paddingRight).toBeGreaterThan(light.paddingLeft);
  expect(light.backgroundColor).not.toBe(dark.backgroundColor);
  expect(light.color).not.toBe(dark.color);
});

test('options and other native controls use the theme palette', async ({ page }) => {
  await expect(page.getByLabel('Appearance preset').locator('option').first()).toHaveCSS(
    'background-color', await tokenColor(page, '--popover'),
  );
  await expect(page.getByLabel('Color mode')).toHaveCSS('border-color', await tokenColor(page, '--input'));
  expect(await page.evaluate(() => getComputedStyle(document.documentElement).colorScheme)).toBe('dark');
});
