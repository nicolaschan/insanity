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
        packageMetadata = builtins.fromTOML (builtins.readFile ./insanity-native-tui-app/Cargo.toml);
        pname = packageMetadata.package.name;
        version = packageMetadata.package.version;
        rustPackageOptions = pkgs: {
          inherit pname version;
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
          buildInputs =
            [
              pkgs.libopus
            ]
            ++ (
              if pkgs.stdenv.isDarwin
              then [
                # SDK automatically includes audio libs
              ]
              else [
                pkgs.alsa-lib
              ]
            );
        };
      in {
        devShells.default = pkgs.mkShell {
          buildInputs = with pkgs;
            [
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
            ]
            ++ (
              if stdenv.isDarwin
              then [
                # SDK automatically includes audio libs
              ]
              else [
                alsa-lib
                gcc
              ]
            );
        };

        packages.default = pkgs.rustPlatform.buildRustPackage (rustPackageOptions pkgs);
        packages.static = pkgs.pkgsStatic.rustPlatform.buildRustPackage (rustPackageOptions pkgs.pkgsStatic);

        packages.docker = pkgs.dockerTools.buildLayeredImage {
          name = pname;
          tag = version;
          config = {
            Entrypoint = ["${self.packages.${system}.default}/bin/insanity"];
          };
        };
      }
    );
}
