//! The `agora` module, as a plugin sees it.
//!
//! This is the *implementation* of the SDK, built into the host. The npm
//! package under `sdk/` ships the matching TypeScript declarations and a dev
//! stub so authors get types and autocomplete, but at runtime the real module
//! is this one — a plugin cannot substitute its own, and cannot reach the two
//! host primitives except through it.
//!
//! Everything here is ordinary JavaScript with no platform assumptions. There
//! is no `require`, no `process`, no `fs`, no `fetch`; QuickJS provides none
//! of them and the host adds none. `agora.net.fetch` exists only when the
//! plugin holds the `network` capability, and it is brokered by the launcher.

/// Source of the builtin `agora` module.
pub const AGORA_MODULE: &str = r#"
let nextRequestId = 1;

function call(method, args, options) {
  const requestId = nextRequestId++;
  const deadlineMs = (options && options.deadlineMs) || 0;
  return __agora_call(requestId, method, JSON.stringify(args === undefined ? {} : args), deadlineMs)
    .then((raw) => {
      const response = JSON.parse(raw);
      if (response.status === "error") {
        const error = new Error(response.error.message);
        error.code = response.error.code;
        error.detail = response.error.detail;
        throw error;
      }
      return response.value;
    });
}

const log = {
  debug: (...parts) => __agora_log("debug", parts.map(stringify).join(" ")),
  info: (...parts) => __agora_log("info", parts.map(stringify).join(" ")),
  warn: (...parts) => __agora_log("warn", parts.map(stringify).join(" ")),
  error: (...parts) => __agora_log("error", parts.map(stringify).join(" ")),
};

function stringify(value) {
  if (typeof value === "string") return value;
  if (value instanceof Error) return value.stack || value.message;
  try {
    return JSON.stringify(value);
  } catch {
    return String(value);
  }
}

const instances = {
  list: () => call("instance.list"),
  get: (instanceId) => call("instance.get", { instanceId }),
  rename: (instanceId, name) => call("instance.rename", { instanceId, name }),
  setMemory: (instanceId, memoryMb) => call("instance.setMemory", { instanceId, memoryMb }),
};

const content = {
  list: (instanceId, contentType) => call("content.list", { instanceId, contentType }),
  enable: (instanceId, key) => call("content.enable", { instanceId, key }),
  disable: (instanceId, key) => call("content.disable", { instanceId, key }),
  setUpdatePinned: (instanceId, key, pinned) =>
    call("content.setUpdatePinned", { instanceId, key, pinned }),
};

const launch = {
  state: (instanceId) => call("launch.state", { instanceId }),
  history: (instanceId, limit) => call("launch.history", { instanceId, limit }),
};

const storage = {
  get: (key) => call("storage.get", { key }),
  set: (key, value) => call("storage.set", { key, value }),
  all: () => call("storage.all"),
  remove: (key) => call("storage.remove", { key }),
  forInstance: (instanceId) => ({
    get: (key) => call("storage.get", { key, instanceId }),
    set: (key, value) => call("storage.set", { key, value, instanceId }),
    all: () => call("storage.all", { instanceId }),
    remove: (key) => call("storage.remove", { key, instanceId }),
  }),
};

const net = {
  fetchJson: (url, options) => call("net.fetchJson", { url, ...(options || {}) }, { deadlineMs: 30000 }),
};

const ui = {
  refresh: (viewId) => call("ui.refresh", { viewId }),
  notify: (tone, message) => call("ui.notify", { tone, message }),
};

// A WebAssembly module the plugin ships. It is a WASI command: the request
// goes in on stdin, the answer comes back on stdout. `stdout` and `stderr` are
// plain byte arrays so the result survives JSON, and the `*Text` fields are the
// same bytes decoded as UTF-8.
const wasm = {
  run: (module, options) => {
    const opts = options || {};
    const encode = (value) => (typeof value === "string" ? value : Array.from(value || []));
    const mounts = (opts.mounts || []).map((mount) => {
      const out = { guest: mount.guest };
      if (mount.view) {
        out.view = { folder: mount.view.folder, patterns: mount.view.patterns || [] };
      }
      if (mount.files) {
        out.files = {};
        for (const [name, value] of Object.entries(mount.files)) {
          out.files[name] = encode(value);
        }
      }
      return out;
    });
    const payload = { mounts };
    if (opts.stdin !== undefined) payload.stdin = encode(opts.stdin);
    if (opts.limits) payload.limits = opts.limits;
    return JSON.parse(__agora_wasm_run(module, JSON.stringify(payload)));
  },
};

// Scoped reads for crash parsers and inspectors. They reach only the artifact
// the host put in scope for this call, and throw when there is none.
const crash = {
  readBytes: (artifactId, offset, len) =>
    __agora_crash_read_bytes(artifactId, offset, len),
  readLines: (artifactId, fromLine, maxLines) =>
    JSON.parse(__agora_crash_read_lines(artifactId, fromLine, maxLines)),
};

// The instance view. Reads use the game path the host put in scope for a hook
// call; writes name an instance and land only in this plugin's own generated
// layer. Neither ever exposes a real filesystem path.
const game = {
  readBytes: (path, offset, len) =>
    __agora_game_read_bytes(path, offset || 0, len === undefined ? 65536 : len),
  readLines: (path, fromLine, maxLines) =>
    JSON.parse(__agora_game_read_lines(path, fromLine || 0, maxLines === undefined ? 1000 : maxLines)),
  list: (path) => JSON.parse(__agora_game_list(path || "")),
  writeBytes: (instanceId, path, bytes) =>
    call("game.writeBytes", {
      instanceId,
      path,
      bytes: Array.from(bytes || []),
    }, { deadlineMs: 30000 }),
  writeText: (instanceId, path, text) =>
    call("game.writeText", { instanceId, path, text }, { deadlineMs: 30000 }),
  // For an ordinary plugin call (a view, a command), where no hook has put an
  // instance in scope: name the instance, and the same `game:read`/`game:write`
  // grants are checked by the host.
  forInstance: (instanceId) => ({
    readBytes: (path, offset, len) =>
      call("game.readBytes", { instanceId, path, offset, len }).then((reply) => new Uint8Array(reply.bytes).buffer),
    readLines: (path, fromLine, maxLines) =>
      call("game.readLines", { instanceId, path, fromLine, maxLines }),
    list: (path) => call("game.list", { instanceId, path }),
    writeBytes: (path, bytes) =>
      call("game.writeBytes", { instanceId, path, bytes: Array.from(bytes || []) }, { deadlineMs: 30000 }),
    writeText: (path, text) =>
      call("game.writeText", { instanceId, path, text }, { deadlineMs: 30000 }),
  }),
};

// --- events ---------------------------------------------------------------

const handlers = new Map();

function on(eventName, handler) {
  if (typeof handler !== "function") throw new TypeError("handler must be a function");
  let list = handlers.get(eventName);
  if (!list) {
    list = [];
    handlers.set(eventName, list);
    call("events.subscribe", { event: eventName }).catch((e) =>
      log.error(`could not subscribe to ${eventName}:`, e),
    );
  }
  list.push(handler);
  return () => off(eventName, handler);
}

function off(eventName, handler) {
  const list = handlers.get(eventName);
  if (!list) return;
  const index = list.indexOf(handler);
  if (index >= 0) list.splice(index, 1);
  if (list.length === 0) {
    handlers.delete(eventName);
    call("events.unsubscribe", { event: eventName }).catch(() => {});
  }
}

// Called by the host. A throwing handler is logged and the remaining handlers
// still run: one bad subscriber must not silence the others.
globalThis.__agora_dispatch_event = (json) => {
  const envelope = JSON.parse(json);
  const list = handlers.get(envelope.event);
  if (!list) return;
  for (const handler of list.slice()) {
    try {
      const result = handler(envelope);
      if (result && typeof result.catch === "function") {
        result.catch((e) => log.error(`handler for ${envelope.event} failed:`, e));
      }
    } catch (e) {
      log.error(`handler for ${envelope.event} failed:`, e);
    }
  }
};

const apiVersion = __agora_api_version;
const pluginId = __agora_plugin_id;
const settings = JSON.parse(__agora_settings);

export { call, log, instances, content, launch, storage, net, ui, wasm, crash, game, on, off, apiVersion, pluginId, settings };
export default { call, log, instances, content, launch, storage, net, ui, wasm, crash, game, on, off, apiVersion, pluginId, settings };
"#;

/// Name plugins import: `import { instances } from "agora"`.
pub const AGORA_MODULE_NAME: &str = "agora";
