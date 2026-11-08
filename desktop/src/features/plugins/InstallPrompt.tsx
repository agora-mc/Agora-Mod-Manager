import { AlertTriangle } from 'lucide-react';
import { Button } from '@/components/ui/button';
import type { CapabilityDescription, InstallPreview } from './types';

/**
 * The one consent panel used by every install path: a package from a file, a
 * publisher-hosted update, and an official plugin from the signed catalog.
 *
 * It lists only what the package asks for and never decides for the user. The
 * caller supplies the confirm action, which is what actually repeats the
 * install with consent.
 */

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null;
}

function stringArray(value: unknown): string[] {
  return Array.isArray(value) ? value.filter((item): item is string => typeof item === 'string') : [];
}

function isCapabilityDescription(value: unknown): value is CapabilityDescription {
  return (
    isRecord(value) &&
    typeof value.name === 'string' &&
    typeof value.summary === 'string' &&
    typeof value.isMutating === 'boolean'
  );
}

function isInstallPreview(value: unknown): value is InstallPreview {
  return isRecord(value) && isRecord(value.manifest);
}

/** Whether the prompt has to ask this package's code question. */
function asksAboutCode(preview: InstallPreview): boolean {
  return preview.runsCode === true && (preview.codeChanged ?? !preview.replacesVersion);
}

function capabilityList(value: unknown): CapabilityDescription[] {
  return Array.isArray(value) ? value.filter(isCapabilityDescription) : [];
}

function networkHosts(preview: InstallPreview): string[] {
  const network: unknown = preview.manifest?.network;
  return isRecord(network) ? stringArray(network.hosts) : [];
}

export function InstallPrompt({
  preview,
  development,
  busy,
  confirmLabel = 'Install',
  busyLabel = 'Installing…',
  onCancel,
  onConfirm,
}: {
  preview: InstallPreview;
  development: boolean;
  busy: boolean;
  confirmLabel?: string;
  busyLabel?: string;
  onCancel: () => void;
  /** `allowCode` is the answer to "<name> runs code. Allow it?"; `false` when not asked. */
  onConfirm: (allowCode: boolean) => void;
}) {
  const requiredCapabilities = capabilityList(preview.requiredCapabilities);
  const optionalCapabilities = capabilityList(preview.optionalCapabilities);
  const unsupportedCapabilities = stringArray(preview.unsupportedCapabilities);
  const alsoInstalls = Array.isArray(preview.alsoInstalls)
    ? preview.alsoInstalls.filter(isInstallPreview)
    : [];
  // Coerced: an older backend that predates this field would otherwise put
  // `undefined` where a list is expected, in a component whose whole job is
  // telling the user what they are agreeing to.
  const newlyRequested = Array.isArray(preview.addedCapabilities)
    ? stringArray(preview.addedCapabilities)
    : [];
  const newHosts = Array.isArray(preview.addedHosts) ? stringArray(preview.addedHosts) : [];
  const manifestName = typeof preview.manifest?.name === 'string' ? preview.manifest.name : 'Plugin';
  const manifestVersion =
    typeof preview.manifest?.version === 'string' ? preview.manifest.version : 'unknown version';
  const manifestDescription =
    typeof preview.manifest?.description === 'string'
      ? preview.manifest.description
      : 'No description given.';
  const manifestLicense =
    typeof preview.manifest?.license === 'string' ? preview.manifest.license : 'Unknown license';
  const manifestSource = typeof preview.manifest?.source === 'string' ? preview.manifest.source : null;
  const manifestNetworkValue: unknown = preview.manifest?.network;
  const manifestNetwork = isRecord(manifestNetworkValue)
    ? stringArray(manifestNetworkValue.hosts)
    : [];
  const blocked = unsupportedCapabilities.length > 0;
  // Asked when the code is new to this person: a first install, an update that
  // brings code, changed code, or code from a different publisher. An update
  // that keeps the same code keeps the answer already given. A dependency that
  // runs code is asked about the same way.
  const targetAsksAboutCode = asksAboutCode(preview);
  // A dependency that runs code cannot be installed without allowing it, so the
  // "without running code" path is not offered when one needs code.
  const dependencyNeedsCode = alsoInstalls.some(
    (extra) => extra.runsCode === true && !extra.replacesVersion,
  );
  const needsCodeAnswer =
    targetAsksAboutCode || alsoInstalls.some((extra) => asksAboutCode(extra));

  return (
    <div className="rounded-lg border border-border bg-muted/30 p-4">
      <div className="space-y-1">
        <div className="font-medium">
          {manifestName} {manifestVersion}
        </div>
        <p className="text-sm text-muted-foreground">
          {manifestDescription}
        </p>
        <p className="text-xs text-muted-foreground">
          {manifestLicense}
          {manifestSource ? ` · ${manifestSource}` : ' · no source link given'}
          {development ? ' · loaded from a folder you are editing' : ''}
        </p>
      </div>

      {preview.replacesVersion ? (
        <p className="mt-3 text-sm">
          This replaces version {preview.replacesVersion}.
          {preview.migratesData
            ? ' It changes how it stores data — Agora will keep a copy of the old data first.'
            : ''}
        </p>
      ) : null}

      {preview.publisherChanged === true ? (
        <p className="mt-3 flex items-start gap-2 text-sm text-amber-700 dark:text-amber-400">
          <AlertTriangle className="mt-0.5 h-4 w-4 shrink-0" aria-hidden />
          This update is signed by a different publisher than the one you installed. Only
          continue if you got it from somewhere you trust.
        </p>
      ) : null}

      {/*
        On an update the full permission list is not the decision — the user
        already agreed to most of it. Naming only what is new is what makes a
        release that starts asking for more impossible to skim past.
      */}
      {preview.replacesVersion && (newlyRequested.length > 0 || newHosts.length > 0) ? (
        <p className="mt-2 flex items-start gap-2 text-sm text-amber-700 dark:text-amber-400">
          <AlertTriangle className="mt-0.5 h-4 w-4 shrink-0" aria-hidden />
          This version asks for something it did not have before:{' '}
          {[...newlyRequested, ...newHosts.map((host) => `contact ${host}`)].join(', ')}.
        </p>
      ) : null}

      {requiredCapabilities.length > 0 ? (
        <div className="mt-3">
          <div className="text-sm font-medium">It needs permission to:</div>
          <ul className="mt-1 space-y-1 text-sm text-muted-foreground">
            {requiredCapabilities.map((capability) => (
              <li key={capability.name}>
                {capability.summary}
                {capability.isMutating ? (
                  <span className="ml-1 text-amber-600 dark:text-amber-400">(makes changes)</span>
                ) : null}
              </li>
            ))}
          </ul>
        </div>
      ) : null}

      {/*
        Optional capabilities are granted too — the plugin simply copes if they
        are missing in some future build. Listing only the required ones told
        the user "it asks for no permissions" while handing over the optional
        ones, which is the one sentence this dialog must never get wrong.
      */}
      {optionalCapabilities.length > 0 ? (
        <div className="mt-3">
          <div className="text-sm font-medium">It will also use, if allowed:</div>
          <ul className="mt-1 space-y-1 text-sm text-muted-foreground">
            {optionalCapabilities.map((capability) => (
              <li key={capability.name}>
                {capability.summary}
                {capability.isMutating ? (
                  <span className="ml-1 text-amber-600 dark:text-amber-400">(makes changes)</span>
                ) : null}
              </li>
            ))}
          </ul>
        </div>
      ) : null}

      {requiredCapabilities.length === 0 && optionalCapabilities.length === 0 ? (
        <p className="mt-3 text-sm text-muted-foreground">It asks for no permissions.</p>
      ) : null}

      {alsoInstalls.map((extra) => {
        const extraRequired = capabilityList(extra.requiredCapabilities);
        const extraOptional = capabilityList(extra.optionalCapabilities);
        const extraHosts = networkHosts(extra);
        const extraName =
          typeof extra.manifest?.name === 'string' ? extra.manifest.name : 'Plugin';
        const extraVersion =
          typeof extra.manifest?.version === 'string'
            ? extra.manifest.version
            : 'unknown version';
        return (
          <div
            key={`${extra.manifest?.id ?? extraName}-${extraVersion}`}
            className="mt-3 border-t border-border pt-3"
            data-testid="also-installs"
          >
            <div className="font-medium">
              {extraName} {extraVersion}
            </div>
            <p className="text-xs text-muted-foreground">Needed by {manifestName}</p>
            {typeof extra.manifest?.description === 'string' ? (
              <p className="text-sm text-muted-foreground">{extra.manifest.description}</p>
            ) : null}

            {extraRequired.length > 0 ? (
              <div className="mt-2">
                <div className="text-sm font-medium">It needs permission to:</div>
                <ul className="mt-1 space-y-1 text-sm text-muted-foreground">
                  {extraRequired.map((capability) => (
                    <li key={capability.name}>
                      {capability.summary}
                      {capability.isMutating ? (
                        <span className="ml-1 text-amber-600 dark:text-amber-400">
                          (makes changes)
                        </span>
                      ) : null}
                    </li>
                  ))}
                </ul>
              </div>
            ) : null}

            {extraOptional.length > 0 ? (
              <div className="mt-2">
                <div className="text-sm font-medium">It will also use, if allowed:</div>
                <ul className="mt-1 space-y-1 text-sm text-muted-foreground">
                  {extraOptional.map((capability) => (
                    <li key={capability.name}>
                      {capability.summary}
                      {capability.isMutating ? (
                        <span className="ml-1 text-amber-600 dark:text-amber-400">
                          (makes changes)
                        </span>
                      ) : null}
                    </li>
                  ))}
                </ul>
              </div>
            ) : null}

            {extraRequired.length === 0 && extraOptional.length === 0 ? (
              <p className="mt-2 text-sm text-muted-foreground">It asks for no permissions.</p>
            ) : null}

            {extraHosts.length > 0 ? (
              <p className="mt-2 text-sm text-muted-foreground">
                It may contact:{' '}
                {extraHosts.includes('*') ? 'any website' : extraHosts.join(', ')}
              </p>
            ) : null}

            {asksAboutCode(extra) ? (
              <p className="mt-2 text-sm font-medium">{extraName} runs code. Allow it?</p>
            ) : null}
          </div>
        );
      })}

      {manifestNetwork.length > 0 ? (
        <p className="mt-2 text-sm text-muted-foreground">
          It may contact:{' '}
          {manifestNetwork.includes('*')
            ? 'any website'
            : manifestNetwork.join(', ')}
        </p>
      ) : null}

      {preview.needsReducedSecurity === true ? (
        <p className="mt-3 flex items-start gap-2 text-sm text-amber-700 dark:text-amber-400">
          <AlertTriangle className="mt-0.5 h-4 w-4 shrink-0" aria-hidden />
          This plugin can reach{' '}
          {manifestNetwork.includes('*') ? 'any website' : `${manifestNetwork.length} websites`}, so
          Agora cannot tell you in advance where your data goes. It installs only with Reduced
          security mode turned on in Settings.
        </p>
      ) : null}

      {blocked ? (
        <p className="mt-3 flex items-start gap-2 text-sm text-destructive">
          <AlertTriangle className="mt-0.5 h-4 w-4 shrink-0" aria-hidden />
          This plugin needs something this version of Agora does not provide:{' '}
          {unsupportedCapabilities.join(', ')}. Check for an Agora update.
        </p>
      ) : null}

      {targetAsksAboutCode ? (
        <p className="mt-3 text-sm font-medium">{manifestName} runs code. Allow it?</p>
      ) : null}

      <div className="mt-4 flex gap-2">
        <Button size="sm" onClick={() => onConfirm(needsCodeAnswer)} disabled={busy || blocked}>
          {busy ? busyLabel : needsCodeAnswer ? `${confirmLabel} and allow code` : confirmLabel}
        </Button>
        {needsCodeAnswer && !dependencyNeedsCode ? (
          <Button
            size="sm"
            variant="secondary"
            onClick={() => onConfirm(false)}
            disabled={busy || blocked}
          >
            {confirmLabel} without running code
          </Button>
        ) : null}
        <Button size="sm" variant="ghost" onClick={onCancel} disabled={busy}>
          Cancel
        </Button>
      </div>
    </div>
  );
}
