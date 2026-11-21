import { describe, expect, it, vi } from 'vitest';
import { fireEvent, render, screen } from '@testing-library/react';
import { ToolInputsForm } from './ToolInputsForm';
import type { ToolInput } from './api';

const input = (overrides: Partial<ToolInput> & Pick<ToolInput, 'id' | 'label' | 'kind'>): ToolInput => ({
  required: false,
  default: null,
  choices: [],
  arg: null,
  ...overrides,
});

describe('a tool’s input form', () => {
  it('prefills saved values and submits them', async () => {
    const onSubmit = vi.fn();
    render(
      <ToolInputsForm
        inputs={[input({ id: 'profile', label: 'Profile', kind: 'text' })]}
        initialValues={{ profile: 'my-profile' }}
        busy={false}
        submitLabel="Run Tool"
        onSubmit={onSubmit}
      />,
    );
    expect(screen.getByLabelText('Profile')).toHaveValue('my-profile');
    fireEvent.click(screen.getByRole('button', { name: 'Run Tool' }));
    expect(onSubmit).toHaveBeenCalledWith({ profile: 'my-profile' });
  });

  it('blocks Run and says what is missing when a required input is empty', () => {
    render(
      <ToolInputsForm
        inputs={[input({ id: 'profile', label: 'Profile', kind: 'text', required: true })]}
        initialValues={{}}
        busy={false}
        submitLabel="Run Tool"
        onSubmit={() => undefined}
      />,
    );
    expect(screen.getByRole('button', { name: 'Run Tool' })).toBeDisabled();
    expect(screen.getByTestId('tool-input-hint')).toHaveTextContent('Fill in Profile to run this.');
    fireEvent.change(screen.getByLabelText('Profile (required)'), { target: { value: 'chosen' } });
    expect(screen.getByRole('button', { name: 'Run Tool' })).toBeEnabled();
  });

  it('uses the right control for each kind', () => {
    render(
      <ToolInputsForm
        inputs={[
          input({ id: 'out', label: 'Output folder', kind: 'folder' }),
          input({ id: 'choice', label: 'Profile', kind: 'choice', choices: ['A', 'B'] }),
          input({ id: 'yes', label: 'Keep output', kind: 'flag' }),
        ]}
        initialValues={{ choice: 'B', yes: 'true' }}
        busy={false}
        submitLabel="Run"
        onSubmit={() => undefined}
      />,
    );
    expect(screen.getByRole('combobox', { name: 'Profile' })).toHaveValue('B');
    expect(screen.getByRole('checkbox', { name: 'Keep output' })).toBeChecked();
    expect(screen.getByRole('button', { name: 'Choose folder…' })).toBeInTheDocument();
  });
});
