# SPDX-License-Identifier: AGPL-3.0-or-later
# Native binaries and the production SPA.
{
  pkgs,
  lib,
  crane,
  src,
}:

let
  toolchain = pkgs.rust-bin.fromRustupToolchainFile (src + "/rust-toolchain.toml");
  craneLib = (crane.mkLib pkgs).overrideToolchain toolchain;

  version = (lib.importTOML (src + "/Cargo.toml")).workspace.package.version;

  cargoVendorDir = craneLib.vendorCargoDeps { cargoLock = src + "/Cargo.lock"; };

  # Only what the builds read; documentation, the NixOS module and the deploy scripts stay out.
  source = lib.fileset.toSource {
    root = src;
    fileset = lib.fileset.unions [
      (src + "/Cargo.toml")
      (src + "/Cargo.lock")
      (src + "/crates")
      (src + "/xtask")
      (src + "/data")
      (src + "/fixtures/manifest.toml")
      (src + "/fixtures/pattern")
      (src + "/site")
      (src + "/tools/build_products.py")
      (src + "/rust-toolchain.toml")
      (src + "/.cargo/config.toml")
      (src + "/flake.nix")
      (src + "/flake.lock")
    ];
  };

  commonArgs = {
    src = source;
    inherit version cargoVendorDir;
    pname = "looklab";
    doCheck = false;
    cargoLock = src + "/Cargo.lock";
    nativeBuildInputs = [ pkgs.pkg-config ];
    buildInputs = [
      pkgs.openssl
      pkgs.sqlite
    ];
  };

  # crane's `cargoWithProfile` ignores `cargoBuildFlags`; package selection goes through this.
  packagesArg = "-p looklab-server -p looklab-worker -p looklab-cli";

  # Dependencies for the three native binaries, shared by their derivations.
  cargoArtifacts = craneLib.buildDepsOnly (
    commonArgs
    // {
      pname = "looklab-native-deps";
      cargoBuildExtraArgs = packagesArg;
      cargoCheckExtraArgs = packagesArg;
    }
  );

  nativePackage =
    { name, binary }:
    craneLib.buildPackage (
      commonArgs
      // {
        pname = name;
        inherit cargoArtifacts;
        cargoBuildExtraArgs = "-p ${name}";
        installPhaseCommand = ''
          mkdir -p $out/bin
          install -m755 $postBuildInstallFromCargoBuildLogOut/bin/${binary} $out/bin/
        '';
      }
    );

  # `site/scripts/build-site.sh` runs the wasm-assets hook and cargo-leptos, so the
  # module policy checks and the lazy route chunks stay in the packaged build.
  site = craneLib.mkCargoDerivation (
    commonArgs
    // {
      pname = "looklab-site";
      cargoArtifacts = null;
      doInstallCargoArtifacts = false;
      nativeBuildInputs = commonArgs.nativeBuildInputs ++ [
        pkgs.cargo-leptos
        pkgs.wasm-bindgen-cli
        pkgs.binaryen
        pkgs.wasm-tools
        pkgs.python3
        pkgs.git
      ];
      env = {
        CARGO_NET_OFFLINE = "true";
      };
      preBuild = ''
        export LOOKLAB_UI_TARGET_DIR=$TMPDIR/ui-target
        export LOOKLAB_BUILD_ROOT=$TMPDIR/looklab-build
        unset LOOKLAB_PREVIEW LOOKLAB_API_BASE
      '';
      buildPhaseCargoCommand = "sh site/scripts/build-site.sh --release --dist $out/dist";
      installPhaseCommand = "true";
    }
  );

  server = nativePackage {
    name = "looklab-server";
    binary = "looklab-server";
  };

  manifest = lib.importTOML (src + "/fixtures/manifest.toml");

  # The six photographic preview scenes `prepare_scenes` reads.
  photoSceneFixtures = builtins.filter (fixture: lib.hasPrefix "scene-" fixture.file) manifest.fixture;

  photoSceneSources = pkgs.linkFarm "looklab-scene-sources" (
    map (fixture: {
      name = fixture.file;
      path = pkgs.fetchurl {
        name = fixture.file;
        url = fixture.source_url;
        sha256 = fixture.sha256;
      };
    }) photoSceneFixtures
  );

  # `prepare_scenes` is an example behind the native-only `photo-scenes` feature.
  scenesTool = craneLib.buildPackage (
    commonArgs
    // {
      pname = "looklab-scenes-prepare";
      inherit cargoArtifacts;
      cargoBuildExtraArgs = "-p looklab-publish --features photo-scenes --example prepare_scenes";
      installPhaseCommand = ''
        mkdir -p $out/bin
        install -m755 target/release/examples/prepare_scenes $out/bin/
      '';
    }
  );

  scenes = pkgs.runCommand "looklab-scenes" { nativeBuildInputs = [ scenesTool ]; } ''
    prepare_scenes \
      --manifest ${source}/fixtures/manifest.toml \
      --cache ${photoSceneSources} \
      --out $out
  '';
in
{
  packages = {
    looklab-server = server;
    looklab-worker = nativePackage {
      name = "looklab-worker";
      binary = "looklab-worker";
    };
    looklab-cli = nativePackage {
      name = "looklab-cli";
      binary = "looklab";
    };
    looklab-site = site;
    looklab-scenes = scenes;
    # Seeded into the state directory on first start.
    looklab-data = src + "/data";
    default = server;
  };
}
