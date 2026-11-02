import { Badge } from '@/components/ui/badge';
import type { OfficialStatus } from './types';

/**
 * The official mark means the signed registry vouches for this exact install:
 * core decided it, and the plugin's own manifest never did. A status that is
 * not `official` renders nothing here.
 */
export function OfficialBadge({ status }: { status: OfficialStatus | null | undefined }) {
  if (status?.state !== 'official') return null;
  return (
    <Badge variant="secondary" data-testid="official-badge">
      Official
    </Badge>
  );
}

/**
 * Shown when a plugin was installed from the signed registry but no longer
 * verifies. It keeps working under the community rules, so the sentence says
 * why rather than quietly dropping the badge.
 */
export function OfficialStatusNote({ status }: { status: OfficialStatus | null | undefined }) {
  if (status?.state === 'unverified') {
    return (
      <p className="text-xs text-amber-600 dark:text-amber-400" data-testid="official-status-note">
        It was installed as an official plugin but no longer verifies: {status.reason}
      </p>
    );
  }
  if (status?.state === 'catalogUnavailable' || status?.state === 'notListed') {
    return (
      <p className="text-xs text-amber-600 dark:text-amber-400" data-testid="official-status-note">
        {status.reason}
      </p>
    );
  }
  return null;
}
