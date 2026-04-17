{inputs, ...}: {
  perSystem = {
    inputs',
    config,
    lib,
    pkgs,
    ...
  }: let
    toolchain = with inputs'.fenix.packages;
      combine [
        stable.rustc
        stable.cargo
        stable.clippy
        stable.rustfmt
      ];

    craneLib = (inputs.crane.mkLib pkgs).overrideToolchain toolchain;

    # Include all workspace crates and the proto files used by tonic-build.
    # synaptex-app is excluded from CI/Nix builds — it requires GTK/WebKit
    # headers and the Android NDK, both of which are only in the devShell.
    src = lib.fileset.toSource {
      root = ./../..;
      fileset = lib.fileset.unions [
        ../../Cargo.lock
        ../../Cargo.toml
        ../../crates/synaptex-types
        ../../crates/synaptex-tuya
        ../../crates/synaptex-bond
        ../../crates/synaptex-mysa
        ../../crates/synaptex-core
        ../../crates/synaptex-cli
        ../../crates/synaptex-router-proto
        ../../crates/synaptex-router
        ../../crates/synaptex-app
      ];
    };

    commonArgs = {
      inherit src;
      strictDeps = true;
      # protobuf provides protoc, which tonic-build (synaptex-router-proto)
      # calls at compile time to generate Rust code from .proto files.
      nativeBuildInputs = with pkgs; [
        pkg-config
        protobuf
      ];
    };

    # Build workspace deps once and reuse the artifacts across all packages.
    # synaptex-app is excluded so Dioxus / GTK / WebKit are never compiled.
    cargoArtifacts = craneLib.buildDepsOnly (commonArgs
      // {
        cargoExtraArgs = "--workspace --exclude synaptex-app";
      });
  in {
    # Expose crane primitives to the per-crate package modules via
    # _module.args so they don't have to rebuild the toolchain or deps.
    _module.args = {
      inherit craneLib commonArgs cargoArtifacts;
    };

    checks = {
      workspace-clippy = craneLib.cargoClippy (commonArgs
        // {
          inherit cargoArtifacts;
          cargoClippyExtraArgs = "--workspace --all-targets -- --deny warnings";
        });

      workspace-fmt = craneLib.cargoFmt {inherit src;};
    };

    packages.default = config.packages.synaptex-core;
  };
}
