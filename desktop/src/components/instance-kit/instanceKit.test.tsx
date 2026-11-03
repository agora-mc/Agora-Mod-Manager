import { fireEvent, render, screen, within } from '@testing-library/react';
import { useState } from 'react';
import { describe, expect, it, vi } from 'vitest';
import { Boxes } from 'lucide-react';
import {
  ChoiceCards,
  EmptyState,
  FormField,
  InstanceActionBar,
  InstanceCardFrame,
  InstanceGrid,
  InstancePageHeader,
  InstanceTabBar,
  PageHero,
  PageHeroAction,
  ReadinessLine,
  capabilityFor,
  emphasisOf,
  recommendedLabel,
  supportedActions,
  type InstanceAction,
} from '.';
import { BackButton } from '@/components/ui/back-button';

const noop = () => undefined;
const action = (id: InstanceAction['id'], extra: Partial<InstanceAction> = {}): InstanceAction => ({
  id,
  label: id[0].toUpperCase() + id.slice(1),
  onSelect: noop,
  ...extra,
});

describe('capabilities', () => {
  const all = [action('launch'), action('edit'), action('clone'), action('group'), action('review'), action('delete')];

  it('keeps every action when no capabilities are given', () => {
    expect(supportedActions(all).map((a) => a.id)).toEqual(['launch', 'edit', 'clone', 'group', 'review', 'delete']);
  });

  it('keeps only Edit and Review for a game that supports nothing yet', () => {
    expect(supportedActions(all, {}).map((a) => a.id)).toEqual(['edit', 'review']);
  });

  it('treats a capability set to false as unsupported', () => {
    expect(supportedActions(all, { launch: true, clone: false, delete: true }).map((a) => a.id))
      .toEqual(['launch', 'edit', 'review', 'delete']);
  });

  it('maps Stop to the kill capability and leaves Edit ungated', () => {
    expect(capabilityFor('stop')).toBe('kill');
    expect(capabilityFor('edit')).toBeNull();
  });

  it('gives Launch the primary weight, Stop its own, and Delete the quiet danger weight', () => {
    expect(emphasisOf(action('launch'))).toBe('primary');
    expect(emphasisOf(action('stop'))).toBe('stop');
    expect(emphasisOf(action('delete'))).toBe('danger');
    expect(emphasisOf(action('edit'))).toBe('secondary');
    expect(emphasisOf(action('edit', { emphasis: 'primary' }))).toBe('primary');
  });
});

describe('InstanceActionBar', () => {
  it('leaves out unsupported actions instead of greying them out', () => {
    render(
      <InstanceActionBar
        actions={[action('launch'), action('edit'), action('clone'), action('delete')]}
        capabilities={{ launch: true, delete: true }}
      />,
    );
    expect(screen.getAllByRole('button').map((b) => b.textContent)).toEqual(['Launch', 'Edit', 'Delete']);
    expect(screen.queryByRole('button', { name: 'Clone' })).toBeNull();
    for (const button of screen.getAllByRole('button')) expect(button).toBeEnabled();
  });

  it('renders nothing when no action is supported', () => {
    const { container } = render(<InstanceActionBar actions={[action('clone')]} capabilities={{}} />);
    expect(container).toBeEmptyDOMElement();
  });

  it('pushes Delete to the end of the row and calls the action', () => {
    const onDelete = vi.fn();
    render(<InstanceActionBar actions={[action('edit'), action('delete', { onSelect: onDelete, ariaLabel: 'Delete Grounded' })]} />);
    const remove = screen.getByRole('button', { name: 'Delete Grounded' });
    expect(remove.className).toContain('ml-auto');
    expect(remove.className).toContain('text-destructive');
    fireEvent.click(remove);
    expect(onDelete).toHaveBeenCalledTimes(1);
  });

  it('marks a busy action and ignores presses while it works', () => {
    const onLaunch = vi.fn();
    render(<InstanceActionBar actions={[action('launch', { label: 'Starting…', busy: true, onSelect: onLaunch })]} />);
    const button = screen.getByRole('button', { name: 'Starting…' });
    expect(button).toHaveAttribute('aria-busy', 'true');
    expect(button).toBeDisabled();
  });
});

describe('InstanceCardFrame', () => {
  it('shows the name as a heading, its facts, and skips empty facts', () => {
    render(
      <ul>
        <InstanceCardFrame
          tile={<span>tile</span>}
          name="Grounded"
          status="Never played"
          facts={['Created Oct 1, 2026', null, '']}
          cornerLabel="Locked"
          running
          testId="card"
        />
      </ul>,
    );
    const card = screen.getByTestId('card');
    expect(within(card).getByRole('heading', { name: 'Grounded' })).toBeInTheDocument();
    expect(card).toHaveClass('instance-card--running');
    expect(card).toHaveTextContent('Never played');
    expect(card).toHaveTextContent('Created Oct 1, 2026');
    expect(card).toHaveTextContent('Locked');
    expect(card.querySelectorAll('p')).toHaveLength(2);
  });
});

describe('InstanceGrid', () => {
  const cards = (names: string[]) => names.map((name) => <li key={name}>{name}</li>);

  it('shows skeleton cards while loading, and nothing else', () => {
    render(<InstanceGrid loading groups={[{ id: '', label: 'Instances', count: 1, cards: cards(['A']) }]} empty={<p>Empty</p>} />);
    expect(screen.getByRole('list', { name: 'Loading instances…' })).toHaveAttribute('aria-busy', 'true');
    expect(screen.getAllByTestId('instance-card-skeleton')).toHaveLength(2);
    expect(screen.queryByText('A')).toBeNull();
    expect(screen.queryByText('Empty')).toBeNull();
  });

  it('shows the empty state when every group is empty, including no groups at all', () => {
    const { rerender } = render(<InstanceGrid groups={[]} empty={<p>Empty</p>} />);
    expect(screen.getByText('Empty')).toBeInTheDocument();
    rerender(<InstanceGrid groups={[{ id: '', label: 'Instances', count: 0, cards: [] }]} empty={<p>Empty</p>} />);
    expect(screen.getByText('Empty')).toBeInTheDocument();
  });

  it('lists a single group without a heading', () => {
    render(<InstanceGrid groups={[{ id: '', label: 'Instances', count: 2, cards: cards(['A', 'B']) }]} />);
    expect(within(screen.getByRole('list', { name: 'Instances' })).getAllByRole('listitem')).toHaveLength(2);
    expect(screen.queryByRole('button')).toBeNull();
  });

  it('collapses a group from its heading', () => {
    function Harness() {
      const [collapsed, setCollapsed] = useState<string[]>([]);
      return (
        <InstanceGrid
          groups={[
            { id: 'survival', label: 'Survival', count: 1, cards: cards(['A']) },
            { id: '', label: 'Ungrouped', count: 1, cards: cards(['B']) },
          ]}
          collapsedGroups={collapsed}
          onToggleGroup={(id) => setCollapsed((current) => (current.includes(id) ? current.filter((c) => c !== id) : [...current, id]))}
        />
      );
    }
    render(<Harness />);
    const heading = screen.getByRole('button', { name: /Survival/ });
    expect(heading).toHaveAttribute('aria-expanded', 'true');
    fireEvent.click(heading);
    expect(heading).toHaveAttribute('aria-expanded', 'false');
    expect(screen.queryByText('A')).toBeNull();
    expect(screen.getByText('B')).toBeInTheDocument();
  });
});

describe('PageHero and EmptyState', () => {
  it('shows the title, chips that have content, and the actions', () => {
    const onCreate = vi.fn();
    render(
      <PageHero
        icon={Boxes}
        title="Instances"
        description="Your setups."
        chips={['3 instances', null, '']}
        actions={<PageHeroAction primary onClick={onCreate}>New instance</PageHeroAction>}
        testId="hero"
      />,
    );
    expect(screen.getByRole('heading', { name: 'Instances' })).toBeInTheDocument();
    expect(screen.getByTestId('hero').querySelectorAll('.settings-hero-chip')).toHaveLength(1);
    fireEvent.click(screen.getByRole('button', { name: 'New instance' }));
    expect(onCreate).toHaveBeenCalled();
  });

  it('says what is empty and what to do', () => {
    render(<EmptyState icon={Boxes} title="No instances yet" body="Create one." actions={<button type="button">Create</button>} testId="empty" />);
    expect(screen.getByRole('heading', { name: 'No instances yet' })).toBeInTheDocument();
    expect(screen.getByTestId('empty')).toHaveTextContent('Create one.');
  });
});

describe('InstancePageHeader', () => {
  it('joins facts with dots, shows Launch and the readiness line, and leaves out unsupported actions', () => {
    const onLaunch = vi.fn();
    const onChecks = vi.fn();
    render(
      <InstancePageHeader
        tile={<span>tile</span>}
        name="Grounded"
        facts={['Last played Oct 30, 2026', null, 'Created Oct 1, 2026']}
        secondaryActions={[action('openFolder', { label: 'Open in Folder' }), action('troubleshoot')]}
        capabilities={{ launch: true, troubleshoot: true }}
        primaryAction={action('launch', { ariaLabel: 'Launch Grounded', onSelect: onLaunch })}
        readiness={<ReadinessLine readiness={{ level: 'check', headline: '1 thing to review' }} onSelect={onChecks} />}
        testId="header"
      />,
    );
    expect(screen.getByRole('heading', { name: 'Grounded' })).toBeInTheDocument();
    expect(screen.getByTestId('instance-facts')).toHaveTextContent('Last played Oct 30, 2026·Created Oct 1, 2026');
    expect(screen.queryByRole('button', { name: 'Open in Folder' })).toBeNull();
    expect(screen.getByRole('button', { name: 'Troubleshoot' })).toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: 'Launch Grounded' }));
    expect(onLaunch).toHaveBeenCalled();
    fireEvent.click(screen.getByTestId('instance-readiness'));
    expect(onChecks).toHaveBeenCalled();
    expect(screen.getByTestId('instance-readiness')).toHaveAttribute('data-level', 'check');
  });

  it('shows no Launch at all for a game without the launch capability', () => {
    render(<InstancePageHeader tile={null} name="Grounded" capabilities={{}} primaryAction={action('launch')} />);
    expect(screen.queryByRole('button')).toBeNull();
  });

  it('draws the banner strip only when there is something in it', () => {
    const { container, rerender } = render(<InstancePageHeader tile={null} name="Grounded" banners={false} />);
    expect(container.querySelector('.border-t')).toBeNull();
    rerender(<InstancePageHeader tile={null} name="Grounded" banners={<p>Starting…</p>} />);
    expect(container.querySelector('.border-t')).toHaveTextContent('Starting…');
  });
});

describe('InstanceTabBar', () => {
  it('marks the current page and shows counts that are known', () => {
    const onSelect = vi.fn();
    render(
      <InstanceTabBar
        tabs={[{ id: 'overview', label: 'Overview' }, { id: 'content', label: 'Content', count: 5 }, { id: 'tools', label: 'Tools', count: null }]}
        active="overview"
        onSelect={onSelect}
      />,
    );
    const nav = screen.getByRole('navigation', { name: 'Instance pages' });
    expect(within(nav).getByRole('button', { name: 'Overview' })).toHaveAttribute('aria-current', 'page');
    expect(within(nav).getByRole('button', { name: 'Content (5)' })).not.toHaveAttribute('aria-current');
    fireEvent.click(within(nav).getByRole('button', { name: 'Tools' }));
    expect(onSelect).toHaveBeenCalledWith('tools');
  });
});

describe('ChoiceCards', () => {
  const options = [
    { value: 'linked', label: 'Linked base', description: 'Shares files.', recommended: true },
    { value: 'copied', label: 'Copied base', description: 'Its own copy.' },
  ] as const;

  function Harness({ initial }: { initial: 'linked' | 'copied' }) {
    const [value, setValue] = useState<'linked' | 'copied'>(initial);
    return <ChoiceCards label="Base" options={options} value={value} onChange={setValue} />;
  }

  it('badges the recommended option even when another one is chosen', () => {
    render(<Harness initial="copied" />);
    const linked = screen.getByRole('radio', { name: /Linked base/ });
    expect(linked).toHaveAttribute('aria-checked', 'false');
    expect(linked).toHaveTextContent('Recommended');
    expect(screen.getByRole('radio', { name: /Copied base/ })).toHaveAttribute('aria-checked', 'true');
  });

  it('keeps one tab stop and moves the choice with the arrow keys', () => {
    render(<Harness initial="linked" />);
    const [linked, copied] = screen.getAllByRole('radio');
    expect(linked).toHaveAttribute('tabindex', '0');
    expect(copied).toHaveAttribute('tabindex', '-1');
    fireEvent.keyDown(linked, { key: 'ArrowRight' });
    expect(copied).toHaveAttribute('aria-checked', 'true');
    expect(copied).toHaveFocus();
    fireEvent.keyDown(copied, { key: 'ArrowRight' });
    expect(linked).toHaveAttribute('aria-checked', 'true');
  });

  it('says "(recommended)" where a badge cannot go', () => {
    expect(recommendedLabel({ label: 'Automatic', recommended: true })).toBe('Automatic (recommended)');
    expect(recommendedLabel({ label: 'Copied files' })).toBe('Copied files');
  });
});

describe('FormField', () => {
  it('labels the control and points it at its hint', () => {
    render(
      <FormField label="Name" hint="Type a name to enable Create.">
        {(control) => <input {...control} />}
      </FormField>,
    );
    const input = screen.getByLabelText('Name');
    expect(input).toHaveAccessibleDescription('Type a name to enable Create.');
    expect(input).not.toHaveAttribute('aria-invalid');
  });

  it('marks an error and describes the control with it', () => {
    render(
      <FormField label="Name" error="That name is taken.">
        {(control) => <input {...control} />}
      </FormField>,
    );
    const input = screen.getByLabelText('Name');
    expect(input).toHaveAttribute('aria-invalid', 'true');
    expect(input).toHaveAccessibleDescription('That name is taken.');
  });
});

describe('BackButton', () => {
  it('says Back by default and where it goes when told', () => {
    const onBack = vi.fn();
    const { rerender } = render(<BackButton onBack={onBack} />);
    expect(screen.getByRole('button', { name: '← Back' })).toBeInTheDocument();
    rerender(<BackButton onBack={onBack} label="Instances" />);
    fireEvent.click(screen.getByRole('button', { name: '← Instances' }));
    expect(onBack).toHaveBeenCalled();
  });
});
