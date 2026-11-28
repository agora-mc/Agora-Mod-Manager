import type { ComponentType, ReactNode } from 'react';
import type { SectionFrame } from '../../../features/games/shared/sectionFrame';
import { SettingsSection } from '../SettingsSection';
import type { SettingsCardId } from '../gameScope';

/**
 * A game section (installs, bases, recovery) as a Settings card: the section supplies its title,
 * line, buttons and body, and this puts them in the same card every other Settings page uses.
 * `footer` is a closing line the Settings page adds under the section's own body.
 */
export function settingsFrame(
  card: SettingsCardId,
  icon: ComponentType<{ className?: string }>,
  footer?: ReactNode,
): SectionFrame {
  return function SettingsFrame({ title, description, actions, testId, children }) {
    return (
      <SettingsSection
        card={card}
        icon={icon}
        title={title}
        description={description}
        action={actions}
        contentClassName="space-y-4"
        data-testid={testId}
      >
        {children}
        {footer}
      </SettingsSection>
    );
  };
}
