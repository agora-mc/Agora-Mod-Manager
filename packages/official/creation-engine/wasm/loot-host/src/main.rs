//! Run the LOOT WASI module with read-only `/data` and `/input` preopens.
//!
//! Usage:
//!   loot-host <module.wasm> <request.json> <data-dir> <input-dir|-> <out.json>
//!
//! This is proof scaffolding for the module, not part of the desktop host.

use std::path::PathBuf;
use std::time::Instant;

use wasmtime::{Engine, Linker, Module, Store};
use wasmtime_wasi::p1::{self, WasiP1Ctx};
use wasmtime_wasi::p2::pipe::{MemoryInputPipe, MemoryOutputPipe};
use wasmtime_wasi::{FsPerms, WasiCtxBuilder};

fn main() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() != 5 {
        return Err(
            "usage: loot-host <module.wasm> <request.json> <data-dir> <input-dir|-> <out.json>"
                .into(),
        );
    }
    let wasm_path = PathBuf::from(&args[0]);
    let request_path = PathBuf::from(&args[1]);
    let data_dir = PathBuf::from(&args[2]);
    let input_dir = (&args[3] != "-").then(|| PathBuf::from(&args[3]));
    let out_path = PathBuf::from(&args[4]);

    let request = std::fs::read(&request_path).map_err(|e| format!("read request: {e}"))?;

    let engine_start = Instant::now();
    let mut config = wasmtime::Config::new();
    config.cranelift_opt_level(wasmtime::OptLevel::Speed);
    let engine = Engine::new(&config).map_err(|e| format!("engine: {e}"))?;
    let module = Module::from_file(&engine, &wasm_path).map_err(|e| format!("module: {e}"))?;
    let compile_ms = engine_start.elapsed().as_secs_f64() * 1000.0;

    let mut linker: Linker<WasiP1Ctx> = Linker::new(&engine);
    p1::add_to_linker_sync(&mut linker, |t| t).map_err(|e| format!("linker: {e}"))?;
    let pre = linker
        .instantiate_pre(&module)
        .map_err(|e| format!("instantiate_pre: {e}"))?;

    eprintln!("engine+compile: {compile_ms:.3} ms");

    let stdout = MemoryOutputPipe::new(8 * 1024 * 1024);
    let stderr = MemoryOutputPipe::new(1024 * 1024);
    let mut wasi_builder = WasiCtxBuilder::new();
    wasi_builder
        .args(&["loot-sort"])
        .stdin(MemoryInputPipe::new(request))
        .stdout(stdout.clone())
        .stderr(stderr.clone());
    wasi_builder
        .preopened_dir(&data_dir, "/data", FsPerms::ReadOnly)
        .map_err(|e| format!("preopen data: {e}"))?;
    if let Some(input_dir) = input_dir {
        wasi_builder
            .preopened_dir(&input_dir, "/input", FsPerms::ReadOnly)
            .map_err(|e| format!("preopen input: {e}"))?;
    }
    let wasi_ctx = wasi_builder.build_p1();

    let start_time = Instant::now();
    let mut store = Store::new(&engine, wasi_ctx);
    let instance = pre
        .instantiate(&mut store)
        .map_err(|e| format!("instantiate: {e}"))?;
    let func = instance
        .get_typed_func::<(), ()>(&mut store, "_start")
        .map_err(|e| format!("_start: {e}"))?;
    let call_result = func.call(&mut store, ());
    let elapsed_ms = start_time.elapsed().as_secs_f64() * 1000.0;

    let stderr_bytes = stderr.contents();
    let guest_stderr = String::from_utf8_lossy(&stderr_bytes);
    if let Err(error) = call_result {
        let stdout_bytes = stdout.contents();
        let guest_stdout = String::from_utf8_lossy(&stdout_bytes);
        if let Some(exit) = error.downcast_ref::<wasmtime_wasi::I32Exit>() {
            return Err(format!(
                "guest exited with {}\nstdout: {guest_stdout}\nstderr: {guest_stderr}",
                exit.0
            ));
        }
        return Err(format!(
            "guest trapped: {error}\nstdout: {guest_stdout}\nstderr: {guest_stderr}"
        ));
    }

    if !guest_stderr.trim().is_empty() {
        eprintln!("guest stderr:\n{guest_stderr}");
    }
    eprintln!("run: {elapsed_ms:.3} ms");

    let out = stdout.contents();
    if out.is_empty() {
        return Err("guest produced no stdout".into());
    }
    std::fs::write(&out_path, &*out).map_err(|e| format!("write output: {e}"))?;
    Ok(())
}
