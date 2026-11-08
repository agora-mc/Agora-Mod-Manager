import { render, screen } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';

vi.mock('@tauri-apps/api/core', () => ({ invoke: vi.fn(async () => null) }));

import { LibraryItemCard } from './LibraryItemCard';
import type { LibraryItem } from './api';

const manual: LibraryItem = {
  itemId: 'a'.repeat(64),
  name: 'SKSE64',
  description: 'Script extender.',
  version: '2.3.1',
  gameId: 'skyrim-se',
  gameIds: ['skyrim-se'],
  pageUrl: 'https://www.nexusmods.com/skyrimspecialedition/mods/30379',
  iconSha256: null,
  sourceKind: 'catalog',
  sourceLabel: 'Curated catalog entry for skyrim-se; its source published no hash',
  catalogStrategy: 'manual_link',
  instances: [],
  fileCount: 3,
  totalSize: 1024,
  addedAtUnixMs: 1,
  pluginFiles: [],
  hasInstaller: false,
};

function renderCard(item: LibraryItem) {
  return render(
    <LibraryItemCard
      item={item}
      gameId="skyrim-se"
      gameLabel={(id) => id}
      addDisabledReason={null}
      busy={false}
      report={undefined}
      onAdd={() => undefined}
      onCheck={() => undefined}
      onRemove={() => undefined}
    />,
  );
}

describe('LibraryItemCard', () => {
  it('says updates are unknown for a mod the player downloaded themselves', () => {
    renderCard(manual);
    const chip = screen.getByTestId('updates-unknown');
    expect(chip).toHaveTextContent('Updates unknown');
    expect(chip).toHaveAttribute(
      'title',
      "Agora can't check nexusmods.com for new versions. Visit the page to see if there's an update.",
    );
  });

  it('says nothing about updates for an automatic source', () => {
    renderCard({ ...manual, catalogStrategy: 'github_release' });
    expect(screen.queryByTestId('updates-unknown')).not.toBeInTheDocument();
  });
});
