# insanity peer-to-peer voice chat 🤯

[![Linux GNU](https://github.com/nicolaschan/insanity/actions/workflows/linux-gnu.yml/badge.svg)](https://github.com/nicolaschan/insanity/actions/workflows/linux-gnu.yml)
[![Linux musl](https://github.com/nicolaschan/insanity/actions/workflows/linux-musl.yml/badge.svg)](https://github.com/nicolaschan/insanity/actions/workflows/linux-musl.yml)
[![Windows MinGW](https://github.com/nicolaschan/insanity/actions/workflows/windows-mingw.yml/badge.svg)](https://github.com/nicolaschan/insanity/actions/workflows/windows-mingw.yml)
[![Windows MSVC](https://github.com/nicolaschan/insanity/actions/workflows/windows-msvc.yml/badge.svg)](https://github.com/nicolaschan/insanity/actions/workflows/windows-msvc.yml)
[![macOS Apple Silicon](https://github.com/nicolaschan/insanity/actions/workflows/macos-apple-silicon.yaml/badge.svg)](https://github.com/nicolaschan/insanity/actions/workflows/macos-apple-silicon.yaml)

> All existing voice chat solutions are bad. This one is worse.

An experimental peer-to-peer voice chat application with a terminal UI. Written and intended for learning and fun, not recommended for serious use-cases.

## Running insanity

Choose an option:

1. **Binary download**: Click the badge above for your system. Go to the latest successful run and download the build artifact.
   ```
   insanity --bridge <BAYBRIDGE_SERVER> --room <ROOM>
   ```
2. **Nix**
   ```
   nix run github:nicolaschan/insanity -- --bridge <BAYBRIDGE_SERVER> --room <ROOM>
   ```
3. **Build from source**
   ```
   cargo run --release -- --bridge <BAYBRIDGE_SERVER> --room <ROOM>
   ```
   and keep installing missing system libraries until it works

## Running the Bay Bridge Server

Install [Bay Bridge](https://github.com/nicolaschan/baybridge) and run `baybridge serve`.

## Features
- NAT holepunch connections
- Encrypted with the noise protocol
- Background noise suppression
- No central server required, bootstraps connections over Tor 🥸


