import { render, screen } from '@testing-library/react';
import { describe, expect, it } from 'vitest';
import { ImportProgressView } from './ImportProgressView';
import type { ImportProgress } from './api';

const storing: ImportProgress = {
  phase: 'storing',
  bytesDone: 393,
  bytesTotal: 982,
  modsDone: 12,
  modsTotal: 107,
  currentMod: 'SkyUI',
};

describe('ImportProgressView bar', () => {
  it('starts a measured bar as a new element, so it never slides back from full', () => {
    const props = { title: 'Importing', sourceName: 'Mod Organizer 2', startedAt: Date.now(), error: null, onBack: () => {} };
    const { rerender } = render(<ImportProgressView {...props} progress={null} />);
    const stripes = screen.getByTestId('import-bar-indeterminate');

    rerender(<ImportProgressView {...props} progress={storing} />);

    const fill = screen.getByTestId('import-bar-fill');
    expect(fill).not.toBe(stripes);
    expect(fill.style.width).toBe('40%');
  });
});
