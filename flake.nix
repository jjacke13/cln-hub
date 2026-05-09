# flake.nix
#
# Nix flake for cln-hub. Two outputs:
#
#   packages.<system>.cln-hub  — the production binary built by Nix
#                                 (`nix build .#cln-hub`)
#   devShells.<system>.default — the cargo-driven dev environment
#                                 (`nix develop`)
#
# Supported `<system>` values: x86_64-linux, aarch64-linux.
# (Cross-compilation between the two is possible but not configured
# here — building for the other arch needs either matching hardware,
# a remote builder, or `pkgsCross`.)
#
# The package build is fully reproducible: `cargoLock.lockFile`
# pins every transitive crate by hash. SQLite is statically linked
# via libsqlite3-sys's bundled copy, so the resulting binary has no
# runtime C-library dependencies beyond glibc.

{
  description = "cln-hub: an LndHub-compatible HTTP API as a Core Lightning plugin";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";
    flake-utils.url = "github:numtide/flake-utils";
    # rust-overlay is only used by the dev shell; the package build
    # uses nixpkgs' default rustc/cargo, which is plenty.
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, flake-utils, rust-overlay }:
    flake-utils.lib.eachSystem
      [ "x86_64-linux" "aarch64-linux" ]
      (system:
        let
          pkgs = import nixpkgs {
            inherit system;
            overlays = [ rust-overlay.overlays.default ];
          };

          # Dev-shell toolchain (latest stable + IDE niceties).
          rustToolchain = pkgs.rust-bin.stable.latest.default.override {
            extensions = [ "rust-src" "rust-analyzer" "clippy" "rustfmt" ];
          };

          cln-hub = pkgs.rustPlatform.buildRustPackage {
            pname = "cln-hub";
            version = "0.1.0";

            src = ./.;
            cargoLock.lockFile = ./Cargo.lock;

            # `pkg-config` may be probed by native build scripts.
            # libsqlite3-sys vendors its own SQLite source, so we
            # don't need pkgs.sqlite at runtime.
            nativeBuildInputs = [ pkgs.pkg-config ];
            buildInputs = [ ];

            # Skip the cargo test suite during the Nix build — most
            # tests need a running CLN node and live network. CI / dev
            # shell are the right place to run them.
            doCheck = false;

            meta = with pkgs.lib; {
              description = "LndHub-compatible HTTP API as a Core Lightning plugin";
              license = licenses.mit;
              platforms = [ "x86_64-linux" "aarch64-linux" ];
              mainProgram = "cln-hub";
            };
          };
        in
        {
          # ─── Package outputs ──────────────────────────────────────
          packages = {
            default = cln-hub;
            cln-hub = cln-hub;
          };

          # ─── Dev shell ────────────────────────────────────────────
          devShells.default = pkgs.mkShell {
            buildInputs = [
              rustToolchain

              # Lightning + Bitcoin so we can run integration tests
              # against a real lightningd in regtest.
              pkgs.clightning
              pkgs.bitcoind

              # Database tooling (the actual library is linked statically
              # by sqlx via libsqlite3-sys, but having the CLI is handy).
              pkgs.sqlite

              # Crypto deps: some Rust crates link against system openssl.
              pkgs.pkg-config
              pkgs.openssl
            ];

            shellHook = ''
              echo "── cln-hub dev shell ──"
              echo "rustc:      $(rustc --version)"
              echo "cargo:      $(cargo --version)"
              echo "lightningd: $(lightningd --version 2>/dev/null || echo 'not found')"
              echo "sqlite3:    $(sqlite3 --version)"
            '';
          };
        });
}
