{
  description = "A development environment for Rust projects";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = {
    self,
    nixpkgs,
    flake-utils,
  }:
    flake-utils.lib.eachDefaultSystem (
      system: let
        pkgs = nixpkgs.legacyPackages.${system};
      in {
        devShells.default = pkgs.mkShell {
          buildInputs = with pkgs; [
            rustup
            gcc
            alsa-lib
            cmake
            libopus
            automake
            autoconf
            perl
            pkg-config
          ];
        };

        packages.default = pkgs.rustPlatform.buildRustPackage {
          pname = "insanity";
          version = "0.1.0";
          src = ./.;
          cargoLock = {
            lockFile = ./Cargo.lock;
            allowBuiltinFetchGit = true;
          };

          nativeBuildInputs = [pkgs.pkg-config pkgs.perl pkgs.cmake];
          buildInputs = [
            # Add your build dependencies here, for example:
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