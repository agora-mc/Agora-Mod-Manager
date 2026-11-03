/**
 * The bordered "← Back" button on top of a sub-page. Minecraft's instance page and every game's
 * instance page label it "← Instances", so going back looks and reads the same everywhere.
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
