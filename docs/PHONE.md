# Testing on a physical Android phone (arm64-v8a)

This project targets **arm64-v8a only** — every real phone is arm64, and the
bundled node binary (`native/arm64-v8a/libsolochain-node.so`) is built with
`cargo ndk -t arm64-v8a`. There is no emulator/x86_64 variant.

## 1. Prepare the phone

1. **Settings → About phone** → tap **Build number** 7× to unlock
   Developer options.
2. **Settings → Developer options**:
   - enable **USB debugging**;
   - set **Default USB configuration → File transfer (MTP)** — on
     ColorOS/OPPO, ADB usually only binds when the USB mode is MTP
     ("Charging only" silently hides the ADB interface);
   - (optional, recommended) enable **Stay awake** while charging;
   - if installs get rejected, also enable **Install via USB**.
3. Plug the phone into the build machine via USB and accept the
   "Allow USB debugging?" prompt (tick *Always allow*).

Verify the connection and that the device is arm64:

```bash
adb devices                       # must list your phone as "device"
adb shell getprop ro.product.cpu.abi   # → arm64-v8a
```

## 2. Build the APK

On the machine with the polkadot-sdk checkout (one-time):

## Compile to android

[PolkadotSDK Solochain](https://github.com/paritytech/polkadot-sdk/tree/master/templates/solochain)

```bash
git clone https://github.com/paritytech/polkadot-sdk-solochain-template.git solochain-template

cd solochain-template
```

solochain-template/rust-toolchain.toml

```toml
[toolchain]
channel = "1.88.0"
components = [
	"cargo",
	"clippy",
	"rust-analyzer",
	"rust-src",
	"rust-std",
	"rustc",
	"rustc-dev",
	"rustfmt",
]
targets = ["wasm32-unknown-unknown", "aarch64-linux-android"]
profile = "minimal"
```
```bash
cargo build --release
```

### Wastime Patch

The template pulls everything from crates.io (`sc-executor = "0.42.0"`), so wasmtime only arrives as a transitive dependency of `sc-executor`. To change it, you override that transitive crate with `[patch.crates-io]`. You don't need to clone the whole polkadot-sdk repo.

**1. Find the exact version in your lockfile**

```bash
grep -A1 'name = "sc-executor-wasmtime"' Cargo.lock
```

**2. Copy that exact crate source into your project**

```bash
mkdir -p patches
cp -r ~/.cargo/registry/src/*/sc-executor-wasmtime-<VERSION> patches/sc-executor-wasmtime
```

If it isn't there yet, run `cargo fetch` first. Using the copy from your own registry guarantees the version matches what the template expects.

**3. Edit `patches/sc-executor-wasmtime/src/runtime.rs`**

Search for `Pooling` or `PoolingAllocationConfig`. That's where `max_memory_size` and the pool reservation are set. Make the code never take the pooling branch, so the config always uses the default on-demand strategy:

```rust
// force on-demand allocation
config.allocation_strategy(wasmtime::InstanceAllocationStrategy::OnDemand);
```

Remove or bypass the block that builds the pooling config. The exact lines vary by version, so I can't give you a precise diff without seeing the file. Paste the surrounding block if you want me to rewrite it.

**4. Add the patch to the root `Cargo.toml`**

```toml
[patch.crates-io]
sc-executor-wasmtime = { path = "patches/sc-executor-wasmtime" }
```


### Android release

```bash
cargo install cargo-ndk
cargo ndk -t arm64-v8a --platform 26 build --release -p solochain-template-node
```

These two libc functions (used by the if_addrs and network_interface crates for libp2p/mDNS) only exist in Android API level 24 and higher. cargo-ndk defaults to API 21, so the linker can’t find them. So use API level 26. 


```bash
cargo install cargo-ndk
cargo ndk -t arm64-v8a --platform 26 build --release -p solochain-template-node
cp target/aarch64-linux-android/release/solochain-template-node \
   /path/to/solochain-dioxus/native/arm64-v8a/libsolochain-node.so
```

Also copy the NDK's shared C++ runtime next to it — cargo-ndk links the
node against `libc++_shared.so` (`DT_NEEDED`), and an exec'd binary doesn't
get the app's library path automatically (the app sets `LD_LIBRARY_PATH`,
but the file must be bundled to be found):

```bash
# if ANDROID_NDK_HOME is set:
cp "$ANDROID_NDK_HOME"/toolchains/llvm/prebuilt/linux-x86_64/sysroot/usr/lib/aarch64-linux-android/libc++_shared.so \
   native/arm64-v8a/

# otherwise locate it inside the SDK:
cp "$(find ~/Android/Sdk -name libc++_shared.so -path '*aarch64*' 2>/dev/null | head -1)" \
   native/arm64-v8a/

# verify — native/arm64-v8a/ must contain BOTH files:
ls -la native/arm64-v8a/
#   libsolochain-node.so (~76 MB)
#   libc++_shared.so     (~2 MB)
```

The patch script copies every `.so` under `native/arm64-v8a/` into the APK
and fails loudly (with the exact `cp` command) if `libc++_shared.so` is
missing — do not ignore that warning; the node exec fails on-device with
`CANNOT LINK EXECUTABLE … cannot locate symbol _ZTVNSt6__ndk1…` without it.

Then, in the project:

```bash
# 1. native libs + gradle project (+ first gradle attempt)
#    NOTE: --target is REQUIRED. Without it dx builds for the HOST
#    architecture (x86_64 on a typical laptop) and the APK ends up with
#    no arm64 app library. patch-generated-project.sh fails loudly in
#    that case.
dx build --platform android --release --target aarch64-linux-android

# 2. patch the generated project — MUST be re-run after every `dx build`
#    (dx wipes the jniLibs directory on each build)
./patch-generated-project.sh

# 3. rebuild the APK with the node binary and patches applied
APKDIR=$(ls -d target/dx/*/release/android/app)
cd "$APKDIR" && ./gradlew assembleDebug

# 4. sanity-check: the APK must contain all three native libs
unzip -l app/build/outputs/apk/debug/app-debug.apk | grep '\.so'
#   expect exactly:
#     lib/arm64-v8a/libmain.so          (the Dioxus app library)
#     lib/arm64-v8a/libsolochain-node.so (~76 MB, the node)
#     lib/arm64-v8a/libc++_shared.so     (NDK C++ runtime for the node)
#   If libsolochain-node.so is missing from the listing, run
#   `./gradlew clean assembleDebug` and check again.
```

## 3. Install & watch logs

```bash
adb install -r app/build/outputs/apk/debug/app-debug.apk
adb logcat -s dioxus node Node    # live app + node logs
```

Launch the app from the launcher. Expected logcat flow:

1. `node started` — the bundled node spawned from `nativeLibraryDir`
2. Substrate banner (`Substrate Node`, `version 0.1.0-…`, `Chain specification: Development`)
3. `Running JSON-RPC server: addr=127.0.0.1:9944`
4. `rpc connected` — the app's dashboard client attached

The dashboard then shows live best block / finalized height, peers,
uptime, chain-DB size, and the log tail.

## 4. Installing without ADB (manual sideload)

If ADB won't cooperate, you can sideload completely without it — the app is
self-contained (the node runs inside it), so ADB is not needed at runtime.
Trade-off: no `adb logcat`, so diagnose via the in-app dashboard/log tail.

1. Build the APK (section 2) and run the `unzip -l` sanity check — do not
   transfer anything until all **three** `lib/arm64-v8a/...` files are
   listed (`libmain.so`, `libsolochain-node.so`, `libc++_shared.so`).
2. Copy the APK to the phone:
   - USB: set the USB mode to **File transfer (MTP)** (works even when ADB
     is broken — MTP and ADB are independent interfaces), then drag
     `app-debug.apk` into e.g. `Download/`.
   - Alternatively: Bluetooth, Telegram/WhatsApp *saved messages*, a cloud
     drive, or an SD card.
3. On the phone, open the file manager ("Files" on OPPO) and tap the APK.
   First time: allow "install unknown apps" for that file manager.
   ColorOS/Play Protect may show a security warning — choose *install anyway*.
4. Launch and verify in-app: the log tail should show the Substrate banner →
   `Running JSON-RPC server: addr=127.0.0.1:9944` → `rpc connected`, and
   best block / finalized should start ticking. The persistent foreground
   notification appears while the node runs.

## 5. ADB connection troubleshooting

Run this first — it decides which branch you're in:

```bash
lsusb | grep -i -E "oppo|oneplus|xiaomi|android|18d1"
```

- **Phone listed** (e.g. `ID 22d9:… OPPO Electronics Corp.`) → kernel sees
  it; the problem is ADB-mode or permissions. → A then B.
- **Not listed** → cable/port issue: use a *data* cable (not charge-only),
  try another USB port, switch the USB mode to File transfer (MTP).

**A. Make the phone actually expose ADB** (ColorOS/OPPO quirks — the
Developer-options toggle alone is often not enough):

1. Toggle **USB debugging** off, wait a few seconds, back on.
2. Switch the USB mode from the notification shade to **File
   transfer (MTP)** — ADB usually binds only after the mode switch, not on
   plug-in.
3. Accept any "USB debugging connected" / authorization popup.
4. Enable **USB debugging (Security settings)** in Developer options — some
   OPPO/Realme units won't enumerate ADB without it.
5. Replug. Check the interface appeared:
   ```bash
   lsusb -v -d 22d9:2769 2>/dev/null \
     | grep -iE 'bInterfaceClass|bInterfaceSubClass|bInterfaceProtocol'
   # ADB signature: bInterfaceClass 255, bInterfaceSubClass 66,
   #                bInterfaceProtocol 1
   ```
   (Replace `22d9:2769` with your own vendor:product from `lsusb`.)

**B. udev permissions + ADB server restart** (Linux):

```bash
echo 'SUBSYSTEM=="usb", ATTR{idVendor}=="22d9", MODE="0666", GROUP="plugdev"' \
  | sudo tee /etc/udev/rules.d/51-android.rules
sudo chmod a+r /etc/udev/rules.d/51-android.rules
sudo udevadm control --reload-rules && sudo udevadm trigger

adb kill-server
adb devices     # watch the phone screen for the RSA "Allow USB debugging?" prompt
```

(Use your phone's vendor ID instead of `22d9` if it isn't an OPPO.)

If the device shows but as `unauthorized`, accept the prompt on the phone or
revoke & re-grant in Developer options. If nothing works, check
`adb version` — distro ADB packages are sometimes ancient; the
SDK's `platform-tools` is the reliable one.

**Wireless fallback (no USB at all):** Android 11+ supports wireless
debugging — Developer options → *Wireless debugging* → *Pair device with
pairing code*, then `adb pair <ip:port>` (enter the 6-digit code) and
`adb connect <ip:port>` from the main Wireless debugging screen. Phone and
laptop must be on the same WiFi network. The debugging workflow in
section 6 is identical either way.

## 6. Debugging errors & crashes

### The standard loop (USB or wireless ADB)

```bash
# clean the log buffer, then launch the app on the phone
adb logcat -c

# watch live while you open the app / press buttons:
adb logcat AndroidRuntime:E DEBUG:E dioxus:V node:V *:S
#   AndroidRuntime:E → uncaught Java/Kotlin exceptions (crash dialogs)
#   DEBUG:E          → native crashes (SIGSEGV/SIGABRT, tombstone headers)
#   dioxus / node    → our app's own log lines

# or capture everything and inspect afterwards:
adb logcat -c            # clear
# ... reproduce the crash ...
adb logcat -d > /tmp/crash.log
grep -A25 -E "FATAL EXCEPTION|SIGSEGV|SIGABRT|UnsatisfiedLink|tombstone" /tmp/crash.log
```

To reinstall the current build without losing chain data (updates only):

```bash
adb install -r app/app/build/outputs/apk/debug/app-debug.apk
```

### What the traces mean

| Trace | Meaning / fix |
|---|---|
| `CANNOT LINK EXECUTABLE … cannot locate symbol _ZTVNSt6__ndk1…` | The node binary needs `libc++_shared.so` (cargo-ndk links the NDK shared C++ runtime) and it isn't bundled: copy it from the NDK into `native/arm64-v8a/` (see step 2), re-run the patch script, rebuild the APK |
| `wasm call error … failed to create memory pool mapping: mmap failed to allocate 0x… bytes: Out of memory (os error 12)` (node exits right after `Database: RocksDb at …` prints) | wasmtime's default instantiation strategy (`pooling-copy-on-write`) pre-mmaps ~414 GB of *virtual* address space for its memory pool — desktop Linux allows it, Android's per-app VA limits don't. The node runner passes `--wasmtime-instantiation-strategy recreate-instance` on Android, which allocates Wasm instance memory on demand instead. **First check the dashboard log's `args:` line** — it must contain that flag; if it's missing, the APK was built from stale app code (re-sync `app/src/node.rs` and rebuild) |
| `NoSuchMethodError: no non-static method ...stopService(...)` | JNI signature bug in an older build (stopService returns boolean, not ComponentName) — fixed in current code; rebuild + `adb install -r`. Any JNI failure is now logged instead of crashing the app |
| `UnsatisfiedLinkError: ... libmain.so` | The APK's app library is missing or wrong-ABI — re-verify the APK with `unzip -l ... \| grep '\.so'` (all three arm64 libs required); if rebuilding, always pass `--target aarch64-linux-android` to dx |
| `FATAL EXCEPTION` + Java/Kotlin stack | App-side problem — the topmost frames name the component (activity, service, JNI) |
| `signal 11 (SIGSEGV)` / `SIGABRT` with a `#00 ... #xx` native backtrace | Rust-side panic or C-level fault — the frames under `DEBUG` (tag) show which Rust crate; often a `dioxus` or `ndk_context` frame |
| App closes with no crash trace in logcat | The process was killed by the system (low memory / OEM battery manager) — exclude the app from battery optimization in ColorOS settings |

### Installing without ADB? Get traces from the phone itself

Developer options → **Generate bug report** → share the resulting `.zip`.
It contains the full logcat and tombstones; look inside
`dumpstate/` / `FS/data/tombstones/` for `FATAL EXCEPTION`,
`UnsatisfiedLinkError`, or `SIGSEGV` frames.

## 7. What runs where (important)

- The node runs **inside the app** as an exec'd child process — nothing
  else is needed; no adb reverse, no host node.
- The foreground service (`dataSync`) keeps the node alive while the app
  is backgrounded; a persistent notification appears while running.
- Storage guard rails: the node is stopped automatically if the chain DB
  reaches 5 GiB or free space drops below 1 GiB.
- Logs are kept in an in-memory ring buffer only (300 lines) — nothing is
  written to disk besides the chain data itself.

## Troubleshooting

| Symptom | Cause / fix |
|---|---|
| `adb devices` shows `unauthorized` | Accept the USB-debugging prompt on the phone; revoke & re-grant in Developer options if stuck |
| `adb: no devices` | Wrong USB mode — set USB connection to *File transfer (MTP)*, or re-plug; some OEMs need "USB debugging (Security settings)" too |
| `INSTALL_FAILED_OLDER_SDK` | Phone runs Android < 8.0 (min_sdk is 26) — a newer device is required |
| `INSTALL_FAILED_INSUFFICIENT_STORAGE` | ~200 MB free needed (APK + extracted libs + fresh dev chain); uninstall the old build first (`adb uninstall dev.solochain.node`) |
| `Node binary not found at /data/app/.../lib/arm64/libsolochain-node.so` | The APK has no node binary: re-run `./patch-generated-project.sh` **after** `dx build` and **before** `./gradlew assembleDebug`, then verify with the `unzip -l` command above |
| APK contains only `lib/arm64-v8a/libsolochain-node.so`, no `libmain.so` | dx built the app lib for the wrong ABI: dx defaults to the **host architecture** when `--target` is omitted (x86_64 on a laptop). Re-run `dx build --platform android --release --target aarch64-linux-android`, then patch + gradle again. The patch script now fails loudly with this exact hint |
| `unzip` lists `libmain.so` but not `libsolochain-node.so` (even though `jniLibs/arm64-v8a/` contains it) | Gradle's incremental packaging skipped the merge after a mid-build jniLibs change — `./gradlew clean assembleDebug`, then re-verify the listing |
| APK contains `lib/x86_64/libmain.so` (and no arm64 libs) | A stale build from the removed emulator flow is being reused: delete `docs/EMULATOR.md` if still present, confirm `grep -rn "x86_64\|emulator" Dioxus.toml app/Cargo.toml patch-generated-project.sh` prints nothing, then `rm -rf target/dx` and rebuild **with** `--target aarch64-linux-android` |
| App opens, then shows "database locked" | A previous run's node is still holding the DB — fully stop the app (swipe away recents) and relaunch |
| `installx86incompatible` / wrong-ABI error | You're installing an arm64 APK on a non-arm64 device — `adb shell getprop ro.product.cpu.abi` must say `arm64-v8a` |
| `adb` not found | Install platform-tools: `sdkmanager "platform-tools"` or your distro's `android-platform-tools` package |
