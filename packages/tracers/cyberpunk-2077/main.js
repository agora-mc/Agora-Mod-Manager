// Cyberpunk 2077 platform hooks (MASTER_SPEC §30.12).
//
// Data places the two shapes the game defines (`mods/<name>/info.json`, and a bare
// `*.archive` into `archive/pc/mod`). Everything here covers what data cannot say: the
// REDmod load order format and its rule, a loose redscript set, the RED4ext loader check,
// and the deploy tool's freshness.

import { game } from "agora";

// A place for a loose set of `*.reds` files. The game reads redscript from `r6/scripts`,
// not from `archive/pc/mod`, so no data rule can name it: the folder has no marker file.
// `<Mod>/init.reds` becomes `r6/scripts/<Mod>/init.reds`. A set with no single folder is
// left to the Unknown prompt, because a script is never told the item's name.
// Source for `r6/scripts`: https://wiki.redmodding.org/cyberpunk-2077-modding/for-mod-users/users-modding-cyberpunk-2077
export function classifyItem(args) {
  const files = (args.files || []).map((path) => String(path).replace(/\\/g, "/"));
  if (files.length === 0 || !files.every((path) => path.toLowerCase().endsWith(".reds"))) {
    return null;
  }
  if (files.some((path) => path.split("/").length < 2)) {
    return null;
  }
  const folder = files[0].split("/")[0];
  if (files.some((path) => path.split("/")[0] !== folder)) {
    return null;
  }
  return {
    type: "place",
    source_path: folder,
    mount_path: "r6/scripts/" + folder,
    reason:
      "loose `*.reds` files for `" +
      folder +
      "` go in `r6/scripts/" +
      folder +
      "` (redscript is read from `r6/scripts`), not in an archive",
  };
}

// REDmod's order is written as one mod folder per line, highest precedence first.
// The `-modlist` file takes the same names as `-mod`.
// Source: https://wiki.redmodding.org/cyberpunk-2077-modding/for-mod-users/users-modding-cyberpunk-2077/redmod/commands/deploy
// The one-name-per-line layout is unverified; the page names the option but not the layout.
export function parseModlist(args) {
  const text = (args.bytes || [])
    .map((byte) => String.fromCharCode(byte))
    .join("");
  return text
    .replace(/^\uFEFF/, "")
    .split(/\r?\n/)
    .map((line) => line.trim())
    .filter((line) => line.length > 0 && !line.startsWith("#"))
    .map((name) => ({ name: name, active: true }));
}

export function renderModlist(args) {
  const names = (args.lines || []).map((line) => line.name);
  return names.length === 0 ? "" : names.join("\n") + "\n";
}

// REDmod loads folders in the order given and the first mod to change a file wins.
// ASCII order already puts `!` before letters; a folder prefixed with `!` is the
// documented way to pull a compatibility patch to the front, so make that explicit.
// Source (ASCII order, first wins): https://wiki.redmodding.org/cyberpunk-2077-modding/for-mod-users/users-modding-cyberpunk-2077/load-order
// Only names that are actually listed are named back, so a rule never reports a mod
// the player does not have.
export function redmodRules(args) {
  const entries =
    (args.context &&
      args.context.load_order &&
      args.context.load_order.entries) ||
    [];
  const names = entries.map((entry) => String(entry.id));
  const first = names.filter((name) => name.startsWith("!"));
  const rest = names.filter((name) => !name.startsWith("!"));
  const rules = [];
  for (const ahead of first) {
    for (const behind of rest) {
      rules.push({ kind: "before", first: ahead, second: behind });
    }
  }
  return rules;
}

function hasEntry(reply, name) {
  const wanted = name.toLowerCase();
  return (reply.entries || []).some(
    (entry) => String(entry.name).toLowerCase() === wanted
  );
}

// RED4ext is two files: the DLL the game loads is `red4ext/RED4ext.dll`, but it is reached
// through `bin/x64/winmm.dll`. Half an install looks present and does nothing.
// Sources: https://docs.red4ext.com/getting-started/installing-red4ext
//          https://wiki.redmodding.org/cyberpunk-2077-modding/for-mod-users/user-guide-troubleshooting/finding-and-reading-log-files
export function diagnose(args) {
  const found = [];
  let red4ext = false;
  let loader = false;
  try {
    red4ext = hasEntry(game.list("red4ext"), "RED4ext.dll");
  } catch (error) {
    red4ext = false;
  }
  try {
    loader = hasEntry(game.list("bin/x64"), "winmm.dll");
  } catch (error) {
    loader = false;
  }
  if (red4ext && !loader) {
    found.push({
      code: "RED4EXT_NO_LOADER",
      message:
        "RED4ext is installed but `bin/x64/winmm.dll` is missing, so the game never loads it.",
      severity: "warning",
      repairs: [
        "Reinstall RED4ext so both `red4ext/RED4ext.dll` and `bin/x64/winmm.dll` are in the game folder.",
      ],
    });
  }

  // Reading the log by game path also proves the `game:read` scope reaches a real file.
  try {
    const reply = game.readLines("red4ext/logs/red4ext.log", 0, 200);
    const lines = (reply.lines || []).filter((line) => line.trim().length > 0);
    const last = lines[lines.length - 1] || "";
    if (/error|fail/i.test(last)) {
      found.push({
        code: "RED4EXT_LOG_ERROR",
        message: "RED4ext's log ends with an error: " + last,
        severity: "warning",
        repairs: [
          "Read `red4ext/logs/red4ext.log` and update or remove the plugin it names.",
        ],
      });
    }
  } catch (error) {
    // No log yet (the game has not run with RED4ext) or unreadable: nothing to add.
  }
  return found;
}
