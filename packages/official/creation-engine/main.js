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

import { game, net, storage, wasm } from "agora";

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
