# Perf harness

Two steps: capture profiles with `profile_three_node.sh` (one run per tree),
then compare them with `compare_profiles.py`.

## Prerequisites

- Run everything inside `nix develop`.
- CPU governor `performance`.
- Reference audio at `~/.local/share/insanity-perf/recording_amplified_12.wav`.
  The harness validates it (format + pinned sha256) before every run and
  refuses to measure with a bad file.
- No `pw-play` node and no Rhythmbox↔insanity links in the PipeWire graph
  (the harness refuses to run until the graph is clean).

## Quickstart

```bash
nix develop

# 1. Profile the current tree (includes uncommitted changes) as tag "current".
tools/perf/profile_three_node.sh --tag current --mode both --window 60

# 2. Profile the baseline in a clean worktree as tag "48433b".
# (For releases predating the harness itself, see "Profiling an old
# release" below instead.)
git worktree add /tmp/base-48433b 48433b
cd /tmp/base-48433b
tools/perf/profile_three_node.sh --tag 48433b --mode both --window 60
cd - >/dev/null
git worktree remove /tmp/base-48433b

# 3. Compare.
tools/perf/compare_profiles.py --dataset0 48433b --dataset1 current
```

Each `--tag TAG` run produces `/tmp/insanity-TAG` (the exact profiling
binary — keep it until analysis is done), `/tmp/TAG_quiet.data` (room-tone
baseline CPU) and `/tmp/TAG_music.data` (standardized music workload driving
all three instances identically), plus `/tmp/TAG_{quiet,music}.stat`
(`perf stat` counters with `/proc` context-switch deltas appended).
The compare step requires the `.stat` files alongside the `.data` files.
The compare step prints, per mode: a sanity
header (samples, implied CPU time, binary status), per-category `%` and
absolute CPU-s with deltas vs the baseline dataset, top symbol movers in
CPU-s, and a one-line verdict. Deltas are absolute because shares mislead
when totals differ: a shrinking total inflates every surviving share.

## Profiling an old release (e.g. v1.6.6)

Old trees predate the harness and lack `[profile.profiling]`, so build the
binary by hand with profiling-equivalent settings and pass `--skip-build`:

```bash
git worktree add /tmp/base-v166 v1.6.6
cd /tmp/base-v166
CARGO_PROFILE_RELEASE_DEBUG=true CARGO_PROFILE_RELEASE_STRIP=false \
    cargo build --release --bin insanity
cp target/release/insanity /tmp/insanity-v166
file /tmp/insanity-v166 | grep -q with.debug_info
cd -
tools/perf/profile_three_node.sh --tag v166 --binary /tmp/insanity-v166 \
    --skip-build --mode both --window 60
git worktree remove /tmp/base-v166
tools/perf/compare_profiles.py --dataset0 v166 --dataset1 current
```

Notes:

- Build old and new binaries with identical `RUSTFLAGS` (or none); the
  harness script itself sets none.
- Pre-1.7 audio goes through the ALSA bridge (`alsa_capture.<tag>` nodes)
  and opens one capture stream per peer, so a 3-node run fans out to six
  input ports. Port matching is suffix-based and the fan-out count is read
  off the live graph, so no flags are needed — but expect the multiplicity
  in the capture log.
- Old releases only open audio once peers connect; single-instance runs
  yield no ports. The harness always runs three nodes, so this is handled.
- Captures run with microphones unlinked (quiet asserts zero input links;
  music asserts exactly the `pw-play` fan-out), so ambient sound cannot
  contaminate a measurement.

## Reference

`profile_three_node.sh --tag TAG` (`--tag` is required, `[A-Za-z0-9_-]+`):

| Flag | Default | Meaning |
|---|---|---|
| `--profiler perf\|samply` | `perf` | `perf record -F 997 -g`, or `samply record` |
| `--mode quiet\|music\|both` | `both` | Room tone, music fan-out, or quiet-then-music |
| `--window S` | `60` | Capture seconds per mode (integer, ≥ 10) |
| `--room NAME` | random `perf-xxxxxxxx` | Pin the room instead of a fresh random one |
| `--wav FILE` | reference WAV above | Replacement must pass validation |
| `--out-dir DIR` | `/tmp` | Where `TAG_{quiet,music}.data` go |
| `--binary PATH` | `/tmp/insanity-TAG` | Build output / binary to profile |
| `--allow-desktop-audio` | off | Skip the Rhythmbox-link refusal |
| `--skip-build` | off | Reuse the existing `--binary` as-is |

`compare_profiles.py` takes indexed datasets; dataset 0 is the baseline
unless `--baseline N` says otherwise:

```bash
tools/perf/compare_profiles.py \
  --dataset0 48433b --dataset1 current [--dataset2 NEXT ...] \
  [--baseline 0] [--top 10] [--mode both|quiet|music] \
  [--data-dir /tmp] [--min-abs 0.5] [--min-rel 10.0]
```

`--datasetN TAG` expands to `--datasetN-quiet/--datasetN-music`
(`<data-dir>/TAG_{quiet,music}.data`) plus `--datasetN-bin`
(`insanity-TAG` looked up in `<data-dir>`, then `/tmp`). Any explicit
`--datasetN-name/-quiet/-music/-bin` overrides just that piece, so shorthand
and explicit paths mix freely within and across datasets.

Rules:

- Run captures sequentially, never concurrently (PipeWire port prefixes and
  CPU contend).
- Never rebuild or delete a per-tag binary before comparing; symbol
  resolution silently misattributes against the wrong binary.
- Compare quiet→quiet and music→music only, with equal `--window` values
  (absolute CPU scales with capture length). Deltas past ±0.5 CPU-s and
  ±10% relative get flagged; run-to-run timing noise is about ±5%.

## When it complains

- Stale `pw-play`/binary ports or a running profiler for the tag: kill
  leftovers first.
- Rhythmbox linked into insanity inputs: disconnect it or pass
  `--allow-desktop-audio`.
- `wav file missing` / reference validation failure: restore the pinned WAV.
- `suspiciously small` output: capture failed; check the run log.
- `compare_profiles` `binary missing`: the exact binary is gone; recapture.
- `stat file unreadable` / `stat file has no counters`: the `.stat` file
  for that tag/mode is missing or empty; re-run the capture for that mode
  (or the whole tag) — compare requires `.stat` files, not just `.data`.
- `compare_profiles` warns `newer than its captures` or `raw addresses`:
  symbols may be wrong; recapture with the binary in place.
