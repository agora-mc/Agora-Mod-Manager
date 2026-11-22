# Loader fixtures

Real upstream files, kept byte for byte. They are the input to the loader
install tests, so a test that passes here passes on what upstream actually
publishes, not on an invented shape that happens to hide the bug.

`-text` in the repository's `.gitattributes` keeps the bytes as captured.

## Generated profiles (Fabric, Quilt)

Fetched from the `source_url` of the catalog entry in
`loader-manifests/loader_manifests.json`. Meta servers regenerate these, so
the catalog pins no hash for them and the file is a snapshot, not a pin.

| File | Source URL |
|---|---|
| `fabric-1.21.1-0.20.1.json` | https://meta.fabricmc.net/v2/versions/loader/1.21.1/0.20.1/profile/json |
| `fabric-26.3-0.20.1.json` | https://meta.fabricmc.net/v2/versions/loader/26.3/0.20.1/profile/json |
| `quilt-1.21.1-0.30.1.json` | https://meta.quiltmc.org/v3/versions/loader/1.21.1/0.30.1/profile/json |
| `quilt-26.3-0.31.0-beta.6.json` | https://meta.quiltmc.org/v3/versions/loader/26.3/0.31.0-beta.6/profile/json |

Fabric 26.3 publishes `sha1`, `sha256` and `sha512` on every library except
`net.fabricmc:fabric-loader`, which publishes none. Quilt 26.3 publishes no
hash at all — only `name` and `url`.

## Installer outputs (Forge, NeoForge)

`*-install-profile.json` is `install_profile.json` and `*-version.json` is
`version.json`, both extracted from the installer JAR named below. The JAR's
SHA-256 is the catalog's pin for that entry; it was verified before
extraction.

| Files | Installer `source_url` | Installer SHA-256 |
|---|---|---|
| `forge-1.21.1-52.1.21-{install-profile,version}.json` | https://maven.minecraftforge.net/net/minecraftforge/forge/1.21.1-52.1.21/forge-1.21.1-52.1.21-installer.jar | `3a67cbffd8761df06256cf7bf31abf9e056edba686082270e9eef9a87cb84901` |
| `forge-26.3-66.1.5-{install-profile,version}.json` | https://maven.minecraftforge.net/net/minecraftforge/forge/26.3-66.1.5/forge-26.3-66.1.5-installer.jar | `4ba54a1bc216ad12dde032eecb999cb3c617b1d0c92ad67add4b6531f45e58b4` |
| `neoforge-21.1.228-{install-profile,version}.json` | https://maven.neoforged.net/releases/net/neoforged/neoforge/21.1.228/neoforge-21.1.228-installer.jar | `5123d7ae23492a512bc6418a6da5d3c765c8346c0558b64a0e1bb47234b87fe9` |
| `neoforge-26.3.0.102-{install-profile,version}.json` | https://maven.neoforged.net/releases/net/neoforged/neoforge/26.3.0.102/neoforge-26.3.0.102-installer.jar | `e26e42b0414da4f68840835b4d637928b971a01852b32b6424747290f84e9f1c` |

The catalog's `version_json_sha256` is the SHA-256 of the *normalized*
`version.json` (`agora_core::download::stable_version_json`: recursive key
sort, `time`/`releaseTime` dropped, compact separators), not of the raw file.
The extracted files match it, which is what `loader_fixtures.rs` asserts.
