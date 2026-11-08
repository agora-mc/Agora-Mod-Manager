// @vitest-environment jsdom
import { describe, expect, it, vi } from 'vitest';
import { fireEvent, render, screen } from '@testing-library/react';
import { InstallPrompt } from './InstallPrompt';
import type { InstallPreview } from './types';

function preview(over: Partial<InstallPreview> = {}): InstallPreview {
  return {
    manifest: {
      manifest: 1,
      id: 'acme.one',
      name: 'One',
      version: '1.0.0',
      license: 'MIT',
      network: { hosts: [] },
    },
    requiredCapabilities: [],
    optionalCapabilities: [],
    unsupportedCapabilities: [],
    replacesVersion: null,
    addedCapabilities: [],
    addedHosts: [],
    updateSource: null,
    migratesData: false,
    fileCount: 1,
    uncompressedBytes: 10,
    runsCode: true,
    addsCode: true,
    codeChanged: true,
    ...over,
  } as unknown as InstallPreview;
}

function renderPrompt(previewValue: InstallPreview, onConfirm = vi.fn()) {
  render(
    <InstallPrompt
      preview={previewValue}
      development={false}
      busy={false}
      onCancel={vi.fn()}
      onConfirm={onConfirm}
    />,
  );
  return onConfirm;
}

describe('the install prompt code question', () => {
  it('explains an official plugin that names what its code is for', () => {
    const value = preview({
      official: true,
      manifest: {
        manifest: 1,
        id: 'agora.one',
        name: 'One',
        version: '1.0.0',
        license: 'MIT',
        apiRange: '>=0.1, <0.2',
        withoutCode: 'Sorting will not work.',
      },
    });
    renderPrompt(value);

    expect(screen.getByText('One runs code')).toBeInTheDocument();
    expect(
      screen.getByText(
        'This is an official Agora plugin. Allowing its code is strongly recommended.',
      ),
    ).toBeInTheDocument();
    expect(
      screen.getByText('Without its code, One will be missing features: Sorting will not work.'),
    ).toBeInTheDocument();
    expect(screen.getByText('You can allow it later in Settings > Plugins.')).toBeInTheDocument();
    expect(
      screen.getByRole('button', { name: 'Install and allow code (recommended)' }),
    ).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Install without running code' })).toBeInTheDocument();
  });

  it('quotes a community plugin in its own words', () => {
    const value = preview({
      official: false,
      manifest: {
        manifest: 1,
        id: 'acme.one',
        name: 'One',
        version: '1.0.0',
        license: 'MIT',
        apiRange: '>=0.1, <0.2',
        withoutCode: 'The charts stay empty.',
      },
    });
    renderPrompt(value);

    expect(
      screen.getByText(
        'Without its code, One will be missing features. The plugin says: “The charts stay empty.”',
      ),
    ).toBeInTheDocument();
    expect(
      screen.queryByText(/This is an official Agora plugin/),
    ).toBeNull();
    expect(screen.getByRole('button', { name: 'Install and allow code' })).toBeInTheDocument();
    expect(
      screen.queryByRole('button', { name: /\(recommended\)/ }),
    ).toBeNull();
  });

  it('says only that features will be missing when the author gave no sentence', () => {
    renderPrompt(preview({ official: false }));

    expect(
      screen.getByText(
        'Without its code, One will be missing the features its code provides.',
      ),
    ).toBeInTheDocument();
  });

  it('asks about a dependency that runs code, and offers both answers', () => {
    const dependency = preview({
      official: true,
      manifest: {
        manifest: 1,
        id: 'agora.dep',
        name: 'Dep',
        version: '2.0.0',
        license: 'MIT',
        apiRange: '>=0.1, <0.2',
        withoutCode: 'Its checks do not run.',
      },
    });
    const value = preview({ official: true, runsCode: false, alsoInstalls: [dependency] });
    renderPrompt(value);

    expect(screen.getByText('Dep runs code')).toBeInTheDocument();
    expect(
      screen.getByRole('button', { name: 'Install and allow code (recommended)' }),
    ).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Install without running code' })).toBeInTheDocument();
  });

  it('does not recommend allowing code when no package is official', () => {
    const value = preview({ official: false });
    renderPrompt(value);

    expect(screen.getByRole('button', { name: 'Install and allow code' })).toBeInTheDocument();
    const withoutCode = screen.getByRole('button', { name: 'Install without running code' });
    expect(withoutCode).toHaveClass('bg-secondary');
    expect(withoutCode).not.toHaveClass('border-input');
  });

  it('sends the two answers to onConfirm', () => {
    const onConfirm = renderPrompt(preview({ official: true }));

    fireEvent.click(screen.getByRole('button', { name: 'Install and allow code (recommended)' }));
    expect(onConfirm).toHaveBeenLastCalledWith(true);

    fireEvent.click(screen.getByRole('button', { name: 'Install without running code' }));
    expect(onConfirm).toHaveBeenLastCalledWith(false);
  });
});
