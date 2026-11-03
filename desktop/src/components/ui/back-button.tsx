/**
 * The bordered "← Back" button on top of a sub-page. Used by Minecraft's instance page and by
 * every game's instance page, so going back looks the same everywhere.
 */

interface BackButtonProps {
  onBack: () => void;
  /** Where Back goes, when "Back" alone is vague ("Instances"). */
  label?: string;
}

export function BackButton({ onBack, label = 'Back' }: BackButtonProps) {
  return (
    <button
      type="button"
      onClick={onBack}
      className="rounded-lg border border-input bg-background hover:bg-accent px-3 py-1.5 text-sm font-medium"
    >
      ← {label}
    </button>
  );
}
