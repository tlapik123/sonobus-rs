#!/usr/bin/env bash
# Builds the plugin and lays out the CLAP and VST3 bundles for this OS under target/bundled/.
#   crates/sonobus-plugin/bundle.sh [--debug] [extra cargo args...]
# Linux:   SonoBus RS.clap (the .so), SonoBus RS.vst3/Contents/x86_64-linux/SonoBus RS.so
# Windows: SonoBus RS.clap (the .dll), SonoBus RS.vst3/Contents/x86_64-win/SonoBus RS.vst3
# macOS:   SonoBus RS.clap/ and SonoBus RS.vst3/ bundles (Contents/MacOS + Info.plist), unsigned
set -euo pipefail

root="$(cd "$(dirname "$0")/../.." && pwd)"
profile=release
if [[ "${1:-}" == "--debug" ]]; then profile=debug; shift; fi
flags=()
[[ $profile == release ]] && flags+=(--release)

cd "$root"
cargo build -p sonobus-plugin "${flags[@]}" "$@"

name="SonoBus RS"
out="$root/target/bundled"
target_dir="$(cargo metadata --format-version 1 --no-deps | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')"
bin_dir="$target_dir/$profile"
arch="$(uname -m)"
rm -rf "$out/$name.clap" "$out/$name.vst3"
mkdir -p "$out"

case "$(uname -s)" in
  Linux)
    lib="$bin_dir/libsonobus_plugin.so"
    cp "$lib" "$out/$name.clap"
    mkdir -p "$out/$name.vst3/Contents/$arch-linux"
    cp "$lib" "$out/$name.vst3/Contents/$arch-linux/$name.so"
    ;;
  MINGW*|MSYS*|CYGWIN*)
    lib="$bin_dir/sonobus_plugin.dll"
    cp "$lib" "$out/$name.clap"
    mkdir -p "$out/$name.vst3/Contents/$arch-win"
    cp "$lib" "$out/$name.vst3/Contents/$arch-win/$name.vst3"
    ;;
  Darwin)
    lib="$bin_dir/libsonobus_plugin.dylib"
    for ext in clap vst3; do
      b="$out/$name.$ext/Contents"
      mkdir -p "$b/MacOS"
      cp "$lib" "$b/MacOS/$name"
      printf 'BNDL????' > "$b/PkgInfo"
      cat > "$b/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleExecutable</key><string>$name</string>
<key>CFBundleIdentifier</key><string>rs.sonobus.sonobus.$ext</string>
<key>CFBundleName</key><string>$name</string>
<key>CFBundlePackageType</key><string>BNDL</string>
<key>CFBundleSignature</key><string>????</string>
<key>CFBundleVersion</key><string>1</string>
<key>NSMicrophoneUsageDescription</key><string>Audio from the host</string>
</dict></plist>
EOF
    done
    ;;
  *) echo "unsupported OS: $(uname -s)" >&2; exit 1 ;;
esac

echo "bundled into $out:"
ls -1 "$out"
