# Skyrim Special Edition package

Notes on package values checked against real archives.

## Checked paths

- `pgpatcher`: checked on 2026-11-07 against `PGPatcher-2.3.1.zip` from
  https://github.com/hakasapl/PGPatcher/releases/tag/2.3.1 (SHA-256
  `50e50cb6ccd9c5e052efae641dee2127d101db1778d1f92508074fdddf5749b8`, matching
  GitHub's published digest). The archive holds `PGPatcher/PGPatcher.exe`, plus
  `PG_YOU_CAN_DELETE_THIS_FILE.ini` at its root. Installed into `Data`, that is
  `Data/PGPatcher/PGPatcher.exe`, which is the tool's path. Not yet checked:
  whether Agora's content layout places this archive in `Data`, since neither
  of its top-level entries is a data marker.
