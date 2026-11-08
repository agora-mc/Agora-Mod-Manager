//! WebAssembly modules a plugin ships (MASTER_SPEC §30.13).
//!
//! A module is a WASI command: it reads its request from stdin and writes its
//! answer to stdout. This module owns the sandbox around it — a fresh staging
//! folder with read-only preopens, a memory cap, an epoch deadline, and
//! captured output — so `agora.wasm.run` needs no capability of its own.
//!
//! The module never gets a real instance, layer or game path. View mounts are
//! resolved by `GameReads` (the same merged view `game.readBytes` uses) and
//! hardlinked into a staging folder that is deleted when the run ends.

use agora_plugin_api::host::GameReads;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use wasmtime::{
    Cache, CacheConfig, Config, Engine, Linker, Module, Store, StoreLimits, StoreLimitsBuilder,
    Trap,
};
use wasmtime_wasi::cli::StdinStream;
use wasmtime_wasi::p1::{self, WasiP1Ctx};
use wasmtime_wasi::p2::pipe::{MemoryInputPipe, MemoryOutputPipe};
use wasmtime_wasi::{FsPerms, WasiCtxBuilder};

/// Memory a module gets unless the script asks for less.
pub const DEFAULT_MEMORY_LIMIT: u64 = 1024 * 1024 * 1024;
/// Wall-clock deadline a module gets unless the script asks for less.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);
/// Most bytes one output stream may produce before the run fails.
pub const MAX_OUTPUT_BYTES: usize = 64 * 1024 * 1024;
/// Largest module file Agora will read and compile.
pub const MAX_MODULE_BYTES: u64 = 64 * 1024 * 1024;
/// Most entries one module's table may hold.
///
/// A fixed small number rather than a share of the linear-memory cap: a
/// `funcref` entry is a pointer, so this is ~512 KiB on a 64-bit host, far
/// above the tens of entries a Rust module uses and small enough that a module
/// cannot turn `table.grow` or a huge declared table into host memory beyond
/// its cap. `StoreLimitsBuilder::table_elements` is documented at
/// <https://docs.rs/wasmtime/latest/wasmtime/struct.StoreLimitsBuilder.html#method.table_elements>.
const MAX_TABLE_ELEMENTS: usize = 65_536;

/// How often the epoch clock ticks. A module is stopped on the first tick
/// after its deadline, so this is also the deadline's granularity.
const EPOCH_TICK: Duration = Duration::from_millis(20);
const EPOCH_TICK_MILLIS: u64 = 20;

static ENGINE: OnceLock<Engine> = OnceLock::new();
static TICKER: OnceLock<()> = OnceLock::new();
static STAGING_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Why a module run failed. `code` is a stable token; `message` is for a
/// plugin author and the plugin log.
#[derive(Debug, Clone)]
pub struct WasmError {
    pub code: &'static str,
    pub message: String,
}

impl WasmError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// What a finished run produced.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WasmRunResult {
    pub exit_code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub stdout_text: String,
    pub stderr_text: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawOptions {
    #[serde(default)]
    stdin: Option<RawBytes>,
    #[serde(default)]
    mounts: Vec<RawMount>,
    #[serde(default)]
    limits: Option<RawLimits>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawBytes {
    Text(String),
    Bytes(Vec<u8>),
}

impl RawBytes {
    fn into_bytes(self) -> Vec<u8> {
        match self {
            RawBytes::Text(text) => text.into_bytes(),
            RawBytes::Bytes(bytes) => bytes,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawMount {
    guest: String,
    #[serde(default)]
    view: Option<RawView>,
    #[serde(default)]
    files: Option<BTreeMap<String, RawBytes>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawView {
    folder: String,
    #[serde(default)]
    patterns: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawLimits {
    #[serde(default)]
    memory_bytes: Option<u64>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

enum Mount {
    View {
        guest: String,
        folder: String,
        patterns: Vec<String>,
    },
    Files {
        guest: String,
        files: BTreeMap<String, Vec<u8>>,
    },
}

impl Mount {
    fn guest(&self) -> &str {
        match self {
            Mount::View { guest, .. } | Mount::Files { guest, .. } => guest,
        }
    }
}

/// Run one module in its own thread and wait for it.
///
/// The thread is deliberate: `wasmtime-wasi`'s synchronous preview 1 adapter
/// needs a Tokio runtime, and the plugin thread is already inside one. A fresh
/// thread also keeps a module panic from taking down the plugin's runtime.
pub fn run(
    module: &str,
    package_root: &Path,
    data_root: &Path,
    options_json: &str,
    game_reads: Option<Arc<dyn GameReads>>,
) -> Result<WasmRunResult, WasmError> {
    let module = module.to_string();
    let package_root = package_root.to_path_buf();
    let data_root = data_root.to_path_buf();
    let options_json = options_json.to_string();
    let handle = std::thread::Builder::new()
        .name("agora-wasm-run".into())
        .spawn(move || {
            run_inner(
                &module,
                &package_root,
                &data_root,
                &options_json,
                game_reads,
            )
        })
        .map_err(|e| WasmError::new("internal", format!("could not start a module runner: {e}")))?;
    handle
        .join()
        .map_err(|_| WasmError::new("internal", "the module runner panicked"))?
}

fn run_inner(
    module: &str,
    package_root: &Path,
    data_root: &Path,
    options_json: &str,
    game_reads: Option<Arc<dyn GameReads>>,
) -> Result<WasmRunResult, WasmError> {
    let module_path = crate::modules::resolve_package_file(package_root, module)
        .map_err(|message| WasmError::new("not_found", message))?;
    let options: RawOptions = serde_json::from_str(options_json).map_err(|e| {
        WasmError::new(
            "invalid_arguments",
            format!("the module options are not valid: {e}"),
        )
    })?;
    let stdin = options.stdin.map(RawBytes::into_bytes).unwrap_or_default();
    let mounts = validate_mounts(options.mounts, game_reads.as_ref())?;
    let (memory_limit, timeout) = limits(options.limits)?;

    let staging = StagingDir::create(data_root)?;
    let mut preopens = Vec::new();
    for (index, mount) in mounts.iter().enumerate() {
        let host_dir = staging.0.join(format!("mount-{index}"));
        std::fs::create_dir_all(&host_dir).map_err(|e| {
            WasmError::new("staging_failed", format!("could not stage a mount: {e}"))
        })?;
        match mount {
            Mount::View {
                folder, patterns, ..
            } => {
                let reads = game_reads.as_ref().ok_or_else(|| {
                    WasmError::new(
                        "capability_denied",
                        "a view mount needs `game:read` inside a hook bound to an instance",
                    )
                })?;
                reads
                    .stage_view(folder, patterns, &host_dir)
                    .map_err(|error| WasmError::new("staging_failed", error.message))?;
            }
            Mount::Files { files, .. } => {
                for (name, bytes) in files {
                    let dest = name
                        .split('/')
                        .fold(host_dir.clone(), |path, part| path.join(part));
                    if let Some(parent) = dest.parent() {
                        std::fs::create_dir_all(parent).map_err(|e| {
                            WasmError::new(
                                "staging_failed",
                                format!("could not stage `{name}`: {e}"),
                            )
                        })?;
                    }
                    std::fs::write(&dest, bytes).map_err(|e| {
                        WasmError::new("staging_failed", format!("could not stage `{name}`: {e}"))
                    })?;
                }
            }
        }
        preopens.push((host_dir, mount.guest().to_string()));
    }

    let wasm = read_module(&module_path, module)?;

    // Run on a current-thread runtime so the wall-clock deadline can drop an
    // in-flight host call. The outer thread already isolates this from the
    // plugin runtime, so creating a runtime here cannot nest one.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .map_err(|e| {
            WasmError::new("internal", format!("could not start a module runtime: {e}"))
        })?;
    runtime.block_on(execute(
        &wasm,
        module,
        MemoryInputPipe::new(stdin),
        preopens,
        memory_limit,
        timeout,
        data_root,
    ))
}

/// Read a module file, refusing anything larger than [`MAX_MODULE_BYTES`].
fn read_module(module_path: &Path, module: &str) -> Result<Vec<u8>, WasmError> {
    let metadata = std::fs::metadata(module_path)
        .map_err(|e| WasmError::new("not_found", format!("could not read `{module}`: {e}")))?;
    if metadata.len() > MAX_MODULE_BYTES {
        return Err(module_too_large(module, metadata.len()));
    }
    let file = std::fs::File::open(module_path)
        .map_err(|e| WasmError::new("not_found", format!("could not read `{module}`: {e}")))?;
    let mut wasm = Vec::with_capacity(metadata.len() as usize);
    // Also bound the read, in case the file grows between the metadata and it.
    file.take(MAX_MODULE_BYTES + 1)
        .read_to_end(&mut wasm)
        .map_err(|e| WasmError::new("not_found", format!("could not read `{module}`: {e}")))?;
    if wasm.len() as u64 > MAX_MODULE_BYTES {
        return Err(module_too_large(module, wasm.len() as u64));
    }
    Ok(wasm)
}

fn module_too_large(module: &str, bytes: u64) -> WasmError {
    WasmError::new(
        "module_too_large",
        format!("`{module}` is {bytes} bytes; a module may be at most {MAX_MODULE_BYTES}"),
    )
}

/// The async half of [`run_inner`].
///
/// Compilation and execution share one wall-clock deadline. Compilation runs
/// on a blocking thread so the deadline can abandon it (the compile cache makes
/// a repeat cheap), and the call itself runs under `timeout_at`, which drops
/// the in-flight future — including a host call such as `poll_oneoff` or a
/// blocked `fd_read` — instead of waiting for it.
async fn execute<S: StdinStream + 'static>(
    wasm: &[u8],
    module_arg: &str,
    stdin: S,
    preopens: Vec<(PathBuf, String)>,
    memory_limit: u64,
    timeout: Duration,
    data_root: &Path,
) -> Result<WasmRunResult, WasmError> {
    let engine = engine(data_root);
    start_ticker(&engine);
    let deadline = tokio::time::Instant::now() + timeout;

    let engine_for_compile = engine.clone();
    let wasm_for_compile = wasm.to_vec();
    let compiled = tokio::time::timeout_at(
        deadline,
        tokio::task::spawn_blocking(move || Module::new(&engine_for_compile, &wasm_for_compile)),
    )
    .await
    .map_err(|_| timeout_error(timeout))?
    .map_err(|e| {
        WasmError::new(
            "internal",
            format!("the module compiler did not finish: {e}"),
        )
    })?;
    let module = compiled.map_err(|e| {
        WasmError::new(
            "module_invalid",
            format!("the module could not be loaded: {e}"),
        )
    })?;

    let stdout_pipe = MemoryOutputPipe::new(MAX_OUTPUT_BYTES + 1);
    let stderr_pipe = MemoryOutputPipe::new(MAX_OUTPUT_BYTES + 1);

    let mut builder = WasiCtxBuilder::new();
    builder.stdin(stdin);
    builder.stdout(stdout_pipe.clone());
    builder.stderr(stderr_pipe.clone());
    builder.arg(module_arg);
    for (host_dir, guest) in &preopens {
        builder
            .preopened_dir(host_dir, guest, FsPerms::ReadOnly)
            .map_err(|e| {
                WasmError::new(
                    "staging_failed",
                    format!("could not open the mount at `{guest}`: {e}"),
                )
            })?;
    }

    let mut linker: Linker<WasmState> = Linker::new(&engine);
    p1::add_to_linker_async(&mut linker, |state| &mut state.wasi).map_err(|e| {
        WasmError::new(
            "internal",
            format!("could not wire the module's sandbox: {e}"),
        )
    })?;

    // Every limit below is documented on `StoreLimitsBuilder`:
    // - memory_size: #method.memory_size (linear-memory bytes)
    // - memories: #method.memories (this module declares one, and
    //   `wasm_multi_memory` is off)
    // - tables: #method.tables (a core module may declare one)
    // - table_elements: #method.table_elements (see `MAX_TABLE_ELEMENTS`)
    // - instances: #method.instances (a core module cannot instantiate others)
    // <https://docs.rs/wasmtime/latest/wasmtime/struct.StoreLimitsBuilder.html>
    let state = WasmState {
        wasi: builder.build_p1(),
        limits: StoreLimitsBuilder::new()
            .memory_size(memory_limit as usize)
            .memories(1)
            .tables(1)
            .table_elements(MAX_TABLE_ELEMENTS)
            .instances(1)
            .trap_on_grow_failure(true)
            .build(),
    };
    let mut store = Store::new(&engine, state);
    store.limiter(|state| &mut state.limits);

    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
    if remaining.is_zero() {
        return Err(timeout_error(timeout));
    }
    // The epoch covers wasm that never yields; the deadline above covers host
    // calls. Both start from what compilation left rather than from the full
    // timeout.
    let ticks = remaining
        .as_millis()
        .div_ceil(EPOCH_TICK_MILLIS as u128)
        .max(1) as u64;
    store.set_epoch_deadline(ticks);

    let outcome = tokio::time::timeout_at(deadline, call_start(&mut store, &linker, &module)).await;
    let stdout = stdout_pipe.contents().to_vec();
    let stderr = stderr_pipe.contents().to_vec();
    if stdout.len() > MAX_OUTPUT_BYTES || stderr.len() > MAX_OUTPUT_BYTES {
        return Err(WasmError::new(
            "output_too_large",
            format!(
                "the module wrote more than {} bytes to stdout or stderr",
                MAX_OUTPUT_BYTES
            ),
        ));
    }
    let outcome = match outcome {
        Err(_) => return Err(timeout_error(timeout)),
        Ok(Err(error)) => return Err(error),
        Ok(Ok(result)) => result,
    };

    let exit_code = match outcome {
        Ok(()) => 0,
        Err(error) => {
            if let Some(exit) = error.downcast_ref::<wasmtime_wasi::I32Exit>() {
                exit.0
            } else if error.downcast_ref::<Trap>() == Some(&Trap::Interrupt) {
                return Err(timeout_error(timeout));
            } else {
                return Err(WasmError::new(
                    "module_trap",
                    format!("the module stopped with an error: {error}"),
                ));
            }
        }
    };

    Ok(WasmRunResult {
        exit_code,
        stdout_text: String::from_utf8_lossy(&stdout).into_owned(),
        stderr_text: String::from_utf8_lossy(&stderr).into_owned(),
        stdout,
        stderr,
    })
}

async fn call_start(
    store: &mut Store<WasmState>,
    linker: &Linker<WasmState>,
    module: &Module,
) -> Result<wasmtime::Result<()>, WasmError> {
    let instance = linker
        .instantiate_async(&mut *store, module)
        .await
        .map_err(|e| {
            WasmError::new("module_invalid", format!("the module could not start: {e}"))
        })?;
    let start = instance
        .get_typed_func::<(), ()>(&mut *store, "_start")
        .map_err(|_| {
            WasmError::new(
                "module_invalid",
                "the module has no `_start`; a module must be a WASI command",
            )
        })?;
    Ok(start.call_async(&mut *store, ()).await)
}

fn timeout_error(timeout: Duration) -> WasmError {
    WasmError::new(
        "timeout",
        format!(
            "the module did not finish within {} ms and was stopped",
            timeout.as_millis()
        ),
    )
}

struct WasmState {
    wasi: WasiP1Ctx,
    limits: StoreLimits,
}

/// Delete a run's staging folder however the run ends.
struct StagingDir(PathBuf);

impl StagingDir {
    fn create(data_root: &Path) -> Result<Self, WasmError> {
        let root = data_root.join("staging");
        std::fs::create_dir_all(&root).map_err(|e| {
            WasmError::new(
                "staging_failed",
                format!("could not create the module staging area: {e}"),
            )
        })?;
        let id = STAGING_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = root.join(format!("wasm-{}-{id}", std::process::id()));
        std::fs::create_dir_all(&path).map_err(|e| {
            WasmError::new(
                "staging_failed",
                format!("could not create a staging folder for the module: {e}"),
            )
        })?;
        Ok(Self(path))
    }
}

impl Drop for StagingDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn engine(data_root: &Path) -> Engine {
    ENGINE
        .get_or_init(|| {
            let mut config = Config::new();
            config.epoch_interruption(true);
            // The preview 1 adapter is built on the component model.
            config.wasm_component_model(true);
            // Features this sandbox has no use for are off; a module that
            // needs them is refused at load rather than given a way out.
            config.wasm_simd(false);
            config.wasm_relaxed_simd(false);
            config.wasm_memory64(false);
            config.wasm_multi_memory(false);

            let cache_dir = data_root.join("wasm-cache");
            if std::fs::create_dir_all(&cache_dir).is_ok() {
                let mut cache_config = CacheConfig::new();
                cache_config.with_directory(&cache_dir);
                if let Ok(cache) = Cache::new(cache_config) {
                    config.cache(Some(cache));
                }
            }
            Engine::new(&config).unwrap_or_else(|e| {
                // A config this small cannot fail for a reason the user could
                // act on; fall back to the defaults rather than refusing every
                // module for the rest of the process.
                eprintln!("agora: could not configure the WebAssembly engine: {e}");
                Engine::default()
            })
        })
        .clone()
}

/// Epoch interruption needs something to advance the clock. One thread for the
/// process is enough; it costs a sleep and an atomic add per tick.
fn start_ticker(engine: &Engine) {
    TICKER.get_or_init(|| {
        let engine = engine.clone();
        let _ = std::thread::Builder::new()
            .name("agora-wasm-epoch".into())
            .spawn(move || loop {
                std::thread::sleep(EPOCH_TICK);
                engine.increment_epoch();
            });
    });
}

fn limits(raw: Option<RawLimits>) -> Result<(u64, Duration), WasmError> {
    let raw = raw.unwrap_or(RawLimits {
        memory_bytes: None,
        timeout_ms: None,
    });
    let memory = raw.memory_bytes.unwrap_or(DEFAULT_MEMORY_LIMIT);
    if memory > DEFAULT_MEMORY_LIMIT {
        return Err(WasmError::new(
            "invalid_arguments",
            format!("a script may lower the module's memory limit, not raise it above {DEFAULT_MEMORY_LIMIT} bytes"),
        ));
    }
    if memory == 0 {
        return Err(WasmError::new(
            "invalid_arguments",
            "a module needs some memory",
        ));
    }
    let timeout_ms = raw.timeout_ms.unwrap_or(DEFAULT_TIMEOUT.as_millis() as u64);
    if timeout_ms > DEFAULT_TIMEOUT.as_millis() as u64 {
        return Err(WasmError::new(
            "invalid_arguments",
            format!(
                "a script may lower the module's time limit, not raise it above {} ms",
                DEFAULT_TIMEOUT.as_millis()
            ),
        ));
    }
    if timeout_ms == 0 {
        return Err(WasmError::new(
            "invalid_arguments",
            "a module needs a little time to run",
        ));
    }
    Ok((memory, Duration::from_millis(timeout_ms)))
}

fn validate_mounts(
    raw: Vec<RawMount>,
    game_reads: Option<&Arc<dyn GameReads>>,
) -> Result<Vec<Mount>, WasmError> {
    let mut mounts = Vec::with_capacity(raw.len());
    for mount in raw {
        let guest = validate_guest(&mount.guest)?;
        match (mount.view, mount.files) {
            (Some(view), None) => {
                let reads = game_reads.ok_or_else(|| {
                    WasmError::new(
                        "capability_denied",
                        "a view mount needs `game:read` inside a hook bound to an instance",
                    )
                })?;
                if reads.instance().is_none() {
                    return Err(WasmError::new(
                        "capability_denied",
                        "a view mount needs `game:read` inside a hook bound to an instance",
                    ));
                }
                mounts.push(Mount::View {
                    guest,
                    folder: view.folder,
                    patterns: view.patterns,
                });
            }
            (None, Some(files)) => {
                let mut staged = BTreeMap::new();
                for (name, bytes) in files {
                    if !is_safe_relative(&name) {
                        return Err(WasmError::new(
                            "invalid_arguments",
                            format!("`{name}` is not a safe file name inside a mount"),
                        ));
                    }
                    staged.insert(name, bytes.into_bytes());
                }
                mounts.push(Mount::Files {
                    guest,
                    files: staged,
                });
            }
            _ => {
                return Err(WasmError::new(
                    "invalid_arguments",
                    "a mount is either a `view` or `files`, never both",
                ))
            }
        }
    }

    for (index, mount) in mounts.iter().enumerate() {
        for other in mounts.iter().skip(index + 1) {
            let a = mount.guest();
            let b = other.guest();
            if a == b || is_below(a, b) || is_below(b, a) {
                return Err(WasmError::new(
                    "invalid_arguments",
                    format!("guest paths `{a}` and `{b}` overlap; a mount cannot nest or repeat"),
                ));
            }
        }
    }
    Ok(mounts)
}

fn validate_guest(guest: &str) -> Result<String, WasmError> {
    if !guest.starts_with('/') || guest.contains('\\') {
        return Err(WasmError::new(
            "invalid_arguments",
            format!("`{guest}` is not an absolute guest path, like /data"),
        ));
    }
    let parts: Vec<&str> = guest.split('/').filter(|part| !part.is_empty()).collect();
    if parts.is_empty() || parts.iter().any(|part| *part == "." || *part == "..") {
        return Err(WasmError::new(
            "invalid_arguments",
            format!("`{guest}` is not a usable guest path"),
        ));
    }
    Ok(format!("/{}", parts.join("/")))
}

fn is_below(parent: &str, child: &str) -> bool {
    child.starts_with(parent) && child.as_bytes().get(parent.len()) == Some(&b'/')
}

fn is_safe_relative(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('/')
        && !name.contains('\\')
        && !name.contains(':')
        && name
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guest_paths_are_absolute_and_normalised() {
        assert_eq!(validate_guest("/data/").unwrap(), "/data");
        assert!(validate_guest("data").is_err());
        assert!(validate_guest("/data/../x").is_err());
        assert!(validate_guest("/").is_err());
    }

    #[test]
    fn nested_mounts_are_refused() {
        let raw = vec![
            RawMount {
                guest: "/data".into(),
                view: None,
                files: Some(BTreeMap::from([("a".into(), RawBytes::Text("x".into()))])),
            },
            RawMount {
                guest: "/data/sub".into(),
                view: None,
                files: Some(BTreeMap::from([("b".into(), RawBytes::Text("y".into()))])),
            },
        ];
        assert!(validate_mounts(raw, None).is_err());
    }

    #[test]
    fn a_file_name_cannot_climb_out_of_a_mount() {
        assert!(!is_safe_relative("../x"));
        assert!(!is_safe_relative("/x"));
        assert!(!is_safe_relative("a//b"));
        assert!(is_safe_relative("a/b.txt"));
    }

    /// A stdin that is open and never ready, like a pipe a writer holds.
    struct StallingStdin;

    impl wasmtime_wasi::cli::IsTerminal for StallingStdin {
        fn is_terminal(&self) -> bool {
            false
        }
    }

    impl StdinStream for StallingStdin {
        fn async_stream(&self) -> Box<dyn tokio::io::AsyncRead + Send + Sync> {
            Box::new(StallingReader)
        }
    }

    struct StallingReader;

    impl tokio::io::AsyncRead for StallingReader {
        fn poll_read(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            _buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Pending
        }
    }

    #[test]
    fn a_blocked_stdin_read_is_stopped_by_the_time_limit() {
        let data = tempfile::tempdir().expect("temp data root");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("test runtime");
        let started = std::time::Instant::now();
        let result = runtime.block_on(execute(
            include_bytes!("../tests/wasm-fixtures/wasm/fs_probe.wasm"),
            "fs_probe.wasm",
            StallingStdin,
            Vec::new(),
            64 * 1024 * 1024,
            Duration::from_millis(200),
            data.path(),
        ));
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(2),
            "a blocked fd_read outlived the deadline: {elapsed:?}"
        );
        let error = result
            .err()
            .unwrap_or_else(|| panic!("the blocked read was not stopped"));
        assert_eq!(error.code, "timeout", "{}", error.message);
    }
}
