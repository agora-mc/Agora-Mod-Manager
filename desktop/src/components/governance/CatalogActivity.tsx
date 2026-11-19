import { useEffect, useState } from 'react';
import { Activity } from 'lucide-react';
import {
  formatError,
  registryAuditLog,
  registryResolutions,
  registryUnderReview,
  type AuditLogEntry,
  type UnderReviewItem,
} from '../../lib/tauri';

type Tab = 'review' | 'resolutions' | 'log';

const TABS: { id: Tab; label: string }[] = [
  { id: 'review', label: 'Under review' },
  { id: 'resolutions', label: 'Recent decisions' },
  { id: 'log', label: 'Activity log' },
];

function when(timestamp: string): string {
  const date = new Date(timestamp);
  return Number.isNaN(date.getTime()) ? timestamp : date.toLocaleString();
}

/**
 * What the community is reviewing and what the catalog has decided recently, read from the
 * signed catalog on this device (the CLI's `agora registry under-review`, `resolutions` and
 * `audit-log`). Nothing here reaches the network.
 */
export function CatalogActivity() {
  const [tab, setTab] = useState<Tab>('review');
  const [review, setReview] = useState<UnderReviewItem[] | null>(null);
  const [resolutions, setResolutions] = useState<AuditLogEntry[] | null>(null);
  const [log, setLog] = useState<AuditLogEntry[] | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let live = true;
    setError(null);
    const load = tab === 'review'
      ? registryUnderReview().then((value) => { if (live) setReview(value); })
      : tab === 'resolutions'
        ? registryResolutions(20).then((value) => { if (live) setResolutions(value); })
        : registryAuditLog(50).then((value) => { if (live) setLog(value); });
    load.catch((failure: unknown) => { if (live) setError(formatError(failure)); });
    return () => { live = false; };
  }, [tab]);

  const entries = tab === 'resolutions' ? resolutions : log;

  return (
    <section className="rounded-xl border border-border bg-card p-6" data-testid="catalog-activity">
      <div className="mb-4 flex flex-wrap items-center justify-between gap-3">
        <h2 className="flex items-center gap-2 text-lg font-semibold">
          <Activity className="h-5 w-5 text-primary" aria-hidden="true" />
          Catalog activity
        </h2>
        <div className="flex gap-1 rounded-lg bg-muted p-1" role="tablist" aria-label="Catalog activity">
          {TABS.map((option) => (
            <button
              key={option.id}
              type="button"
              role="tab"
              aria-selected={tab === option.id}
              onClick={() => setTab(option.id)}
              className={[
                'rounded-md px-3 py-1 text-sm font-medium',
                tab === option.id ? 'bg-background shadow-sm' : 'text-muted-foreground hover:text-foreground',
              ].join(' ')}
            >
              {option.label}
            </button>
          ))}
        </div>
      </div>
      {error && <p role="alert" className="text-sm text-destructive">{error}</p>}
      {!error && tab === 'review' && (
        review === null ? <p className="text-sm text-muted-foreground">Loading…</p>
          : review.length === 0 ? <p className="text-sm text-muted-foreground">Nothing in the catalog is under review right now.</p>
            : (
              <ul className="divide-y divide-border text-sm">
                {review.map((item) => (
                  <li key={item.id} className="flex items-center justify-between gap-3 py-2">
                    <span className="min-w-0 truncate">{item.name} <span className="text-muted-foreground">({item.content_type})</span></span>
                    <span className="shrink-0 tabular-nums text-muted-foreground">score {item.net_score}</span>
                  </li>
                ))}
              </ul>
            )
      )}
      {!error && tab !== 'review' && (
        entries === null ? <p className="text-sm text-muted-foreground">Loading…</p>
          : entries.length === 0 ? (
            <p className="text-sm text-muted-foreground">
              {tab === 'resolutions' ? 'No recent decisions in this catalog.' : 'The catalog on this device has no activity recorded.'}
            </p>
          ) : (
            <ul className="divide-y divide-border text-sm">
              {entries.map((entry) => (
                <li key={entry.id} className="py-2">
                  <div className="flex flex-wrap items-baseline justify-between gap-2">
                    <span className="font-medium">{entry.action}</span>
                    <span className="text-xs text-muted-foreground">{when(entry.timestamp)}</span>
                  </div>
                  {entry.details && <p className="text-muted-foreground wrap-anywhere">{entry.details}</p>}
                </li>
              ))}
            </ul>
          )
      )}
    </section>
  );
}
