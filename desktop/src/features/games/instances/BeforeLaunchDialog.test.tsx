import { describe, expect, it, vi } from 'vitest';
import { fireEvent, render, screen } from '@testing-library/react';
import { BeforeLaunchDialog } from './InstanceHero';
import type { LaunchReply } from './api';

function reply(overrides: Partial<LaunchReply>): LaunchReply {
  return {
    status: 'before_launch_needed',
    pid: null,
    deployment: null,
    notice: null,
    findings: [],
    reason: null,
    next: null,
    beforeLaunch: [],
    beforeLaunchFailure: null,
    ...overrides,
  };
}

describe('the before-launch question', () => {
  it('shows no dialog for a run-mode tool', () => {
    render(
      <BeforeLaunchDialog
        reply={reply({ beforeLaunch: [{ tool: 'deploy', name: 'REDmod deploy', mode: 'run', reason: 'it has never run here' }] })}
        busy={false}
        onRun={() => undefined}
        onLaunchAnyway={() => undefined}
        onCancel={() => undefined}
      />,
    );
    expect(screen.queryByRole('dialog')).toBeNull();
  });

  it('names one ask-mode tool and offers all three answers', () => {
    const onRun = vi.fn();
    const onLaunchAnyway = vi.fn();
    const onCancel = vi.fn();
    render(
      <BeforeLaunchDialog
        reply={reply({ beforeLaunch: [{ tool: 'deploy', name: 'REDmod deploy', mode: 'ask', reason: 'it has never run here' }] })}
        busy={false}
        onRun={onRun}
        onLaunchAnyway={onLaunchAnyway}
        onCancel={onCancel}
      />,
    );
    const dialog = screen.getByRole('dialog');
    expect(dialog).toHaveTextContent('REDmod deploy should run first');
    expect(dialog).toHaveTextContent('It hasn’t run for this instance yet.');
    expect(screen.getByRole('button', { name: 'Run and launch' }).parentElement).toHaveClass('sm:flex-wrap');
    fireEvent.click(screen.getByRole('button', { name: 'Run and launch' }));
    expect(onRun).toHaveBeenCalledWith([expect.objectContaining({ tool: 'deploy', mode: 'ask' })]);
    fireEvent.click(screen.getByRole('button', { name: 'Launch anyway' }));
    expect(onLaunchAnyway).toHaveBeenCalledTimes(1);
    fireEvent.click(screen.getByRole('button', { name: 'Cancel' }));
    expect(onCancel).toHaveBeenCalledTimes(1);
  });

  it('says plainly when it cannot tell whether a tool needs to run', () => {
    render(
      <BeforeLaunchDialog
        reply={reply({ beforeLaunch: [{ tool: 'deploy', name: 'REDmod deploy', mode: 'ask', reason: 'Agora cannot tell whether its output is current' }] })}
        busy={false}
        onRun={() => undefined}
        onLaunchAnyway={() => undefined}
        onCancel={() => undefined}
      />,
    );
    expect(screen.getByRole('dialog')).toHaveTextContent("Agora can't tell whether it has run since your mods last changed.");
  });

  it('lists several tools under “These should run first”', () => {
    render(
      <BeforeLaunchDialog
        reply={reply({
          beforeLaunch: [
            { tool: 'a', name: 'Deploy', mode: 'ask', reason: 'its output is out of date' },
            { tool: 'b', name: 'Sort', mode: 'ask', reason: 'it has never run here' },
          ],
        })}
        busy={false}
        onRun={() => undefined}
        onLaunchAnyway={() => undefined}
        onCancel={() => undefined}
      />,
    );
    const dialog = screen.getByRole('dialog');
    expect(dialog).toHaveTextContent('These should run first');
    expect(dialog).toHaveTextContent('Deploy');
    expect(dialog).toHaveTextContent('Sort');
    expect(dialog).toHaveTextContent('Your mods have changed since it last ran.');
  });

  it('shows a failed run as plain text and offers launching anyway', () => {
    const onLaunchAnyway = vi.fn();
    render(
      <BeforeLaunchDialog
        reply={reply({
          status: 'before_launch_failed',
          beforeLaunchFailure: { tool: 'deploy', name: 'REDmod deploy', message: 'the tool exited with code 1' },
        })}
        busy={false}
        onRun={() => undefined}
        onLaunchAnyway={onLaunchAnyway}
        onCancel={() => undefined}
      />,
    );
    const dialog = screen.getByRole('dialog');
    expect(dialog).toHaveTextContent('REDmod deploy didn’t finish');
    expect(dialog).toHaveTextContent('the tool exited with code 1');
    fireEvent.click(screen.getByRole('button', { name: 'Launch anyway' }));
    expect(onLaunchAnyway).toHaveBeenCalledTimes(1);
  });
});
