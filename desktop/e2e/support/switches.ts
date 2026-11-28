import { expect, type Locator } from '@playwright/test';

/**
 * Put a role="switch" control into `on` (checked) or off, the way `.check()` /
 * `.uncheck()` do for a checkbox: only click when the state differs, so the
 * call is idempotent.
 */
export async function setSwitch(control: Locator, on: boolean): Promise<void> {
  const want = String(on);
  if ((await control.getAttribute('aria-checked')) !== want) {
    await control.click();
  }
  await expect(control).toHaveAttribute('aria-checked', want);
}
