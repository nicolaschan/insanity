# insanity P2P voice chat

All existing voice chat solutions are bad.
This one is worse.

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
- [ ] Better console UI
- [ ] Encryption
- [ ] UDP
- [ ] Better interpolation for missing packets
- [ ] Handle different sampling (degrade sampling rate if necessary)
- [ ] Volume normalization
- [ ] Multi-channel and mono audio
- [ ] Coordination server and channel rooms
- [x] Better compression