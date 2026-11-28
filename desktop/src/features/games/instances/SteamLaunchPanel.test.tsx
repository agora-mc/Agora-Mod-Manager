/**
 * The Linux Steam/Proton panel: the untested line, the exact launch-options text, and a copy
 * button. Nothing is drawn on a platform the backend says is unsupported.
 */
import { render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { SteamLaunchPanel } from './SteamLaunchPanel';
import { getSteamLaunchInfo } from './api';

vi.mock('./api', async (importOriginal) => {
  const actual = await importOriginal<typeof import('./api')>();
  return { ...actual, getSteamLaunchInfo: vi.fn() };
});

const mocked = vi.mocked(getSteamLaunchInfo);

beforeEach(() => {
  mocked.mockReset();
});

describe('SteamLaunchPanel', () => {
  it('shows the untested line, the wrapper text and copies it', async () => {
    mocked.mockResolvedValue({
      supported: true,
      appId: '4242',
      expected: 'agora games wrap %command%',
      configured: null,
      wrapperPresent: false,
      files: ['/home/player/.steam/steam/userdata/1/config/localconfig.vdf'],
      problems: [],
      note: 'Launching through Proton on Linux has not been tested on real hardware yet.',
    });
    const writeText = vi.fn(async () => undefined);
    Object.assign(navigator, { clipboard: { writeText } });
    render(<SteamLaunchPanel instanceId="grounded-1" />);

    expect(await screen.findByRole('note')).toHaveTextContent(
      'Launching through Proton on Linux has not been tested on real hardware yet.',
    );
    expect(screen.getByTestId('steam-launch-text')).toHaveTextContent(
      'agora games wrap %command%',
    );
    await userEvent.click(screen.getByRole('button', { name: /copy/i }));
    expect(writeText).toHaveBeenCalledWith('agora games wrap %command%');
    await waitFor(() => expect(screen.getByRole('button', { name: /copied/i })).toBeInTheDocument());
  });

  it('draws nothing when the command says the platform is unsupported', async () => {
    mocked.mockResolvedValue({
      supported: false,
      appId: null,
      expected: '',
      configured: null,
      wrapperPresent: false,
      files: [],
      problems: [],
      note: '',
    });
    const { container } = render(<SteamLaunchPanel instanceId="grounded-1" />);
    await waitFor(() => expect(mocked).toHaveBeenCalled());
    expect(container).toBeEmptyDOMElement();
  });

  it('names an unreadable launch-options file instead of hiding it', async () => {
    mocked.mockResolvedValue({
      supported: true,
      appId: '4242',
      expected: 'agora games wrap %command%',
      configured: null,
      wrapperPresent: false,
      files: ['/home/player/.steam/steam/userdata/1/config/localconfig.vdf'],
      problems: ['/home/player/.steam/steam/userdata/1/config/localconfig.vdf: line 2: unexpected end'],
      note: 'Launching through Proton on Linux has not been tested on real hardware yet.',
    });
    render(<SteamLaunchPanel instanceId="grounded-1" />);
    expect(await screen.findByRole('alert')).toHaveTextContent('Could not read');
  });
});
