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

import { crash, game, net, storage, wasm } from "agora";

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

// ---------------------------------------------------------------------------
// Crash log parsers (MASTER_SPEC §30.11)
//
// Two tools write Creation Engine crash logs: Crash Logger (CrashLoggerSSE) and
// Trainwreck. Both are plain text with the same broad sections, but the exact
// line shapes changed across versions. These parsers feature-detect: they keep
// whatever section of the version they are given, and their coverage says what
// they looked for so an absent section is "could not check", not "not there".
//
// Crash Logger's format and its default output folder, `Documents/My Games/
// <game>/SKSE/crash-<timestamp>.log`, come from its own source:
// https://github.com/alandtse/CrashLoggerSSE (MIT). Trainwreck has no source
// here, so its format was learned from the logs themselves; its folder and
// filename pattern are marked unverified in crash/FACTS.md.
//
// A parser sees no paths. It reads only the artifact id it was given.

const CRASH_CONTRACT = 1;

const CRASH_SECTIONS = new Set([
  "SYSTEM SPECS:",
  "VR SPECS:",
  "PROCESS INFO:",
  "PROBABLE CALL STACK:",
  "CALL STACK ([P]robable / [S]tack scan):",
  "CALL STACK (HYBRID):",
  "RAW CALL STACK:",
  "REGISTERS:",
  "STACK:",
  "MODULES:",
  "SKSE PLUGINS:",
  "SCRIPT EXTENDER PLUGINS:",
  "PLUGINS:",
  "POSSIBLE RELEVANT OBJECTS:",
  "ACCESS VIOLATION ANALYSIS:",
  "THREAD CONTEXT (HEURISTIC):",
  "GAME STATE:",
  "C++ EXCEPTION:",
  "CRASH LOG REPORT:",
  "CRASHED WHILE LOADING A MODEL (this thread, newest first):",
  "MODEL PATHS FOR RESOURCE IDS ON THIS THREAD'S STACK (innermost first):",
]);

const SCALAR_TYPE = /^(?:void|char|bool|float|double|long|short|int|unsigned(?:\s+\w+)?|signed(?:\s+\w+)?|size_t|ssize_t|intptr_t|uintptr_t|std::size_t|std::ssize_t|std::intptr_t|std::uintptr_t|u?int(?:8|16|32|64)_t|DWORD|QWORD|WORD|BYTE|HANDLE|HMODULE|LPVOID|PVOID)$/;

const ASSET_EXTENSION = /\.(?:nif|dds|hkx|pex|psc|tri|kf|kfm|egm|egt|lip|wav|xwm|fuz|swf|gfx|ini|json|txt|bsa|ba2)$/i;
const PLUGIN_EXTENSION = /\.(?:esp|esm|esl)$/i;
const SOURCE_PATTERN = /(?:^|[\s\[])((?:[A-Za-z]:[\\/])?[^\[\]\s|]+?\.(?:cpp|c|cc|cxx|h|hpp|hxx)):(\d+)/;
const STRENGTH_ORDER = { mention: 0, object: 1, exec: 2, exec_top: 3 };

function crashText(value) {
  return value === undefined || value === null ? "" : String(value);
}

function crashTrim(value) {
  return crashText(value).trim();
}

function crashUnquote(value) {
  const text = crashTrim(value);
  if (text.length >= 2 && text.startsWith('"') && text.endsWith('"')) {
    return text.slice(1, -1);
  }
  return text;
}

function crashClean(value, maxAttr) {
  const text = crashText(value);
  return text.length > maxAttr ? text.slice(0, maxAttr) : text;
}

function crashNumber(value) {
  const number = Number(value);
  return Number.isFinite(number) ? number : undefined;
}

/// Prefix uppercase, as the logs print addresses. Addresses and form ids travel
/// as text because a script number holds only 53 bits.
function crashHex(text) {
  return "0x" + crashTrim(text).replace(/^0x/i, "").toUpperCase();
}

function crashIndent(line) {
  let width = 0;
  for (const character of line) {
    if (character === "\t") width += 4;
    else if (character === " ") width += 1;
    else break;
  }
  return width;
}

function crashIsSectionHeader(line) {
  const text = line.trim();
  if (CRASH_SECTIONS.has(text)) return true;
  if (text.length < 4 || text.length > 120 || !text.endsWith(":")) return false;
  if (!/^[A-Z]/.test(text)) return false;
  return /^[A-Z][A-Z0-9 ()'\/\[\]\-\.\+]+:$/.test(text);
}

function crashSections(lines) {
  const headers = [];
  for (let index = 0; index < lines.length; index += 1) {
    if (crashIsSectionHeader(lines[index])) {
      headers.push({
        name: lines[index].trim().replace(/:$/, ""),
        start: index,
        end: lines.length,
      });
    }
  }
  for (let index = 0; index + 1 < headers.length; index += 1) {
    headers[index].end = headers[index + 1].start;
  }
  const map = {};
  for (const header of headers) {
    if (!(header.name in map)) map[header.name] = header;
  }
  return { headers, map };
}

function crashReadAllLines(request) {
  const artifact = request.artifact.id;
  const maxLines = crashNumber(request.limits.max_lines) || 100000;
  const chunk = Math.max(1, Math.min(maxLines, 20000));
  const lines = [];
  let from = 0;
  let cut = false;
  let gap = false;
  let budget = false;

  for (let guard = 0; guard < 64; guard += 1) {
    let reply;
    try {
      reply = crash.readLines(artifact, from, chunk);
    } catch (error) {
      return {
        lines,
        cut,
        gap,
        budget,
        error: String(error && error.message ? error.message : error),
      };
    }
    if (!reply || !Array.isArray(reply.lines)) {
      return { lines, cut, gap, budget, error: "the host returned no lines" };
    }
    for (const line of reply.lines) lines.push(line);
    if (reply.cut) cut = true;
    if (reply.gap) gap = true;
    if (reply.next_line === undefined || reply.next_line === null) break;
    if (reply.next_line <= from) break;
    from = reply.next_line;
    if (lines.length >= maxLines * 2) {
      budget = true;
      break;
    }
  }
  return { lines, cut, gap, budget, error: null };
}

function crashVersion(text) {
  const value = crashTrim(text).replace(/^v/i, "").replace(/-/g, ".");
  return value || undefined;
}

function crashGameVersionAtLeast(version, major, minor, patch) {
  const parts = crashText(version)
    .split(".")
    .map((part) => Number(part));
  const got = [parts[0] || 0, parts[1] || 0, parts[2] || 0];
  const wanted = [major || 0, minor || 0, patch || 0];
  for (let index = 0; index < 3; index += 1) {
    if (got[index] > wanted[index]) return true;
    if (got[index] < wanted[index]) return false;
  }
  return true;
}

function crashProducerHeader(lines, format) {
  let producer;
  let gameLine;
  for (const line of lines.slice(0, 40)) {
    if (!producer) {
      if (format === "crash_logger") {
        const match = line.match(/CrashLoggerSSE\s+v([0-9][0-9A-Za-z.\-]*)/);
        if (match) producer = crashVersion(match[1]);
      } else if (format === "trainwreck") {
        const match = line.match(/^trainwreck\s+v([0-9][0-9A-Za-z.\-]*)/);
        if (match) producer = crashVersion(match[1]);
      }
    }
    if (!gameLine) {
      // Some uploaded logs lost the first character of "Skyrim"; the rest of
      // the line still has everything that matters.
      const match = line.match(/^\s*S?kyrim\s*(SSE|SE|VR)\s+v([0-9][0-9A-Za-z.\-]*)/);
      if (match) gameLine = { edition: match[1], version: match[2] };
    }
  }
  if (!producer) return null;
  let edition = gameLine ? gameLine.edition : undefined;
  if (!edition) {
    const exception = lines.find((line) => /^\s*Unhandled exception\b/.test(line));
    if (exception && /SkyrimVR\.exe/i.test(exception)) edition = "VR";
    else if (exception && /SkyrimSE\.exe/i.test(exception)) edition = "SSE";
  }
  const vr = edition === "VR";
  return {
    format,
    version: producer,
    game: {
      name: "Skyrim",
      edition: edition === "VR" ? "VR" : edition === "SE" || edition === "SSE" ? "SSE" : undefined,
      version: gameLine ? gameLine.version : undefined,
      vr: edition ? vr : undefined,
      executable: edition ? (vr ? "SkyrimVR.exe" : "SkyrimSE.exe") : undefined,
    },
  };
}

function crashFindLine(lines, pattern) {
  for (let index = 0; index < lines.length; index += 1) {
    const match = lines[index].match(pattern);
    if (match) return { match, index, line: lines[index] };
  }
  return null;
}

function crashFindSection(sections, name) {
  if (sections.map[name]) return sections.map[name];
  for (const header of sections.headers) {
    if (header.name.startsWith(name)) return header;
  }
  return undefined;
}

function crashScalar(value) {
  return typeof value === "string" || typeof value === "number" || typeof value === "boolean";
}

function crashAddAttr(attrs, name, value, limits) {
  if (value === undefined || value === null) return;
  if (!crashScalar(value)) return;
  if (typeof value === "string") {
    if (value.length === 0) return;
    attrs[name] = crashClean(value, limits.max_attr_len);
  } else {
    attrs[name] = value;
  }
}

function crashAddRef(refs, kind, value, strength, limits) {
  const text = crashClean(crashUnquote(value), limits.max_attr_len);
  if (!text) return;
  const key = kind + "\u0000" + text;
  const previous = refs.get(key);
  if (previous) {
    if ((STRENGTH_ORDER[strength] || 0) > (STRENGTH_ORDER[previous.strength] || 0)) {
      previous.strength = strength;
    }
    return;
  }
  refs.set(key, { kind, value: text, strength: strength || "mention" });
}

function crashRefList(refs, limits, state) {
  const all = Array.from(refs.values());
  const max = crashNumber(limits.max_refs_per_fact) || 16;
  if (all.length > max) {
    state.refsTruncated = true;
    return all.slice(0, max);
  }
  return all;
}

function crashPushFact(state, limits, fact) {
  if (state.facts.length >= (crashNumber(limits.max_facts) || 20000)) {
    state.budgetSpent = true;
    return false;
  }
  state.facts.push(fact);
  state.kinds[fact.kind] = (state.kinds[fact.kind] || 0) + 1;
  return true;
}

function crashSpan(section, start, end) {
  const span = { section };
  if (start !== undefined) span.line_start = start + 1;
  if (end !== undefined) span.line_end = end + 1;
  return span;
}

function crashLooksLikePath(value) {
  const text = crashUnquote(value);
  return (text.includes("\\") || text.includes("/")) && ASSET_EXTENSION.test(text);
}

function crashPointerClass(address) {
  if (!address) return undefined;
  let value;
  try {
    value = BigInt(address);
  } catch (error) {
    return undefined;
  }
  if (value === 0n) return "null";
  if (value < 0x10000n) return "near_null";
  if (value >= 0xfffffffffffffff0n) return "non_canonical";
  if (value >= 0xffff800000000000n) return "kernel";
  if (value > 0x00007fffffffffffn) return "non_canonical";
  return "user";
}

function crashExceptionCategory(code) {
  const name = crashText(code).toUpperCase();
  if (name.includes("ACCESS_VIOLATION")) return "access_violation";
  if (name.includes("IN_PAGE_ERROR")) return "access_violation";
  if (name.includes("STACK_OVERFLOW")) return "unknown";
  if (name.includes("BREAKPOINT")) return "assert";
  if (name.includes("ASSERT")) return "assert";
  if (name.includes("OUT_OF_MEMORY") || name.includes("MEMORY")) return "oom";
  if (name.includes("FAST_FAIL")) return "fast_fail";
  return "unknown";
}

function crashIsObjectType(type) {
  const text = crashTrim(type)
    .replace(/^(?:const|struct|class)\s+/, "")
    .replace(/\s*\*+$/, "")
    .trim();
  return text.length > 0 && !SCALAR_TYPE.test(text);
}

function crashSplitType(type) {
  const text = crashText(type);
  const arrow = text.indexOf(" -> ");
  const base = (arrow >= 0 ? text.slice(0, arrow) : text).trim();
  let module;
  let offset;
  if (arrow >= 0) {
    const target = text.slice(arrow + 4);
    const match = target.match(/^(\S+?)\+([0-9A-Fa-f]+)/);
    if (match) {
      module = match[1];
      offset = crashHex(match[2]);
    }
  }
  return { type: base, module, offset };
}

function crashObjectHeader(line, logType) {
  const compact = line.match(/^[\t ]+([A-Z][A-Z0-9+]*):\s*\((.*?)\)(.*)$/);
  if (compact) {
    return {
      compact: true,
      place: compact[1],
      value: "",
      type: compact[2],
      rest: compact[3],
    };
  }
  const stack = line.match(/^[\t ]+\[RSP\+([0-9A-Fa-f]+)\s*\]\s+0x([0-9A-Fa-f]+)\s+\((.*)\)\s*(?:\[[^\]]*\])?\s*$/);
  if (stack) {
    const split = crashSplitType(stack[3]);
    return {
      compact: false,
      place: "RSP+" + stack[1].toUpperCase(),
      value: crashHex(stack[2]),
      type: split.type,
      module: split.module,
      moduleOffset: split.offset,
    };
  }
  const register = line.match(/^[\t ]+([A-Z][A-Z0-9]*)\s+0x([0-9A-Fa-f]+)\s+\((.*)\)\s*(?:\[[^\]]*\])?\s*$/);
  if (register) {
    const split = crashSplitType(register[3]);
    return {
      compact: false,
      place: register[1],
      value: crashHex(register[2]),
      type: split.type,
      module: split.module,
      moduleOffset: split.offset,
    };
  }
  return null;
}

function crashField(line) {
  const match = line.match(/^[\t ]*([A-Za-z][A-Za-z0-9 _\/\-\.]*):\s*(.*)$/);
  if (!match) return null;
  return { label: match[1].trim(), value: match[2].trim() };
}

function crashAddField(object, label, value) {
  if (!(label in object.fields)) object.fields[label] = [];
  object.fields[label].push(value);
}

// Iterative on purpose: real logs nest decoded objects deeply enough that a
// recursive walk overflows the script stack.
function crashParseBlock(lines, root, baseIndent) {
  const stack = [{ indent: baseIndent, object: root }];
  let index = 0;
  while (index < lines.length) {
    const line = lines[index];
    if (line.trim() === "") {
      index += 1;
      continue;
    }
    const indent = crashIndent(line);
    const field = crashField(line);
    if (!field) {
      index += 1;
      continue;
    }
    while (stack.length > 1 && indent <= stack[stack.length - 1].indent) {
      stack.pop();
    }
    const object = stack[stack.length - 1].object;
    let peek = index + 1;
    while (peek < lines.length && lines[peek].trim() === "") peek += 1;
    const hasChildren = peek < lines.length && crashIndent(lines[peek]) > indent;
    if (hasChildren) {
      const child = {
        place: object.place,
        value: "",
        type: field.label,
        field: field.label,
        name: field.value,
        depth: object.depth + 1,
        fields: {},
        children: [],
        line: object.line,
        endLine: object.endLine,
      };
      object.children.push(child);
      stack.push({ indent, object: child });
    } else {
      crashAddField(object, field.label, field.value);
    }
    index += 1;
  }
}

function crashFieldList(object, names) {
  for (const name of names) {
    const values = object.fields[name];
    if (values && values.length > 0) return values;
  }
  return [];
}

function crashObjectAttrs(object, limits) {
  const attrs = {};
  crashAddAttr(attrs, "type", object.type, limits);
  crashAddAttr(attrs, "place", object.place, limits);
  crashAddAttr(attrs, "value", object.value, limits);
  if (object.field) crashAddAttr(attrs, "field", object.field, limits);
  if (object.depth) crashAddAttr(attrs, "depth", object.depth, limits);
  if (object.name) crashAddAttr(attrs, "name", crashUnquote(object.name), limits);
  if (!object.name) {
    for (const name of crashFieldList(object, ["Name", "Full Name", "Object Reference", "GetFullName"])) {
      const value = crashUnquote(name);
      if (value && value !== "None" && value !== "---") {
        crashAddAttr(attrs, "name", value, limits);
        break;
      }
    }
  }
  for (const form of crashFieldList(object, ["FormID", "Form ID"])) {
    crashAddAttr(attrs, "form_id", crashUnquote(form), limits);
    break;
  }
  for (const editor of crashFieldList(object, ["EditorID", "Editor ID"])) {
    crashAddAttr(attrs, "editor_id", crashUnquote(editor), limits);
    break;
  }
  for (const type of crashFieldList(object, ["FormType", "Form Type"])) {
    crashAddAttr(attrs, "form_type", crashTrim(type), limits);
    break;
  }
  for (const flags of crashFieldList(object, ["Flags"])) {
    crashAddAttr(attrs, "flags", crashTrim(flags), limits);
    break;
  }
  for (const rtti of crashFieldList(object, ["RTTIName"])) {
    crashAddAttr(attrs, "rtti", crashUnquote(rtti), limits);
    break;
  }
  return attrs;
}

function crashObjectRefs(object, limits, state) {
  const refs = new Map();
  for (const file of crashFieldList(object, ["File"])) {
    const value = crashUnquote(file);
    if (!value) continue;
    if (PLUGIN_EXTENSION.test(value) && !value.includes("\\") && !value.includes("/")) {
      crashAddRef(refs, "plugin", value, "object", limits);
    } else if (crashLooksLikePath(value)) {
      crashAddRef(refs, "file", value, "object", limits);
    }
  }
  for (const form of crashFieldList(object, ["FormID", "Form ID"])) {
    crashAddRef(refs, "form", crashUnquote(form), "object", limits);
  }
  for (const script of crashFieldList(object, ["Script", "Script Name"])) {
    crashAddRef(refs, "script", crashUnquote(script), "object", limits);
  }
  for (const name of [
    "Name",
    "Full Name",
    "Object Reference",
    "GetFullName",
  ]) {
    for (const value of crashFieldList(object, [name])) {
      if (crashLooksLikePath(value)) crashAddRef(refs, "file", value, "object", limits);
    }
  }
  for (const name of [
    "Animation Name",
    "Skeleton Name",
    "fxpFilename",
    "inputFilePath",
    "filePath",
    "streamName",
    "Path",
    "Model",
  ]) {
    for (const value of crashFieldList(object, [name])) {
      if (crashLooksLikePath(value)) crashAddRef(refs, "file", value, "object", limits);
    }
  }
  // Any other field that plainly names an asset is still a file ref.
  for (const values of Object.values(object.fields)) {
    for (const value of values) {
      if (crashLooksLikePath(value)) crashAddRef(refs, "file", value, "object", limits);
    }
  }
  return crashRefList(refs, limits, state);
}

function crashEmitChain(state, limits, object, section) {
  const chains = crashFieldList(object, ["Modified by"]);
  for (const chain of chains) {
    const parts = crashText(chain)
      .split("->")
      .map((part) => crashUnquote(part))
      .filter((part) => part.length > 0);
    if (parts.length === 0) continue;
    const max = crashNumber(limits.max_refs_per_fact) || 16;
    for (let start = 0; start < parts.length; start += max) {
      const refs = new Map();
      for (const part of parts.slice(start, start + max)) {
        crashAddRef(refs, "plugin", part, "object", limits);
      }
      const attrs = {
        place: object.place,
        depth: object.depth,
        field: "modified_by",
        start,
      };
      crashAddAttr(attrs, "form_id", object.fields["FormID"] ? crashUnquote(object.fields["FormID"][0]) : undefined, limits);
      crashPushFact(state, limits, {
        kind: "creation.plugin_chain",
        attrs,
        refs: crashRefList(refs, limits, state),
        span: crashSpan(section, object.line, object.endLine),
      });
    }
  }
}

function crashEmitObject(state, limits, root, section, isRoot) {
  const stack = [{ object: root, isRoot }];
  while (stack.length > 0) {
    const current = stack.pop();
    const object = current.object;
    if (current.isRoot && !crashIsObjectType(object.type) && object.children.length === 0) {
      const interesting = [
        "File",
        "FormID",
        "Form ID",
        "EditorID",
        "Editor ID",
        "Modified by",
        "RTTIName",
      ].some((name) => (object.fields[name] || []).length > 0);
      if (!interesting) continue;
    }
    const attrs = crashObjectAttrs(object, limits);
    const refs = crashObjectRefs(object, limits, state);
    if (crashPushFact(state, limits, {
      kind: "creation.object",
      attrs,
      refs,
      span: crashSpan(section, object.line, object.endLine),
    })) {
      crashEmitChain(state, limits, object, section);
    }
    for (let index = object.children.length - 1; index >= 0; index -= 1) {
      stack.push({ object: object.children[index], isRoot: false });
    }
  }
}

function crashParseObjects(state, limits, lines, section, logType) {
  const start = section.start + 1;
  let index = start;
  while (index < section.end) {
    const line = lines[index];
    const header = crashObjectHeader(line, logType);
    if (!header) {
      index += 1;
      continue;
    }
    if (header.compact) {
      const attrs = { place: header.place };
      crashAddAttr(attrs, "type", header.type, limits);
      const refs = new Map();
      const name = header.rest.match(/"([^"]*)"/);
      if (name) crashAddAttr(attrs, "name", name[1], limits);
      const plugin = header.rest.match(/\("([^"]*)"\)/) || header.rest.match(/"([^"]*\.(?:esp|esm|esl))"/i);
      if (plugin) crashAddRef(refs, "plugin", plugin[1], "object", limits);
      const forms = Array.from(header.rest.matchAll(/\[(0x[0-9A-Fa-f]+)\]/g));
      if (forms.length > 0) {
        crashAddAttr(attrs, "form_id", forms[0][1], limits);
        for (const form of forms) crashAddRef(refs, "form", form[1], "object", limits);
      }
      crashPushFact(state, limits, {
        kind: "creation.object",
        attrs,
        refs: crashRefList(refs, limits, state),
        span: crashSpan(section.name, index, index),
      });
      index += 1;
      continue;
    }
    const headerIndent = crashIndent(line);
    let end = index + 1;
    while (end < section.end) {
      if (lines[end].trim() === "") break;
      const indent = crashIndent(lines[end]);
      if (indent <= headerIndent) break;
      end += 1;
    }
    const object = {
      place: header.place,
      value: header.value,
      type: header.type,
      depth: 0,
      fields: {},
      children: [],
      line: index,
      endLine: end - 1,
    };
    crashParseBlock(lines.slice(index + 1, end), object, headerIndent);
    crashEmitObject(state, limits, object, section.name, true);
    if (header.module) {
      // A scalar pointer into a module is a code address, not an object.
      const refs = new Map();
      crashAddRef(refs, "module", header.module, "mention", limits);
      const attrs = { place: header.place, value: header.value, module_offset: header.moduleOffset };
      crashPushFact(state, limits, {
        kind: "creation.code_pointer",
        attrs,
        refs: crashRefList(refs, limits, state),
        span: crashSpan(section.name, index, index),
      });
    }
    index = end;
  }
}

function crashParseSystem(state, limits, sections, lines) {
  const section = crashFindSection(sections, "SYSTEM SPECS");
  if (!section) return false;
  const attrs = {};
  const gpus = [];
  const memory = {};
  const processor = {};
  for (let index = section.start + 1; index < section.end; index += 1) {
    const line = lines[index];
    const text = line.trim();
    let match = text.match(/^OS:\s*(.*)$/);
    if (match) crashAddAttr(attrs, "os", match[1], limits);
    match = text.match(/^CPU:\s*(.*)$/);
    if (match) {
      const cpu = match[1].trim();
      const space = cpu.indexOf(" ");
      crashAddAttr(processor, "vendor", space >= 0 ? cpu.slice(0, space) : cpu, limits);
      crashAddAttr(processor, "name", space >= 0 ? cpu.slice(space + 1).trim() : cpu, limits);
    }
    match = text.match(/^CPU Cores:\s*(\d+)\s+logical,\s*(\d+)\s+physical,\s*(\d+)\s+packages/);
    if (match) {
      processor.logical = Number(match[1]);
      processor.physical = Number(match[2]);
      processor.packages = Number(match[3]);
    }
    match = text.match(/^GPU #\d+:\s*(.*)$/);
    if (match) gpus.push(match[1].trim());
    match = text.match(/^PHYSICAL MEMORY:\s*([\d.]+)\s*(?:GB|GiB)\s*\/\s*([\d.]+)\s*(?:GB|GiB)/);
    if (!match) match = text.match(/^RAM:\s*([\d.]+)\s*(?:GB|GiB)\s*\/\s*([\d.]+)\s*(?:GB|GiB)/);
    if (match) {
      const used = Number(match[1]);
      const total = Number(match[2]);
      if (Number.isFinite(used) && Number.isFinite(total) && total > 0) {
        memory.physical_used_mb = Math.round(used * 1024);
        memory.physical_total_mb = Math.round(total * 1024);
        attrs.ram_free_mb = Math.round((total - used) * 1024);
      }
    }
    match = text.match(/^VIRTUAL MEMORY:\s*([\d.]+)\s*(?:GB|GiB)\s*\/\s*([\d.]+)\s*(?:GB|GiB)/);
    if (match) {
      const used = Number(match[1]);
      const total = Number(match[2]);
      if (Number.isFinite(used) && Number.isFinite(total) && total > 0) {
        memory.virtual_used_mb = Math.round(used * 1024);
        memory.virtual_total_mb = Math.round(total * 1024);
        attrs.commit_pct = Math.round((used / total) * 1000) / 10;
      }
    }
    match = text.match(/^PAGE FILE:\s*([\d.]+)\s*(?:GB|GiB)\s*\/\s*([\d.]+)\s*(?:GB|GiB)/);
    if (match) {
      const used = Number(match[1]);
      const total = Number(match[2]);
      if (Number.isFinite(used) && Number.isFinite(total) && total > 0) {
        memory.pagefile_used_mb = Math.round(used * 1024);
        memory.pagefile_total_mb = Math.round(total * 1024);
        attrs.commit_pct = Math.round((used / total) * 1000) / 10;
      }
    }
    match = text.match(/^GPU MEMORY:\s*([\d.]+)\s*\/\s*([\d.]+)\s*(?:GB|GiB)/);
    if (match) {
      const used = Number(match[1]);
      const total = Number(match[2]);
      if (Number.isFinite(used) && Number.isFinite(total) && total > 0) {
        memory.gpu_used_mb = Math.round(used * 1024);
        memory.gpu_total_mb = Math.round(total * 1024);
        attrs.vram_pct = Math.round((used / total) * 1000) / 10;
      }
    }
  }
  if (gpus.length > 0) {
    const max = crashNumber(limits.max_list_len) || 64;
    attrs.gpu = gpus.slice(0, max);
  }
  crashPushFact(state, limits, {
    kind: "core.system",
    attrs,
    span: crashSpan("SYSTEM SPECS", section.start, section.end - 1),
  });
  if (Object.keys(memory).length > 0) {
    crashPushFact(state, limits, {
      kind: "creation.system_memory",
      attrs: memory,
      span: crashSpan("SYSTEM SPECS", section.start, section.end - 1),
    });
  }
  if (Object.keys(processor).length > 0) {
    crashPushFact(state, limits, {
      kind: "creation.processor",
      attrs: processor,
      span: crashSpan("SYSTEM SPECS", section.start, section.end - 1),
    });
  }
  return true;
}

function crashParseProcess(state, limits, sections, lines, game) {
  const attrs = {};
  crashAddAttr(attrs, "image", game.executable, limits);
  crashAddAttr(attrs, "image_version", game.version, limits);
  const section = crashFindSection(sections, "PROCESS INFO");
  if (section) {
    for (let index = section.start + 1; index < section.end; index += 1) {
      const text = lines[index].trim();
      let match = text.match(/^Process ID:\s*(\d+)/);
      if (match) attrs.pid = Number(match[1]);
      match = text.match(/^Working Directory:\s*(.*)$/);
      if (match) crashAddAttr(attrs, "cwd", match[1], limits);
      match = text.match(/^Command Line:\s*(.*)$/);
      if (match) crashAddAttr(attrs, "command_line", match[1], limits);
      match = text.match(/^Process Uptime:\s*[\d:.]+(?:ms)?\s*\((\d+)\s*ms\)/);
      if (match) attrs.uptime_s = Number(match[1]) / 1000;
      match = text.match(/^Executable MD5:\s*(\S+)/);
      if (match) crashAddAttr(attrs, "image_hash", match[1], limits);
    }
  }
  crashPushFact(state, limits, {
    kind: "core.process",
    attrs,
    span: section ? crashSpan("PROCESS INFO", section.start, section.end - 1) : undefined,
  });
  return section !== undefined;
}

function crashParseGame(state, limits, game) {
  const attrs = {};
  crashAddAttr(attrs, "game", game.name, limits);
  crashAddAttr(attrs, "edition", game.edition, limits);
  crashAddAttr(attrs, "version", game.version, limits);
  if (game.vr !== undefined) attrs.vr = game.vr;
  crashAddAttr(attrs, "executable", game.executable, limits);
  crashPushFact(state, limits, { kind: "creation.game", attrs });
}

function crashParseException(state, limits, sections, lines, game, logType) {
  const exception = crashFindLine(lines, /^\s*Unhandled exception\b/);
  let mainCode;
  let mainType;
  let access;
  let faultAddress;
  let site = {};
  let exceptionLine;

  if (exception) {
    exceptionLine = exception.line;
    const match = exception.line
      .trim()
      .match(/^Unhandled exception(?:\s+"([^"]*)")?\s+at\s+0x([0-9A-Fa-f]+)(?:\s+(\S+?)\+([0-9A-Fa-f]+))?(.*)$/);
    if (match) {
      mainCode = match[1] || undefined;
      site.address = crashHex(match[2]);
      if (match[3]) {
        site.module = match[3];
        site.offset = crashHex(match[4]);
      }
      let rest = match[5] || "";
      const source = rest.match(SOURCE_PATTERN);
      if (source) {
        site.source_file = source[1];
        site.line = Number(source[2]);
        rest = rest.replace(source[0], " ");
      }
      if (logType === "trainwreck") {
        const symbol = rest.trim().split(/\s+/)[0];
        if (symbol && !symbol.startsWith("0x") && !/^[a-z]{2,}\s/.test(rest.trim())) {
          site.symbol = symbol;
        }
      } else {
        const tab = rest.indexOf("\t");
        if (tab >= 0) {
          const instruction = rest.slice(tab + 1).trim();
          if (instruction) site.instruction = instruction;
        }
        const pipe = rest.indexOf("|");
        if (pipe >= 0) {
          const tail = rest.slice(pipe + 1).trim();
          const source2 = tail.match(SOURCE_PATTERN);
          if (source2) {
            site.source_file = source2[1];
            site.line = Number(source2[2]);
          }
        }
      }
    }
  }

  const accessLine = crashFindLine(lines, /^\s*(?:Access Violation|In-Page Error):\s*Tried to\s+(read|write|execute)\s+memory at\s+0x([0-9A-Fa-f]+)/);
  if (accessLine) {
    access = accessLine.match[1];
    faultAddress = crashHex(accessLine.match[2]);
  }

  if (mainCode !== undefined || site.address !== undefined || access !== undefined) {
    const attrs = {};
    const exceptionClass = mainCode !== undefined ? mainCode : access !== undefined ? "EXCEPTION_ACCESS_VIOLATION" : undefined;
    crashAddAttr(attrs, "class", exceptionClass, limits);
    crashAddAttr(attrs, "category", crashExceptionCategory(exceptionClass), limits);
    crashAddAttr(attrs, "access", access, limits);
    crashAddAttr(attrs, "fault_address", faultAddress, limits);
    if (mainType) crashAddAttr(attrs, "message", mainType, limits);
    const pointerClass = crashPointerClass(faultAddress);
    crashAddAttr(attrs, "pointer_class", pointerClass, limits);
    if (faultAddress) {
      let nearNull = false;
      try {
        nearNull = BigInt(faultAddress) < 0x10000n;
      } catch (error) {
        nearNull = false;
      }
      attrs.near_null = nearNull;
    }
    attrs.depth = 0;
    const refs = new Map();
    if (site.module) crashAddRef(refs, "module", site.module, "exec_top", limits);
    crashPushFact(state, limits, {
      kind: "core.exception",
      attrs,
      refs: crashRefList(refs, limits, state),
      span: exceptionLine !== undefined ? crashSpan("exception", exception.index, exception.index) : undefined,
    });
  }

  if (site.instruction) site.instruction = crashClean(site.instruction, limits.max_attr_len);
  if (site.source_file) site.source_file = crashClean(site.source_file, limits.max_attr_len);
  if (Object.keys(site).length > 0) {
    crashPushFact(state, limits, {
      kind: "creation.exception_site",
      attrs: site,
      span: exceptionLine !== undefined ? crashSpan("exception", exception.index, exception.index) : undefined,
    });
  }

  // Crash Logger 1.20+ writes a compact access-violation analysis.
  const analysis = crashFindSection(sections, "ACCESS VIOLATION ANALYSIS");
  if (analysis) {
    const attrs = {};
    for (let index = analysis.start + 1; index < analysis.end; index += 1) {
      const text = lines[index].trim();
      let match = text.match(/^Instruction:\s*(.*)$/);
      if (match) crashAddAttr(attrs, "instruction", match[1], limits);
      match = text.match(/^Memory Operand:\s*\[base=([^,\]]+),\s*index=([^,\]]+),\s*scale=([^,\]]+),\s*disp=([^\]]+)\]/);
      if (match) {
        crashAddAttr(attrs, "base", match[1].trim(), limits);
        crashAddAttr(attrs, "index", match[2].trim(), limits);
        crashAddAttr(attrs, "scale", match[3].trim(), limits);
        crashAddAttr(attrs, "disp", match[4].trim(), limits);
      }
      match = text.match(/^Base Register:\s*(\S+)\s*=\s*(0x[0-9A-Fa-f]+)/);
      if (match) crashAddAttr(attrs, "base_value", match[2], limits);
      match = text.match(/^Computed Address:\s*(0x[0-9A-Fa-f]+)/);
      if (match) crashAddAttr(attrs, "computed_address", match[1], limits);
      match = text.match(/^Fault Address:\s*(0x[0-9A-Fa-f]+)(.*)$/);
      if (match) {
        crashAddAttr(attrs, "fault_address", match[1], limits);
        if (match[2].includes("mismatch")) attrs.mismatch = true;
      }
    }
    crashPushFact(state, limits, {
      kind: "creation.access_violation",
      attrs,
      span: crashSpan("ACCESS VIOLATION ANALYSIS", analysis.start, analysis.end - 1),
    });
  }

  // A C++ exception block is its own exception, one level in when a main
  // exception already exists.
  const cpp = crashFindSection(sections, "C++ EXCEPTION");
  if (cpp) {
    const attrs = { depth: exception || mainCode !== undefined ? 1 : 0 };
    const refs = new Map();
    const site2 = {};
    for (let index = cpp.start + 1; index < cpp.end; index += 1) {
      const text = lines[index].trim();
      let match = text.match(/^Type:\s*\((.*?)\)\s*$/);
      if (match) {
        const type = crashTrim(match[1]).replace(/\*+$/, "");
        crashAddAttr(attrs, "class", type, limits);
        crashAddAttr(attrs, "category", "unknown", limits);
      }
      match = text.match(/^Info:\s*(.*)$/);
      if (match) crashAddAttr(attrs, "message", match[1], limits);
      match = text.match(/^Throw Location:\s*(\S+?)\+([0-9A-Fa-f]+)/);
      if (match) {
        site2.module = match[1];
        site2.offset = crashHex(match[2]);
        crashAddRef(refs, "module", match[1], "exec_top", limits);
      }
    }
    crashPushFact(state, limits, {
      kind: "core.exception",
      attrs,
      refs: crashRefList(refs, limits, state),
      span: crashSpan("C++ EXCEPTION", cpp.start, cpp.end - 1),
    });
    if (Object.keys(site2).length > 0) {
      crashPushFact(state, limits, {
        kind: "creation.exception_site",
        attrs: site2,
        span: crashSpan("C++ EXCEPTION", cpp.start, cpp.end - 1),
      });
    }
  }

  return { exception: Boolean(exception || cpp || access !== undefined), accessViolation: analysis !== undefined };
}

function crashSymbolScheme(game) {
  if (game.vr) return "addrlib-vr";
  if (!game.version) return "addrlib-unknown";
  return crashGameVersionAtLeast(game.version, 1, 6, 0) ? "addrlib-ae" : "addrlib-se";
}

function crashCallStackSection(sections) {
  for (const name of [
    "PROBABLE CALL STACK",
    "CALL STACK ([P]robable / [S]tack scan)",
    "CALL STACK (HYBRID)",
    "CALL STACK",
  ]) {
    const section = crashFindSection(sections, name);
    if (section) return section;
  }
  return undefined;
}

function crashParseCallStack(state, limits, sections, lines, game, logType) {
  const section = crashCallStackSection(sections);
  if (!section) return false;
  const scheme = crashSymbolScheme(game);
  for (let index = section.start + 1; index < section.end; index += 1) {
    const line = lines[index];
    const match = line.match(/^\s*\[\s*(\d+)\s*\]\s*(?:\[([PS])\])?\s*0x([0-9A-Fa-f]+)\s+(\S+?)\+([0-9A-Fa-f]+)\s*(.*)$/);
    if (!match) continue;
    const frameIndex = Number(match[1]);
    const kind = match[2] === "S" ? "scan" : "probable";
    const attrs = {
      index: frameIndex,
      kind,
      address: crashHex(match[3]),
      rva: crashHex(match[5]),
    };
    const refs = new Map();
    const strength = kind === "scan" ? "mention" : frameIndex === 0 ? "exec_top" : "exec";
    crashAddRef(refs, "module", match[4], strength, limits);
    let rest = match[6] || "";
    let functionName;
    if (logType === "crash_logger") {
      const arrow = rest.match(/->\s*([^\s+]+)\+0x([0-9A-Fa-f]+)\s*(.*)$/);
      if (arrow) {
        const symbol = arrow[1];
        attrs.symbol_offset = Number.parseInt(arrow[2], 16);
        if (/^[0-9A-Fa-f]+$/.test(symbol) && /[A-Fa-f]/.test(symbol)) {
          attrs.symbol_scheme = scheme;
          attrs.symbol = symbol;
        } else if (/^\d+$/.test(symbol)) {
          attrs.symbol_scheme = scheme;
          attrs.symbol = symbol;
        } else {
          attrs.symbol_scheme = "pdb";
          attrs.symbol = symbol;
        }
        const tail = crashTrim(arrow[3]);
        if (tail && !/^(?:mov|call|jmp|test|cmp|add|sub|lea|push|pop|ret|nop|xor|and|or|inc|dec|int3|lock|rep|movs|movss|cmp|jz|jnz|je|jne|jb|ja|jbe|jae|jl|jg|jle|jge|jmp|ud2|v?mov|v?add|v?sub|v?mul|v?div|v?xor|v?and|v?or|v?cmp|v?test|v?ucomis|v?comis|pxor|pand|por|xmm)/i.test(tail)) {
          functionName = tail.split(/\s+/)[0];
        }
      } else {
        const tab = rest.indexOf("\t");
        if (tab >= 0) {
          const instruction = rest.slice(tab + 1).trim();
          if (instruction) {
            const source = rest.match(SOURCE_PATTERN);
            if (source) {
              attrs.source_file = source[1];
              attrs.line = Number(source[2]);
            }
          }
        }
      }
    } else {
      const symbol = rest.trim().split(/\s+/)[0];
      if (symbol && !/^(?:mov|call|jmp|test|cmp|add|sub|lea|push|pop|ret|nop|xor|and|or|inc|dec|int3|lock|rep|movs|movss|ud2)/i.test(symbol)) {
        attrs.symbol_scheme = "pdb";
        attrs.symbol = symbol;
        functionName = symbol;
      }
      // Source and instruction lines follow the frame, indented.
      for (let next = index + 1; next < section.end; next += 1) {
        const text = lines[next];
        if (/^\s*\[\s*\d+\s*\]\s+0x[0-9A-Fa-f]+/.test(text)) break;
        if (text.trim() === "") break;
        const source = text.match(SOURCE_PATTERN);
        if (source) {
          attrs.source_file = source[1];
          attrs.line = Number(source[2]);
        }
      }
    }
    const source = rest.match(SOURCE_PATTERN);
    if (source && attrs.source_file === undefined) {
      attrs.source_file = source[1];
      attrs.line = Number(source[2]);
    }
    crashPushFact(state, limits, {
      kind: "core.frame",
      attrs,
      refs: crashRefList(refs, limits, state),
      span: crashSpan("PROBABLE CALL STACK", index, index),
    });
    if (functionName) {
      crashPushFact(state, limits, {
        kind: "creation.frame_name",
        attrs: { index: frameIndex, name: functionName },
        span: crashSpan("PROBABLE CALL STACK", index, index),
      });
    }
  }
  return true;
}

function crashParseModules(state, limits, sections, lines, logType) {
  const section = crashFindSection(sections, "MODULES");
  if (!section) return false;
  if (logType === "crash_logger") {
    for (let index = section.start + 1; index < section.end; index += 1) {
      const match = lines[index].trim().match(/^(\S+)\s+0x([0-9A-Fa-f]+)\s*$/);
      if (!match) continue;
      const refs = new Map();
      crashAddRef(refs, "module", match[1], "mention", limits);
      crashPushFact(state, limits, {
        kind: "core.module",
        attrs: { base_address: crashHex(match[2]) },
        refs: crashRefList(refs, limits, state),
        span: crashSpan("MODULES", index, index),
      });
    }
    return true;
  }
  let current;
  let index = section.start + 1;
  while (index < section.end) {
    const text = lines[index];
    if (text.trim() === "") {
      index += 1;
      continue;
    }
    const indent = crashIndent(text);
    if (indent === 4 && !text.trim().endsWith(":") && !text.trim().includes(" ")) {
      if (current) crashEmitModule(state, limits, current);
      current = { name: text.trim(), attrs: {}, line: index };
      index += 1;
      continue;
    }
    if (current) {
      const match = text.trim().match(/^Address:\s*(0x[0-9A-Fa-f]+)/);
      if (match) current.attrs.base_address = match[1];
      const hash = text.trim().match(/^SHA:\s*([0-9A-Fa-f]+)/);
      if (hash) {
        current.hash = hash[1];
      }
      const version = text.trim().match(/^Version:\s*v?(.+)$/);
      if (version) current.attrs.version = crashTrim(version[1]);
    }
    index += 1;
  }
  if (current) crashEmitModule(state, limits, current);
  return true;
}

function crashEmitModule(state, limits, module) {
  const refs = new Map();
  crashAddRef(refs, "module", module.name, "mention", limits);
  crashPushFact(state, limits, {
    kind: "core.module",
    attrs: module.attrs,
    refs: crashRefList(refs, limits, state),
    span: crashSpan("MODULES", module.line, module.line),
  });
  if (module.hash) {
    crashPushFact(state, limits, {
      kind: "creation.module_hash",
      attrs: { hash: module.hash, algorithm: module.hash.length === 128 ? "sha512" : "unknown" },
      refs: crashRefList(new Map([["m", { kind: "module", value: module.name, strength: "mention" }]]), limits, state),
      span: crashSpan("MODULES", module.line, module.line),
    });
  }
}

function crashParseScriptExtender(state, limits, sections, lines, logType) {
  const name = logType === "crash_logger" ? "SKSE PLUGINS" : "SCRIPT EXTENDER PLUGINS";
  const section = crashFindSection(sections, name);
  if (!section) return false;
  for (let index = section.start + 1; index < section.end; index += 1) {
    const text = lines[index].trim();
    if (!text) continue;
    const match = text.match(/^(\S+?)(?:\s+(v?[0-9][0-9A-Za-z.\-]*))?\s*$/);
    if (!match) continue;
    const attrs = {};
    if (match[2]) attrs.version = crashVersion(match[2]);
    const refs = new Map();
    crashAddRef(refs, "module", match[1], "mention", limits);
    crashPushFact(state, limits, {
      kind: "creation.script_extender_plugin",
      attrs,
      refs: crashRefList(refs, limits, state),
      span: crashSpan(name, index, index),
    });
  }
  return true;
}

function crashParseLoadOrder(state, limits, sections, lines, logType) {
  if (logType === "trainwreck") return { present: false, entries: 0, unreadable: false };
  const section = crashFindSection(sections, "PLUGINS");
  if (!section) return { present: false, entries: 0, unreadable: false };
  let entries = 0;
  let unreadable = false;
  for (let index = section.start + 1; index < section.end; index += 1) {
    const text = lines[index].trim();
    if (!text) continue;
    const summary = text.match(/^Light:\s*(\d+)\s+Regular:\s*(\d+)\s+Total:\s*(\d+)/);
    if (summary) {
      crashPushFact(state, limits, {
        kind: "creation.load_order_summary",
        attrs: {
          light: Number(summary[1]),
          regular: Number(summary[2]),
          total: Number(summary[3]),
        },
        span: crashSpan("PLUGINS", index, index),
      });
      continue;
    }
    let match = text.match(/^\[FE:\s*([0-9A-Fa-f]+)\]\s*(.+?)\s*$/);
    let light = true;
    if (!match) {
      match = text.match(/^\[([0-9A-Fa-f ]+)\]\s*(.+?)\s*$/);
      light = false;
    }
    if (!match) {
      unreadable = true;
      crashPushFact(state, limits, {
        kind: "creation.load_order_error",
        attrs: { message: text },
        span: crashSpan("PLUGINS", index, index),
      });
      continue;
    }
    entries += 1;
    const indexText = light ? "FE:" + match[1].trim().toUpperCase() : match[1].trim().toUpperCase();
    const refs = new Map();
    crashAddRef(refs, "plugin", match[2], "object", limits);
    crashPushFact(state, limits, {
      kind: "creation.plugin",
      attrs: { index: indexText, light },
      refs: crashRefList(refs, limits, state),
      span: crashSpan("PLUGINS", index, index),
    });
  }
  return { present: true, entries, unreadable };
}

function crashParseModels(state, limits, sections, lines) {
  let found = false;
  for (const header of sections.headers) {
    if (!header.name.startsWith("CRASHED WHILE LOADING A MODEL") && !header.name.startsWith("MODEL PATHS FOR RESOURCE IDS")) {
      continue;
    }
    found = true;
    for (let index = header.start + 1; index < header.end; index += 1) {
      const text = lines[index];
      if (!text || !text.trim()) continue;
      const attrs = {};
      const place = text.match(/\[(RSP\+[0-9A-Fa-f]+)\]/);
      if (place) attrs.place = place[1].toUpperCase();
      const path = text.trim();
      if (crashLooksLikePath(path)) {
        attrs.path = crashUnquote(path);
        const refs = new Map();
        crashAddRef(refs, "file", path, "object", limits);
        crashPushFact(state, limits, {
          kind: "creation.model_request",
          attrs,
          refs: crashRefList(refs, limits, state),
          span: crashSpan(header.name, index, index),
        });
      } else {
        attrs.text = crashTrim(path);
        crashPushFact(state, limits, {
          kind: "creation.model_request",
          attrs,
          span: crashSpan(header.name, index, index),
        });
      }
    }
  }
  return found;
}


function crashCoverage(state, limits, sections, flags) {
  const coverage = [];
  const add = (kind, has, why) => {
    if (has) {
      coverage.push({ kind, state: "checked" });
    } else {
      coverage.push({ kind, state: "absent", why });
    }
  };
  add("core.exception", flags.exception, "The log has no unhandled-exception line.");
  add("core.frame", flags.callStack, "The log has no call-stack section.");
  add("core.module", flags.modules, "The log has no MODULES section.");
  add("creation.module_hash", flags.modules, "The log has no MODULES section.");
  add("core.process", true, "The game executable and its version are in the header.");
  add("core.system", flags.system, "The log has no SYSTEM SPECS section.");
  add("creation.game", true, "The log header names the game and version.");
  add("creation.processor", flags.system, "The log has no SYSTEM SPECS section.");
  add("creation.system_memory", flags.system, "The log has no SYSTEM SPECS section.");
  add("creation.exception_site", flags.exception, "The log has no unhandled-exception line.");
  add("creation.access_violation", flags.accessAnalysis, "The log has no ACCESS VIOLATION ANALYSIS section.");
  add("creation.object", flags.objects, "The log has no registers, stack or relevant-object section.");
  add("creation.plugin_chain", flags.objects, "The log has no registers, stack or relevant-object section.");
  add("creation.code_pointer", flags.objects, "The log has no registers, stack or relevant-object section.");
  add("creation.frame_name", flags.callStack, "The log has no call stack.");
  add("creation.script_extender_plugin", flags.scriptExtender, "The log has no script-extender plugin section.");
  if (flags.loadOrderApplicable) {
    if (!flags.loadOrder) {
      coverage.push({ kind: "creation.plugin", state: "absent", why: "The log has no PLUGINS section, so its load order was not recorded." });
      coverage.push({ kind: "creation.load_order_summary", state: "absent", why: "The log has no PLUGINS section, so its load order was not recorded." });
      coverage.push({ kind: "creation.load_order_error", state: "absent", why: "The log has no PLUGINS section." });
    } else {
      coverage.push({ kind: "creation.load_order_summary", state: "checked" });
      coverage.push({ kind: "creation.load_order_error", state: "checked" });
      if (flags.loadOrderEntries > 0 || !flags.loadOrderUnreadable) {
        coverage.push({ kind: "creation.plugin", state: "checked" });
      } else {
        coverage.push({ kind: "creation.plugin", state: "absent", why: "The logger wrote an error instead of the plugin list." });
      }
    }
  } else {
    coverage.push({ kind: "creation.plugin", state: "not_applicable", why: "Trainwreck writes no plugin list." });
    coverage.push({ kind: "creation.load_order_summary", state: "not_applicable", why: "Trainwreck writes no plugin list." });
    coverage.push({ kind: "creation.load_order_error", state: "not_applicable", why: "Trainwreck writes no plugin list." });
  }
  add("creation.model_request", flags.models, "The log has no model-loading section.");
  coverage.push({ kind: "core.log_line", state: "not_applicable" });
  coverage.push({ kind: "core.load_verdict", state: "not_applicable" });
  coverage.push({ kind: "core.dependency_problem", state: "not_applicable" });
  return coverage;
}

function crashBuildBucket(exception, frames, game, logType) {
  const bucket = [];
  if (exception && exception.class) {
    bucket.push({ scheme: "exception", value: exception.class });
  }
  for (const frame of frames) {
    if (bucket.length >= 8) break;
    if (!frame.symbol) continue;
    const scheme = frame.symbol_scheme || (logType === "trainwreck" ? "pdb" : crashSymbolScheme(game));
    let value;
    if (scheme.startsWith("addrlib")) {
      value = frame.symbol + "+0x" + Number(frame.symbol_offset || 0).toString(16);
    } else {
      value = frame.symbol;
    }
    bucket.push({ scheme, value });
  }
  return bucket;
}

function crashParseCreationEngine(request, logType) {
  const read = crashReadAllLines(request);
  if (read.error) throw new Error(read.error);
  const lines = read.lines;
  if (lines.length === 0) {
    return {
      contract: CRASH_CONTRACT,
      producer: { format: logType },
      facts: [],
      notes: [{ kind: "wrong_format", message: "the file is empty" }],
    };
  }
  const header = crashProducerHeader(lines, logType);
  if (!header) {
    return {
      contract: CRASH_CONTRACT,
      producer: { format: logType },
      facts: [],
      notes: [{ kind: "wrong_format", message: "the header is not this logger's" }],
    };
  }

  const sections = crashSections(lines);
  const limits = request.limits;
  const state = { facts: [], kinds: {}, budgetSpent: false, refsTruncated: false };
  const notes = [];

  crashParseGame(state, limits, header.game);
  const processSection = crashParseProcess(state, limits, sections, lines, header.game);
  const systemSection = crashParseSystem(state, limits, sections, lines);
  const exceptionFlags = crashParseException(state, limits, sections, lines, header.game, logType);
  const callStack = crashParseCallStack(state, limits, sections, lines, header.game, logType);
  const modules = crashParseModules(state, limits, sections, lines, logType);
  const scriptExtender = crashParseScriptExtender(state, limits, sections, lines, logType);
  const loadOrder = crashParseLoadOrder(state, limits, sections, lines, logType);

  let objects = false;
  for (const name of ["REGISTERS", "STACK", "POSSIBLE RELEVANT OBJECTS", "THREAD CONTEXT (HEURISTIC)"]) {
    const section = crashFindSection(sections, name);
    if (!section) continue;
    objects = true;
    crashParseObjects(state, limits, lines, section, logType);
  }
  const models = crashParseModels(state, limits, sections, lines);

  const flags = {
    exception: exceptionFlags.exception,
    accessAnalysis: exceptionFlags.accessViolation,
    callStack,
    modules,
    system: systemSection,
    objects,
    scriptExtender,
    loadOrder: loadOrder.present,
    loadOrderEntries: loadOrder.entries,
    loadOrderUnreadable: loadOrder.unreadable,
    loadOrderApplicable: logType === "crash_logger",
    models,
  };
  const coverage = crashCoverage(state, limits, sections, flags);

  // A header with neither an exception nor a call stack is a log that stopped
  // early; say so rather than letting the missing sections look like absence.
  if (!exceptionFlags.exception && !callStack) {
    notes.push({ kind: "truncated", message: "the log stops before its exception or call stack" });
  }
  if (read.cut) notes.push({ kind: "truncated", message: "a line was longer than the read limit and was cut" });
  if (read.gap) notes.push({ kind: "truncated", message: "the kept head and tail are not adjacent" });
  if (read.budget || state.budgetSpent) notes.push({ kind: "budget_spent", message: "the parser reached its fact or line budget" });
  if (state.refsTruncated) notes.push({ kind: "other", message: "some references were dropped to stay inside the per-fact limit" });
  if (!processSection && logType === "crash_logger") {
    notes.push({ kind: "other", message: "this logger version has no process id, so the session is tied by time" });
  }
  if (!header.game.version || !header.game.edition) {
    notes.push({ kind: "truncated", message: "the header line naming the game is incomplete" });
  }

  const exceptionFacts = state.facts.filter((fact) => fact.kind === "core.exception" && Number(fact.attrs.depth || 0) === 0);
  const exception = exceptionFacts.length > 0 ? {
    class: exceptionFacts[0].attrs.class,
  } : undefined;
  const frames = state.facts
    .filter((fact) => fact.kind === "core.frame")
    .map((fact) => ({
      symbol: fact.attrs.symbol,
      symbol_scheme: fact.attrs.symbol_scheme,
      symbol_offset: fact.attrs.symbol_offset,
    }));
  const bucket = crashBuildBucket(exception, frames, header.game, logType);

  return {
    contract: CRASH_CONTRACT,
    producer: { format: header.format, version: header.version },
    facts: state.facts,
    bucket,
    coverage,
    notes,
  };
}

/// Crash Logger (CrashLoggerSSE) crash logs, versions 1.0 through 1.25.
export async function parseCrashLogger(request) {
  return crashParseCreationEngine(request, "crash_logger");
}

/// Trainwreck crash logs, versions 1.2 through 1.4.
export async function parseTrainwreck(request) {
  return crashParseCreationEngine(request, "trainwreck");
}

// ---------------------------------------------------------------------------
// LOOT's sorter (MASTER_SPEC §30.13)
//
// The order, the notes and the masterlist parser are libloot's, compiled into
// wasm/loot-sort.wasm. This script only keeps the verified masterlist, hands
// the module its request, and maps the reply. The masterlist is fetched from
// the pinned branch, checked against the git blob SHA-1 GitHub reports, and
// saved under the plugin's own storage. A failed check keeps the old copy.
// ---------------------------------------------------------------------------

const STATUS_KEY = "sorter.status";
// The masterlist and prelude are blobs in the plugin's own store, named for the
// commit they came from. The status key (small) names the current generation,
// so writing it last makes a set of blobs switch over at once.
function blobKey(name, generation) {
  return name + "." + generation + ".yaml";
}

/// UTF-8 bytes for `storage.putBytes`; the module reads the files as text.
function utf8Bytes(text) {
  const out = [];
  for (let index = 0; index < text.length; index += 1) {
    let code = text.codePointAt(index);
    if (code > 0xffff) index += 1;
    if (code < 0x80) {
      out.push(code);
    } else if (code < 0x800) {
      out.push(0xc0 | (code >> 6), 0x80 | (code & 0x3f));
    } else if (code < 0x10000) {
      out.push(0xe0 | (code >> 12), 0x80 | ((code >> 6) & 0x3f), 0x80 | (code & 0x3f));
    } else {
      out.push(
        0xf0 | (code >> 18),
        0x80 | ((code >> 12) & 0x3f),
        0x80 | ((code >> 6) & 0x3f),
        0x80 | (code & 0x3f),
      );
    }
  }
  return new Uint8Array(out);
}

const BASE64 = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

function base64Bytes(text) {
  let clean = "";
  for (let index = 0; index < text.length; index += 1) {
    const character = text.charAt(index);
    if (character !== "\n" && character !== "\r" && character !== " " && character !== "\t") {
      clean += character;
    }
  }
  const out = [];
  let buffer = 0;
  let bits = 0;
  for (let index = 0; index < clean.length; index += 1) {
    const character = clean.charAt(index);
    if (character === "=") break;
    const value = BASE64.indexOf(character);
    if (value < 0) throw new Error("GitHub sent bytes that are not base64");
    buffer = (buffer << 6) | value;
    bits += 6;
    if (bits >= 8) {
      bits -= 8;
      out.push((buffer >> bits) & 0xff);
      buffer &= (1 << bits) - 1;
    }
  }
  return new Uint8Array(out);
}

function utf8Text(bytes) {
  let text = "";
  let index = 0;
  while (index < bytes.length) {
    const first = bytes[index];
    index += 1;
    if (first < 0x80) {
      text += String.fromCharCode(first);
    } else if (first < 0xe0) {
      text += String.fromCharCode(((first & 0x1f) << 6) | (bytes[index] & 0x3f));
      index += 1;
    } else if (first < 0xf0) {
      text += String.fromCharCode(
        ((first & 0x0f) << 12) | ((bytes[index] & 0x3f) << 6) | (bytes[index + 1] & 0x3f),
      );
      index += 2;
    } else {
      let point =
        ((first & 0x07) << 18) |
        ((bytes[index] & 0x3f) << 12) |
        ((bytes[index + 1] & 0x3f) << 6) |
        (bytes[index + 2] & 0x3f);
      index += 3;
      point -= 0x10000;
      text += String.fromCharCode(0xd800 + (point >> 10), 0xdc00 + (point & 0x3ff));
    }
  }
  return text;
}

// `sha1("blob " + byteLength + "\0" + bytes)`, the way git names a blob.
function gitBlobSha1(bytes) {
  const header = "blob " + bytes.length + "\0";
  const all = new Uint8Array(header.length + bytes.length);
  for (let index = 0; index < header.length; index += 1) {
    all[index] = header.charCodeAt(index);
  }
  all.set(bytes, header.length);
  return sha1Hex(all);
}

function sha1Hex(bytes) {
  const bits = bytes.length * 8;
  const blocks = Math.ceil((bytes.length + 9) / 64);
  const padded = new Uint8Array(blocks * 64);
  padded.set(bytes);
  padded[bytes.length] = 0x80;
  const view = new DataView(padded.buffer);
  view.setUint32(padded.length - 8, Math.floor(bits / 0x100000000));
  view.setUint32(padded.length - 4, bits >>> 0);

  let h0 = 0x67452301;
  let h1 = 0xefcdab89;
  let h2 = 0x98badcfe;
  let h3 = 0x10325476;
  let h4 = 0xc3d2e1f0;
  const words = new Uint32Array(80);
  for (let block = 0; block < padded.length; block += 64) {
    for (let index = 0; index < 16; index += 1) {
      words[index] = view.getUint32(block + index * 4);
    }
    for (let index = 16; index < 80; index += 1) {
      const value = words[index - 3] ^ words[index - 8] ^ words[index - 14] ^ words[index - 16];
      words[index] = ((value << 1) | (value >>> 31)) >>> 0;
    }
    let a = h0;
    let b = h1;
    let c = h2;
    let d = h3;
    let e = h4;
    for (let index = 0; index < 80; index += 1) {
      let f;
      let k;
      if (index < 20) {
        f = (b & c) | (~b & d);
        k = 0x5a827999;
      } else if (index < 40) {
        f = b ^ c ^ d;
        k = 0x6ed9eba1;
      } else if (index < 60) {
        f = (b & c) | (b & d) | (c & d);
        k = 0x8f1bbcdc;
      } else {
        f = b ^ c ^ d;
        k = 0xca62c1d6;
      }
      const temp = (((a << 5) | (a >>> 27)) + f + e + k + words[index]) >>> 0;
      e = d;
      d = c;
      c = ((b << 30) | (b >>> 2)) >>> 0;
      b = a;
      a = temp;
    }
    h0 = (h0 + a) >>> 0;
    h1 = (h1 + b) >>> 0;
    h2 = (h2 + c) >>> 0;
    h3 = (h3 + d) >>> 0;
    h4 = (h4 + e) >>> 0;
  }
  return [h0, h1, h2, h3, h4]
    .map((value) => value.toString(16).padStart(8, "0"))
    .join("");
}

async function githubJson(url) {
  const reply = await net.fetchJson(url);
  if (reply.status < 200 || reply.status >= 300) {
    throw new Error("GitHub answered " + reply.status + " for " + url);
  }
  return reply.body;
}

async function fetchPinnedFile(repo, ref, path) {
  const contents = await githubJson(
    "https://api.github.com/repos/" + repo + "/contents/" + path + "?ref=" +
      encodeURIComponent(ref),
  );
  const blobSha = contents.sha;
  let bytes;
  if (contents.encoding === "base64" && contents.content) {
    bytes = base64Bytes(contents.content);
  } else {
    // The contents endpoint leaves `content` out above 1 MiB, so ask the blobs
    // endpoint for the same object by the SHA it reported.
    const blob = await githubJson(
      "https://api.github.com/repos/" + repo + "/git/blobs/" + blobSha,
    );
    if (blob.encoding !== "base64" || !blob.content) {
      throw new Error("GitHub sent no bytes for " + repo + "/" + path);
    }
    bytes = base64Bytes(blob.content);
  }
  const actual = gitBlobSha1(bytes);
  if (actual.toLowerCase() !== String(blobSha).toLowerCase()) {
    throw new Error(
      "the bytes for " + repo + "/" + path + " do not match the hash GitHub reported",
    );
  }
  return { text: utf8Text(bytes), sha: blobSha };
}

function sorterEntries(args) {
  const entries = (args && args.entries) || [];
  const names = [];
  const active = [];
  for (const entry of entries) {
    if (!entry.present) continue;
    names.push(entry.name);
    if (entry.active) active.push(entry.name);
  }
  return { names: names, active: active };
}

/// Keep the masterlist, its prelude and its provenance in the plugin's storage.
export async function lootUpdate(args) {
  const options = (args && args.options) || {};
  if (!args || !args.update) {
    const status = await storage.get(STATUS_KEY);
    if (!status || !status.fetchedAt) return null;
    return status;
  }
  const repo = options.masterlist_repo;
  const branch = options.branch;
  if (!repo || !branch) {
    throw new Error("the list's sorter options name no masterlist");
  }
  const commit = await githubJson(
    "https://api.github.com/repos/" + repo + "/commits/" + encodeURIComponent(branch),
  );
  if (!commit || !commit.sha) throw new Error("GitHub named no commit for " + repo);
  const masterlist = await fetchPinnedFile(repo, commit.sha, "masterlist.yaml");
  let prelude = null;
  if (options.prelude_repo) {
    prelude = await fetchPinnedFile(options.prelude_repo, branch, "prelude.yaml");
  }
  const generation = commit.sha;
  const masterlistKey = blobKey("masterlist", generation);
  const preludeKey = prelude ? blobKey("prelude", generation) : null;
  const previous = await storage.get(STATUS_KEY);
  await storage.putBytes(masterlistKey, utf8Bytes(masterlist.text));
  if (prelude) await storage.putBytes(preludeKey, utf8Bytes(prelude.text));
  const status = {
    fetchedAt: new Date().toISOString(),
    source:
      "https://api.github.com/repos/" + repo + "/contents/masterlist.yaml?ref=" + commit.sha,
    dataVersion: generation,
    commitSha: commit.sha,
    blobSha: masterlist.sha,
    masterlistKey: masterlistKey,
    preludeKey: preludeKey,
  };
  // Naming the new generation last is the switch: a sort that reads the old
  // status still sees the old, complete set.
  await storage.set(STATUS_KEY, status);
  if (previous && previous.masterlistKey && previous.masterlistKey !== masterlistKey) {
    try {
      await storage.deleteBytes(previous.masterlistKey);
    } catch (error) {
      // An old generation that cannot be removed is wasted space, not a bad sort.
    }
    if (previous.preludeKey && previous.preludeKey !== preludeKey) {
      try {
        await storage.deleteBytes(previous.preludeKey);
      } catch (error) {
        // Same as above.
      }
    }
  }
  return {
    fetchedAt: status.fetchedAt,
    source: status.source,
    dataVersion: status.dataVersion,
  };
}

/// Sort through the module and map libloot's answer to the sorter's reply.
export async function lootSort(args) {
  const options = (args && args.options) || {};
  const entries = sorterEntries(args);
  const status = await storage.get(STATUS_KEY);
  if (!status || !status.masterlistKey) {
    return {
      order: entries.names,
      notes: [],
      general: [{ level: "info", text: "Update the masterlist first" }],
    };
  }
  if (!options.game_type) {
    throw new Error("the list's sorter options name no game type");
  }
  const stored = [status.masterlistKey];
  if (status.preludeKey) stored.push(status.preludeKey);
  const result = wasm.run("wasm/loot-sort.wasm", {
    stdin: JSON.stringify({
      game_type: options.game_type,
      data: "/data",
      load_order: entries.names,
      active: entries.active,
      masterlist: "/input/" + status.masterlistKey,
      prelude: status.preludeKey ? "/input/" + status.preludeKey : null,
    }),
    mounts: [
      {
        guest: "/data",
        view: { folder: "Data", patterns: ["*.esm", "*.esl", "*.esp"] },
      },
      { guest: "/input", stored: stored },
    ],
    limits: { timeoutMs: 90000 },
  });
  if (result.exitCode !== 0) {
    let message = result.stderrText || "the sorter did not finish";
    try {
      const failure = JSON.parse(result.stdoutText);
      if (failure && failure.error) message = failure.error;
    } catch (error) {
      // Not its JSON error reply; the stderr text is the best we have.
    }
    throw new Error(message);
  }
  const reply = JSON.parse(result.stdoutText);
  const notes = [];
  for (const plugin of reply.plugins || []) {
    for (const message of plugin.messages || []) {
      notes.push({ item: plugin.name, level: message.level, text: message.text });
    }
  }
  const general = [];
  for (const message of reply.general || []) {
    general.push({ level: message.level, text: message.text });
  }
  return { order: reply.order || [], notes: notes, general: general };
}
