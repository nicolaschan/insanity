# insanity peer-to-peer voice chat 🤯

[![Build](https://github.com/nicolaschan/insanity/actions/workflows/build.yml/badge.svg)](https://github.com/nicolaschan/insanity/actions/workflows/build.yml)

> All existing voice chat solutions are bad. This one is worse.

An experimental peer-to-peer voice chat application with a terminal UI. Written and intended for learning and fun, not recommended for serious use-cases.

## Running insanity

Choose an option:

1. **Binary download**: [Go to the latest release](https://github.com/nicolaschan/insanity/releases/) and download the binary for your system.
   ```
   insanity run --bridge <BAYBRIDGE_SERVER> --room <ROOM>
   ```
2. **Nix**
   ```
   nix run github:nicolaschan/insanity -- run --bridge <BAYBRIDGE_SERVER> --room <ROOM>
   ```
3. **Build from source**
   ```
   cargo run --release -- run --bridge <BAYBRIDGE_SERVER> --room <ROOM>
   ```
   and keep installing missing system libraries until it works

### Update

If you use the insanity binary, you can update it in place with `insanity update`.

## Running the Bay Bridge Server

Install [Bay Bridge](https://github.com/nicolaschan/baybridge) and run `baybridge serve`.

The bridge server is used to bootstrap peer-to-peer connections. All audio and chat is peer-to-peer, not through the bridge server.

## Features
- NAT holepunch connections for direct P2P audio
- Encrypted with the noise protocol
- Background noise suppression
- Text chat messages
- Terminal UI
