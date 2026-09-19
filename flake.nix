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
        rustPackageOptions = pkgs: extraFlags: noPipewire: {
          inherit pname version;
          src = ./.;
          cargoLock = {
            lockFile = ./Cargo.lock;
            allowBuiltinFetchGit = true;
          };
          cargoBuildFlags =
            [
              "--bin"
              "insanity"
            ]
            ++ extraFlags;
          cargoTestFlags = extraFlags;
          nativeBuildInputs = [pkgs.pkg-config pkgs.perl pkgs.cmake pkgs.rustPlatform.bindgenHook];
          buildInputs =
            [
              pkgs.libopus
            ]
            ++ (
              if pkgs.stdenv.hostPlatform.isDarwin
              then [
                # SDK automatically includes audio libs
              ]
              else
                [pkgs.alsa-lib]
                ++ (
                  if noPipewire
                  then [
                    # static musl: ALSA-only (`--no-default-features`)
                  ]
                  else [
                    pkgs.pipewire
                  ]
                )
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
              if stdenv.hostPlatform.isDarwin
              then [
                # SDK automatically includes audio libs
              ]
              else [
                alsa-lib
                # default pipewire backend (pulseaudio fallback is pure Rust)
                pipewire
                rustPlatform.bindgenHook
                gcc
                # profiling (Linux-only)
                samply
                hyperfine
                heaptrack
                valgrind
                perf
                linuxPackages.cpupower
              ]
            );
        };

        packages.default = pkgs.rustPlatform.buildRustPackage (rustPackageOptions pkgs [] false);
        packages.static =
          pkgs.pkgsStatic.rustPlatform.buildRustPackage
          (rustPackageOptions pkgs.pkgsStatic ["--no-default-features"] true);

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
