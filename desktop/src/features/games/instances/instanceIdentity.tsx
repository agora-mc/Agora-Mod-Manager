/**
 * What a game instance's card and header say about it, in the kit's shapes: which actions the
 * games backend supports today, and the chips under the name. The store/version chip leads, with
 * a coloured dot (as Minecraft's loader chip does); the facts after it are muted.
 */

import { MetaChip } from '@/components/InstanceIcon';
import { HueChip, type InstanceCapabilities } from '@/components/instance-kit';
import type { DeploymentMode } from './api';
import { deploymentLabel } from './instanceText';

/**
 * What a game instance can do today. Rename, clone, group, stop, trash and play time light up
 * here as their games commands land; until then the kit simply does not show them.
 */
export const GAME_INSTANCE_CAPABILITIES: InstanceCapabilities = {
  launch: true,
  delete: true,
};

const STORE_NAMES: Record<string, string> = {
  steam: 'Steam',
  gog: 'GOG',
  epic: 'Epic',
  'microsoft-store': 'Microsoft Store',
  direct: 'Folder',
};

/** A hue per store, so two instances of different stores tell apart at a glance. */
const STORE_HUES: Record<string, number> = {
  steam: 205,
  gog: 280,
  epic: 30,
  'microsoft-store': 130,
  direct: 170,
};

/** "Steam 1.6.1170", or null when the runtime was not recorded. */
export function runtimeText(store: string | null, version: string | null, build?: string | null): string | null {
  const parts = [store === null ? null : STORE_NAMES[store] ?? store, version, build ?? null].filter(
    (part): part is string => part !== null && part !== '',
  );
  return parts.length === 0 ? null : parts.join(' ');
}

interface InstanceChipsProps {
  store: string | null;
  version: string | null;
  build?: string | null;
  deployment: DeploymentMode | null;
  pinned: boolean;
}

/** The chips under an instance's name: runtime (coloured), how it runs, and its base. */
export function InstanceChips({ store, version, build, deployment, pinned }: InstanceChipsProps) {
  const runtime = runtimeText(store, version, build);
  return (
    <>
      {runtime === null ? (
        <MetaChip>Runtime not recorded</MetaChip>
      ) : (
        <HueChip hue={store === null ? STORE_HUES.direct : STORE_HUES[store] ?? STORE_HUES.direct}>{runtime}</HueChip>
      )}
      <MetaChip>{deploymentLabel(deployment)}</MetaChip>
      <MetaChip>{pinned ? 'Pinned base' : 'Runs from install'}</MetaChip>
    </>
  );
}
