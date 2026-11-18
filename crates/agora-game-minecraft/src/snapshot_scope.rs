//! The folders a Minecraft instance's snapshot covers (MASTER_SPEC §23, §32.7).
//!
//! The list lives here because it is Minecraft's, not the launcher core's: a
//! generic game instance declares its own scope from its game definition, and
//! core's snapshot code takes the roots as an argument.

/// The full tracked-entry set, the starting point for an operation's recovery
/// scope.
pub const TRACKED_ENTRIES: &[&str] = &[
    "mods",
    "config",
    // Pack overrides and instance templates write these, so a rollback that
    // did not capture them left the failed operation's partial writes in place
    // while reporting a clean restore.
    "defaultconfigs",
    "kubejs",
    "resourcepacks",
    "shaderpacks",
    "datapacks",
    "saves",
    "options.txt",
    "instance_manifest.json",
];

/// Tracked entries captured by the mandatory pre-launch snapshot.
///
/// World data (`saves/`) is deliberately excluded: it changes on every game
/// session, so including it forced the pre-launch path to re-walk and re-hash
/// potentially gigabytes of world files each launch. Pre-launch recovery
/// points therefore cover the mod/config/layout state that actually causes
/// launch failures; world data remains protected by the full snapshots taken
/// by install, import, and explicit backup operations.
pub const PRELAUNCH_TRACKED_ENTRIES: &[&str] = &[
    "mods",
    "config",
    "defaultconfigs",
    "kubejs",
    "resourcepacks",
    "shaderpacks",
    "datapacks",
    "options.txt",
    "instance_manifest.json",
];

/// The full tracked-entry set.
pub fn tracked_entries() -> &'static [&'static str] {
    TRACKED_ENTRIES
}

/// The tracked-entry set a mandatory pre-launch snapshot captures.
pub fn prelaunch_tracked_entries() -> &'static [&'static str] {
    PRELAUNCH_TRACKED_ENTRIES
}
