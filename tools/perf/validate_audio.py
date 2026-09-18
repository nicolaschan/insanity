#!/usr/bin/env python3
"""Preflight gate for the perf-harness reference audio file.

Checks format (WAV PCM 16-bit stereo 48kHz), duration, peak/clipping, and
the fraction of samples above the denoiser quiet-gate (0.02). Fails fast
with an actionable message so a bad reference file can never silently
poison a measurement.

Usage:
    validate_audio.py <file.wav> [--min-duration S] [--min-active F]
                      [--max-peak P] [--gate G]
                      [--expect-sha256 HEX]
"""
import array
import hashlib
import sys
import wave

GATE = 0.02


def fail(msg):
    print(f"validate_audio: FAIL: {msg}", file=sys.stderr)
    return 1


def main(argv):
    import argparse

    p = argparse.ArgumentParser()
    p.add_argument("wav")
    p.add_argument("--min-duration", type=float, default=150.0)
    p.add_argument("--min-active", type=float, default=0.30)
    p.add_argument("--max-active", type=float, default=0.70)
    p.add_argument("--max-peak", type=float, default=0.9)
    p.add_argument("--gate", type=float, default=GATE)
    p.add_argument("--expect-sha256", default=None)
    a = p.parse_args(argv)

    if a.expect_sha256:
        h = hashlib.sha256()
        try:
            with open(a.wav, "rb") as f:
                for chunk in iter(lambda: f.read(1 << 20), b""):
                    h.update(chunk)
        except OSError as e:
            return fail(f"cannot hash {a.wav}: {e}")
        if h.hexdigest() != a.expect_sha256.lower():
            return fail(
                f"sha256 {h.hexdigest()} != expected {a.expect_sha256}; "
                f"wrong reference file"
            )

    try:
        w = wave.open(a.wav)
    except Exception as e:
        return fail(f"cannot open {a.wav}: {e}")
    ch, sw, fr, n, comptype, _ = w.getparams()
    raw = w.readframes(n)
    w.close()

    if comptype != "NONE":
        return fail(f"compressed WAV ({comptype}); need PCM")
    if (ch, sw, fr) != (2, 2, 48000):
        return fail(
            f"want stereo/16-bit/48000Hz, got ch={ch} width={sw} rate={fr}"
        )
    dur = n / fr
    if dur < a.min_duration:
        return fail(f"duration {dur:.1f}s < minimum {a.min_duration:.0f}s")

    samples = array.array("h", raw)
    scaled = [abs(x) / 32768.0 for x in samples]
    peak = max(scaled)
    clipped = sum(1 for x in samples if x >= 32767 or x <= -32767)
    if clipped:
        return fail(f"{clipped} clipped samples (rails hit); reduce gain")
    if peak > a.max_peak:
        return fail(f"peak {peak:.3f} > {a.max_peak}; reduce gain")
    active = sum(1 for x in scaled if x >= a.gate) / len(scaled)
    if not (a.min_active <= active <= a.max_active):
        return fail(
            f"gate-active fraction {active:.3f} outside "
            f"[{a.min_active:.2f},{a.max_active:.2f}]; adjust gain"
        )
    rms = (sum(x * x for x in scaled) / len(scaled)) ** 0.5
    print(
        f"validate_audio: OK dur={dur:.1f}s peak={peak:.3f} "
        f"rms={rms:.4f} active={active:.3f}"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
