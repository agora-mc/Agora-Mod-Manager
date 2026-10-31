/**
 * The Tools tab's own list: candidates offered by Find tools, the ones already added, and what
 * "Add selected" does with the ticked rows.
 */
import { fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { InstanceToolsTab } from './InstanceToolsTab';
import {
  addUserTool,
  findUserTools,
  gameToolSwapStatus,
  listGameTools,
  listUserTools,
  listUserToolOutputs,
  removeUserTool,
  runUserTool,
  useUserToolOutput,
  type ToolCandidate,
} from './api';

vi.mock('./api', async (importOriginal) => {
  const actual = await importOriginal<typeof import('./api')>();
  return {
    ...actual,
    addUserTool: vi.fn(),
    findUserTools: vi.fn(),
    gameToolSwapStatus: vi.fn(),
    listGameTools: vi.fn(),
    listUserTools: vi.fn(),
    listUserToolOutputs: vi.fn(),
    removeUserTool: vi.fn(),
    restoreGameToolSwap: vi.fn(),
    runUserTool: vi.fn(),
    useUserToolOutput: vi.fn(),
  };
});

const unknown: ToolCandidate = {
  exe: 'Data/tools/FNIS.exe',
  modId: 'aaaa1111',
  modName: 'FNIS',
  knownToolId: null,
  suggestedName: 'FNIS',
  alreadyAdded: false,
};

const known: ToolCandidate = {
  exe: 'Data/CalienteTools/BodySlide/BodySlide x64.exe',
  modId: 'bbbb2222',
  modName: 'BodySlide and Outfit Studio',
  knownToolId: 'bodyslide',
  suggestedName: 'BodySlide',
  alreadyAdded: false,
};

const added: ToolCandidate = {
  exe: 'Data/tools/Old.exe',
  modId: 'cccc3333',
  modName: 'Old Tools',
  knownToolId: null,
  suggestedName: 'Old',
  alreadyAdded: true,
};

beforeEach(() => {
  vi.mocked(listGameTools).mockReset().mockResolvedValue({ instanceId: 'inst-1', tools: [] });
  vi.mocked(listUserTools).mockReset().mockResolvedValue({ instanceId: 'inst-1', tools: [] });
  vi.mocked(gameToolSwapStatus).mockReset().mockResolvedValue({ interrupted: null });
  vi.mocked(findUserTools).mockReset().mockResolvedValue([unknown, known, added]);
  vi.mocked(addUserTool).mockReset().mockImplementation(async (_id, tool) => ({
    id: 'made-1',
    toolId: 'user-made1',
    name: tool.name,
    exe: tool.exe,
    args: tool.args,
    knownToolId: null,
    outputIsMod: tool.outputIsMod ?? false,
    outputFolder: tool.outputFolder ?? null,
  }));
  vi.mocked(removeUserTool).mockReset();
  vi.mocked(runUserTool).mockReset();
  vi.mocked(listUserToolOutputs).mockReset().mockResolvedValue([]);
  vi.mocked(useUserToolOutput).mockReset();
});

function mount() {
  return render(<InstanceToolsTab gameId="skyrim-se" instanceId="inst-1" />);
}

describe('Find tools', () => {
  it('offers the candidates, greys out one already added, and adds only the ticked rows', async () => {
    mount();
    await screen.findByTestId('user-tools-empty');
    fireEvent.click(screen.getByRole('button', { name: 'Find tools' }));

    const rows = await screen.findAllByTestId('tool-candidate');
    expect(rows).toHaveLength(3);
    expect(within(rows[0]).getByDisplayValue('FNIS')).toBeInTheDocument();
    expect(within(rows[0]).getByText(/From FNIS/)).toBeInTheDocument();
    expect(within(rows[1]).getByText('Known tool')).toBeInTheDocument();
    expect(within(rows[2]).getByText('Already added')).toBeInTheDocument();

    const addOld = within(rows[2]).getByRole('checkbox');
    expect(addOld).toBeDisabled();

    const addBodySlide = within(rows[1]).getByRole('checkbox', { name: /BodySlide/ });
    fireEvent.click(addBodySlide);
    fireEvent.click(screen.getByRole('button', { name: 'Add selected' }));

    await waitFor(() => expect(addUserTool).toHaveBeenCalledTimes(1));
    expect(addUserTool).toHaveBeenCalledWith('inst-1', {
      name: 'FNIS',
      exe: 'Data/tools/FNIS.exe',
      args: [],
    });
    expect(await screen.findByText('Added 1 tool.')).toBeInTheDocument();
  });

  it('says when the mods hold no tools', async () => {
    vi.mocked(findUserTools).mockResolvedValue([]);
    mount();
    await screen.findByTestId('user-tools-empty');
    fireEvent.click(screen.getByRole('button', { name: 'Find tools' }));
    expect(await screen.findByText('No tools found in this instance’s mods')).toBeInTheDocument();
  });

  it('names the failure when the search cannot run, rather than showing no tools', async () => {
    vi.mocked(findUserTools).mockRejectedValue({ code: 'core_failed', message: 'The base is missing.' });
    mount();
    await screen.findByTestId('user-tools-empty');
    fireEvent.click(screen.getByRole('button', { name: 'Find tools' }));
    const alert = await screen.findByRole('alert');
    expect(alert).toHaveTextContent('Your mods could not be searched: The base is missing.');
    expect(screen.queryByText('No tools found in this instance’s mods')).toBeNull();
  });
});

describe('the list of the person’s own tools', () => {
  it('lists a tool with its executable and removes it', async () => {
    vi.mocked(listUserTools).mockResolvedValue({
      instanceId: 'inst-1',
      tools: [{ id: 'tool-1', toolId: 'user-tool1', name: 'FNIS', exe: 'Data/tools/FNIS.exe', args: ['-a'], knownToolId: null, outputIsMod: false, outputFolder: null }],
    });
    vi.mocked(removeUserTool).mockResolvedValue({ instanceId: 'inst-1', tools: [] });
    mount();
    const card = await screen.findByTestId('user-tool-card');
    expect(within(card).getByText('Data/tools/FNIS.exe')).toBeInTheDocument();
    expect(within(card).getByText('Starts with: -a')).toBeInTheDocument();
    fireEvent.click(within(card).getByRole('button', { name: 'Remove' }));
    await waitFor(() => expect(removeUserTool).toHaveBeenCalledWith('inst-1', 'tool-1'));
  });

  it('says why the list cannot be read instead of showing it as empty', async () => {
    vi.mocked(listUserTools).mockRejectedValue({ code: 'core_failed', message: 'The file is malformed.' });
    mount();
    const alert = await screen.findByTestId('user-tools-error');
    expect(alert).toHaveTextContent('Your own tools could not be listed: The file is malformed.');
    expect(screen.queryByTestId('user-tools-empty')).toBeNull();
  });
});

describe('running the person’s own tools', () => {
  const fnis = {
    id: 'tool-1',
    toolId: 'user-tool1',
    name: 'FNIS',
    exe: 'Data/tools/FNIS.exe',
    args: ['-a'],
    knownToolId: null,
    outputIsMod: false,
    outputFolder: null,
  };

  it('runs a tool from its card through the user-tool command', async () => {
    vi.mocked(listUserTools).mockResolvedValue({ instanceId: 'inst-1', tools: [fnis] });
    vi.mocked(runUserTool).mockResolvedValue({
      status: 'finished',
      outcome: {
        toolId: 'user-tool1',
        name: 'FNIS',
        run: 'run-1',
        capture: 'vfs',
        captureReason: null,
        promoted: true,
        current: '1',
        previous: null,
        written: ['Data/tools/out.txt'],
        deleted: [],
        exitCode: 0,
        cancelled: false,
        failedFolder: null,
        failure: null,
        copiedLayer: null,
        installChanges: { created: [], changed: [], deleted: [], mayBeIncomplete: false, unavailable: null },
        swap: null,
        offers: [],
        unchangedDropped: 0,
      },
    });
    mount();
    await screen.findByTestId('user-tool-card');
    fireEvent.click(screen.getByRole('button', { name: 'Build FNIS' }));
    await waitFor(() => expect(runUserTool).toHaveBeenCalledWith('inst-1', 'tool-1', 'auto', {}));
    expect(await screen.findByTestId('tool-outcome')).toBeInTheDocument();
  });

  it('adds a tool whose output is a mod, with the folder the person chose', async () => {
    mount();
    await screen.findByTestId('user-tools-empty');
    fireEvent.change(screen.getByLabelText('Name'), { target: { value: 'DynDOLOD' } });
    fireEvent.change(screen.getByLabelText('Executable'), { target: { value: 'C:/Modding/DynDOLOD/DynDOLODx64.exe' } });
    fireEvent.click(screen.getByLabelText('Its output is a mod'));
    fireEvent.change(screen.getByLabelText('Output folder (relative to the executable, or absolute)'), { target: { value: 'DynDOLOD_Output' } });
    fireEvent.click(screen.getByRole('button', { name: 'Add this tool' }));
    await waitFor(() => expect(addUserTool).toHaveBeenCalledWith('inst-1', {
      name: 'DynDOLOD',
      exe: 'C:/Modding/DynDOLOD/DynDOLODx64.exe',
      args: [],
      outputIsMod: true,
      outputFolder: 'DynDOLOD_Output',
    }));
  });

  it('offers an earlier output and points the layer back at it', async () => {
    vi.mocked(listUserTools).mockResolvedValue({
      instanceId: 'inst-1',
      tools: [{ ...fnis, outputIsMod: true, outputFolder: 'DynDOLOD_Output' }],
    });
    vi.mocked(listUserToolOutputs).mockResolvedValue([
      { itemId: 'aaaa', name: 'FNIS Output (2026-10-31 14:02)', addedAtUnixMs: 1 },
    ]);
    vi.mocked(useUserToolOutput).mockResolvedValue([
      { itemId: 'aaaa', name: 'FNIS Output (2026-10-31 14:02)', addedAtUnixMs: 1 },
    ]);
    mount();
    await screen.findByTestId('user-tool-card');
    fireEvent.click(screen.getByRole('button', { name: 'Use an earlier output' }));
    fireEvent.click(await screen.findByRole('button', { name: 'Use this output' }));
    await waitFor(() => expect(useUserToolOutput).toHaveBeenCalledWith('inst-1', 'tool-1', 'aaaa'));
  });
});
