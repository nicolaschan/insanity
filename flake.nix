{
  description = "A development environment for Rust projects";
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
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
        rustToolchain = pkgs.rust-bin.stable.latest.default.override {
          extensions = ["rust-src"];
        };
      in {
        devShells.default = pkgs.mkShell {
          buildInputs = with pkgs; [
            rustToolchain
            rust-analyzer
            cargo-edit
            cmake
            libopus
            automake
            autoconf
            perl
            pkg-config
            # web app
            nodejs_22
          ] ++ (if stdenv.isDarwin then [
            # SDK automatically includes audio libs
          ] else [
            alsa-lib
            gcc
          ]);
        };
        packages.default = pkgs.rustPlatform.buildRustPackage {
          pname = "insanity";
          version = (builtins.fromTOML (builtins.readFile ./insanity-native-tui-app/Cargo.toml)).package.version;
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
          ] ++ (if pkgs.stdenv.isDarwin then [
            # SDK automatically includes audio libs
          ] else [
            pkgs.alsa-lib
          ]);
          # If you have any runtime dependencies, add them here:
          # propagatedBuildInputs = [ ... ];
        };
      }
    );
}
