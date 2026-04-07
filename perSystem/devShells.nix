top@{ inputs, ... }: {
  perSystem = {
    config,
    pkgs,
    inputs',
    system,
    ...
  }: let
    # Separate nixpkgs instance with allowUnfree for the Android SDK (unfree license).
    unfreePkgs = import inputs.nixpkgs {
      inherit system;
      config.allowUnfree = true;
      config.android_sdk.accept_license = true;
    };

    toolchain = with inputs'.fenix.packages;
      combine [
        stable.rustc
        stable.cargo
        stable.clippy
        stable.rustfmt
        stable.rust-analyzer
        # wasm32 target required for dx build --platform web/desktop
        targets.wasm32-unknown-unknown.stable.rust-std
        # Android targets — aarch64 for physical devices, x86_64 for emulator
        targets.aarch64-linux-android.stable.rust-std
        targets.x86_64-linux-android.stable.rust-std
      ];

    android = unfreePkgs.androidenv.composeAndroidPackages {
      includeNDK         = true;
      ndkVersions        = [ "25.2.9519653" ];
      buildToolsVersions = [ "34.0.0" ];
      platformVersions   = [ "34" ];
      abiVersions        = [ "arm64-v8a" "x86_64" ];
    };

    ndkRoot = "${android.androidsdk}/libexec/android-sdk/ndk/25.2.9519653";
  in {
    devShells.default = with pkgs;
      mkShell {
        packages = [
          toolchain
          cmake
          pkg-config
          openssl
          zlib
          # wry/Dioxus desktop WebView deps (also required when cross-compiling
          # the workspace on the host for cargo check / synaptex-core builds)
          glib
          cairo
          pango
          gdk-pixbuf
          gtk3
          webkitgtk_4_1
          libsoup_3
          xdotool    # libxdo — required by tao/wry desktop window management
          protobuf   # protoc required by tonic-build
          dioxus-cli # dx 0.7.4 — pin synaptex-app to dioxus =0.7.4 to match
          android.androidsdk
          unfreePkgs.jdk17
          config.treefmt.build.wrapper
        ];

        shellHook = ''
          export LD_LIBRARY_PATH="${pkgs.lib.makeLibraryPath [
            pkgs.xdotool
            pkgs.gtk3
            pkgs.webkitgtk_4_1
            pkgs.glib
            pkgs.cairo
            pkgs.pango
            pkgs.gdk-pixbuf
            pkgs.libsoup_3
            pkgs.openssl
            pkgs.zlib
          ]}:''${LD_LIBRARY_PATH:-}"

          export ANDROID_HOME="${android.androidsdk}/libexec/android-sdk"
          export ANDROID_NDK_HOME="${ndkRoot}"
          export ANDROID_NDK_ROOT="${ndkRoot}"

          # AGP downloads its own AAPT2 from Maven — a pre-built ELF that won't start
          # on NixOS. Point it at the Nix-patched binary from the SDK instead.
          # We also clear any cached AAPT2 transforms the first time we set the
          # property, so Gradle re-runs them with the override in effect.
          _gradle_props="''${GRADLE_USER_HOME:-$HOME/.gradle}/gradle.properties"
          mkdir -p "$(dirname "$_gradle_props")"
          if ! grep -qF "aapt2FromMavenOverride" "$_gradle_props" 2>/dev/null; then
            echo "android.aapt2FromMavenOverride=$ANDROID_HOME/build-tools/34.0.0/aapt2" \
              >> "$_gradle_props"
            # Clear stale cached transforms so they get regenerated with the override.
            rm -rf "''${GRADLE_USER_HOME:-$HOME/.gradle}"/caches/*/transforms/*/transformed/aapt2-* \
              2>/dev/null || true
          fi
        '';
      };
  };
}
