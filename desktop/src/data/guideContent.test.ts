import { describe, expect, it } from 'vitest';
import { GUIDE_TOPICS } from './guideContent';

describe('Packs & sharing guide', () => {
  it('has a topic that explains moving a setup to another computer, and search can find it', () => {
    const topic = GUIDE_TOPICS.find((t) => t.id === 'packs-sharing');
    const titles = topic?.basic.sections.map((s) => s.title) ?? [];
    expect(titles).toContain('Send a setup to a friend or another computer');
    const text = JSON.stringify(topic).toLowerCase();
    expect(text).toContain('another computer');
  });

  it('points at the buttons that exist for pack import, and says what an export carries', () => {
    const topic = GUIDE_TOPICS.find((t) => t.id === 'packs-sharing');
    const text = JSON.stringify(topic);
    // The primary control is on My Instances; it was once described as being on the Mods tab.
    expect(text).toContain('Import pack file');
    expect(text).not.toContain('on the Mods tab');
    expect(text).toContain('resource packs, shader packs and datapacks');
  });
});

describe('Playing other games guide', () => {
  const topic = GUIDE_TOPICS.find((t) => t.id === 'other-games');

  it('exists, is searchable by the first game, and has unique ids across the guide', () => {
    expect(topic).toBeDefined();
    expect(JSON.stringify(topic).toLowerCase()).toContain('skyrim');
    const ids = GUIDE_TOPICS.map((t) => t.id);
    expect(new Set(ids).size).toBe(ids.length);
  });

  it('names the controls that exist for adding a game and for a game Agora did not find', () => {
    const text = JSON.stringify(topic);
    expect(text).toContain('More games');
    expect(text).toContain('Settings > Game support');
    expect(text).toContain('Add game folder');
  });

  it('does not promise that the game folder is never changed, because a Linked base can', () => {
    const text = JSON.stringify(topic);
    expect(text).not.toMatch(/never changed/i);
    expect(text).toContain('Linked base');
  });
});
