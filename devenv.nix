# Dev shell: `devenv shell`, then cargo / ./gradlew as in the READMEs.
# Android versions must match android/app/build.gradle.kts and android/setup-toolchain.sh.
{ pkgs, lib, ... }:
let
  # Loaded with dlopen by winit/glutin (app) and baseview (plugin editor), so not found by the linker.
  guiLibs = with pkgs; [
    libGL
    libxkbcommon
    wayland
    fontconfig
    libx11
    libxcursor
    libxi
    libxrandr
    libxcb
  ];
in
{
  packages = with pkgs; [
    pkg-config
    libopus # aoo links the system libopus (opusic-sys)
    alsa-lib # cpal's default Linux backend
    cmake # opus-bundled (Android) builds libopus from source
    cargo-ndk
    xvfb-run # headless UI: `xvfb-run cargo run`, plugin validators with the editor
  ] ++ guiLibs;

  env.LD_LIBRARY_PATH = lib.makeLibraryPath guiLibs;

  languages.rust = {
    enable = true;
    channel = "stable";
    targets = [ "aarch64-linux-android" "x86_64-linux-android" ];
  };

  android = {
    enable = true;
    platforms.version = [ "36" ];
    buildTools.version = [ "36.1.0" ];
    ndk.enable = true;
    cmake.version = [ "4.1.2" ];
    emulator.enable = false;
    systemImages.enable = false;
    sources.enable = false;
    googleAPIs.enable = false;
    googleTVAddOns.enable = false;
    extras = [ ];
    android-studio.enable = false;
  };
}
