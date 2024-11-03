{
  description = "A development environment for Rust projects";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay.url = "github:oxalica/rust-overlay";
  };

  outputs = {
    self,
    nixpkgs,
    flake-utils,
    rust-overlay,
  }:
    flake-utils.lib.eachDefaultSystem (
      system: let
        overlays = [(import rust-overlay)];
        pkgs = import nixpkgs {
          inherit system overlays;
        };
        rustToolchain = pkgs.rust-bin.stable.latest.default;
      in {
        devShells.default = pkgs.mkShell {
          buildInputs = with pkgs; [
            rustToolchain
            rust-analyzer
            cargo-edit
            gcc
            alsa-lib
            cmake
            libopus
            automake
            autoconf
            perl
            pkg-config

            # web app
            nodejs_22
          ];
        };

        packages.default = pkgs.rustPlatform.buildRustPackage {
          pname = "insanity";
          version = "1.5.0";
          src = ./.;
          cargoLock = {
            lockFile = ./Cargo.lock;
            allowBuiltinFetchGit = true;
          };
          cargoBuildFlags = [
            "--bin"
            "insanity"
          ];

          nativeBuildInputs = [pkgs.pkg-config pkgs.perl pkgs.cmake];
          buildInputs = [
            pkgs.openssl
            pkgs.libopus
            pkgs.alsa-lib
          ];

          # If you have any runtime dependencies, add them here:
          # propagatedBuildInputs = [ ... ];
        };
      }
    );
}
