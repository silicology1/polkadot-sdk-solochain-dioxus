# Solochain Node — Android Dioxus Dashboard

An Android APK (Dioxus 0.7 + Tailwind) that bundles a prebuilt polkadot-sdk
solochain node binary, runs it in the background, and shows live running-status
analytics over its JSON-RPC WebSocket.

```
┌─────────────────────────────────────────────┐
│ APK                                         │
│  ├─ Dioxus app (Rust, in-process)           │
│  │   ├─ Tailwind dashboard (live signals)   │
│  │   ├─ RPC client → ws://127.0.0.1:9944    │
│  │   └─ Node runner: exec libsolochain-node.so│
│  ├─ jniLibs/arm64-v8a/                      │  ← node binary (aarch64)
│  │   ├─ libsolochain-node.so                │    + libc++_shared.so (NDK C++ runtime)
│  └─ DioxusForegroundService (Kotlin)        │  ← keeps it alive in background
└─────────────────────────────────────────────┘
```

## How the node runs

- The node binary is packaged as `jniLibs/arm64-v8a/libsolochain-node.so`,
  together with `libc++_shared.so` (cargo-ndk links the node against the
  NDK's shared C++ runtime — it must be bundled next to it).
  The generated manifest uses `extractNativeLibs="true"`, so Android extracts
  them to `nativeLibraryDir` at install time — the one location an app may
  `execve` from, even on Android 10+. The node runner also sets
  `LD_LIBRARY_PATH` to that directory so the exec'd binary resolves its
  shared dependencies.
- On startup the app spawns the node with:
  `--dev --base-path {files_dir}/chain --rpc-port 9944 --db-cache 256 --rpc-cors all`
  plus, on Android only, `--wasmtime-instantiation-strategy recreate-instance`
  (wasmtime's default memory pool pre-mmaps ~414 GB of virtual address space,
  which Android's per-app VA limits reject)
  (`filesDir` resolved via JNI from the app context).
- `DioxusForegroundService` (type `dataSync`) pins the process group so the
  node keeps running while the app is backgrounded. Persistent notification,
  `START_NOT_STICKY` (no lying notification after a real kill).
- Storage guard rails: the dashboard shows chain-DB size and device free
  space; the node is auto-stopped if the DB exceeds **5 GiB** or free space
  drops below **1 GiB** (constants in `app/src/node.rs`).
- Logs are an in-memory 300-line ring buffer only — never written to disk.

## Project layout

```
Dioxus.toml                 # app + android config (min_sdk 26, foreground service)
Cargo.toml                  # workspace (release profile tuned for size)
app/                        # the Dioxus app crate
  src/main.rs               #   UI: Tailwind dashboard, cards, controls, logs
  src/node.rs               #   node lifecycle, JNI dirs, storage watcher, service
  src/rpc.rs                #   JSON-RPC over WS: name/version/chain, head subs, health
  tailwind.css              #   Tailwind v4 input (dx generates assets/tailwind.css)
  assets/tailwind.css       #   generated output (committed so cargo builds offline)
  package.json              #   tailwindcss pinned for local regen
android/
  DioxusForegroundService.kt  # copied into the gradle module by the patch script
native/arm64-v8a/libsolochain-node.so   # your aarch64 binary (from cargo ndk)
node/solochain-template-node            # host (desktop) binary for desktop dev
patch-generated-project.sh  # jniLibs + kotlin service + gradle packaging fixes
```

## Desktop (quick test on Linux)

Requires the usual GTK/webkit dev packages (gtk3, webkit2gtk-4.1, ...).

```bash
dx serve --platform desktop
```

The host node binary is picked up from `node/solochain-template-node`
(override with `SOLOCHAIN_NODE_BIN=/path/to/node`). Chain data lands in
`chaindata/`.

## Android device (physical phone)

arm64-v8a only — every real phone is arm64. See
[docs/PHONE.md](docs/PHONE.md) for the device setup (USB debugging,
install, live logcat) and troubleshooting.

## Android APK build

Prereqs (on your build machine):

```bash
rustup target add aarch64-linux-android
# ANDROID_HOME, NDK_HOME (or ANDROID_NDK_ROOT), JAVA_HOME (17+) set
dx --help   # dioxus 0.7.x
```

Build:

```bash
# 1. native libs + gradle project + APK (dx runs gradle assembleDebug itself)
#    --target is required: dx otherwise defaults to the HOST architecture
#    (x86_64 on a typical laptop), which our arm64-only abiFilters excludes
dx build --platform android --release --target aarch64-linux-android

# 2. patch the generated project: add the node binary + libc++_shared.so
#    to jniLibs, the foreground-service kotlin class, and gradle packaging
#    options (fails loudly if native/arm64-v8a/ is incomplete)
./patch-generated-project.sh

# 3. rebuild the APK with the patch applied
APKDIR=$(ls -d target/dx/*/release/android/app)
cd "$APKDIR" && ./gradlew assembleDebug
# → app/build/outputs/apk/debug/app-debug.apk

# 4. sanity-check — all three must be listed:
unzip -l app/build/outputs/apk/debug/app-debug.apk | grep '\.so'
#   lib/arm64-v8a/libmain.so
#   lib/arm64-v8a/libsolochain-node.so
#   lib/arm64-v8a/libc++_shared.so
```

One-time native-binary prerequisites go into `native/arm64-v8a/` — the
node binary from `cargo ndk` **and** the NDK's `libc++_shared.so` (see
[docs/PHONE.md](docs/PHONE.md) step 2 for the exact commands).

Install & run:

```bash
adb install -r "$APKDIR/app/build/outputs/apk/debug/app-debug.apk"
adb logcat -s dioxus node Node
```

On launch the app auto-starts the node; the dashboard shows:

- header: health dot, node name/version/chain, phase badge
- cards: best block (live), finalized height, peers, uptime, chain-DB size,
  RPC endpoint
- controls: Start / Stop / Restart / Reset chain data
- log tail (ring buffer) and storage banners

Notes:

- `--dev` seals a block per transaction but the dev chain still emits heads
  periodically, so height advances even without txs.
- The APK is ~100 MB (the node binary is 76 MB, plus libc++_shared.so and
  the app library). Fine for sideloading.
- **ABI**: the build targets `arm64-v8a` only. The patch script injects
  `ndk { abiFilters += listOf("arm64-v8a") }` into the generated gradle
  config, so no other ABI is ever packaged.
- If `dx` regenerates the gradle project, re-run `patch-generated-project.sh`
  before the final gradle invocation.
- Rebuilding the node binary? Refresh `native/arm64-v8a/libsolochain-node.so`
  (and `node/solochain-template-node` for desktop) and re-run the steps.

## Regenerating Tailwind CSS (optional)

dx runs tailwind automatically during `dx build`/`dx serve` (it detects
`tailwind.css` and fetches the tailwindcss v4 binary). To regenerate by hand:

```bash
cd app && npx tailwindcss --input tailwind.css --output assets/tailwind.css
```

The generated `assets/tailwind.css` is committed so plain `cargo` builds
resolve the `asset!` offline.

## Storage / RAM expectations

- Chain DB starts around 10–50 MB and grows a few KB per block; the 5 GiB cap
  is a hard safety stop, with a warning banner from 75%.
- The node uses ~300–600 MB RAM; `--db-cache 256` bounds RocksDB memory.
- If a device gets tight, use **Reset chain data** (wipes `files/chain` and
  restarts with a fresh genesis).
