import { useState } from 'react';
import { Stethoscope } from 'lucide-react';
import { SettingsSection } from './SettingsSection';
import { Button } from '@/components/ui/button';
import {
  formatError,
  localStateReport,
  msaAccountStoreStatus,
  type AccountStoreStatus,
  type LocalStateReport,
} from '@/lib/tauri';

const INTEGRITY_TEXT: Record<LocalStateReport['integrity'], string> = {
  ok: 'Healthy',
  corrupt: 'Damaged: the next start keeps it aside and restores the newest good backup',
  missing: 'Not created yet',
  unreadable: 'Could not be read',
};

/**
 * Agora's own database and saved accounts, checked on request: what `agora doctor` reports.
 * Nothing here changes anything; it says what Agora would do at the next start.
 */
export function DataHealthSettings() {
  const [report, setReport] = useState<LocalStateReport | null>(null);
  const [accounts, setAccounts] = useState<AccountStoreStatus | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [checking, setChecking] = useState(false);

  const check = async () => {
    setChecking(true);
    setError(null);
    try {
      const [db, store] = await Promise.all([localStateReport(), msaAccountStoreStatus()]);
      setReport(db);
      setAccounts(store);
    } catch (failure) {
      setError(formatError(failure));
    } finally {
      setChecking(false);
    }
  };

  const healthy = report !== null && !report.problems && !(accounts?.needs_repair ?? false);

  return (
    <SettingsSection
      card="data-health"
      id="settings-data-health"
      icon={Stethoscope}
      title="Data health"
      description="Check Agora's own database, its backups and your saved sign-ins. Checking changes nothing."
      action={(
        <Button size="sm" variant="outline" disabled={checking} onClick={() => void check()}>
          {checking ? 'Checking…' : report ? 'Check again' : 'Check now'}
        </Button>
      )}
    >
      {error && <p role="alert" className="text-sm text-destructive">{error}</p>}
      {report && (
        <div className="space-y-2 text-sm" data-testid="data-health-report">
          <p className={healthy ? 'text-green-700 dark:text-green-400' : 'text-destructive'}>
            {healthy ? 'Everything looks fine.' : 'Something needs attention.'}
          </p>
          <dl className="grid gap-x-4 gap-y-1 sm:grid-cols-[auto_minmax(0,1fr)]">
            <dt className="text-muted-foreground">Database</dt>
            <dd className="wrap-anywhere">
              {INTEGRITY_TEXT[report.integrity]}
              {report.integrityDetail ? ` (${report.integrityDetail})` : ''}
            </dd>
            <dt className="text-muted-foreground">Version</dt>
            <dd>
              {report.schemaVersion === null
                ? 'Could not be read'
                : report.databaseIsNewer
                  ? `${report.schemaVersion}, written by a newer Agora (this one reads up to ${report.supportedVersion})`
                  : `${report.schemaVersion}`}
            </dd>
            <dt className="text-muted-foreground">Backups</dt>
            <dd className="wrap-anywhere">
              {report.backupsError
                ? `Could not be read (${report.backupsError})`
                : report.backups.length === 0
                  ? `None yet. Agora keeps the newest ${report.backupsKept}, made before each update to the database.`
                  : `${report.backups.length} kept (newest ${report.backupsKept} are kept)`}
            </dd>
            {(report.keptAside.length > 0 || report.keptAsideError) && (
              <>
                <dt className="text-muted-foreground">Set aside</dt>
                <dd className="wrap-anywhere">
                  {report.keptAsideError
                    ? `Could not be read (${report.keptAsideError})`
                    : `${report.keptAside.length} damaged file${report.keptAside.length === 1 ? '' : 's'} kept after a recovery, for you to inspect or delete: ${report.keptAside.join(', ')}`}
                </dd>
              </>
            )}
            {accounts && (
              <>
                <dt className="text-muted-foreground">Sign-ins</dt>
                <dd>{accounts.needs_repair ? accounts.message ?? 'The saved sign-ins need repair (Settings → Accounts).' : 'Fine'}</dd>
              </>
            )}
          </dl>
          <p className="text-xs text-muted-foreground wrap-anywhere">{report.path}</p>
        </div>
      )}
    </SettingsSection>
  );
}
