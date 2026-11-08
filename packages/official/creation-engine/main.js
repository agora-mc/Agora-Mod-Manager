// Creation Engine load-order rules (MASTER_SPEC §30.2, §30.12).
//
// A plugin's TES4 record says which masters it needs and whether it is a master or a light
// plugin. These are the same rules Agora used to keep in core; they live here now so core knows
// nothing about a particular engine's file format.
//
// Header layout, as `esplugin` reads it for Skyrim SE: a 24-byte record header (type, data size,
// flags at offset 8, form id, then unused words), then subrecords of a 4-byte type, a u16 length
// and the data. Flag `0x1` is master and `0x200` is light. The `.esm` and `.esl` extensions imply
// the master flag, and `.esl` implies light.
// Source for the flags and extensions: https://github.com/Ortham/esplugin (GameId handling).

import { game } from "agora";

const MASTER_FLAG = 0x1;
const LIGHT_FLAG = 0x200;
const FULL_PLUGIN_LIMIT = 254;
const LIGHT_PLUGIN_LIMIT = 4096;
const FIRST_READ = 512;
const MAX_HEADER = 1024 * 1024;

function extensionOf(name) {
  const dot = name.lastIndexOf(".");
  return dot < 0 ? "" : name.slice(dot).toLowerCase();
}

function ascii(bytes, at, length) {
  let text = "";
  for (let index = at; index < at + length; index += 1) {
    text += String.fromCharCode(bytes[index]);
  }
  return text;
}

function text(bytes, start, end) {
  let last = end;
  while (last > start && bytes[last - 1] === 0) last -= 1;
  let out = "";
  for (let index = start; index < last; index += 1) {
    out += String.fromCharCode(bytes[index]);
  }
  return out;
}

/// Read one plugin's header from the instance as the game sees it.
function readHeader(name) {
  const path = "Data/" + name;
  let buffer;
  try {
    buffer = game.readBytes(path, 0, FIRST_READ);
  } catch (error) {
    return { error: String(error && error.message ? error.message : error) };
  }
  let bytes = new Uint8Array(buffer);
  if (bytes.length < 24) {
    return { error: "the file is shorter than a plugin header" };
  }
  if (ascii(bytes, 0, 4) !== "TES4") {
    return { error: "the file does not start with a TES4 record" };
  }
  let view = new DataView(buffer);
  const dataSize = view.getUint32(4, true);
  const flags = view.getUint32(8, true);
  const wanted = dataSize + 24;
  if (wanted > MAX_HEADER) {
    return { error: "the header is larger than Agora reads" };
  }
  if (wanted > bytes.length) {
    try {
      buffer = game.readBytes(path, 0, wanted);
    } catch (error) {
      return { error: String(error && error.message ? error.message : error) };
    }
    bytes = new Uint8Array(buffer);
    view = new DataView(buffer);
    if (bytes.length < wanted) {
      return { error: "the header is truncated" };
    }
  }

  const masters = [];
  let at = 24;
  const end = 24 + dataSize;
  let large = 0;
  while (at + 6 <= end) {
    const kind = ascii(bytes, at, 4);
    const shortSize = view.getUint16(at + 4, true);
    const start = at + 6;
    if (start + shortSize > end) {
      return { error: "a subrecord runs past the end of the header" };
    }
    if (kind === "XXXX") {
      // A large subrecord states its size in the four bytes after the XXXX header.
      if (shortSize < 4) {
        return { error: "an XXXX subrecord is too short" };
      }
      large = view.getUint32(start, true);
      at = start + shortSize;
      continue;
    }
    const size = large > 0 ? large : shortSize;
    large = 0;
    if (start + size > end) {
      return { error: "a subrecord runs past the end of the header" };
    }
    if (kind === "MAST") {
      masters.push(text(bytes, start, start + size));
    }
    at = start + size;
  }

  const extension = extensionOf(name);
  return {
    masters: masters,
    master:
      (flags & MASTER_FLAG) !== 0 ||
      extension === ".esm" ||
      extension === ".esl",
    light: (flags & LIGHT_FLAG) !== 0 || extension === ".esl",
  };
}

// The rules, badges and list findings core applies to a Creation Engine plugin list.
export function creationEngineRules(args) {
  const context = (args && args.context) || {};
  const entries =
    (context.load_order && context.load_order.entries) || [];
  const rules = [];
  const badges = {};
  const findings = [];
  let full = 0;
  let light = 0;

  for (const entry of entries) {
    const name = String(entry.id);
    if (!entry.present) continue;
    const header = readHeader(name);
    if (header.error) {
      findings.push({
        message: "'" + name + "' has a header that cannot be read: " + header.error,
        items: [name],
      });
      continue;
    }
    const marks = [];
    if (header.master) marks.push("Master");
    if (header.light) marks.push("Light");
    if (marks.length > 0) badges[name] = marks;
    if (!entry.enabled) continue;
    if (header.light) {
      light += 1;
    } else {
      full += 1;
    }
    for (const master of header.masters) {
      rules.push({ kind: "requires", item: name, master: master });
    }
  }

  if (full > FULL_PLUGIN_LIMIT) {
    findings.push({
      message:
        full +
        " full plugins are active; the game loads at most " +
        FULL_PLUGIN_LIMIT,
      refuses: true,
    });
  }
  if (light > LIGHT_PLUGIN_LIMIT) {
    findings.push({
      message:
        light +
        " light plugins are active; the game loads at most " +
        LIGHT_PLUGIN_LIMIT,
      refuses: true,
    });
  }

  return { rules: rules, badges: badges, findings: findings };
}
