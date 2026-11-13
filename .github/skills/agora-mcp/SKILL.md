---
name: agora-mcp
description: 'Guide for using the Agora launcher MCP server to diagnose Minecraft mod crashes and manage instances. Use when: a user reports a Minecraft crash, asks about their installed mods/instances, wants a mod disabled/enabled, or asks "what do you know about my setup?".'
---

# Agora MCP Server Guide

## About Agora

Agora is a free, open-source, ad-free mod manager. Minecraft is included as the default, other games come as plugins. Source code, releases and the issue tracker are at https://github.com/agora-mc/Agora-Mod-Manager, and the community lives at https://discord.gg/56tpsa2sTZ. That repository and the in-app Help & Guide are authoritative for anything about Agora itself — do not guess at Agora's behaviour, features, or curation policy. Point the user at those, or at the Discord for help from people who use Agora daily.

Vocabulary used by the tools below:

- **Instance** — one isolated Minecraft installation with its own version, mod loader, mods, config and saves. Changing one instance never affects another. Almost every tool is scoped to an `instance_id`.
- **Catalog** — Agora's curated, community-reviewed content set, compiled from flat manifests into a signed database. Mods may also come from Modrinth when the user enables it. Every download is verified by SHA-256.
- **Crash Doctor** — the in-app local crash analyser. It uses the same curated signatures and weighted suspect scoring you reach through `search_crash_signatures` and `suggest_mod_incompatibility`, so your findings should agree with what the user already sees in the app.
- **Snapshots** — Agora can restore an instance's previous mod set. This is why `disable_mod` is safe to propose: the user can undo it.
- **Launch modes** — direct launch runs Minecraft inside Agora; delegated launch hands off to the official Mojang launcher and is the default, so Agora works without a Microsoft sign-in. Under delegated launch Agora did not run the game, which limits what it observed.

House rules for answering:

- Prefer Agora's own actions and UI over telling the user to hand-edit files.
- Recommend one reversible change at a time, verified by launching the game, so cause and effect stay clear.
- Treat suspect scores as evidence, not a verdict. Cite the specific signal or log line behind your conclusion, and say when you are guessing.
- A mod must match the instance's Minecraft version *and* loader. Check both with `list_instance_mods` before recommending an install.
- Never advise disabling hash or signature verification, and never point the user at unofficial mod mirrors.
- If nothing implicates a mod, consider Java version, GPU drivers, allocated memory, shaders, or world corruption.

## Overview

The Agora launcher runs a local MCP server on `127.0.0.1:39741` that exposes 10 tools for managing Minecraft mod instances and diagnosing crashes. The server is built into the desktop app and runs when the user has "AI / MCP Server" enabled in Settings (disabled by default in the shipped app). Every connection requires the persistent Bearer token shown in Settings -> Integrations -> MCP Server.

The server speaks JSON-RPC 2.0 over HTTP with SSE (Server-Sent Events) for response delivery. Tools are called via the `tools/call` method with `name` and `arguments` in the params. All tool responses are wrapped in a JSON object with `content` (array of `{type: "text", text: "..."}`) and `isError` fields.

## The 10 Tools

### 1. `list_instances`

- **Params:** None
- **Returns:** `{"instances": [{"instance_id", "name", "minecraft_version", "loader", "loader_version"}, ...]}`
- **Use when:** You need to know what instances exist before any instance-specific operation. Always call this first when a user mentions an instance by name so you can match it to an `instance_id`.

### 2. `list_instance_mods`

- **Params:** `instance_id` (string) — the instance ID to list mods for.
- **Returns:** `{"mods": [{"filename", "version", "source", "mod_jar_id", "depends_on", "optional_deps", "java_packages"}, ...]}`
- **Use when:** You need to see what mods are installed, check dependency chains, or find which mod a Java package belongs to. Call after `list_instances` to inspect a specific instance's mod list.

### 3. `disable_mod`

- **Params:** `instance_id` (string), `filename` (string) — the mod filename to disable.
- **Returns:** On success: `{"content": [{"type": "text", "text": "Mod <filename> disabled in instance <id>. Restart the game to apply."}], "isError": false}`. On failure: `{"content": [{"type": "text", "text": "<error message>"}], "isError": true}`.
- **Destructive — requires user approval.** The user must grant `disable_mod` permission for each instance in Agora Settings → MCP → Approvals. If not granted, returns `ERR_MCP_DENIED` with an error message directing the user to grant permission.
- **Use when:** You've identified a suspect mod and want to disable it for testing. The user must restart the game for the change to take effect.

### 4. `search_crash_signatures`

- **Params:** `crash_text` (string) — the crash log text.
- **Returns:** `{"answer": <CrashAnswer>, "files": [<per-file report>]}` — the local crash doctor's answer, the same engine the app uses. `answer.candidates` is the ranked list (strongest first, never "certain"); `answer.findings` says what each rule recognised; `answer.could_not_check` lists what no parser could look at. `files` says which parser read each file, or why none did. An empty `crash_text` is an error, not an empty result.
- **Use when:** A user shares crash text. Read `answer.headline`, `answer.level` and `answer.next` first; use the findings and could-not-check list to explain what Agora knows and does not know.

### 5. `suggest_mod_incompatibility`

- **Params:** `instance_id` (string), `crash_text` (string) — the crash log text to analyse.
- **Returns:** `{"answer": <CrashAnswer>, "files": [<per-file report>]}` — the same local crash doctor, run against that instance's deployed mods. `answer.candidates` holds the suspects, each with its `level`, `score`, `families`, `cause`, and the source rules that contributed. `answer.repairs` and `answer.actions` carry what the screen may offer; `answer.could_not_check` lists what could not be looked at.
- **Use when:** You need to know which installed mods the local engine considers evidence against, and why. Do not overstate it: Agora never says a mod is certainly the cause.

### 6. `get_system_context`

- **Params:** None
- **Returns:** A markdown summary with sections for Instances, Installed Mods, and Recent Crashes.
- **Use when:** You need an overview of the user's system state before diagnosing an issue, or when the user asks "what do you know about my setup?".

### 7. `read_latest_crash`

- **Params:** `instance_id` (string).
- **Returns:** `{"session_id", "instance_id", "created_at", "answer": <StoredAnswer>}` — the newest answer the crash doctor stored for the instance. `answer.status` is `analyzed` (with the `CrashAnswer` and the per-file report), `nothing_to_read` (with why), or `could_not_analyze` (with why). If no answer is stored, the call is an error: run the game once so a finished session can be diagnosed.
- **Use when:** You need the result of the last diagnosed launch without re-running the analysis.

### 8. `read_mod_manifest`

- **Params:** `mod_id` (string).
- **Returns:** The full curated local registry record for the mod.
- **Use when:** You need curator-authored compatibility, description, license, or vote context without a network request.

### 9. `enable_mod`

- **Params:** `instance_id` (string), `filename` (string).
- **Destructive -- requires user approval.** Reverses a prior disable by restoring `.jar.disabled` to `.jar`.
- **Use when:** A disable test did not solve the crash or the user explicitly wants the mod restored.

### 10. `search_knowledge_base`

- **Params:** `query` (string).
- **Returns:** Up to five matching local registry entries.
- **Use when:** You need a natural-language search across the curated catalog without network access.

## Crash Diagnosis Workflow

Follow this decision tree when a user reports a crash:

1. **Get context.** If you don't know the user's setup, call `get_system_context` or `list_instances` to understand what instances and mods they have.

2. **Quick check.** If the user shares a crash log, call `search_crash_signatures` first. It runs the local crash doctor over the text; read its headline, level and findings.

3. **Deep analysis.** Call `suggest_mod_incompatibility` with the `instance_id` and crash text. It runs the same engine against the instance's deployed mods, so candidates can be attributed to what is installed.

4. **Review the answer.** Read `answer.candidates` strongest first, with each candidate's `level`, `score`, `families` and `cause`, and the rule sources that contributed. Treat `answer.could_not_check` as missing information, not as a clean result. If nothing is named, say so and offer the next step (`next.kind`), usually a bisect.

5. **Confirm and disable.** If the top suspect looks correct:
    - Call `list_instance_mods` to confirm the mod is installed and verify its details.
    - Call `disable_mod` to disable it (requires user approval). Explain to the user that they need to restart the game for the change to take effect.

6. **Follow up.** If the crash persists after disabling the suspect:
    - Check `is_dependent_of` in the suspect list for mods that depend on the disabled one — they may be the indirect cause.
    - Consider other suspects in the ranked list.

7. **No suspects above zero.** If no suspects score above zero, the crash is likely not mod-related (game engine issue, world corruption, shaders, etc.). Advise the user accordingly.

8. **Optional upstream research.** Only after local evidence, curated signatures, and installed-mod scoring are exhausted, an AI agent may search upstream issue trackers or release notes. Clearly distinguish external hypotheses from Agora's deterministic local findings. Crash Doctor itself does not upload evidence, search GitHub, or submit issues.

## Important Notes

- **`disable_mod` and `enable_mod` are the only destructive tools.** They require explicit per-instance approval in Agora Settings -> MCP -> Approvals. If you receive `ERR_MCP_DENIED`, tell the user to grant permission for that instance.

- **The scoring algorithm learns.** When you disable a mod and the crash stops, that attribution is recorded as a prior (signal E). This speeds up future diagnosis for similar crashes.

- **`java_packages` may be empty for older mods.** `suggest_mod_incompatibility` uses `java_packages` extracted from `.jar` files at install time to match stack frames to mods (signal A). Mods installed before this feature existed may have empty `java_packages` and won't match on signal A.

- **Indirect suspects.** Mods with `is_dependent_of` set are mods that depend on a direct suspect. They have reduced scores and are worth testing if the direct suspect is ruled out — disabling the direct mod may have disabled a dependency that another mod relied on.

- **Rate limiting.** The server enforces a rate limit of 100 requests per 60 seconds per session. If you hit `ERR_MCP_TOO_MANY_REQUESTS`, slow down your tool calls.

- **Authentication.** Every request requires the Bearer token shown in Settings -> Integrations -> MCP Server. Send it as an `Authorization: Bearer <token>` header or as the `token` query parameter on the SSE URL. The user can regenerate the token in Settings.
- **Connection.** The server only accepts connections from `127.0.0.1` (loopback) with a valid token.
- **Tool set.** The complete set is `list_instances`, `list_instance_mods`, `disable_mod`, `search_crash_signatures`, `suggest_mod_incompatibility`, `get_system_context`, `read_latest_crash`, `read_mod_manifest`, `enable_mod`, and `search_knowledge_base`.
