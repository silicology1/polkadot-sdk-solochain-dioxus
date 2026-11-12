#!/usr/bin/env bash
# Post-build patch for the gradle project `dx build --platform android`
# generates under target/dx/<crate>/<profile>/android/app/.
#
# 1. Copies every .so from native/arm64-v8a/ into jniLibs/arm64-v8a/ — the
#    node binary (libsolochain-node.so, named lib*.so so the Android packager
#    keeps it) plus any shared runtime libs placed next to it
#    (libc++_shared.so is REQUIRED: cargo-ndk links the node against the
#    NDK's shared C++ runtime). The generated manifest has
#    extractNativeLibs="true", so the loader extracts everything to
#    nativeLibraryDir at install time, where execve is permitted and the
#    app sets LD_LIBRARY_PATH for the exec'd node.
# 2. Adds packaging { jniLibs { useLegacyPackaging = true } } — required by
#    AGP because the manifest sets extractNativeLibs="true", and required
#    by us so the node binary really lands on disk in nativeLibraryDir.
# 3. Disables native lib stripping so the 75 MB node binary isn't corrupted.
# 4. Kills leftover extractNativeLibs/useLegacyPackaging mismatch errors on
#    release builds.
#
# 5. Copies android/DioxusForegroundService.kt into the gradle module's
#    kotlin source dir (the manifest <service> entry dx generates from
#    Dioxus.toml [android].foreground_service_types references
#    dev.solochain.node.DioxusForegroundService, which must exist in dex).
#
# Run after `dx build --platform android` and before manual gradle runs:
#   ./patch-generated-project.sh [path-to-generated-android-app-dir]
#
# If no path is given, the newest target/dx/*/android is used.

set -euo pipefail
cd "$(dirname "$0")"

ROOT=""
if [[ $# -ge 1 ]]; then
  ROOT="$1"
else
  ROOT=$(ls -dt target/dx/*/release/android target/dx/*/debug/android 2>/dev/null | head -1 || true)
  if [[ -z "$ROOT" ]]; then
    echo "error: no generated android project found; run 'dx build --platform android' first" >&2
    exit 1
  fi
fi
echo "patching gradle project: $ROOT"

# dx nests the gradle project: .../android/app (root, has settings.gradle)
# with the :app module at .../android/app/app. Descend if given the android/ dir.
if [[ ! -f "$ROOT/settings.gradle" ]]; then
  ROOT="$ROOT/app"
fi
if [[ ! -f "$ROOT/settings.gradle" ]]; then
  echo "error: no settings.gradle under $ROOT — run 'dx build --platform android' first" >&2
  exit 1
fi

# Locate the :app module (the one with its own build.gradle.kts + src/).
MODULE=""
for CAND in "$ROOT/app" "$ROOT"; do
  if [[ -f "$CAND/build.gradle.kts" && -d "$CAND/src/main" ]]; then
    MODULE="$CAND"
    break
  fi
done
if [[ -z "$MODULE" ]]; then
  echo "error: could not locate the :app module under $ROOT" >&2
  exit 1
fi
echo "  module: $MODULE"

# 1. node binary + shared runtime libs -> jniLibs (arm64-v8a only — this
#    project targets arm64 phones)
SRC="native/arm64-v8a/libsolochain-node.so"
if [[ ! -f "$SRC" ]]; then
  echo "error: missing $SRC" >&2
  echo "       build the node with: cargo ndk -t arm64-v8a --platform 26 build --release -p solochain-template-node" >&2
  echo "       then copy the result here and rename it libsolochain-node.so" >&2
  exit 1
fi
ABI="arm64-v8a"
JNI="$MODULE/src/main/jniLibs/$ABI"
mkdir -p "$JNI"
# The node is exec'd directly, so everything it needs at runtime must sit
# in nativeLibraryDir. Copy the node binary plus every shared runtime lib
# placed alongside it in native/arm64-v8a/ (libc++_shared.so, etc.).
shopt -s nullglob
for SO in native/arm64-v8a/*.so; do
  cp -f "$SO" "$JNI/$(basename "$SO")"
  echo "  copied native lib -> $JNI/$(basename "$SO")"
done
shopt -u nullglob
if [[ ! -f "$JNI/libc++_shared.so" ]]; then
  echo "warning: no libc++_shared.so in $JNI" >&2
  echo "         the node binary links the NDK's shared C++ runtime and needs it" >&2
  echo "         at exec time (\"cannot locate symbol _ZTVNSt6__ndk1…\" otherwise)." >&2
  echo "         Copy it from your NDK once:" >&2
  echo "           cp \$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/linux-x86_64/sysroot/usr/lib/aarch64-linux-android/libc++_shared.so native/arm64-v8a/" >&2
  echo "         then re-run this script." >&2
fi

# 1b. the app library (libmain.so) is copied by dx itself, into the jniLibs
#     dir of the TARGET IT BUILT FOR. If dx was invoked without an explicit
#     --target, it defaults to the HOST architecture (x86_64 on a typical
#     laptop) — which our arm64-only abiFilters then excludes from the APK.
LIBMAIN="$MODULE/src/main/jniLibs/arm64-v8a/libmain.so"
if [[ ! -f "$LIBMAIN" ]]; then
  echo "error: missing $LIBMAIN" >&2
  echo "       dx built the app library for the wrong target (or didn't copy it)." >&2
  echo "       Re-run dx with an explicit arm64 target:" >&2
  echo "         dx build --platform android --release --target aarch64-linux-android" >&2
  echo "       (dx defaults to the HOST architecture when --target is omitted!)" >&2
  exit 1
fi
echo "  dx app library present -> $LIBMAIN"

# 2./3. gradle packaging options
GRADLE="$MODULE/build.gradle.kts"
if [[ -f "$GRADLE" ]] && ! grep -q "useLegacyPackaging" "$GRADLE"; then
  python3 - "$GRADLE" <<'PY'
import sys, re
path = sys.argv[1]
src = open(path).read()
packaging = '''    packaging {
        jniLibs {
            // extractNativeLibs="true" in the generated manifest requires this
            useLegacyPackaging = true
            // the node binary is a full ELF executable — never strip it
            keepDebugSymbols.add("**/*.so")
        }
    }
'''
abifilter = '        ndk { abiFilters += listOf("arm64-v8a") }\n'
# insert packaging right after `android {`, abiFilters inside defaultConfig
new = re.sub(r"(android\s*\{)", r"\1\n" + packaging, src, count=1)
new = re.sub(r"(defaultConfig\s*\{)", r"\1\n" + abifilter, new, count=1)
open(path, "w").write(new)
print('  patched packaging options + abiFilters "arm64-v8a" in', path)
PY
else
  echo "  gradle packaging options already present"
fi

# 5. Kotlin foreground service
KT_DIR="$MODULE/src/main/kotlin/dev/solochain/node"
mkdir -p "$KT_DIR"
cp -f android/DioxusForegroundService.kt "$KT_DIR/DioxusForegroundService.kt"
echo "  copied DioxusForegroundService.kt -> $KT_DIR"

echo "done. now run:"
echo "  cd $ROOT && ./gradlew assembleDebug"
