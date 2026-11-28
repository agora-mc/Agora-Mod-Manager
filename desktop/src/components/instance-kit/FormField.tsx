/**
 * A labelled form control with a hint and an error under it, in the style of Minecraft's create
 * dialog: medium label, a quiet italic placeholder, and a muted hint that says what is missing
 * ("Type a name to enable Create.").
 */

import { useId, type ReactNode } from 'react';
import { cn } from '@/lib/utils';

/** The control style the create dialogs use, for raw `<input>` and `<select>` elements. */
export const FORM_CONTROL_CLASS =
  'mt-1 w-full rounded-lg border border-input bg-background px-3 py-2 text-sm placeholder:italic placeholder:text-muted-foreground/70 focus-visible:outline-hidden focus-visible:ring-2 focus-visible:ring-ring disabled:cursor-not-allowed disabled:opacity-50';

export interface FormFieldControlProps {
  id: string;
  /** The ids of the hint and error, for `aria-describedby`; undefined when neither shows. */
  'aria-describedby': string | undefined;
  'aria-invalid': true | undefined;
}

interface FormFieldProps {
  label: ReactNode;
  /** Renders the control with the ids it needs. */
  children: (control: FormFieldControlProps) => ReactNode;
  hint?: ReactNode;
  error?: ReactNode;
  className?: string;
}

export function FormField({ label, children, hint, error, className }: FormFieldProps) {
  const id = useId();
  const hintId = `${id}-hint`;
  const errorId = `${id}-error`;
  const describedBy = [hint ? hintId : null, error ? errorId : null].filter(Boolean).join(' ') || undefined;
  return (
    <div className={cn('block', className)}>
      <label htmlFor={id} className="text-sm font-medium">{label}</label>
      {children({ id, 'aria-describedby': describedBy, 'aria-invalid': error ? true : undefined })}
      {hint && <span id={hintId} className="mt-1 block text-xs text-muted-foreground">{hint}</span>}
      {error && <span id={errorId} role="alert" className="mt-1 block text-xs text-destructive">{error}</span>}
    </div>
  );
}
