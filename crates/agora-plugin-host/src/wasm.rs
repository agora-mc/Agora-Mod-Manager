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

use agora_plugin_api::host::{
    validate_blob_key, GameReads, ScriptReadError, StageBudget, StageLimits, StoredBlobs,
};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
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
    #[serde(default)]
    stored: Option<Vec<String>>,
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
    #[serde(default)]
    mount_bytes: Option<u64>,
    #[serde(default)]
    mount_files: Option<u64>,
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
    Stored {
        guest: String,
        keys: Vec<String>,
    },
}

impl Mount {
    fn guest(&self) -> &str {
        match self {
            Mount::View { guest, .. }
            | Mount::Files { guest, .. }
            | Mount::Stored { guest, .. } => guest,
        }
    }
}

/// Run one module in its own thread and wait for it, never past its deadline.
///
/// The thread is deliberate: `wasmtime-wasi`'s synchronous preview 1 adapter
/// needs a Tokio runtime, and the plugin thread is already inside one. A fresh
/// thread also keeps a module panic from taking down the plugin's runtime.
/// A compile the deadline abandons keeps running in the background; see
/// [`run_inner`].
pub fn run(
    module: &str,
    package_root: &Path,
    data_root: &Path,
    options_json: &str,
    game_reads: Option<Arc<dyn GameReads>>,
    stored_blobs: Option<Arc<dyn StoredBlobs>>,
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
                stored_blobs,
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
    stored_blobs: Option<Arc<dyn StoredBlobs>>,
) -> Result<WasmRunResult, WasmError> {
    // The deadline covers the whole run, not just the module's call. Work
    // before the module starts (staging, reading the file) spends it too.
    let started = Instant::now();
    let module_path = crate::modules::resolve_package_file(package_root, module)
        .map_err(|message| WasmError::new("not_found", message))?;
    let options: RawOptions = serde_json::from_str(options_json).map_err(|e| {
        WasmError::new(
            "invalid_arguments",
            format!("the module options are not valid: {e}"),
        )
    })?;
    let stdin = options.stdin.map(RawBytes::into_bytes).unwrap_or_default();
    let mounts = validate_mounts(options.mounts, game_reads.as_ref(), stored_blobs.as_ref())?;
    let (memory_limit, timeout, stage_limits) = limits(options.limits)?;
    // One allowance for every mount in this run. Its bytes stay inside the
    // sandbox, so they are not charged to the script's `game:read` budget.
    let stage_budget = StageBudget::default();

    let staging = StagingDir::create(data_root)?;
    let mut preopens = Vec::new();
    for (index, mount) in mounts.iter().enumerate() {
        let host_dir = staging.0.join(format!("mount-{index}"));
        std::fs::create_dir_all(&host_dir).map_err(|e| {
            WasmError::new("staging_failed", format!("could not stage a mount: {e}"))
        })?;
        let guest = mount.guest().to_string();
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
                    .stage_view(folder, patterns, &host_dir, &stage_limits, &stage_budget)
                    .map_err(|error| mount_error(&guest, error))?;
            }
            Mount::Stored { keys, .. } => {
                let blobs = stored_blobs.as_ref().ok_or_else(|| {
                    WasmError::new(
                        "capability_denied",
                        "a stored mount needs a plugin blob store",
                    )
                })?;
                for key in keys {
                    let dest = host_dir.join(key);
                    blobs
                        .stage(key, &dest, &stage_limits, &stage_budget)
                        .map_err(|error| mount_error(&guest, error))?;
                }
            }
            Mount::Files { files, .. } => {
                for (name, bytes) in files {
                    stage_budget
                        .spend(&stage_limits, bytes.len() as u64)
                        .map_err(|error| mount_error(&guest, error))?;
                    let dest = name
                        .split('/')
                        .fold(host_dir.clone(), |path, part| path.join(part));
                    if let Some(parent) = dest.parent() {
                        std::fs::create_dir_all(parent).map_err(|e| {
                            WasmError::new(
                                "staging_failed",
                                format!("mount `{guest}`: could not stage `{name}`: {e}"),
                            )
                        })?;
                    }
                    std::fs::write(&dest, bytes).map_err(|e| {
                        WasmError::new(
                            "staging_failed",
                            format!("mount `{guest}`: could not stage `{name}`: {e}"),
                        )
                    })?;
                }
            }
        }
        preopens.push((host_dir, guest));
    }

    let wasm = read_module(&module_path, module)?;

    let remaining = timeout.saturating_sub(started.elapsed());
    if remaining.is_zero() {
        return Err(timeout_error(timeout));
    }

    // Run on a current-thread runtime so the wall-clock deadline can drop an
    // in-flight host call. The outer thread already isolates this from the
    // plugin runtime, so creating a runtime here cannot nest one.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .map_err(|e| {
            WasmError::new("internal", format!("could not start a module runtime: {e}"))
        })?;
    let outcome = runtime.block_on(execute(
        &wasm,
        module,
        MemoryInputPipe::new(stdin),
        preopens,
        memory_limit,
        timeout,
        remaining,
        data_root,
    ));
    // Dropping a runtime waits for its `spawn_blocking` tasks. A compile the
    // deadline abandoned must not hold the call open, so shut the runtime down
    // in the background: the compile keeps running on its blocking thread until
    // it finishes, but the caller gets its timeout at the deadline.
    runtime.shutdown_background();
    outcome
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

/// A staging failure as the script sees it: it names the guest mount.
fn mount_error(guest: &str, error: ScriptReadError) -> WasmError {
    WasmError::new(
        "staging_failed",
        format!("mount `{guest}`: {}", error.message),
    )
}

/// The async half of [`run_inner`].
///
/// Compilation and execution share one wall-clock deadline. Compilation runs
/// on a blocking thread so the deadline can abandon it (the compile cache makes
/// a repeat cheap), and the call itself runs under `timeout_at`, which drops
/// the in-flight future — including a host call such as `poll_oneoff` or a
/// blocked `fd_read` — instead of waiting for it.
///
/// An abandoned compile cannot be cancelled; it runs to completion on its
/// blocking thread. [`run_inner`] shuts the runtime down in the background so
/// that work never delays the caller past the deadline.
#[allow(clippy::too_many_arguments)]
async fn execute<S: StdinStream + 'static>(
    wasm: &[u8],
    module_arg: &str,
    stdin: S,
    preopens: Vec<(PathBuf, String)>,
    memory_limit: u64,
    timeout: Duration,
    remaining: Duration,
    data_root: &Path,
) -> Result<WasmRunResult, WasmError> {
    let engine = engine(data_root);
    start_ticker(&engine);
    let deadline = tokio::time::Instant::now() + remaining;

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

fn limits(raw: Option<RawLimits>) -> Result<(u64, Duration, StageLimits), WasmError> {
    let raw = raw.unwrap_or(RawLimits {
        memory_bytes: None,
        timeout_ms: None,
        mount_bytes: None,
        mount_files: None,
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
    let defaults = StageLimits::default();
    let mount_bytes = raw.mount_bytes.unwrap_or(defaults.max_bytes);
    if mount_bytes > defaults.max_bytes {
        return Err(WasmError::new(
            "invalid_arguments",
            format!(
                "a script may lower the run's mount byte limit, not raise it above {} bytes",
                defaults.max_bytes
            ),
        ));
    }
    let mount_files = raw.mount_files.unwrap_or(defaults.max_files);
    if mount_files > defaults.max_files {
        return Err(WasmError::new(
            "invalid_arguments",
            format!(
                "a script may lower the run's mount file limit, not raise it above {}",
                defaults.max_files
            ),
        ));
    }
    Ok((
        memory,
        Duration::from_millis(timeout_ms),
        StageLimits {
            max_bytes: mount_bytes,
            max_files: mount_files,
        },
    ))
}

fn validate_mounts(
    raw: Vec<RawMount>,
    game_reads: Option<&Arc<dyn GameReads>>,
    stored_blobs: Option<&Arc<dyn StoredBlobs>>,
) -> Result<Vec<Mount>, WasmError> {
    let mut mounts = Vec::with_capacity(raw.len());
    for mount in raw {
        let guest = validate_guest(&mount.guest)?;
        let kinds = [
            mount.view.is_some(),
            mount.files.is_some(),
            mount.stored.is_some(),
        ]
        .into_iter()
        .filter(|present| *present)
        .count();
        if kinds != 1 {
            return Err(WasmError::new(
                "invalid_arguments",
                "a mount is a `view`, `files` or `stored`, never more than one",
            ));
        }
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
            (None, None) => {
                let keys = mount.stored.unwrap_or_default();
                if keys.is_empty() {
                    return Err(WasmError::new(
                        "invalid_arguments",
                        "a `stored` mount names at least one blob",
                    ));
                }
                if stored_blobs.is_none() {
                    return Err(WasmError::new(
                        "capability_denied",
                        "a stored mount needs a plugin blob store",
                    ));
                }
                for key in &keys {
                    validate_blob_key(key).map_err(|message| {
                        WasmError::new(
                            "invalid_arguments",
                            format!("`{key}` is not a blob name: {message}"),
                        )
                    })?;
                }
                mounts.push(Mount::Stored { guest, keys });
            }
            _ => {
                return Err(WasmError::new(
                    "invalid_arguments",
                    "a mount is a `view`, `files` or `stored`, never more than one",
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
                stored: None,
            },
            RawMount {
                guest: "/data/sub".into(),
                view: None,
                files: Some(BTreeMap::from([("b".into(), RawBytes::Text("y".into()))])),
                stored: None,
            },
        ];
        assert!(validate_mounts(raw, None, None).is_err());
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
        let limit = Duration::from_millis(200);
        let result = runtime.block_on(execute(
            include_bytes!("../tests/wasm-fixtures/wasm/fs_probe.wasm"),
            "fs_probe.wasm",
            StallingStdin,
            Vec::new(),
            64 * 1024 * 1024,
            limit,
            limit,
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

    /// A valid WASI command module with `functions` small functions, large
    /// enough that a cold compile is much slower than a short deadline.
    /// `nonce` makes the bytes unique so the wasmtime compile cache cannot
    /// hide the compile time.
    fn large_module(nonce: u64, functions: u32) -> Vec<u8> {
        use wasm_encoder::{
            CodeSection, CustomSection, ExportKind, ExportSection, Function, FunctionSection,
            Instruction, Module as EncodedModule, TypeSection,
        };
        let mut module = EncodedModule::new();
        let mut types = TypeSection::new();
        types.ty().function([], []);
        module.section(&types);
        let mut funcs = FunctionSection::new();
        for _ in 0..functions {
            funcs.function(0);
        }
        module.section(&funcs);
        let mut exports = ExportSection::new();
        exports.export("_start", ExportKind::Func, 0);
        module.section(&exports);
        let mut code = CodeSection::new();
        for _ in 0..functions {
            let mut body = Function::new([]);
            for _ in 0..8 {
                body.instruction(&Instruction::I32Const(1));
                body.instruction(&Instruction::I32Const(2));
                body.instruction(&Instruction::I32Add);
                body.instruction(&Instruction::Drop);
            }
            body.instruction(&Instruction::End);
            code.function(&body);
        }
        module.section(&code);
        module.section(&CustomSection {
            name: "nonce".into(),
            data: nonce.to_le_bytes().to_vec().into(),
        });
        module.finish()
    }

    /// The deadline covers compilation. When a compile outlasts it, the call
    /// must return at the deadline, not once the abandoned compile finishes.
    ///
    /// The module is deliberately large enough that a cold compile is far
    /// slower than the limit, and the probe uses different bytes so the
    /// wasmtime compile cache cannot hide that. This does not depend on the
    /// machine being slow: the probe measures how long a cold compile takes,
    /// and the assertion is relative to that measurement.
    #[test]
    fn a_compile_that_outlasts_the_deadline_does_not_hold_the_call() {
        const FUNCTIONS: u32 = 12_000;
        let limit = Duration::from_millis(20);

        let data = tempfile::tempdir().expect("temp data root");
        let package = data.path().join("package");
        std::fs::create_dir_all(&package).unwrap();

        // How long a cold compile of this size takes. A different nonce for
        // the module below keeps this from warming the cache for it.
        let probe = large_module(1, FUNCTIONS);
        let started = std::time::Instant::now();
        Module::new(&engine(data.path()), &probe).expect("the probe module compiles");
        let compile_time = started.elapsed();

        let wasm = large_module(2, FUNCTIONS);
        std::fs::write(package.join("big.wasm"), &wasm).unwrap();
        let started = std::time::Instant::now();
        let outcome = run(
            "big.wasm",
            &package,
            data.path(),
            r#"{"module":"big.wasm","limits":{"timeoutMs":20}}"#,
            None,
            None,
        );
        let elapsed = started.elapsed();

        let error = outcome.expect_err("the module should hit its deadline");
        assert_eq!(error.code, "timeout", "{}", error.message);
        // If a cold compile is not clearly slower than the limit, the test
        // cannot tell an abandoned compile from a fast one.
        assert!(
            compile_time > limit * 10,
            "the probe module compiled in {compile_time:?}, too fast to test the deadline"
        );
        assert!(
            elapsed < compile_time / 2,
            "the call waited {elapsed:?} for a compile that takes {compile_time:?}"
        );
    }
}
