# insanity P2P voice chat

All existing voice chat solutions are bad.
This one is worse.

[![Linux](https://github.com/nicolaschan/insanity/actions/workflows/linux.yml/badge.svg)](https://github.com/nicolaschan/insanity/actions/workflows/linux.yml)
[![Windows MinGW](https://github.com/nicolaschan/insanity/actions/workflows/windows-mingw.yml/badge.svg)](https://github.com/nicolaschan/insanity/actions/workflows/windows-mingw.yml)
[![Windows MSVC](https://github.com/nicolaschan/insanity/actions/workflows/windows-msvc.yml/badge.svg)](https://github.com/nicolaschan/insanity/actions/workflows/windows-msvc.yml)

## Usage

You run
```bash
cargo run --release -- --peer-address $FRIENDS_IP:1337
```

Your friend runs
```bash
cargo run --release -- --peer-address $YOUR_IP:1337
```

## To Do
- [ ] Optimize for latency (low ping/pong delay)
- [ ] Mesh networking (add peers automatically)
- [ ] Encryption
- [x] UDP/QUIC
- [ ] Better interpolation for missing packets
- [ ] Handle different sampling rates
- [ ] Multi-channel and mono audio
- [ ] Coordination server and channel rooms
- [ ] User selectable audio device (input and output)
- [ ] Add/remove peers from console UI
- [ ] Adjust audio preferences per peer (volume and denoise options)
- [ ] Expose compression options
- [ ] Performance benchmarks
- [ ] Full test coverage
- [ ] Measure bandwidth and ping to peers (and show in console)
- [ ] Set up CI to build binaries + Docker image on GitHub
