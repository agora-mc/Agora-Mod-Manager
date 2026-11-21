/**
 * The words the instance kit speaks. Every game hands the kit its data in these shapes; the kit
 * never asks a game anything itself (no IPC, no game knowledge). See MASTER_SPEC §29.
 */

/**
 * What a game's instances can do today. The kit shows an action only when the game supports it,
 * so a game without Clone simply has no Clone button, never a greyed-out one. Minecraft supports
 * all of them; another game gains them as its backend commands land.
 */
export type InstanceCapability =
  | 'launch'
  | 'kill'
  | 'rename'
  | 'clone'
  | 'group'
  | 'lock'
  | 'trash'
  | 'delete'
  | 'notes'
  | 'openFolder'
  | 'image'
  | 'export'
  | 'playTime'
  | 'troubleshoot';

/** A game's supported capabilities. A capability that is missing or `false` is not supported. */
export type InstanceCapabilities = Readonly<Partial<Record<InstanceCapability, boolean>>>;

/** True when `capabilities` supports `capability`. */
export function supports(capabilities: InstanceCapabilities, capability: InstanceCapability): boolean {
  return capabilities[capability] === true;
}

/**
 * How ready an instance is to play, for the line under Launch. `check` means "worth a look, the
 * launch still goes ahead"; `unknown` means the checks have not answered (or could not run).
 */
export type ReadinessLevel = 'ready' | 'check' | 'blocked' | 'unknown';

export interface ReadinessView {
  level: ReadinessLevel;
  headline: string;
  /** Short facts shown under the headline, joined with " · ". */
  details?: readonly string[];
  /** The checks are running again; the headline is the last answer. */
  refreshing?: boolean;
}

/**
 * The actions an instance card or header can show. The id picks the icon, the emphasis and the
 * capability that gates it: `launch` (launch), `stop` (kill), `clone`, `group`, `troubleshoot`,
 * `openFolder` and `delete` are gated; `edit` and `review` are always available.
 */
export type InstanceActionId =
  | 'launch'
  | 'stop'
  | 'review'
  | 'edit'
  | 'clone'
  | 'group'
  | 'troubleshoot'
  | 'openFolder'
  | 'delete';

export interface InstanceAction {
  id: InstanceActionId;
  /** The visible label. */
  label: string;
  onSelect: () => void;
  /** The accessible name, when the label alone is ambiguous on a page of cards ("Delete Grounded"). */
  ariaLabel?: string;
  /** Hover text: what the action does, or why it is waiting. */
  title?: string;
  /** Working on it: the button shows a spinner and does not take a second press. */
  busy?: boolean;
  /**
   * Waiting for something transient (a launch in flight, a running game). Never use this for
   * "not supported": leave the action out instead.
   */
  disabled?: boolean;
  /** Overrides the emphasis the id implies. */
  emphasis?: InstanceActionEmphasis;
  /** A test hook for the rendered button. */
  testId?: string;
}

/**
 * `primary`: the one filled action (Launch, Review). `stop`: filled and destructive (Stop).
 * `secondary`: outlined. `danger`: quiet red, pushed to the end of the row (Delete).
 */
export type InstanceActionEmphasis = 'primary' | 'stop' | 'secondary' | 'danger';

const ACTION_CAPABILITY: Partial<Record<InstanceActionId, InstanceCapability>> = {
  launch: 'launch',
  stop: 'kill',
  clone: 'clone',
  group: 'group',
  troubleshoot: 'troubleshoot',
  openFolder: 'openFolder',
  delete: 'delete',
};

/** The capability an action needs, or null for an action every game has (Edit, Review). */
export function capabilityFor(id: InstanceActionId): InstanceCapability | null {
  return ACTION_CAPABILITY[id] ?? null;
}

/** The actions `capabilities` supports, in the order given. Without `capabilities`, all of them. */
export function supportedActions(
  actions: readonly InstanceAction[],
  capabilities?: InstanceCapabilities,
): InstanceAction[] {
  if (capabilities === undefined) return [...actions];
  return actions.filter((action) => {
    const needed = capabilityFor(action.id);
    return needed === null || supports(capabilities, needed);
  });
}

/** The emphasis an action shows with: its own override, or the one its id implies. */
export function emphasisOf(action: InstanceAction): InstanceActionEmphasis {
  if (action.emphasis) return action.emphasis;
  switch (action.id) {
    case 'launch':
    case 'review':
      return 'primary';
    case 'stop':
      return 'stop';
    case 'delete':
      return 'danger';
    default:
      return 'secondary';
  }
}
