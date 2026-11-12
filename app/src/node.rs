//! Node process management.
//!
//! On Android the solochain node ships as a prebuilt aarch64 binary inside the
//! APK's `jniLibs/arm64-v8a/` directory (packaged as `libsolochain-node.so` so
//! the Android packager keeps it). The Android package manager extracts
//! `jniLibs` to `<app>/lib/arm64` (`nativeLibraryDir`) at install time — that
//! directory is the one place an app may `execve` a file from, even on
//! Android 10+ where executing from app data is blocked.
//!
//! On desktop we just exec a locally built node binary so `dx serve
//! --platform desktop` behaves the same as the device.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex};

use dioxus::prelude::*;
use tokio::process::{Child, Command};

use crate::{log_line, status};
/// How many log lines we keep in the ring buffer (memory only — nothing is
/// written to disk, so logs can never grow unbounded).
pub const LOG_CAP: usize = 300;

/// Storage guard rails: the node is stopped if the chain database exceeds
/// this, or if the device has less free space than this.
pub const MAX_DB_BYTES: u64 = 5 * 1024 * 1024 * 1024; // 5 GiB
pub const MIN_FREE_BYTES: u64 = 1024 * 1024 * 1024; // 1 GiB

/// Where everything lives for the current platform.
pub struct Dirs {
    /// Executable node binary (the jniLibs-extracted .so on Android).
    pub node_bin: PathBuf,
    /// Writable app-private directory (getFilesDir on Android).
    pub files_dir: PathBuf,
}

#[cfg(target_os = "android")]
pub fn dirs() -> Dirs {
    android_dirs().expect("failed to resolve android directories via JNI")
}

#[cfg(not(target_os = "android"))]
pub fn dirs() -> Dirs {
    // Desktop: env override, else the project-root ./node/ copy of a host
    // build (resolved relative to the crate dir so it works no matter where
    // dx launches the binary from).
    let project_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    let node_bin = std::env::var("SOLOCHAIN_NODE_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|_| project_root.join("node").join("solochain-template-node"));
    Dirs {
        node_bin,
        files_dir: project_root.join("chaindata"),
    }
}

/// Resolve `getFilesDir()` and `getApplicationInfo().nativeLibraryDir` through
/// the JNI surface the Dioxus/wry activity already installed for us.
#[cfg(target_os = "android")]
fn android_dirs() -> Result<Dirs, Box<dyn std::error::Error>> {
    use jni::JavaVM;

    let ctx = ndk_context::android_context();
    let vm = unsafe { JavaVM::from_raw(ctx.vm().cast()) }?;
    let mut env = vm.attach_current_thread()?;

    // Same rule as service_intent_action: never leave a JNI exception
    // pending on this thread — it becomes a fatal crash on re-entry.
    let outcome = android_dirs_jni(&mut env, ctx.context());
    let _ = env.exception_clear();
    outcome
}

#[cfg(target_os = "android")]
fn android_dirs_jni(
    env: &mut jni::JNIEnv,
    context: *mut std::ffi::c_void,
) -> Result<Dirs, Box<dyn std::error::Error>> {
    use jni::objects::JObject;

    let activity = unsafe { JObject::from_raw(context.cast()) };

    // getFilesDir().getAbsolutePath()
    let files = env
        .call_method(&activity, "getFilesDir", "()Ljava/io/File;", &[])?
        .l()?;
    let path = env
        .call_method(&files, "getAbsolutePath", "()Ljava/lang/String;", &[])?
        .l()?;
    let files_dir = PathBuf::from(env.get_string(&path.into())?.to_string_lossy().into_owned());

    // getApplicationInfo().nativeLibraryDir
    let info = env
        .call_method(
            &activity,
            "getApplicationInfo",
            "()Landroid/content/pm/ApplicationInfo;",
            &[],
        )?
        .l()?;
    let nl_dir = env
        .get_field(&info, "nativeLibraryDir", "Ljava/lang/String;")?
        .l()?;
    let native_dir = PathBuf::from(
        env.get_string(&nl_dir.into())?
            .to_string_lossy()
            .into_owned(),
    );

    Ok(Dirs {
        node_bin: native_dir.join("libsolochain-node.so"),
        files_dir,
    })
}

pub static RUNNING: AtomicBool = AtomicBool::new(false);
/// Guards against double-spawn (hot-reload remounts re-run effects).
static STARTING: AtomicBool = AtomicBool::new(false);
static CHILD: Mutex<Option<Child>> = Mutex::new(None);
static LOG_BUF: LazyLock<Mutex<VecDeque<String>>> = LazyLock::new(|| Mutex::new(VecDeque::new()));

pub fn push_log(line: impl AsRef<str>) {
    let line = line.as_ref();
    log_line(line);
    let mut buf = LOG_BUF.lock().unwrap();
    buf.push_back(line.to_string());
    while buf.len() > LOG_CAP {
        buf.pop_front();
    }
    let joined = buf
        .iter()
        .map(|s| s.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    *crate::LOGS.write() = joined;
}

/// Start the node. Returns immediately; a background task owns the process,
/// streams its logs into the ring buffer and detects exit.
pub fn spawn_start() {
    spawn(async move {
        // double-spawn guard: RUNNING covers the running case, STARTING the
        // window between click and process startup (and dev hot-reload
        // remounts, which re-run our bootstrap effect)
        if RUNNING.load(Ordering::SeqCst) || STARTING.swap(true, Ordering::SeqCst) {
            return;
        }

        // Desktop: kill any orphaned node left over from a previous run —
        // a dead app doesn't reap its child, and the orphan holds the
        // RocksDB LOCK (and port 9944), making the fresh spawn die with
        // "Resource temporarily unavailable". (On Android the OS kills the
        // whole process group with the app, so this can't happen.)
        #[cfg(not(target_os = "android"))]
        kill_stale_nodes().await;

        let dirs = dirs();
        let base_path = dirs.files_dir.join("chain");
        if !dirs.node_bin.exists() {
            STARTING.store(false, Ordering::SeqCst);
            status().write().phase = "error".into();
            status().write().error =
                format!("node binary not found at {}", dirs.node_bin.display());
            push_log(format!(
                "ERROR: node binary not found at {}",
                dirs.node_bin.display()
            ));
            return;
        }

        // Fresh-chain guard: if the previous run blew the storage budget we
        // don't auto-delete, but we do surface it.
        status().write().phase = "starting".into();
        push_log(format!("starting {}", dirs.node_bin.display()));

        let args: Vec<String> = vec![
            "--dev".into(),
            "--base-path".into(),
            base_path.to_string_lossy().into(),
            "--rpc-port".into(),
            "9944".into(),
            // keep RocksDB memory bounded on phones
            "--db-cache".into(),
            "256".into(),
            "--rpc-cors".into(),
            "all".into(),
            "--offchain-worker".into(),
            "never".into(),
        ];
        // Android restricts per-app virtual address space, but wasmtime's
        // default instantiation strategy ("pooling-copy-on-write") pre-mmaps
        // ~414 GB of address space for its memory pool, which fails with
        // "mmap failed to allocate 0x6080000000 bytes: Out of memory
        // (os error 12)" right after the DB opens. "recreate-instance"
        // allocates each Wasm instance's memory on demand instead — much
        // slower instantiation, but it fits. Desktop keeps the fast default.
        // #[cfg(target_os = "android")]
        // args.extend([
        //     // 1. Completely bypass the pooling allocator's 384GB virtual memory reservation
        //     "--wasmtime-instantiation-strategy".into(),
        //     "recreate-instance-copy-on-write".into(),
        //     // 2. Force the interpreter to avoid any Wasmtime JIT compilation overhead
        //     "--wasm-execution".into(),
        //     "interpreted-i-know-what-i-do".into(),
        // ]);

        let mut cmd = Command::new(&dirs.node_bin);
        // The node is exec'd directly, so the Android dynamic linker does NOT
        // inherit the app's library search path — and it doesn't search the
        // binary's own directory either. Point it at nativeLibraryDir so the
        // shared runtime libs we ship next to the binary resolve first
        // (libc++_shared.so is required: cargo-ndk links the NDK's shared
        // C++ runtime, and missing it fails exec with
        // "cannot locate symbol _ZTVNSt6__ndk1…"). Harmless on desktop.
        if let Some(native_dir) = dirs.node_bin.parent() {
            let mut ld_paths = vec![native_dir.to_path_buf()];
            ld_paths.extend(std::env::split_paths(
                &std::env::var_os("LD_LIBRARY_PATH").unwrap_or_default(),
            ));
            if let Ok(joined) = std::env::join_paths(&ld_paths) {
                cmd.env("LD_LIBRARY_PATH", joined);
            }
        }
        cmd.args(&args);
        // Log the effective command line — the dashboard log is the only way
        // to verify on-device which flags the node actually received.
        push_log(format!("args: {}", args.join(" ")));

        let mut child = match cmd
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                STARTING.store(false, Ordering::SeqCst);
                status().write().phase = "error".into();
                status().write().error = format!("failed to spawn node: {e}");
                push_log(format!("ERROR: failed to spawn node: {e}"));
                return;
            }
        };

        // Stream stdout/stderr into the ring buffer.
        if let Some(out) = child.stdout.take() {
            spawn(stream_logs(out));
        }
        if let Some(err) = child.stderr.take() {
            spawn(stream_logs(err));
        }

        status().write().started_at = Some(now_secs());
        status().write().uptime_secs = 0;
        *CHILD.lock().unwrap() = Some(child);
        RUNNING.store(true, Ordering::SeqCst);
        STARTING.store(false, Ordering::SeqCst);
        status().write().phase = "running".into();
        push_log("node started");
        start_foreground_service();

        // Reap loop: poll for exit, report status.
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            if !RUNNING.load(Ordering::SeqCst) {
                // stop_node() already finalized the state.
                return;
            }
            let mut guard = CHILD.lock().unwrap();
            match guard.as_mut() {
                Some(child) => match child.try_wait() {
                    Ok(Some(exit)) => {
                        *guard = None;
                        drop(guard);
                        RUNNING.store(false, Ordering::SeqCst);
                        stop_foreground_service();
                        status().write().phase = "crashed".into();
                        // Distinguish the "stale instance held the db lock"
                        // case so the dashboard says something actionable.
                        let lock_conflict = {
                            let buf = LOG_BUF.lock().unwrap();
                            buf.iter().rev().take(40).any(|l| {
                                l.contains("lock file") || l.contains("temporarily unavailable")
                            })
                        };
                        if lock_conflict {
                            status().write().error = "database locked: another node instance \
                                was already running (stale instance was killed — press Start \
                                to try again)"
                                .into();
                        } else {
                            status().write().error = format!("node exited: {exit}");
                        }
                        push_log(format!("node exited: {exit}"));
                        return;
                    }
                    Ok(None) => {}
                    Err(_) => {}
                },
                None => return,
            }
        }
    });
}

/// Kill leftover node processes from a previous app run (desktop only —
/// on Android the OS kills the whole process group with the app).
#[cfg(not(target_os = "android"))]
async fn kill_stale_nodes() {
    let ok = Command::new("pkill")
        .args(["-f", "solochain-template-node"])
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false);
    if ok {
        push_log("killed stale node instance(s) from a previous run");
        // give the OS a beat to release the RocksDB lock file
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
}

#[cfg(target_os = "android")]
#[allow(dead_code)]
async fn kill_stale_nodes() {}

async fn stream_logs<R>(mut reader: R)
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;
    let mut buf = [0u8; 4096];
    let mut tail = Vec::new();
    loop {
        match reader.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                tail.extend_from_slice(&buf[..n]);
                while let Some(pos) = tail.iter().position(|&b| b == b'\n') {
                    let line: Vec<u8> = tail.drain(..=pos).collect();
                    push_log(String::from_utf8_lossy(&line[..pos]).trim_end().to_string());
                }
                if tail.len() > 64 * 1024 {
                    tail.clear();
                }
            }
            Err(_) => break,
        }
    }
    if !tail.is_empty() {
        push_log(String::from_utf8_lossy(&tail).trim_end().to_string());
    }
}

/// Stop the running node (gracefully kill; substrate dev mode exits on SIGTERM).
pub fn stop() {
    if !RUNNING.swap(false, Ordering::SeqCst) {
        return;
    }
    if let Some(child) = CHILD.lock().unwrap().as_mut() {
        let _ = child.start_kill();
        push_log("node stop requested");
    }
    status().write().phase = "stopped".into();
    status().write().error = String::new();
    stop_foreground_service();
}

/// Wipe the chain database (only when the node is stopped) and restart.
pub fn reset_chain() {
    stop();
    spawn(async move {
        // give the reap loop a moment to notice the kill
        tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
        let dirs = dirs();
        let db = dirs.files_dir.join("chain");
        if db.exists() {
            match std::fs::remove_dir_all(&db) {
                Ok(()) => push_log(format!("chain database wiped: {}", db.display())),
                Err(e) => push_log(format!("failed to wipe chain database: {e}")),
            }
        }
        status().write().best_number = None;
        status().write().finalized_number = None;
        status().write().best_hash = String::new();
        spawn_start();
    });
}

// ---------------------------------------------------------------------------
// foreground service (keeps the process group alive when backgrounded)
// ---------------------------------------------------------------------------

/// Start `DioxusForegroundService` so Android pins our process while the
/// dashboard is in the background. Failures are logged, never fatal — the
/// node still runs, it just loses background protection.
#[cfg(target_os = "android")]
fn start_foreground_service() {
    service_intent_action("startForegroundService");
}

#[cfg(target_os = "android")]
fn stop_foreground_service() {
    service_intent_action("stopService");
}

#[cfg(target_os = "android")]
fn service_intent_action(action: &str) {
    use jni::JavaVM;

    let result = (|| -> Result<(), Box<dyn std::error::Error>> {
        let ctx = ndk_context::android_context();
        let vm = unsafe { JavaVM::from_raw(ctx.vm().cast()) }?;
        let mut env = vm.attach_current_thread()?;

        // A failed JNI call leaves the exception *pending* on this thread.
        // When the thread next re-enters Java (or detaches), ART turns that
        // into a fatal uncaught exception and kills the process — so always
        // clear it before returning. Our failures are logged, never fatal.
        let outcome = service_intent_jni(&mut env, ctx.context(), action);
        let _ = env.exception_clear();
        outcome
    })();

    if let Err(e) = result {
        push_log(format!(
            "warning: could not {action} foreground service: {e}"
        ));
    }
}

#[cfg(target_os = "android")]
fn service_intent_jni(
    env: &mut jni::JNIEnv,
    context: *mut std::ffi::c_void,
    action: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    use jni::objects::JObject;

    let activity = unsafe { JObject::from_raw(context.cast()) };

    // new Intent() — framework class, visible to the system classloader
    let intent = env.new_object("android/content/Intent", "()V", &[])?;

    let pkg = env.new_string(crate::APP_PACKAGE)?;
    let cls = env.new_string("dev.solochain.node.DioxusForegroundService")?;
    env.call_method(
        &intent,
        "setClassName",
        "(Ljava/lang/String;Ljava/lang/String;)Landroid/content/Intent;",
        &[(&pkg).into(), (&cls).into()],
    )?;

    // NOTE the return types: Context.startForegroundService returns
    // ComponentName, but Context.stopService returns BOOLEAN. Getting
    // stopService's descriptor wrong makes GetMethodID throw
    // NoSuchMethodError (this was a fatal crash once).
    let (method, sig) = match action {
        "startForegroundService" => (
            "startForegroundService",
            "(Landroid/content/Intent;)Landroid/content/ComponentName;",
        ),
        _ => ("stopService", "(Landroid/content/Intent;)Z"),
    };
    // return value (ComponentName or boolean) — just ignore it
    env.call_method(&activity, method, sig, &[(&intent).into()])?;
    Ok(())
}

#[cfg(not(target_os = "android"))]
fn start_foreground_service() {}

#[cfg(not(target_os = "android"))]
fn stop_foreground_service() {}

// ---------------------------------------------------------------------------
// storage accounting
// ---------------------------------------------------------------------------

pub fn dir_size(path: &Path) -> u64 {
    let mut total = 0u64;
    if let Ok(rd) = std::fs::read_dir(path) {
        for entry in rd.flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                total += dir_size(&entry.path());
            } else {
                total += meta.len();
            }
        }
    }
    total
}

#[cfg(unix)]
pub fn free_bytes(path: &Path) -> u64 {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = CString::new(path.as_os_str().as_bytes()) else {
        return 0;
    };
    unsafe {
        let mut s: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(c.as_ptr(), &mut s) == 0 {
            (s.f_bavail as u64) * (s.f_bsize as u64)
        } else {
            0
        }
    }
}

#[cfg(not(unix))]
pub fn free_bytes(_path: &Path) -> u64 {
    0
}

/// Storage watcher: keeps the dashboard informed and enforces the budget.
pub fn spawn_storage_watcher() {
    spawn(async move {
        let dirs = dirs();
        let db = dirs.files_dir.join("chain");
        loop {
            let used = dir_size(&db);
            let free = free_bytes(&dirs.files_dir);
            {
                let mut sig = status();
                let mut st = sig.write();
                st.db_bytes = used;
                st.free_bytes = free;
            }
            if RUNNING.load(Ordering::SeqCst) && (used > MAX_DB_BYTES || free < MIN_FREE_BYTES) {
                push_log(format!(
                    "STORAGE LIMIT: chain db {} / free {} — stopping node",
                    fmt_bytes(used),
                    fmt_bytes(free)
                ));
                stop();
                status().write().error =
                    "storage limit reached — reset chain data to continue".into();
            }
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        }
    });
}

/// Uptime ticker (1 Hz).
pub fn spawn_uptime_ticker() {
    spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            // NOTE: read the value into a plain variable first — an `if let`
            // scrutinee temporary keeps the read guard alive for the whole
            // statement, which would conflict with the write below.
            let start = status().read().started_at;
            if let Some(start) = start {
                status().write().uptime_secs = now_secs().saturating_sub(start);
            }
        }
    });
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn fmt_bytes(b: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = b as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{b} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}
