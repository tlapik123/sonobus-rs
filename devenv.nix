# Dev shell: `devenv shell`, then cargo / ./gradlew as in the READMEs.
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
  ] ++ guiLibs;

  env.LD_LIBRARY_PATH = lib.makeLibraryPath guiLibs;

  languages.rust = {
    enable = true;
    channel = "stable";
    targets = [ "aarch64-linux-android" ];
  };

  android = {
    enable = true;
    # The only two versions Gradle insists on; the SDK is read-only, so it cannot fetch them itself.
    buildTools.version = [ "36.1.0" ]; # the one the Android Gradle plugin asks for
    ndk.version = [ "30.0.16248370" ]; # ndkVer in android/app/build.gradle.kts
    # Not needed for a build, and large.
    emulator.enable = false;
    systemImages.enable = false;
    googleAPIs.enable = false;
    googleTVAddOns.enable = false;
    extras = [ ];
    android-studio.enable = false;
  };
}
