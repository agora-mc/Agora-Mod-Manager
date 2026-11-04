/**
 * The Tools tab: only the tools present in the instance, the empty state, tools that are no longer
 * installed, the player's own tools, and "Add a tool…".
 */
import { fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { InstanceToolsTab } from './InstanceToolsTab';
import { nameFromExe, splitArguments } from './AddToolPanel';
import {
  addUserTool,
  findUserTools,
  gameToolSwapStatus,
  listGameTools,
  listUserTools,
  listUserToolOutputs,
  pickProgram,
  removeUserTool,
  runUserTool,
  useUserToolOutput,
  type ToolCandidate,
  type ToolRow,
  type UserToolRow,
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
    pickProgram: vi.fn(),
    removeUserTool: vi.fn(),
    restoreGameToolSwap: vi.fn(),
    runUserTool: vi.fn(),
    useUserToolOutput: vi.fn(),
  };
});

const KNOWN = ['Nemesis', 'BodySlide', 'Pandora Behaviour Engine', 'TexGen', 'DynDOLOD', 'PGPatcher'];

const nemesis: ToolRow = {
  toolId: 'nemesis',
  name: 'Nemesis',
  declared: true,
  current: null,
  previous: null,
  status: null,
  conflicts: false,
  outputKind: 'generated_layer',
  outputFolder: null,
  installed: true,
  exe: 'Data/Nemesis_Engine/Nemesis Unlimited Behavior Engine.exe',
  fromMod: 'Nemesis Unlimited Behavior Engine',
  runOnly: false,
};

const fnis: UserToolRow = {
  id: 'tool-1',
  toolId: 'user-tool1',
  name: 'FNIS',
  exe: 'Data/tools/FNIS.exe',
  args: ['-a'],
  knownToolId: null,
  outputIsMod: false,
  outputFolder: null,
  missing: false,
  keepsOutput: false,
};

const unknown: ToolCandidate = {
  exe: 'Data/tools/FNIS.exe',
  modId: 'aaaa1111',
  modName: 'FNIS',
  knownToolId: null,
  suggestedName: 'FNIS',
  alreadyAdded: false,
};

const knownListed: ToolCandidate = {
  exe: 'Data/Nemesis_Engine/Nemesis Unlimited Behavior Engine.exe',
  modId: 'bbbb2222',
  modName: 'Nemesis Unlimited Behavior Engine',
  knownToolId: 'nemesis',
  suggestedName: 'Nemesis',
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
  vi.mocked(listGameTools).mockReset().mockResolvedValue({ instanceId: 'inst-1', tools: [], known: KNOWN });
  vi.mocked(listUserTools).mockReset().mockResolvedValue({ instanceId: 'inst-1', tools: [] });
  vi.mocked(gameToolSwapStatus).mockReset().mockResolvedValue({ interrupted: null });
  vi.mocked(findUserTools).mockReset().mockResolvedValue([unknown, knownListed, added]);
  vi.mocked(pickProgram).mockReset().mockResolvedValue(null);
  vi.mocked(addUserTool).mockReset().mockImplementation(async (_id, tool) => ({
    id: 'made-1',
    toolId: 'user-made1',
    name: tool.name,
    exe: tool.exe,
    args: tool.args,
    knownToolId: null,
    outputIsMod: tool.outputIsMod ?? false,
    outputFolder: tool.outputFolder ?? null,
    missing: false,
    keepsOutput: tool.keepsOutput ?? false,
  }));
  vi.mocked(removeUserTool).mockReset();
  vi.mocked(runUserTool).mockReset();
  vi.mocked(listUserToolOutputs).mockReset().mockResolvedValue([]);
  vi.mocked(useUserToolOutput).mockReset();
});

function mount() {
  return render(<InstanceToolsTab gameId="skyrim-se" instanceId="inst-1" />);
}

describe('the list', () => {
  it('shows a friendly empty state naming the tools that appear when installed, with a way to add one', async () => {
    mount();
    const empty = await screen.findByTestId('tools-empty');
    expect(empty).toHaveTextContent('No tools yet');
    expect(empty).toHaveTextContent('Tools such as Nemesis, BodySlide, Pandora Behaviour Engine and others show up here by themselves when you install them as mods.');
    expect(screen.queryByTestId('tool-card')).toBeNull();
    expect(screen.getByRole('heading', { name: 'Tools (0)' })).toBeInTheDocument();

    fireEvent.click(within(empty).getByRole('button', { name: 'Add a tool…' }));
    expect(await screen.findByTestId('tool-add')).toBeInTheDocument();
    expect(screen.queryByTestId('tools-empty')).toBeNull();
  });

  it('lists an installed tool with the mod it came from, and nothing for tools that are not installed', async () => {
    vi.mocked(listGameTools).mockResolvedValue({ instanceId: 'inst-1', tools: [nemesis], known: KNOWN });
    mount();
    const card = await screen.findByRole('article', { name: 'Nemesis' });
    expect(within(card).getByTestId('tool-origin')).toHaveTextContent('From the mod “Nemesis Unlimited Behavior Engine”');
    expect(within(card).getByTestId('tool-exe')).toHaveTextContent('Data/Nemesis_Engine/Nemesis Unlimited Behavior Engine.exe');
    expect(within(card).getByRole('button', { name: 'Build Nemesis' })).toBeInTheDocument();
    expect(screen.getAllByTestId('tool-card')).toHaveLength(1);
    expect(screen.getByRole('heading', { name: 'Tools (1)' })).toBeInTheDocument();
    expect(screen.queryByTestId('tools-empty')).toBeNull();
  });

  it('keeps a tool whose mod was turned off while it has output, says so, and offers no run', async () => {
    vi.mocked(listGameTools).mockResolvedValue({
      instanceId: 'inst-1',
      known: KNOWN,
      tools: [{ ...nemesis, installed: false, exe: null, fromMod: null, current: '2', previous: '1', status: 'stale' }],
    });
    mount();
    const card = await screen.findByRole('article', { name: 'Nemesis' });
    expect(within(card).getByTestId('tool-missing')).toHaveTextContent('Not installed');
    expect(within(card).queryByTestId('tool-status')).toBeNull();
    expect(within(card).queryByText(/A mod, the load order or a setting changed/)).toBeNull();
    expect(within(card).getByTestId('tool-unavailable')).toHaveTextContent('the mod that had it is off or was removed');
    expect(within(card).queryByRole('button', { name: /Rebuild Nemesis|Build Nemesis|Run Nemesis/ })).toBeNull();
    expect(within(card).getByRole('button', { name: 'Remove output' })).toBeInTheDocument();
    expect(within(card).getByRole('button', { name: 'Roll back' })).toBeInTheDocument();
  });

  it('shows a tool the player added that the game recognises once, as the game’s tool', async () => {
    vi.mocked(listGameTools).mockResolvedValue({ instanceId: 'inst-1', tools: [nemesis], known: KNOWN });
    vi.mocked(listUserTools).mockResolvedValue({
      instanceId: 'inst-1',
      tools: [{ ...fnis, id: 'n-1', toolId: 'user-n1', name: 'Nemesis (mine)', exe: nemesis.exe ?? '', knownToolId: 'nemesis' }],
    });
    mount();
    await screen.findByRole('article', { name: 'Nemesis' });
    expect(screen.getAllByTestId('tool-card')).toHaveLength(1);
    expect(screen.queryByTestId('user-tool-card')).toBeNull();
  });

  it('says why the player’s own tools cannot be read instead of showing them as empty', async () => {
    vi.mocked(listUserTools).mockRejectedValue({ code: 'core_failed', message: 'The file is malformed.' });
    mount();
    const alert = await screen.findByTestId('user-tools-error');
    expect(alert).toHaveTextContent('The tools you added could not be listed: The file is malformed.');
    expect(screen.queryByTestId('tools-empty')).toBeNull();
  });
});

describe('run-only tools', () => {
  it('offer a plain Run, with no build status and no capture choice', async () => {
    vi.mocked(listGameTools).mockResolvedValue({
      instanceId: 'inst-1',
      known: KNOWN,
      tools: [{ ...nemesis, toolId: 'sseedit', name: 'SSEEdit', runOnly: true, exe: 'Data/SSEEdit/SSEEdit.exe', fromMod: 'SSEEdit' }],
    });
    vi.mocked(listUserTools).mockResolvedValue({ instanceId: 'inst-1', tools: [fnis] });
    mount();
    const sseedit = await screen.findByRole('article', { name: 'SSEEdit' });
    expect(within(sseedit).getByRole('button', { name: 'Run SSEEdit' })).toBeInTheDocument();
    expect(within(sseedit).queryByTestId('tool-status')).toBeNull();
    expect(within(sseedit).queryByRole('combobox')).toBeNull();
    expect(within(sseedit).getByTestId('tool-run-only')).toHaveTextContent('Anything it writes is kept for this instance');

    const own = within(screen.getByTestId('user-tool-card')).getByRole('article', { name: 'FNIS' });
    expect(within(own).getByRole('button', { name: 'Run FNIS' })).toBeInTheDocument();
    expect(within(own).queryByTestId('tool-status')).toBeNull();
  });

  it('a tool the player said builds output is tracked as a build', async () => {
    vi.mocked(listUserTools).mockResolvedValue({ instanceId: 'inst-1', tools: [{ ...fnis, keepsOutput: true }] });
    mount();
    const card = await screen.findByTestId('user-tool-card');
    expect(within(card).getByRole('button', { name: 'Build FNIS' })).toBeInTheDocument();
    expect(within(card).getByTestId('tool-status')).toHaveTextContent('Not built yet');
    expect(within(card).getByRole('combobox')).toBeInTheDocument();
  });

  it('the add panel passes the choice to track output', async () => {
    mount();
    fireEvent.click(within(await screen.findByTestId('tools-empty')).getByRole('button', { name: 'Add a tool…' }));
    const panel = await screen.findByTestId('tool-add');
    fireEvent.change(within(panel).getByLabelText('Program'), { target: { value: 'Data/tools/FNIS.exe' } });
    fireEvent.change(within(panel).getByLabelText('Name'), { target: { value: 'FNIS' } });
    fireEvent.click(within(panel).getByLabelText('It builds files the game reads'));
    fireEvent.click(within(panel).getByRole('button', { name: 'Add tool' }));
    await waitFor(() => expect(addUserTool).toHaveBeenCalledWith('inst-1', expect.objectContaining({ keepsOutput: true, outputIsMod: false })));
  });
});

describe('the player’s own tools', () => {
  it('lists a tool with its program and arguments, and removes it from the list', async () => {
    vi.mocked(listUserTools).mockResolvedValue({ instanceId: 'inst-1', tools: [fnis] });
    vi.mocked(removeUserTool).mockResolvedValue({ instanceId: 'inst-1', tools: [] });
    mount();
    const card = await screen.findByTestId('user-tool-card');
    expect(within(card).getByTestId('tool-exe')).toHaveTextContent('Data/tools/FNIS.exe');
    expect(within(card).getByTestId('tool-origin')).toHaveTextContent('Added by you · starts with -a');
    fireEvent.click(within(card).getByRole('button', { name: 'Remove from list' }));
    await waitFor(() => expect(removeUserTool).toHaveBeenCalledWith('inst-1', 'tool-1'));
    expect(await screen.findByText('Removed FNIS from the list. No files were deleted.')).toBeInTheDocument();
  });

  it('marks a tool whose mod is off as missing, keeps it, and offers no run', async () => {
    vi.mocked(listUserTools).mockResolvedValue({ instanceId: 'inst-1', tools: [{ ...fnis, missing: true }] });
    mount();
    const card = await screen.findByTestId('user-tool-card');
    expect(within(card).getByTestId('tool-missing')).toBeInTheDocument();
    expect(within(card).getByTestId('tool-unavailable')).toHaveTextContent('Turn the mod back on, or remove the tool from the list.');
    expect(within(card).queryByRole('button', { name: /FNIS/ })).toBeNull();
    expect(within(card).getByRole('button', { name: 'Remove from list' })).toBeInTheDocument();
  });

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
    fireEvent.click(screen.getByRole('button', { name: 'Run FNIS' }));
    await waitFor(() => expect(runUserTool).toHaveBeenCalledWith('inst-1', 'tool-1', 'auto', {}));
    expect(await screen.findByTestId('tool-outcome')).toBeInTheDocument();
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

describe('Add a tool…', () => {
  async function openPanel() {
    mount();
    fireEvent.click(within(await screen.findByTestId('tools-empty')).getByRole('button', { name: 'Add a tool…' }));
    return screen.findByTestId('tool-add');
  }

  it('offers the programs in the mods, but not one already added or a tool already listed', async () => {
    vi.mocked(listGameTools).mockResolvedValue({ instanceId: 'inst-1', tools: [nemesis], known: KNOWN });
    mount();
    fireEvent.click(await screen.findByRole('button', { name: 'Add a tool…' }));
    const found = await screen.findByTestId('tool-add-found');
    const offered = within(found).getAllByRole('button');
    expect(offered).toHaveLength(1);
    expect(offered[0]).toHaveTextContent('FNIS');
    expect(offered[0]).toHaveTextContent('from FNIS');
  });

  it('adds a program picked from the mods, with the arguments split as typed', async () => {
    const panel = await openPanel();
    fireEvent.click(await within(panel).findByRole('button', { name: /FNIS/ }));
    expect(within(panel).getByLabelText('Program')).toHaveValue('Data/tools/FNIS.exe');
    expect(within(panel).getByLabelText('Name')).toHaveValue('FNIS');
    fireEvent.change(within(panel).getByLabelText('Arguments (optional)'), { target: { value: '-o "C:/My Output"' } });
    fireEvent.click(within(panel).getByRole('button', { name: 'Add tool' }));
    await waitFor(() => expect(addUserTool).toHaveBeenCalledWith('inst-1', {
      name: 'FNIS',
      exe: 'Data/tools/FNIS.exe',
      args: ['-o', 'C:/My Output'],
      outputIsMod: false,
      outputFolder: null,
      keepsOutput: false,
    }));
    expect(await screen.findByText('Added FNIS.')).toBeInTheDocument();
    expect(screen.queryByTestId('tool-add')).toBeNull();
  });

  it('browses for a program anywhere, named after its file unless the player typed a name', async () => {
    vi.mocked(pickProgram).mockResolvedValue('C:\\Modding\\SSEEdit\\SSEEdit.exe');
    const panel = await openPanel();
    fireEvent.click(within(panel).getByRole('button', { name: /Browse/ }));
    await waitFor(() => expect(within(panel).getByLabelText('Program')).toHaveValue('C:\\Modding\\SSEEdit\\SSEEdit.exe'));
    expect(within(panel).getByLabelText('Name')).toHaveValue('SSEEdit');

    fireEvent.change(within(panel).getByLabelText('Name'), { target: { value: 'xEdit' } });
    vi.mocked(pickProgram).mockResolvedValue('C:/Modding/SSEEdit/SSEEdit64.exe');
    fireEvent.click(within(panel).getByRole('button', { name: /Browse/ }));
    await waitFor(() => expect(within(panel).getByLabelText('Program')).toHaveValue('C:/Modding/SSEEdit/SSEEdit64.exe'));
    expect(within(panel).getByLabelText('Name')).toHaveValue('xEdit');
  });

  it('adds a tool whose output is a mod, with the folder the player chose', async () => {
    const panel = await openPanel();
    fireEvent.change(within(panel).getByLabelText('Program'), { target: { value: 'C:/Modding/DynDOLOD/DynDOLODx64.exe' } });
    fireEvent.change(within(panel).getByLabelText('Name'), { target: { value: 'DynDOLOD' } });
    fireEvent.click(within(panel).getByLabelText('Its output is a mod'));
    fireEvent.click(within(panel).getByRole('button', { name: 'Add tool' }));
    expect(await within(panel).findByRole('alert')).toHaveTextContent('needs an output folder');
    expect(addUserTool).not.toHaveBeenCalled();

    fireEvent.change(within(panel).getByLabelText('Output folder'), { target: { value: 'DynDOLOD_Output' } });
    fireEvent.click(within(panel).getByRole('button', { name: 'Add tool' }));
    await waitFor(() => expect(addUserTool).toHaveBeenCalledWith('inst-1', {
      name: 'DynDOLOD',
      exe: 'C:/Modding/DynDOLOD/DynDOLODx64.exe',
      args: [],
      outputIsMod: true,
      outputFolder: 'DynDOLOD_Output',
      keepsOutput: false,
    }));
  });

  it('adds nothing without a program, and shows the core’s refusal', async () => {
    vi.mocked(addUserTool).mockRejectedValue({ code: 'core_failed', message: '\'Data/../x.exe\' cannot leave the game folder' });
    const panel = await openPanel();
    fireEvent.click(within(panel).getByRole('button', { name: 'Add tool' }));
    expect(await within(panel).findByRole('alert')).toHaveTextContent('Choose a program first.');
    expect(addUserTool).not.toHaveBeenCalled();

    fireEvent.change(within(panel).getByLabelText('Program'), { target: { value: 'Data/../x.exe' } });
    fireEvent.change(within(panel).getByLabelText('Name'), { target: { value: 'X' } });
    fireEvent.click(within(panel).getByRole('button', { name: 'Add tool' }));
    expect(await within(panel).findByText(/cannot leave the game folder/)).toBeInTheDocument();
    expect(screen.getByTestId('tool-add')).toBeInTheDocument();
  });

  it('names the failure when the mods cannot be searched, and still allows browsing', async () => {
    vi.mocked(findUserTools).mockRejectedValue({ code: 'core_failed', message: 'The base is missing.' });
    const panel = await openPanel();
    expect(await within(panel).findByRole('alert')).toHaveTextContent('Your mods could not be searched: The base is missing.');
    expect(within(panel).getByRole('button', { name: /Browse/ })).toBeEnabled();
  });

  it('says when the mods hold no other programs', async () => {
    vi.mocked(findUserTools).mockResolvedValue([added]);
    const panel = await openPanel();
    expect(await within(panel).findByTestId('tool-add-none-found')).toHaveTextContent('No other programs in this instance’s mods.');
  });
});

describe('the small helpers', () => {
  it('names a program after its file', () => {
    expect(nameFromExe('C:\\Tools\\SSEEdit.EXE')).toBe('SSEEdit');
    expect(nameFromExe('Data/tools/My Tool.exe')).toBe('My Tool');
    expect(nameFromExe('')).toBe('');
  });

  it('splits arguments on spaces outside quotes', () => {
    expect(splitArguments('  -sse   -o "C:/My Output" "" ')).toEqual(['-sse', '-o', 'C:/My Output', '']);
    expect(splitArguments('   ')).toEqual([]);
  });
});
