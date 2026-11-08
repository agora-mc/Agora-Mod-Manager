# How Agora's documentation is organized

This page is for anyone writing or maintaining Agora's documentation. It explains where each kind of page lives, how the website is built from the repository, and what to update when Agora changes. If you're looking for help using Agora, start at the [documentation home](https://agoramc.com/docs) instead.

## Where each page belongs

Every page is written for one audience. The website's sidebar is built around these audiences.

| Audience | Pages | Covers |
| --- | --- | --- |
| Desktop app users | [Help & guide](https://agoramc.com/docs/guides), [Troubleshooting Agora](./DESKTOP_TROUBLESHOOTING.md), [FAQ](./FAQ.md), and the website's install and tour pages | Doing things in the app, fixing problems, and quick answers, using the labels on screen |
| CLI users | [CLI reference](./CLI.md), [CLI troubleshooting](./TROUBLESHOOTING.md), [Data, logs, and support evidence](./SUPPORT.md) | Commands, flags, output, exit codes, diagnosis, and support evidence |
| Contributors | [Developing Agora](./DEVELOPMENT.md), [Layer ownership](./architecture/layer-ownership.md) | Local builds, tests, validation gates, and which layer owns what |
| Catalog curators and reviewers | [Catalog curation reference](../REGISTRY_CURATION_REFERENCE.md), [Code of Engagement](../CODE_OF_ENGAGEMENT.md) | Writing catalog entries and the rules for reviews |
| Plugin authors | [Community plugins](./plugins/README.md), [Content providers](./plugins/providers.md), [Publishing](./plugins/publishing.md), [Implementation status](./plugins/implementation-status.md) | Writing, publishing, and updating plugins, and what the plugin host supports today |
| Maintainers and operators | [Releasing Agora](./RELEASING.md), [Governance operations](./GOVERNANCE_OPERATIONS.md) | Releases, signing, the governance pipeline, and incident recovery |

Desktop pages explain what a player sees and does. They don't describe how Agora works behind the scenes, and they don't ask players to use a terminal. CLI pages can assume a reader who is comfortable with a shell, logs, and Minecraft modding. Contributor pages can assume knowledge of the codebase.

Superseded plans and finished review logs are not kept as pages. Git history keeps them, and `git log --diff-filter=D` finds a deleted file by path. A page that no longer matches the product is worse than no page, so update or delete it rather than archiving it.

## How the website is built

The website keeps no copy of its own. Two sources feed it:

- **Help & guide** comes from `desktop/src/data/guideContent.ts`, the same file the app's built-in guide uses, published at `/docs/guides/`. The app and the website can't describe the product differently.
- **Every other page** comes from markdown. `scripts/build_docs_web.py` reads every file under `docs/` plus the root-level `CODE_OF_ENGAGEMENT.md` and `REGISTRY_CURATION_REFERENCE.md`, and writes `docs-web.json`, which the site loads. Each file becomes `/docs/<slug>`, its first heading becomes the page title, and relative links to other docs are rewritten to their website addresses.

Files under `docs/plugins/fixtures/` describe test fixtures, so they stay in the repository and are not published.

The install, tour, and documentation home pages are hand-written in `web/src/app/docs/`.

### Placing a page in the navigation

`DOC_CATEGORIES` in `scripts/build_docs_web.py` gives each file an audience and a group:

| Audience | Sidebar section |
| --- | --- |
| `desktop` | **Desktop App Users**, below the hand-written install, tour, and Help & guide links |
| `user` | **CLI Users** |
| `developer` | **For contributors and maintainers**, collapsed until opened or until you're on one of its pages |
| `internal` | **Unsorted pages**, collapsed, and shown only when such a page exists |

A new file with no rule lands in `internal`. It's still published so links to it work, but it carries a banner saying it may be out of date. Add a rule for every new page, so that nothing is left in that section.

After changing a page or a rule, run `python scripts/build_docs_web.py` to rebuild `docs-web.json`. A running development server caches the file, so restart it to see the change.

### Links

- Between markdown pages, use relative links such as `./CLI.md`. They work on GitHub and are rewritten for the website.
- From a markdown page to a Help & guide topic or a hand-written page, use the full address, for example `https://agoramc.com/docs/guides/launching`. `scripts/check_docs.py` rejects site-relative links like `/docs/guides/launching` in markdown, because they would be broken on GitHub.

## Sources of truth

- The current interface is the source of truth for button names and navigation.
- `crates/agora/src/main.rs` is the source of truth for CLI syntax. `docs/CLI.md` explains how to use it.
- `desktop/src/data/guideContent.ts` is the source of truth for the in-app guide and the website's Help & guide.
- `REGISTRY_CURATION_REFERENCE.md` is the source of truth for catalog manifests.
- `CODE_OF_ENGAGEMENT.md` is the source of truth for review conduct.
- Workflow files are the source of truth for automated release triggers.
- Secret values never belong in documentation, examples, screenshots, or support bundles.

## When something changes

A feature change isn't finished until its documentation has been checked.

1. Update the in-app guide when a user-visible workflow, label, warning, or recovery action changes. This updates the website's Help & guide too.
2. Update the desktop troubleshooting page and FAQ when a fix, label, or answer they mention changes.
3. Update `docs/CLI.md` and the CLI help when a command, flag, output shape, or exit code changes.
4. Update the hand-written pages in `web/src/app/docs/` when installation, first run, or the screenshot tour changes.
5. Give any new markdown page a rule in `DOC_CATEGORIES`.
6. Update operator documents when workflows, variables, signing, or governance state change.
7. Test any documented command against a temporary data directory.
8. Run `python scripts/check_docs.py` to check links and known wording problems.
9. Link to the page that already explains a procedure instead of copying it.

## The root README

Keep the root `README.md` short and in this order: what Agora is, download and documentation links, the first-run path, key capabilities, safety and recovery limits, project principles, a short contributor quick start, maintainer references, and license and community links. Incident procedures and full environment tables don't belong there.
