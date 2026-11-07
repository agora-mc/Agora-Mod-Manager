/**
 * One instance of a non-Minecraft game on its Instances page: the kit's card (the same card as
 * Minecraft's) filled with this game's facts, and the actions the games backend supports today.
 * Launch starts the game from here through the shared launch controller, so the instance page
 * shows the same launch; an answer that needs a decision sends the player there with "Review".
 * Stop takes Launch's place while the game runs; Clone sits where Minecraft's does. Only the
 * actions the instance's own capabilities allow are shown.
 */

import {
  InstanceActionBar,
  InstanceCardBanner,
  InstanceCardFrame,
  RunningIndicator,
  type InstanceAction,
  type InstanceCapabilities,
} from '@/components/instance-kit';
import type { InstanceCard } from './api';
import { createdOn, playedWhen } from './instanceDates';
import { gameKitCapabilities, GameInstanceTile, InstanceChips } from './instanceIdentity';
import { playedText } from './instanceActions';
import { dismissGameLaunch, startGameLaunch, useAnyGameLaunching, useGameLaunch } from './launchController';

interface GameInstanceCardProps {
  card: InstanceCard;
  /** Open the instance's page (Edit, and Review after a launch that needs an answer). */
  onOpen: () => void;
  onDelete: () => void;
  /** Clone (opens the dialog). */
  onClone: () => void;
  /** Stop the running game (asks first). */
  onStop: () => void;
  /** This card's delete is in flight. */
  deleting: boolean;
  /** This card's Stop is in flight. */
  stopping?: boolean;
  /** Another card's action is in flight: hold this card's changes until it finishes. */
  locked: boolean;
  /** A launch from this card has answered; the list reloads to show the running state. */
  onLaunchSettled: () => void;
  /** Overrides the card's own capabilities (tests). */
  capabilities?: InstanceCapabilities;
}

export function GameInstanceCard({
  card,
  onOpen,
  onDelete,
  onClone,
  onStop,
  deleting,
  stopping = false,
  locked,
  onLaunchSettled,
  capabilities: capabilitiesOverride,
}: GameInstanceCardProps) {
  const capabilities = capabilitiesOverride ?? gameKitCapabilities(card.capabilities);
  const launch = useGameLaunch(card.instanceId);
  const anyLaunching = useAnyGameLaunching();
  const { reply, error } = launch;

  const startLaunch = async () => {
    const answer = await startGameLaunch(card.instanceId);
    // A plain "it started" needs no banner: the card turns to Running when the list reloads.
    if (answer?.status === 'launched' && answer.notice === null && answer.findings.length === 0) {
      dismissGameLaunch(card.instanceId);
    }
    // A before-launch question is answered on the instance page, where the dialog lives.
    if (answer?.status === 'before_launch_needed' || answer?.status === 'before_launch_failed') {
      onOpen();
    }
    onLaunchSettled();
  };

  const actions: InstanceAction[] = [];
  if (card.running) {
    actions.push({
      id: 'stop',
      label: stopping ? 'Stopping…' : 'Stop',
      ariaLabel: `Stop ${card.name}`,
      busy: stopping,
      title: 'Close the game. Anything not saved in the game is lost.',
      onSelect: onStop,
    });
  } else {
    actions.push({
      id: 'launch',
      label: launch.busy ? 'Starting…' : 'Launch',
      ariaLabel: `Launch ${card.name}`,
      busy: launch.busy,
      disabled: (anyLaunching && !launch.busy) || deleting,
      title: anyLaunching && !launch.busy ? 'Another instance is starting. Wait for it to finish.' : undefined,
      onSelect: () => void startLaunch(),
    });
  }
  actions.push({
    id: 'edit',
    label: 'Edit',
    ariaLabel: `Edit ${card.name}`,
    title: 'Open this instance: its mods, load order, settings and checks',
    onSelect: onOpen,
  });
  actions.push({
    id: 'clone',
    label: 'Clone',
    ariaLabel: `Clone ${card.name}`,
    disabled: locked || launch.busy,
    title: 'Make a copy of this instance',
    onSelect: onClone,
  });
  actions.push({
    id: 'delete',
    label: 'Delete',
    ariaLabel: `Delete ${card.name}`,
    busy: deleting,
    disabled: locked || card.running || launch.busy,
    title: card.running ? 'Close the game before deleting this instance.' : undefined,
    onSelect: onDelete,
  });

  const created = createdOn(card.createdAt);
  const lastPlayed = card.lastPlayedAt ?? card.lastLaunchedAt;
  const played = capabilities.playTime ? playedText(card.playTimeMs) : null;
  const status = card.running ? (
    <RunningIndicator>Running now</RunningIndicator>
  ) : lastPlayed ? (
    `Last played ${playedWhen(lastPlayed)}${played === null ? '' : ` · ${played}`}`
  ) : (
    'Never played'
  );

  return (
    <InstanceCardFrame
      testId="instance-card"
      running={card.running}
      tile={<GameInstanceTile instanceId={card.instanceId} name={card.name} />}
      name={card.name}
      chips={
        <InstanceChips store={card.store} version={card.version} deployment={card.deployment} pinned={card.pinned} />
      }
      status={status}
      facts={[created === null ? null : `Created ${created}`]}
      // The lock-state corner stays empty until game instances can be locked (`lock`).
      actions={<InstanceActionBar actions={actions} capabilities={capabilities} />}
    >
      {error !== null && (
        <InstanceCardBanner tone="danger" title={error} onDismiss={() => dismissGameLaunch(card.instanceId)} />
      )}
      {reply?.status === 'refused' && (
        <InstanceCardBanner
          tone="danger"
          title="The launch checks stopped this launch"
          body="Nothing was started. The instance page says what to fix."
          action={{ label: 'Review', ariaLabel: `Review ${card.name}`, onSelect: onOpen }}
          onDismiss={() => dismissGameLaunch(card.instanceId)}
          testId="card-launch-refused"
        />
      )}
      {reply?.status === 'vfs_unavailable' && (
        <InstanceCardBanner
          tone="warn"
          title="The virtual file system could not start"
          body="Nothing was started. Agora asks how to run it instead on the instance page."
          action={{ label: 'Review', ariaLabel: `Review ${card.name}`, onSelect: onOpen }}
          testId="card-launch-question"
        />
      )}
      {reply?.status === 'launched' && (
        <InstanceCardBanner
          tone="info"
          title="The game is starting"
          body={reply.notice ?? `Launched past ${reply.findings.length === 1 ? 'one check' : `${reply.findings.length} checks`}.`}
          onDismiss={() => dismissGameLaunch(card.instanceId)}
        />
      )}
    </InstanceCardFrame>
  );
}
