/**
 * The parts of Home every game shares: the page's warm header, the Help & Guide card, and the
 * community and support cards. Minecraft's Home (`pages/Home.tsx`) and a game's Home
 * (`features/games/home`) are built from the same pieces, so the two pages look like one app.
 */

import type { ReactNode } from 'react';
import { ArrowRight, BookOpen, GraduationCap, HeartHandshake, House, MessagesSquare } from 'lucide-react';
import { PageHero } from '@/components/PageHero';
import { agoraDiscordUrl, agoraSponsorsUrl } from '@/lib/brandConfig';
import { GUIDE_TOPICS } from '@/data/guideContent';
import { TourStartButton } from '@/features/tour';

/** Home's header: the title, one sentence, and the Ctrl K hint. */
export function HomeHero({ description, children }: { description: ReactNode; children?: ReactNode }) {
  return (
    <PageHero icon={House} title="Home" subtitle={description}>
      <p className="mt-2 text-sm text-muted-foreground">
        Press <kbd className="rounded-sm border border-border bg-background px-1.5 py-0.5 font-mono text-[10px]">Ctrl K</kbd> to search and jump anywhere.
      </p>
      {children}
    </PageHero>
  );
}

const MINECRAFT_GUIDE_BLURB =
  'Follow beginner walkthroughs or open the advanced companion for every topic, from your first mod to JVM tuning and recovery.';

/** The guide card's sentence on a game's Home: no Minecraft-only topics. */
export const GAME_GUIDE_BLURB =
  'Follow beginner walkthroughs or open the advanced companion for every topic, from your first mod to load order and recovery.';

/**
 * Help & Guide, with the guided tour beside it. The tour walks Minecraft's pages, so a game's
 * Home leaves it out (`tour={false}`).
 */
export function GuideCard({ onOpenGuide, tour = true, blurb = MINECRAFT_GUIDE_BLURB }: {
  onOpenGuide: () => void;
  tour?: boolean;
  /** The sentence under the title. Minecraft's names Minecraft topics (JVM tuning). */
  blurb?: string;
}) {
  return (
    <div className="overflow-hidden rounded-xl border border-primary/20 bg-card">
      <div className="grid gap-4 p-5 sm:grid-cols-[auto_minmax(0,1fr)_auto] sm:items-center">
        <div className="flex h-11 w-11 items-center justify-center rounded-xl bg-primary/10 text-primary">
          <BookOpen className="h-5 w-5" aria-hidden="true" />
        </div>
        <div>
          <div className="flex flex-wrap items-center gap-2">
            <h3 className="font-semibold">Learn Agora at your level</h3>
            <span className="inline-flex items-center gap-1 rounded-full bg-secondary px-2 py-0.5 text-[11px] font-semibold text-secondary-foreground">
              <GraduationCap className="h-3 w-3" aria-hidden="true" />
              {/* Counted the way Help & Guide counts: a basic and an advanced page per topic. */}
              {GUIDE_TOPICS.length * 2} guide pages
            </span>
          </div>
          <p className="mt-1 text-sm leading-5 text-muted-foreground">{blurb}</p>
        </div>
        <div className="flex flex-col gap-2">
          <button
            onClick={onOpenGuide}
            className="inline-flex items-center justify-center gap-2 rounded-lg bg-primary px-4 py-2 text-sm font-semibold text-primary-foreground hover:bg-primary/90"
          >
            Open Help & Guide
            <ArrowRight className="h-4 w-4" aria-hidden="true" />
          </button>
          {tour && <TourStartButton label="Take the guided tour" />}
        </div>
      </div>
    </div>
  );
}

/**
 * "Get help from the community" and "Support Agora's future", side by side. Minecraft's wording
 * names Crash Doctor and the Minecraft community; `game` is the same pair without them.
 */
export function CommunityCards({ variant = 'minecraft' }: { variant?: 'minecraft' | 'game' }) {
  const game = variant === 'game';
  return (
    <div className="grid gap-4 md:grid-cols-2">
      {/* Community */}
      <section className="flex h-full flex-col rounded-xl border border-border bg-card p-6">
        <div className="flex items-center gap-2 mb-3">
          <MessagesSquare className="h-5 w-5 text-[#5865F2]" aria-hidden="true" />
          <h3 className="text-lg font-semibold">Get help from the community</h3>
        </div>
        <p className="text-sm leading-6 text-muted-foreground mb-4">
          {game
            ? 'Stuck on a crash, hunting for a mod, or want to help curate the catalog? The Discord is where Agora players and curators hang out.'
            : 'Stuck on a crash, hunting for a mod, or want to help curate the catalog? The Discord is where Agora players and curators hang out. Crash Doctor can copy a ready-to-share report for you — paths and usernames removed.'}
        </p>
        <a
          href={agoraDiscordUrl}
          target="_blank"
          rel="noopener noreferrer"
          className="mt-auto inline-flex w-fit items-center gap-2 rounded-lg bg-[#5865F2] px-5 py-2.5 text-sm font-semibold text-white hover:bg-[#4752c4]"
        >
          <MessagesSquare className="h-4 w-4" aria-hidden="true" />
          Join the Discord
        </a>
      </section>

      {/* Support Agora */}
      <section className="flex h-full flex-col rounded-xl border border-pink-500/40 bg-pink-500/10 p-6">
        <div className="flex items-center gap-2 mb-3">
          <HeartHandshake className="h-5 w-5 text-pink-600 dark:text-pink-400" aria-hidden="true" />
          <h3 className="text-lg font-semibold">Support Agora’s future</h3>
        </div>
        <p className="text-sm leading-6 text-muted-foreground mb-4">
          Agora is free, open source, and ad-free — built by a solo developer who loves {game ? 'modding communities' : 'the Minecraft community'}. If Agora has made modding easier or more enjoyable for you, please consider sponsoring its development. Your donation keeps Agora improving and helps fund awesome new projects for the community. Every contribution means a lot — thank you!
        </p>
        <a
          href={agoraSponsorsUrl}
          target="_blank"
          rel="noopener noreferrer"
          className="mt-auto inline-flex w-fit items-center gap-2 rounded-lg bg-pink-600 px-5 py-2.5 text-sm font-semibold text-white hover:bg-pink-700"
        >
          <HeartHandshake className="h-4 w-4" aria-hidden="true" />
          Sponsor on GitHub
        </a>
      </section>
    </div>
  );
}

/** How long ago, the way Home's "Continue playing" card says it: "5m ago", "3d ago". */
export function timeSince(date: Date): string {
  const now = new Date();
  const diffMs = now.getTime() - date.getTime();
  const mins = Math.floor(diffMs / 60000);
  if (mins < 1) return 'Just now';
  if (mins < 60) return `${mins}m ago`;
  const hours = Math.floor(mins / 60);
  if (hours < 24) return `${hours}h ago`;
  const days = Math.floor(hours / 24);
  if (days < 7) return `${days}d ago`;
  return date.toLocaleDateString();
}
