/**
 * The small form a tool asks for before it runs (MASTER_SPEC §30.12). It shows one control per
 * declared input, keeps the values the player saved last time, and refuses to run while a
 * required input is missing.
 */

import { useState } from 'react';
import { Button } from '@/components/ui/button';
import {
  pickToolFile,
  pickToolFolder,
  type ToolInput,
} from './api';

export interface ToolInputsFormProps {
  inputs: ToolInput[];
  /** The values saved on the instance, by input id. */
  initialValues: Record<string, string>;
  busy: boolean;
  submitLabel: string;
  onSubmit: (values: Record<string, string>) => void | Promise<void>;
  onCancel?: () => void;
}

/** The value an input starts with: its saved value, else its default. */
function startingValue(input: ToolInput, saved: Record<string, string>): string {
  const value = saved[input.id];
  if (value !== undefined && value !== '') return value;
  return input.default ?? '';
}

export function ToolInputsForm({ inputs, initialValues, busy, submitLabel, onSubmit, onCancel }: ToolInputsFormProps) {
  const [values, setValues] = useState<Record<string, string>>(() =>
    Object.fromEntries(inputs.map((input) => [input.id, startingValue(input, initialValues)])),
  );
  const [picking, setPicking] = useState<string | null>(null);

  const set = (id: string, value: string) => setValues((current) => ({ ...current, [id]: value }));
  const missing = inputs.find((input) => input.required && values[input.id]?.trim() === '');

  const choosePath = async (input: ToolInput) => {
    setPicking(input.id);
    try {
      const picked = input.kind === 'folder'
        ? await pickToolFolder(input.label)
        : await pickToolFile(input.label);
      if (picked !== null) set(input.id, picked);
    } finally {
      setPicking(null);
    }
  };

  return (
    <form
      className="space-y-3 rounded-lg border border-border p-4 text-sm"
      data-testid="tool-inputs-form"
      onSubmit={(event) => {
        event.preventDefault();
        if (missing === undefined && !busy) void onSubmit(values);
      }}
    >
      {inputs.map((input) => {
        const value = values[input.id] ?? '';
        const id = `tool-input-${input.id}`;
        return (
          <div key={input.id} className="space-y-1">
            {input.kind !== 'flag' && (
              <label htmlFor={id} className="block font-medium">
                {input.label}{input.required ? ' (required)' : ''}
              </label>
            )}
            {input.kind === 'flag' ? (
              <label className="flex items-center gap-2">
                <input
                  id={id}
                  type="checkbox"
                  checked={value === 'true'}
                  disabled={busy}
                  onChange={(event) => set(input.id, event.target.checked ? 'true' : 'false')}
                />
                <span>{input.label}</span>
              </label>
            ) : input.kind === 'choice' ? (
              <select
                id={id}
                className="h-10 w-full rounded-md border border-input bg-background px-3 text-sm"
                value={value}
                disabled={busy}
                onChange={(event) => set(input.id, event.target.value)}
              >
                <option value="">Choose…</option>
                {input.choices.map((choice) => <option key={choice} value={choice}>{choice}</option>)}
              </select>
            ) : (
              <div className="flex flex-wrap items-center gap-2">
                <input
                  id={id}
                  type="text"
                  className="h-10 min-w-0 flex-1 rounded-md border border-input bg-background px-3 text-sm"
                  value={value}
                  disabled={busy}
                  onChange={(event) => set(input.id, event.target.value)}
                />
                {input.kind === 'file' || input.kind === 'folder' ? (
                  <Button
                    type="button"
                    variant="outline"
                    size="sm"
                    disabled={busy || picking !== null}
                    onClick={() => void choosePath(input)}
                  >
                    {picking === input.id ? 'Choosing…' : input.kind === 'folder' ? 'Choose folder…' : 'Choose file…'}
                  </Button>
                ) : null}
              </div>
            )}
          </div>
        );
      })}
      {missing !== undefined && (
        <p className="text-xs text-muted-foreground" data-testid="tool-input-hint">
          Fill in {missing.label} to run this.
        </p>
      )}
      <div className="flex flex-wrap gap-2">
        <Button type="submit" disabled={busy || missing !== undefined}>{submitLabel}</Button>
        {onCancel && (
          <Button type="button" variant="outline" onClick={onCancel} disabled={busy}>Cancel</Button>
        )}
      </div>
    </form>
  );
}
