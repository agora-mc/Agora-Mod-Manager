# Skyrim Special Edition package

Notes on package values checked against real archives.

## Checked paths

- `pgpatcher`: checked on 2026-11-07 against `PGPatcher-2.3.1.zip` from
  https://github.com/hakasapl/PGPatcher/releases/tag/2.3.1 (SHA-256
  `50e50cb6ccd9c5e052efae641dee2127d101db1778d1f92508074fdddf5749b8`, matching
  GitHub's published digest). The archive holds `PGPatcher/PGPatcher.exe`, plus
  `PG_YOU_CAN_DELETE_THIS_FILE.ini` at its root. Installed into `Data`, that is
  `Data/PGPatcher/PGPatcher.exe`, which is the tool's path. Neither top-level
  entry was a data marker, so the archive asked the player where it goes;
  `pgpatcher` is now a data marker, like the other tool folders, and
  `crates/agora-game-api/tests/official_layouts.rs` checks this file list.
- `skyrim-together` (catalog entry): checked on 2026-11-08 against release
  1.8.4. The unversioned Windows zip is built for 1.7.104 and the
  `sse1.6.1170` zip for 1.6.1170 (Steam) and 1.6.1179 (GOG); see the entry's
  curator note for sources. The same test file checks both asset patterns
  against that release's asset names.
