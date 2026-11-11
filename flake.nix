{
  description = "looklab: measure, compare and reproduce camera looks";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
    rust-overlay.inputs.nixpkgs.follows = "nixpkgs";
    flake-utils.url = "github:numtide/flake-utils";
    crane.url = "github:ipetkov/crane";
    disko.url = "github:nix-community/disko";
    disko.inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs = inputs@{
    self,
    nixpkgs,
    rust-overlay,
    flake-utils,
    crane,
    disko,
    ...
  }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs { inherit system overlays; };
        rusttoolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
        nightly = pkgs.rust-bin.nightly.latest.default.override {
          extensions = [ "rust-src" "llvm-tools-preview" ];
        };
        looklab = import ./nix/packages.nix {
          inherit pkgs crane;
          lib = pkgs.lib;
          src = ./.;
        };

        # colour-science is not in nixpkgs; pinned from PyPI for tools/golden only.
        colour-science = pkgs.python3Packages.buildPythonPackage rec {
          pname = "colour_science";
          version = "0.4.7";
          pyproject = true;
          src = pkgs.python3Packages.fetchPypi {
            inherit pname version;
            hash = "sha256-s0dz3E3T+bqZzKUpf6EOn1MhNNmUs6Iwjay+lw39UHk=";
          };
          build-system = [ pkgs.python3Packages.hatchling ];
          dependencies = with pkgs.python3Packages; [ numpy scipy typing-extensions imageio ];
          doCheck = false;
          pythonImportsCheck = [ "colour" ];
        };
        goldenPython = pkgs.python3.withPackages (ps: [ colour-science ps.numpy ps.scipy ]);

        common = with pkgs; [
          git
          git-lfs
          coreutils
          pkg-config
          openssl
          sqlite
          zstd
          wasm-bindgen-cli
          wasmtime
          wasm-tools
          binaryen
          wabt
          nodejs_22
          python3
          actionlint
          shellcheck
          cargo-nextest
          cargo-leptos
          util-linux
          socat
        ];
        buildShellHook = ''
          if looklab_git_dir=$(git rev-parse --path-format=absolute --git-common-dir 2>/dev/null); then
            looklab_build_root=$(dirname "$looklab_git_dir")
          else
            looklab_build_root=$PWD
          fi
          looklab_build_root=$(realpath -m -- "$looklab_build_root")
          if [ -z "''${LOOKLAB_ISOLATED_BUILD_OWNER:-}" ]; then
            looklab_cargo_root=$(realpath -m -- "''${CARGO_TARGET_DIR:-$looklab_build_root/target}")
            looklab_ui_root=$(realpath -m -- "''${LOOKLAB_UI_TARGET_DIR:-$looklab_build_root/target-ui}")
            if { [ "$looklab_cargo_root" != "$looklab_build_root/target" ] &&
                 [ "$looklab_cargo_root" != "$looklab_build_root/target-ui" ]; } ||
               [ "$looklab_ui_root" != "$looklab_build_root/target-ui" ]; then
              printf '%s\n' 'looklab: inherited build roots differ from the shared checkout. Unset CARGO_TARGET_DIR and LOOKLAB_UI_TARGET_DIR; exceptional isolated builds require LOOKLAB_ISOLATED_BUILD_OWNER.' >&2
              exit 75
            fi
            looklab_checkout=$(git rev-parse --show-toplevel 2>/dev/null || printf '%s\n' "$PWD")
            if [ "$(realpath -m -- "$looklab_checkout")" != "$looklab_build_root" ]; then
              for looklab_cache in target target-ui; do
                looklab_local="$looklab_checkout/$looklab_cache"
                if [ -d "$looklab_local" ] &&
                   [ "$(realpath -m -- "$looklab_local")" != "$looklab_build_root/$looklab_cache" ] &&
                   [ "$(find -H "$looklab_local" -mindepth 1 -maxdepth 1 -printf x -quit)" = x ]; then
                  printf 'looklab: linked checkout contains local build storage at %s. Preserve databases, source and unmerged work; review its retirement or name an exceptional LOOKLAB_ISOLATED_BUILD_OWNER before building.\n' "$looklab_local" >&2
                  exit 75
                fi
              done
            fi
          fi
          export CARGO_TARGET_DIR="''${CARGO_TARGET_DIR:-$looklab_build_root/target}"
          export LOOKLAB_UI_TARGET_DIR="''${LOOKLAB_UI_TARGET_DIR:-$looklab_build_root/target-ui}"
          unset looklab_git_dir looklab_build_root looklab_checkout looklab_cache looklab_local looklab_cargo_root looklab_ui_root
        '';
      in
      {
        devShells.default = pkgs.mkShell {
          buildInputs = [ rusttoolchain ] ++ common;
          shellHook = buildShellHook;
        };
        # Golden-vector generation (dev-time only).
        devShells.golden = pkgs.mkShell {
          buildInputs = [ goldenPython pkgs.libraw ];
        };
        # cargo-fuzz needs a nightly compiler for sanitizer instrumentation.
        devShells.fuzz = pkgs.mkShell {
          buildInputs = [ nightly pkgs.cargo-fuzz pkgs.git ];
          shellHook = buildShellHook;
        };

        packages = looklab.packages;
      }
    )
    // {
      nixosModules.looklab = import ./nix/looklab/module.nix { inherit self; };

      nixosConfigurations.looklab-ovh = nixpkgs.lib.nixosSystem {
        system = "x86_64-linux";
        modules = [
          disko.nixosModules.disko
          self.nixosModules.looklab
          ./nix/hosts/looklab-ovh.nix
        ];
      };

      checks.x86_64-linux.looklab-deploy = import ./nix/looklab/test.nix {
        inherit self nixpkgs;
      };
    };
}
