import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { beforeEach, describe, expect, it, vi } from 'vitest';

const invokeMock = vi.hoisted(() => vi.fn());
vi.mock('@tauri-apps/api/core', () => ({ invoke: invokeMock }));

import { ConfirmProvider } from '@/components/ui/confirm';
import { ContentSharingSettings } from './ContentSharingSettings';

const summary = {
  filesConverted: 3,
  filesAlready: 1,
  bytes: 3 * 1024 * 1024,
  skipped: [],
};

function answer(overrides: Record<string, unknown> = {}) {
  invokeMock.mockImplementation(async (command: string) => {
    if (Object.prototype.hasOwnProperty.call(overrides, command)) {
      const reply = overrides[command];
      if (typeof reply === 'function') return (reply as () => unknown)();
      if (reply instanceof Error) throw reply;
      return reply;
    }
    if (command === 'get_content_sharing') return 'link';
    if (command === 'preview_content_sharing_conversion') {
      return { instances: 2, summary, warnings: [] };
    }
    if (command === 'set_content_sharing') {
      return { instances: 2, summary, failures: [], warnings: [] };
    }
    return null;
  });
}

function renderCard() {
  return render(
    <ConfirmProvider>
      <ContentSharingSettings />
    </ConfirmProvider>,
  );
}

describe('ContentSharingSettings', () => {
  beforeEach(() => {
    invokeMock.mockReset();
    answer();
  });

  it('explains both modes and starts from the stored choice', async () => {
    renderCard();
    expect(await screen.findByText('Shared files (on)')).toBeInTheDocument();
    expect(screen.getByRole('switch', { name: 'Share mod files between instances' })).toBeChecked();
    expect(screen.getByText(/one copy is shared by every instance/)).toHaveTextContent(
      /saves space, but a mod file cannot be edited in place/,
    );
    expect(screen.getByText(/one copy is shared by every instance/)).toHaveTextContent(
      /each instance keeps its own editable copy and uses more space/,
    );
  });

  it('offers to convert existing instances and does it when accepted', async () => {
    renderCard();
    fireEvent.click(await screen.findByRole('switch', { name: 'Share mod files between instances' }));

    expect(await screen.findByText('Also convert my existing instances?')).toBeInTheDocument();
    const dialogText = screen.getByText(/can be converted/).textContent ?? '';
    expect(dialogText).toContain('3 files');
    expect(dialogText).toContain('2 instances');
    expect(dialogText).toContain('uses about 3.0 MB');

    fireEvent.click(screen.getByRole('button', { name: 'Convert' }));
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith('set_content_sharing', { share: false, convert: true }),
    );
    expect(await screen.findByText('Separate copies (off)')).toBeInTheDocument();
  });

  it('changes the setting without converting when the offer is declined', async () => {
    renderCard();
    fireEvent.click(await screen.findByRole('switch', { name: 'Share mod files between instances' }));
    fireEvent.click(await screen.findByRole('button', { name: 'Not now' }));

    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith('set_content_sharing', { share: false, convert: false }),
    );
    expect(await screen.findByText('Separate copies (off)')).toBeInTheDocument();
  });

  it('does not offer conversion when there is nothing to convert', async () => {
    answer({
      preview_content_sharing_conversion: {
        instances: 0,
        summary: { filesConverted: 0, filesAlready: 0, bytes: 0, skipped: [] },
        warnings: [],
      },
    });
    renderCard();
    fireEvent.click(await screen.findByRole('switch', { name: 'Share mod files between instances' }));

    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith('set_content_sharing', { share: false, convert: false }),
    );
    expect(screen.queryByText('Also convert my existing instances?')).not.toBeInTheDocument();
  });

  it('shows an error instead of a default for a malformed stored value', async () => {
    answer({
      get_content_sharing: new Error(
        "content placement setting is invalid: 'share_content_between_instances' must be true or false, found \"sometimes\"",
      ),
    });
    renderCard();

    expect(await screen.findByRole('alert')).toHaveTextContent(/could not read the sharing setting/);
    expect(screen.queryByRole('switch', { name: 'Share mod files between instances' })).not.toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Try again' })).toBeInTheDocument();
  });
});
