/**
 * What a game instance's card and header say about it, in the kit's shapes: which actions the
 * games backend supports today, and the chips under the name. The store/version chip leads, with
 * a coloured dot (as Minecraft's loader chip does); the facts after it are muted.
 */

import { InstanceIcon, MetaChip } from '@/components/InstanceIcon';
import { instanceFamilyHue } from '@/lib/instanceIdentity';
import { HueChip, type InstanceCapabilities } from '@/components/instance-kit';
import type { DeploymentMode, InstanceCapabilities as GameCapabilities } from './api';
import { deploymentLabel } from './instanceText';
import { useGameInstanceIcon } from './instanceIcons';

/**
 * What a game instance can do when the backend has not said (an older reply): launch and delete.
 * Everything else lights up from the instance's own `capabilities`.
 */
export const GAME_INSTANCE_CAPABILITIES: InstanceCapabilities = {
  launch: true,
  delete: true,
};

/**
 * The instance's capabilities in the kit's words. The backend says `stop` where the kit says
 * `kill`; Delete is always there (to Recently deleted when `trash` is set, for good otherwise).
 */
export function gameKitCapabilities(capabilities: GameCapabilities | undefined): InstanceCapabilities {
  if (capabilities === undefined) return GAME_INSTANCE_CAPABILITIES;
  return {
    launch: capabilities.launch,
    kill: capabilities.stop,
    rename: capabilities.rename,
    clone: capabilities.clone,
    trash: capabilities.trash,
    delete: true,
    openFolder: capabilities.openFolder,
    playTime: capabilities.playTime,
  };
}

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

/**
 * A game instance's tile: the image the player set, or Minecraft's generated tile (gradient and
 * initials) with a hue family drawn from the instance, so no two instances look alike.
 */
export function GameInstanceTile({ instanceId, name, size }: { instanceId: string; name: string; size?: number }) {
  const icon = useGameInstanceIcon(instanceId);
  return (
    <InstanceIcon
      name={name}
      seed={instanceId}
      familyHue={instanceFamilyHue(instanceId)}
      iconSrc={icon}
      size={size}
    />
  );
}
