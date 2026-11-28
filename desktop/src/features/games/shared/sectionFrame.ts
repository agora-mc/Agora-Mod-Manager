import type { ReactNode } from 'react';

/**
 * The parts of a game section the page around it frames: a title, a line under it, the section's
 * own buttons and its body. The installs, bases and recovery sections are shown as Settings
 * cards; Settings supplies the card (`SettingsSection`) through this, so the sections keep their
 * behaviour in their slices and the look stays the one every Settings card has.
 */
export interface SectionFrameParts {
  title: string;
  description: ReactNode;
  /** The section's buttons, shown at the right of its title. */
  actions?: ReactNode;
  testId: string;
  children: ReactNode;
}

export type SectionFrame = (parts: SectionFrameParts) => ReactNode;
