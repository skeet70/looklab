{
  description = "looklab: measure, compare and reproduce camera looks";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
    rust-overlay.inputs.nixpkgs.follows = "nixpkgs";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { nixpkgs, rust-overlay, flake-utils, ... }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs { inherit system overlays; };
        rusttoolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
        nightly = pkgs.rust-bin.nightly.latest.default.override {
          extensions = [ "rust-src" "llvm-tools-preview" ];
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
          cargo-nextest
          trunk
        ];
      in
      {
        devShells.default = pkgs.mkShell {
          buildInputs = [ rusttoolchain ] ++ common;
          # trunk 0.21 parses NO_COLOR as a bool; the conventional value 1 fails its CLI parser.
          shellHook = ''
            if [ "''${NO_COLOR:-}" = "1" ]; then export NO_COLOR=true; fi
          '';
        };
        # Golden-vector generation (dev-time only).
        devShells.golden = pkgs.mkShell {
          buildInputs = [ goldenPython pkgs.libraw ];
        };
        # cargo-fuzz needs a nightly compiler for sanitizer instrumentation.
        devShells.fuzz = pkgs.mkShell {
          buildInputs = [ nightly pkgs.cargo-fuzz ];
        };
      });
}
