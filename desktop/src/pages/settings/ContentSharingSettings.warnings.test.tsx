import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { beforeEach, describe, expect, it, vi } from 'vitest';

const invokeMock = vi.hoisted(() => vi.fn());
vi.mock('@tauri-apps/api/core', () => ({ invoke: invokeMock }));

import { ConfirmProvider } from '@/components/ui/confirm';
import { ContentSharingSettings } from './ContentSharingSettings';

const none = { filesConverted: 0, filesAlready: 0, bytes: 0, skipped: [] };
const LISTING_FAILED = 'Instances could not be listed: Access is denied. (os error 5)';

describe('ContentSharingSettings: core warnings', () => {
  beforeEach(() => invokeMock.mockReset());

  // Review finding (final-ui). core's BulkPreview/BulkConversion carry `warnings`: "Never silently
  // counted as empty". When the instances folder cannot be listed, `convert_all` returns Ok with
  // zero instances and a warning. ContentSharingSettings never reads `warnings`, so the user is told
  // "Converted 0 files." (a success toast) or nothing at all, and believes every instance is converted.
  // Run: npx vitest run src/pages/settings/ContentSharingSettings.warnings.test.tsx (un-skip first)
  // proves bug: ContentSharingSettings drops core's conversion `warnings`, so an unlistable instances folder reads as "converted 0 files"
  it('tells the user when core could not list the instances to convert', async () => {
    invokeMock.mockImplementation(async (command: string) => {
      if (command === 'get_content_sharing') return 'link';
      if (command === 'preview_content_sharing_conversion') {
        return { instances: 0, summary: none, warnings: [LISTING_FAILED] };
      }
      if (command === 'set_content_sharing') {
        return { instances: 0, summary: none, failures: [], warnings: [LISTING_FAILED] };
      }
      return null;
    });
    render(
      <ConfirmProvider>
        <ContentSharingSettings />
      </ConfirmProvider>,
    );
    fireEvent.click(await screen.findByRole('switch', { name: 'Share mod files between instances' }));
    await waitFor(() => expect(invokeMock).toHaveBeenCalledWith('set_content_sharing', { share: false, convert: false }));
    // The warning must reach the user somewhere on the card (not only a toast that says "success").
    expect(await screen.findByText(/could not be listed/)).toBeInTheDocument();
  });
});
