# Nexus Mods integration research for Agora

Researched 2026-11-01. Read-only: no keys used, no accounts created, nothing downloaded. Primary sources: Nexus help center, Nexus's published OpenAPI spec (SwaggerHub), the v2 GraphQL reference, modding.wiki's OAuth guide (Nexus's own developer docs, linked from their GraphQL page), and Nexus's own open-source repos (Vortex, node-nexus-api, sso-integration-demo, oauth2-demo-app). MO2 and Wabbajack source were read for how others do it.

Quoting note: to stay within copyright limits I paraphrase nearly everything and give the URL for each claim. Where a short exact phrase matters it is in quotation marks. Anything I could not confirm is marked **unverified**.

---

## 0. Headline findings (read this first)

1. **A user-pasted personal API key in a public app is explicitly listed as unacceptable by Nexus.** Nexus's API Acceptable Use Policy (AUP) tolerates personal keys only for apps in testing or for personal use, and lists "Using personal API keys for a public-facing application" under unacceptable usage. https://help.nexusmods.com/article/114-api-acceptable-use-policy. So idea 1 as literally stated ("personal key for everyone") is the one idea most likely to annoy Nexus. GameNative removed personal-key support for exactly this reason (https://github.com/utkarshdalal/GameNative/pull/1763).
2. **Registering an open-source app is just an email, not a business agreement.** The official OAuth guide says there is no web UI; the developer emails Nexus with app name, short description, logo, optional source link and callback URI. Source-available apps are favoured; closed ones "may be declined". https://modding.wiki/en/api/oauth2-guide. The only terms that attach are the AUP. This fits the owner's constraint (a form/email, not a CurseForge-style deal), though Nexus can still decline. Treat it as a request, not a guarantee.
3. **Free users cannot get download links from the API without visiting the website.** Official spec text: non-premium members must provide the `key` and expiry from the `.nxm` link the website produces, and the design "ensures that all non-premium members must access the website". Without them the API answers 403. https://api.swaggerhub.com/apis/NexusMods/nexus-mods_public_api_params_in_form_data/1.0 (the JSON behind the SwaggerHub page; the human page returned 404 to my fetcher). So the nxm flow is the sanctioned free-user path, and anything that bypasses or automates it is out.
4. **Nexus's own client (Vortex) already moved to OAuth**, and Nexus tells developers that all apps will require OAuth to act on behalf of a user (https://modding.wiki/en/api/oauth2-guide). Personal keys are being phased down in favour of it (MO2 issue says "slowly being deprecated", written by a Nexus-affiliated contributor offering to arrange a client ID: https://github.com/ModOrganizer2/modorganizer/issues/2276). No official Nexus announcement of a removal date was found.

---

## 1. API access: personal keys, headers, rate limits

**Can every free account get a personal key?** Yes in practice. The spec says users view their keys on the API Access tab of their account page (https://www.nexusmods.com/users/myaccount?tab=api%20access). The AUP says any user can generate one, "mainly to experiment". I did not log in, so I could not confirm that nothing gates it for free accounts; no source suggests a gate. The legacy node client README also says keys are per user and per application. https://github.com/Nexus-Mods/node-nexus-api

**What Nexus says about third-party use of a personal key:** see headline 1. AUP details (https://help.nexusmods.com/article/114-api-acceptable-use-policy):
- Tolerated: apps in testing, or for personal use only.
- Public-facing apps are expected to contact support once they have a working test build, after which Nexus issues an app registration (an SSO slug). Registered apps show on the user's API Access page and users can revoke them.
- Nexus does not endorse personal keys for public apps and may block abuse.
- The spec's own note says never to include a personal key as part of your software.

**Required headers** (AUP and spec):
- `apikey: <key>` for key auth, or `Authorization: Bearer <token>` for OAuth (modding.wiki says both v1 and v2 accept the bearer token).
- `Application-Name` (stable across versions) and `Application-Version` (matches the release). The AUP lists blank metadata or impersonating another app as unacceptable.
- A descriptive `User-Agent` is requested by the spec.
- node-nexus-api also sends `Protocol-Version`; MO2 sends `Application-Name: MO2` (https://github.com/ModOrganizer2/modorganizer/blob/master/src/nxmaccessmanager.cpp).

**Rate limits. Nexus's own pages disagree, so budget for the lower one:**
- Spec (SwaggerHub JSON): 2,500 requests per 24h, then 100 per hour once exceeded; user-based (rotating your key does not help); nginx may also 429 above ~30 requests/second. Response headers `X-RL-Hourly-Limit/Remaining/Reset` and `X-RL-Daily-Limit/Remaining/Reset`. `/v1/users/validate.json` does not count.
- Help center: 20,000 per 24h, then 500 per hour; resets at 00:00 GMT daily and on the hour; requests from mod managers count; Vortex uses about 3 requests per mod download. https://help.nexusmods.com/article/105-i-have-reached-a-daily-or-hourly-limit-api-requests-have-been-consumed-rate-limit-exceeded-what-does-this-mean
- The help article looks newer and larger, but I could not date either. Read `X-RL-*` headers at runtime rather than hard-coding.
- **When exceeded:** HTTP 429 with the same headers; mod downloads and update queries are blocked until reset (help article). node-nexus-api treats 429 as a rate-limit error and notes that waiting does not restore an exhausted hourly budget (https://github.com/Nexus-Mods/node-nexus-api, `src/Nexus.ts`).
- The AUP says registration review checks that an app does not use an excessive number of requests.

---

## 2. Free vs Premium downloads, exactly

**The `nxm://` link.** Parsed by Vortex's own code (https://github.com/Nexus-Mods/Vortex, `src/renderer/src/extensions/nexus_integration/NXMUrl.ts`):
- Mod file: `nxm://<game_domain>/mods/<mod_id>/files/<file_id>?key=<k>&expires=<unix_ts>&user_id=<id>` (hostname = game domain; path matched by `/mods/(\d+)/files/(\d+)`).
- `key`: identifies the requesting user's authorisation for this file. `expires`: timestamp after which the link is invalid. `user_id`: the account the link was minted for. A `view` flag also exists. Other forms: collections (`/collections/<slug>/revisions/<n|latest>`), `nxm://oauth/callback?code=&state=`, and `nxm://premium`.
- Vortex refuses a link whose `user_id` differs from the signed-in account and tells the user to use the same account on the site and in the app.
- Whether `user_id` is present on every link is unverified; Vortex treats it as optional.

**The endpoint.** `GET https://api.nexusmods.com/v1/games/{game}/mods/{mod_id}/files/{file_id}/download_link.json?key=&expires=` (spec above; MO2 builds the same URL: https://github.com/ModOrganizer2/modorganizer/blob/master/src/nexusinterface.cpp).
- Premium: no key needed; returns an array of download locations, the user's preferred first.
- Free: `key` + `expires` from the link are mandatory.
- Documented errors: 400 key/expiry wrong for this user/file; 403 no permission without visiting the site ("premium users only"); 404 file missing; 410 link expired, revisit the mod page.

**What Premium adds** (for the API question): API-minted links with no website visit. The wider Premium perks (speed, no ads) are not covered by the pages I could access; unverified from Nexus sources.

**Exception that helps free users:** authors can enable "direct download" on a mod, which lets it be downloaded without Premium. The v2 mutation is described as updating whether a mod "can be downloaded directly without requiring premium status", and `directDownloadEnabled` is a field on `Mod` (https://graphql.nexusmods.com/). Vortex checks this flag and, if true, calls the API without a key (`nxmProtocol.ts`, `#isDirectDownload`). I cannot say how many Skyrim mods enable it.

**How Vortex does the free-user flow** (copy this): park the download, then open the system browser at `https://www.nexusmods.com/<game>/mods/<mod>?tab=files&file_id=<file>&nmm=1`, wait for the nxm link to come back via the OS protocol handler, then call `download_link` with its key/expires. (Vortex `nxmProtocol.ts`. What `nmm=1` does server-side is unverified.)

**Wabbajack** does not even try: it requires Premium for automatic Nexus downloads and otherwise throws a "manual download required" error (https://github.com/wabbajack-tools/wabbajack, `Wabbajack.Downloaders.Nexus/NexusDownloader.cs`).

**What is forbidden or risky here:**
- Generating a link for a free user without the key: the API refuses, and the spec states this is by design.
- Automated clicking / scraping the site to get links: the Terms of Service ban any robot, bot, spider, scraper or other automated device used to access or copy parts of the site (section 11), and ban data mining/scraping for any purpose. https://help.nexusmods.com/article/18-terms-of-service
- Downloading in a way that significantly exceeds normal and expected usage can get an account suspended (section 10, same URL).
- Third-party "skip the countdown" userscripts exist; one script page warns it may violate the ToS and that Nexus reportedly throttles accounts injecting scripts (https://greasyfork.org/pt/scripts/519037-nexus-no-wait, secondhand; **unverified**). Do not add anything like this.

---

## 3. App registration, SSO and OAuth

- **Current official path: OAuth 2.0 + PKCE** against `https://users.nexusmods.com/oauth/authorize` and `/oauth/token`; well-known config at `https://users.nexusmods.com/.well-known/openid-configuration`. Public apps (mod managers) use PKCE; server apps get a client secret. Desktop apps are advised to listen on a loopback address; a custom `nxm://oauth/callback` redirect is also mentioned. Tokens are JWTs carrying user id and `membership_roles` (including `premium`), signed with a Nexus public key; refresh via the refresh-token grant; 400-range failure on refresh means the user revoked access. Users can revoke at https://users.nexusmods.com/oauth/authorized_applications. All from https://modding.wiki/en/api/oauth2-guide.
- **Getting a client ID:** email Nexus (address obfuscated on the page; the AUP names support@nexusmods.com, and the v2 docs name support+api@nexusmods.com for API help) with name, description, logo suitable for dark backgrounds, source link, callback URI. No web form yet.
- **Legacy SSO (websocket, issues an API key to the app):** needs an application slug that only Nexus staff can generate (https://github.com/Nexus-Mods/sso-integration-demo). The AUP lists the same steps: working test build with a personal key, name/description/logo, then a slug. SSO is "recommended but optional" per the AUP.
- **Agreements beyond a form:** none found. The OAuth guide says apps must comply with the AUP and that failure, intentional or not, may lead to Nexus blocking the app's use of its data. The AUP adds that registration is not an endorsement, and Nexus may refuse registration over doubts about purpose, user-data handling, or code compliance; including source code may speed review.
- **Is a personal key pasted by the user acceptable for an unregistered open-source app?** For a public release: no, per the AUP wording above. For your own development and for a clearly labelled test build: tolerated. Real-world signal: GameNative pulled personal-key support to stay compliant while awaiting OAuth credentials (PR above); a G1L release note says an earlier version's personal-key update check was not allowed (https://github.com/ISKL23/G1L-Releases/releases, seen in search summary only; **unverified** beyond that). I found no case of Nexus revoking an app's access, so enforcement against small apps is unproven either way.
- MO2 and Vortex: both now offer Nexus OAuth login (Vortex client id `vortex_loopback`, loopback redirect; MO2 has OAuth config in `nexusoauthconfig.cpp`). MO2 still contains an API-key code path. Both are Nexus-recognised apps, so their precedent does not transfer to an unregistered app.

---

## 4. Policies that matter

**API AUP** (https://help.nexusmods.com/article/114-api-acceptable-use-policy). Unacceptable, non-exhaustive: bulk-fetching data to rehost it on your own service (scraping); storing users' API keys on your server or using them without the user initiating the action; personal keys in a public-facing app; blank metadata or impersonating another application; anything Nexus deems harmful to the modding community or the site. The page says nothing explicit about caching, display of mod data, or redistribution beyond the rehosting/scraping line. Practical reading: local per-user caching to cut requests is fine; shipping a mirror of Nexus mod descriptions or file metadata in Agora's catalog is the thing to avoid (store IDs and hashes, link back).

**Main Terms of Service** (https://help.nexusmods.com/article/18-terms-of-service): covers apps such as Vortex and the Nexus Mods App and the APIs (sections 1-2); personal use only and no commercial use of site content (section 10, 28; Agora is non-commercial but check any future monetisation); bot/scraping bans (section 11). Nothing on framing or embedding the site (section 27 covers only linking to the home page and not implying endorsement).

**Embedding nexusmods.com in an app webview:** the ToS and AUP do not address it, so there is no written prohibition and also no written permission. A human browsing with the real page and ads is not a bot. What would cross the line is injecting scripts that click buttons, skip countdowns, or read pages programmatically. Wabbajack has an internal browser for manual downloads (its FAQ says it can resort to an internal browser and download "as if a user would go to the website": https://wiki.wabbajack.org/user_documentation/Troubleshooting FAQ.html, from a search summary; **unverified** that Nexus has blessed it). Technical unknowns I could not test: whether WebView2 raises a catchable navigation event for `nxm://` links, whether Cloudflare challenges or social logins (Google/Steam/Discord) work inside an embedded webview. Both **unverified**.

**How Nexus treats open-source managers:** it publishes the OAuth demo and docs for them, "strongly encourages" open-source licensing in the AUP, and runs its own open-source Nexus Mods App (https://github.com/Nexus-Mods/NexusMods.App). I found no evidence of Nexus blocking an open-source manager and no public list of blocked apps. Wabbajack's constraint (Premium for automation) is a policy outcome of the download_link rule above, not a ban.

**Resumable downloads (beta)** applies to browser downloads of files over 500 MB for free users, not to managers: https://help.nexusmods.com/article/170-resumable-downloads.

---

## 5. Useful endpoints

v1 REST (stable; the v2 page says v1 "will continue to be supported for the foreseeable future", https://graphql.nexusmods.com/). Base `https://api.nexusmods.com`; all paths below are from the SwaggerHub JSON listed in section 2:

| Need | Endpoint | Notes |
|---|---|---|
| Identify file by MD5 | `GET /v1/games/{game}/mods/md5_search/{md5}.json` | Returns the mod and file details (`{mod, file_details}` per node-nexus-api types). 404 = unknown. One request per hash. |
| List a mod's files | `GET /v1/games/{game}/mods/{mod_id}/files.json?category=` | categories: main, update, optional, old_version, miscellaneous. File entries include update chain info (`file_updates`, used by Vortex). |
| One file | `.../files/{file_id}.json` | |
| Mod info/description | `GET /v1/games/{game}/mods/{id}.json` | cached 5 minutes server-side |
| Update check | `GET /v1/games/{game}/mods/updated.json?period=1d|1w|1m` | only those three periods; one call per game returns all recently updated mod ids with timestamps; cached 5 minutes. Ideal for cheap update checks. |
| Changelogs | `.../mods/{id}/changelogs.json` | |
| Download link | `.../files/{id}/download_link.json?key=&expires=` | see section 2 |
| Validate key | `GET /v1/users/validate.json` | not counted against limits |

v2 GraphQL: `https://api.nexusmods.com/v2/graphql`, reference at https://graphql.nexusmods.com/. The page says most of it works without authentication and some queries need OAuth. It has `fileHash(md5)` and bulk `fileHashes(md5s:[...])` (returns game id, mod file, file id, size), and `directDownloadEnabled` on `mod`. Bulk hash lookup would save a lot of rate limit compared with v1 md5_search, but see status. I did not run any query, so which fields need auth is **unverified**. I found no v2 "updated in period" query in the sections I read (the reference is huge; only the first parts were read in detail), so use v1 `updated.json`. **Status:** v2 is not marked deprecated for v1; the v2 docs describe themselves as unfinished, with changing endpoints and a promise of self-service OAuth registration later. Vortex and the Nexus Mods App already use v2 (e.g. `getInfoGraphQL`), so it is usable but moves.

---

## 6. Recommendation for Agora

Ranking by safety with respect to the AUP/ToS and the owner's no-partnership constraint:

**Idea 3 (watch Downloads folder, identify by checksum): safe, and needs no Nexus account at all if the hashes come from Agora's own catalog.** It touches no Nexus service, so it cannot get any key or account in trouble. If you identify by asking Nexus (md5_search / `fileHashes`), you are back to needing an API credential, so prefer matching against hashes already in Agora's catalog manifests, falling back to Nexus lookup only once Agora has an OAuth client. Caveats: a free user still downloads manually (click Slow/Manual Download), which is the sanctioned behaviour. Filename conventions of Nexus archives (name-modid-version-timestamp) are widely known, but I have no Nexus source for them: **unverified**, so use them only as a hint, never as identity.

**Idea 1 (nxm:// via API) is the right feature but must not ship on personal keys.** Do it the way Vortex does, with OAuth: register the app (email, link the repo, loopback or nxm callback), PKCE login in the system browser, keep tokens in the OS credential store on the user's machine only, and only call the API when the user initiated the action. In the interim, develop and test with your own personal key in a dev build (AUP: tolerated for testing), and gate the release on getting the client ID. If registration is refused or slow, ship the feature disabled and fall back to idea 3. Do not ship a "paste your API key" box in public builds; that is the exact listed violation and the easiest way to attract an app block. If you want the escape hatch anyway, make it a hidden, warned, opt-in advanced setting; this is the owner's call, and my reading is it still fits "unauthorised use of personal keys in a public app", so the risk is Nexus blocking Agora's app name, with revocation falling on the user's key.
 - Also register `nxm://` as the handler only with the user's consent and show who currently owns it (Vortex/MO2 fight over it). Claiming it silently would annoy users and, indirectly, Nexus.
 - Free users: open the file page in the system browser (as Vortex does), take the link back via nxm, call `download_link` with key and expiry within the expiry window, handle 400/403/410, and do not retry blindly.
 - Premium and direct-download mods: call `download_link` without a key (check `directDownloadEnabled` first).
 - Keep rate usage low: one `updated.json` per game for update checks, bulk hash lookup, cache responses, and read `X-RL-*` headers. Send honest `Application-Name`/`Application-Version`.

**Idea 4 (update checks): safe and cheap once there is a credential; same OAuth caveat as idea 1.** Use `updated.json?period=1w` plus `files.json` for the few that changed. Do not store users' tokens server-side (none of Agora's design needs it).

**Idea 2 (embedded nexusmods.com webview): needs care; I would not make it the main path.** Nothing written forbids a real human browsing inside a webview, but: (a) it gains nothing over opening the system browser, which Vortex proves works and keeps the user's real login, extensions and Cloudflare reputation; (b) embedded webviews are the classic place for bot-detection and social-login failures (**unverified** for WebView2); (c) it tempts script injection or auto-clicking, which section 11 of the ToS bans; (d) users would type Nexus credentials into Agora's window, which is a trust and security concern for an open-source app. If you do it anyway: no injected scripts, no auto-clicks, no reading page content, ads left intact, login done in the system browser, and just catch the `nxm://` navigation (feasibility **unverified**; test with Tauri's navigation handler before committing).

**Better idea found:** copy Vortex's free-user queue UI (a dialog that lists parked downloads, a "Download on website" button per file, a Skip option, and automatic resume when the nxm link arrives) plus the direct-download shortcut. It is the one flow Nexus itself ships to free users, so it is by definition within what they accept. Source: https://github.com/Nexus-Mods/Vortex (`nxmProtocol.ts`, `NXMUrl.ts`).

**Things that could get a key or users' accounts in trouble:**
1. A public build shipping with, or asking users for, personal API keys (AUP).
2. Any server component that stores or proxies user keys/tokens (AUP).
3. Calling the API when the user did not initiate the action, for example background auto-download (AUP).
4. Automating the website: auto-clicks, countdown skipping, scraping pages for links (ToS section 11; throttling risk reported secondhand).
5. Excessive request volume: per-user limits are shared across all of a user's tools, so a chatty Agora can lock a user out of Vortex/MO2 for the day.
6. Generic or blank Application-Name/Version, or impersonating another app (AUP).
7. Bulk-mirroring Nexus mod metadata into Agora's public catalog (AUP "rehosting" rule). Keep catalog entries to IDs, hashes and your own curation.
8. A leaked personal key lets anyone act on that account's API access; never log or commit it (and the spec warns not to bundle keys in software).

**Suggested sequence:** (1) ship idea 3 using catalog hashes, no Nexus dependency; (2) build idea 1/4 against a dev personal key behind a dev flag; (3) email Nexus for an OAuth client with the repo link and a loopback callback; (4) enable for users when the client ID arrives.

---

## Source index
- AUP: https://help.nexusmods.com/article/114-api-acceptable-use-policy
- Rate limits: https://help.nexusmods.com/article/105-i-have-reached-a-daily-or-hourly-limit-api-requests-have-been-consumed-rate-limit-exceeded-what-does-this-mean
- Terms of Service: https://help.nexusmods.com/article/18-terms-of-service
- v1 OpenAPI JSON: https://api.swaggerhub.com/apis/NexusMods/nexus-mods_public_api_params_in_form_data/1.0 (human page https://app.swaggerhub.com/apis-docs/NexusMods/nexus-mods_public_api_params_in_form_data/1.0 returned 404 to my fetcher)
- v2 GraphQL reference: https://graphql.nexusmods.com/
- OAuth guide: https://modding.wiki/en/api/oauth2-guide
- OAuth demo: https://github.com/Nexus-Mods/oauth2-demo-app ; SSO demo: https://github.com/Nexus-Mods/sso-integration-demo
- Vortex: https://github.com/Nexus-Mods/Vortex (`nexus_integration/NXMUrl.ts`, `nxmProtocol.ts`, `constants.ts`, `CHANGELOG.md`)
- node-nexus-api: https://github.com/Nexus-Mods/node-nexus-api
- MO2: https://github.com/ModOrganizer2/modorganizer (`nexusinterface.cpp`, `nxmaccessmanager.cpp`), issue 2276
- Wabbajack: https://github.com/wabbajack-tools/wabbajack (`NexusDownloader.cs`)
- GameNative compliance PR: https://github.com/utkarshdalal/GameNative/pull/1763
- Resumable downloads: https://help.nexusmods.com/article/170-resumable-downloads
- Not reachable: https://www.nexusmods.com/about/api (403), https://www.nexusmods.com/news/13921 API launch post (403); SSO/legacy details came from the GitHub demo instead.
