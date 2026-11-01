import { render, screen } from '@testing-library/react';
import { describe, expect, it } from 'vitest';
import { OfficialBadge, OfficialStatusNote } from './OfficialBadge';

describe('Official mark', () => {
  it('is shown only when core says the plugin is verified official', () => {
    const { rerender } = render(<OfficialBadge status={{ state: 'official', version: '1.0.0' }} />);
    expect(screen.getByTestId('official-badge')).toHaveTextContent('Official');

    rerender(<OfficialBadge status={{ state: 'community' }} />);
    expect(screen.queryByTestId('official-badge')).toBeNull();

    rerender(<OfficialBadge status={{ state: 'unverified', reason: 'files changed' }} />);
    expect(screen.queryByTestId('official-badge')).toBeNull();

    rerender(<OfficialBadge status={undefined} />);
    expect(screen.queryByTestId('official-badge')).toBeNull();
  });

  it('says why an official install lost its standing', () => {
    render(<OfficialStatusNote status={{ state: 'unverified', reason: 'files changed after install' }} />);
    expect(screen.getByTestId('official-status-note')).toHaveTextContent(
      'files changed after install',
    );
  });

  it('says nothing for a plugin that still verifies or was never official', () => {
    const { rerender } = render(<OfficialStatusNote status={{ state: 'official', version: '1.0.0' }} />);
    expect(screen.queryByTestId('official-status-note')).toBeNull();

    rerender(<OfficialStatusNote status={{ state: 'community' }} />);
    expect(screen.queryByTestId('official-status-note')).toBeNull();
  });
});
