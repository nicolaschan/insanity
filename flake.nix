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
        
        # Platform-specific dependencies
        audioLibs = if pkgs.stdenv.isDarwin
          then with pkgs.darwin.apple_sdk.frameworks; [
            AudioToolbox
            AudioUnit
            CoreAudio
            CoreFoundation
          ]
          else [ pkgs.alsa-lib ];
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
          ] ++ audioLibs ++ (if stdenv.isDarwin then [
            libiconv
          ] else [
            gcc
          ]);
        };
        packages.default = pkgs.rustPlatform.buildRustPackage {
          pname = "insanity";
          version = "1.5.9";
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
          ] ++ audioLibs ++ (if pkgs.stdenv.isDarwin then [
            pkgs.libiconv
          ] else []);
          # If you have any runtime dependencies, add them here:
          # propagatedBuildInputs = [ ... ];
        };
      }
    );
}
