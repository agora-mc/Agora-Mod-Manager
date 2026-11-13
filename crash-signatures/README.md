# Community crash signatures

Each `*.json` file here is one **format v2** crash signature. The nightly compiler stores its
fields in the signed `registry.db`; the launcher turns every valid row into a rule in its
`agora.community` crash pack. A signature can be written for **any game**, not only Minecraft.

## Format

```json
{
  "id": "example_signature",
  "name": "Example Crash",
  "games": ["skyrim-se"],
  "regex_pattern": "Unhandled exception",
  "level": "likely",
  "cause": "memory",
  "text": "The game reported an unhandled exception.",
  "action": {
    "kind": "open_jvm_settings",
    "label": "Open JVM Settings"
  }
}
```

| Field | Required | Meaning |
|---|---|---|
| `id` | yes | Slug matching the filename. Lower-case letters, digits, `.`, `_`, `-`. |
| `name` | yes | Display name. |
| `games` | yes | Non-empty list of game ids the signature applies to (for example `minecraft`, `skyrim-se`). |
| `regex_pattern` | yes | Rust regex over the whole crash artifact. At most 256 characters; no look-around or backreferences. |
| `level` | no | `possible` (default) or `likely`. Agora never claims certainty. |
| `cause` | no | One of `mod`, `version_mismatch`, `missing_requirement`, `file`, `setting`, `save`, `memory`, `driver`, `game_install`, `other` (default). |
| `text` | yes | Plain-text headline. No Markdown and no HTML; the launcher shows it as text. |
| `action` | no | A button from the launcher's closed set: `install_content` (needs `content_key`), `open_console`, `open_jvm_settings`, `open_crash_log`, `review_mods`. |
| `action.label` | no | Button text. |

A community signature never names a suspect: it produces a finding and an action, and it cannot
raise any mod's level. If a row fails validation or the rule linter, it is reported as "could not
check" with its id and the other signatures still load.

See [`REGISTRY_CURATION_REFERENCE.md` §7](../REGISTRY_CURATION_REFERENCE.md#7-crash-signatures-crash-signaturesidjson)
for the curator-facing notes and the regex safety rules.

## Testing a signature locally

1. Write an **invented** log (never paste a real crash log into the repository) that contains the
   line your regex should match.
2. Build a catalog, or add the file and run the compiler tests:
   `python compiler/test_compile.py` and `python compiler/test_compile_integration.py`.
3. Run the engine over the log against an instance of the game:
   `agora crash analyze invented.log --instance <instance id>`.
4. Check that the answer contains a finding with your `text` and the action you chose.

The Rust test `every_signature_file_in_the_repository_converts_and_lints` in
`crates/agora-core/tests/crash_community_pack.rs` runs steps 2–3 over every file here, so a broken
signature fails CI.
