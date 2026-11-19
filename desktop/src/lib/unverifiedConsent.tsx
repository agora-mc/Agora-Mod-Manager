import type { ConfirmOptions } from '../components/ui/confirm';
import { formatError, hasErrorCode, setSetting } from './tauri';

/**
 * The error core returns when an install holds files with no integrity
 * information and the user has not confirmed them (`authorize_plan`,
 * `technic::install_zip_pack`).
 */
export const LOW_SECURITY_CODE = 'ERR_LOW_SECURITY_DISABLED';

/** The setting that answers the prompt below for every install. */
const ALWAYS_SETTING = 'allow_unverified_packs';

/**
 * Ask whether to install files Agora cannot check, this once. Ticking
 * "Don't ask again" turns on *Allow low security downloads*, which Settings
 * can turn back off. Resolves `true` to go ahead.
 */
export async function askUnverifiedInstall(
  confirm: (options: ConfirmOptions) => Promise<boolean>,
  detail: string,
): Promise<boolean> {
  let always = false;
  const ok = await confirm({
    title: 'Install files Agora cannot check?',
    body: (
      <>
        {detail}
        {'\n\n'}
        <label className="flex items-start gap-2 whitespace-normal">
          <input
            type="checkbox"
            className="mt-0.5"
            onChange={(event) => {
              always = event.currentTarget.checked;
            }}
          />
          <span>
            Don’t ask again. This turns on Allow low security downloads, which you can turn off in
            Settings → Content sources.
          </span>
        </label>
      </>
    ),
    confirmLabel: 'Install anyway',
    tone: 'danger',
  });
  if (ok && always) {
    try {
      await setSetting(ALWAYS_SETTING, true);
    } catch {
      // The install still goes ahead on this answer; only "don't ask again" is lost.
    }
  }
  return ok;
}

/**
 * Run an install, and if core asks for confirmation of unverified files, ask
 * the user and run it again with their answer. Resolves `null` if they decline.
 */
export async function withUnverifiedConsent<T>(
  confirm: (options: ConfirmOptions) => Promise<boolean>,
  run: (allowUnverified: boolean) => Promise<T>,
): Promise<T | null> {
  try {
    return await run(false);
  } catch (e) {
    if (!hasErrorCode(e, LOW_SECURITY_CODE)) throw e;
    if (!(await askUnverifiedInstall(confirm, formatError(e)))) return null;
    return run(true);
  }
}
