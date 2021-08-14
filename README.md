# insanity P2P voice chat

All existing voice chat solutions are bad.
This one is worse.

## Usage

You run
```
cargo run --release -- --bind-address $FRIENDS_IP:1337 --peer-address 127.0.0.1:1338
```

Your friend runs
```
cargo run --release -- --bind-address 127.0.0.1:1338 --peer-address $YOUR_IP:1337
```
