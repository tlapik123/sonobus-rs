#!/usr/bin/env bash
# Installs everything `./gradlew assembleDebug` (in android/) needs, idempotently:
# Rust Android targets, cargo-ndk, Android SDK command-line tools, platform, build-tools, NDK,
# CMake (libopus is built from source by opusic-sys). Needs a JDK 17+ on PATH, curl, unzip.
#
# The SDK goes to $ANDROID_HOME (default: ~/android-sdk), never into the repository.
# Afterwards: export ANDROID_HOME=...; see android/README.md.
set -euo pipefail

ANDROID_HOME="${ANDROID_HOME:-$HOME/android-sdk}"
CMDLINE_TOOLS_BUILD=16111833        # cmdline-tools 20.0 (repository2-3.xml, Nov 2026)
PLATFORM="platforms;android-36"     # compile/target SDK 36: Play requirement since 2026-08-31
BUILD_TOOLS="build-tools;36.1.0"
NDK_VERSION=30.0.16248370           # NDK r30, the current LTS
CMAKE_VERSION=4.1.2
CARGO_NDK_VERSION=4.1.2
RUST_TARGETS=(aarch64-linux-android x86_64-linux-android)

# Rust tools come from mise when the repo uses it.
rs() { if command -v mise >/dev/null; then mise exec -- "$@"; else "$@"; fi; }

for t in curl unzip; do command -v $t >/dev/null || { echo "need $t" >&2; exit 1; }; done
java -version 2>&1 | head -1 | grep -Eq '"(1[7-9]|[2-9][0-9])' || { echo "need JDK 17 or newer on PATH" >&2; exit 1; }

rs rustup target add "${RUST_TARGETS[@]}"
if ! rs cargo ndk --version 2>/dev/null | grep -q "$CARGO_NDK_VERSION"; then
    rs cargo install cargo-ndk --version "$CARGO_NDK_VERSION" --locked
fi

SDKMANAGER="$ANDROID_HOME/cmdline-tools/latest/bin/sdkmanager"
if [ ! -x "$SDKMANAGER" ]; then
    tmp=$(mktemp -d)
    curl -fsSL -o "$tmp/tools.zip" \
        "https://dl.google.com/android/repository/commandlinetools-linux-${CMDLINE_TOOLS_BUILD}_latest.zip"
    unzip -q "$tmp/tools.zip" -d "$tmp"
    mkdir -p "$ANDROID_HOME/cmdline-tools"
    rm -rf "$ANDROID_HOME/cmdline-tools/latest"
    mv "$tmp/cmdline-tools" "$ANDROID_HOME/cmdline-tools/latest"
    rm -rf "$tmp"
fi

yes | "$SDKMANAGER" --sdk_root="$ANDROID_HOME" --licenses >/dev/null || true
"$SDKMANAGER" --sdk_root="$ANDROID_HOME" --install \
    "platform-tools" "$PLATFORM" "$BUILD_TOOLS" "ndk;$NDK_VERSION" "cmake;$CMAKE_VERSION" | grep -v '^\[' || true

echo
echo "Android toolchain ready. Use:"
echo "  export ANDROID_HOME=$ANDROID_HOME"
echo "  export ANDROID_NDK_HOME=$ANDROID_HOME/ndk/$NDK_VERSION"
echo "  export PATH=$ANDROID_HOME/cmake/$CMAKE_VERSION/bin:\$PATH"
