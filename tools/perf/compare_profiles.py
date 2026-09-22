#!/usr/bin/env python3
import argparse
import concurrent.futures
import os
import re
import shutil
import subprocess
import sys
from typing import NoReturn

DATASET_ARG = re.compile(r"^--dataset(\d+)(?:-(name|quiet|music|stat-quiet|stat-music|bin))?$")
SYMBOL_LINE = re.compile(r"^\s*([\d.]+)%\s+(\S+)\s+(\S+)\s+\[.?\]\s+(.*)")
SAMPLES_LINE = re.compile(r"^#\s*Samples:\s*([\d.]+)([KMB]?)")
LOST_LINE = re.compile(r"^#\s*Total Lost Samples:\s*(\d+)")
STATS_SECTION = re.compile(r"^\s*(\S.*?)\s+stats:\s*$")
STATS_SAMPLES = re.compile(r"^\s*SAMPLE events:\s*(\d+)")
HEX_SYMBOL = re.compile(r"^0x[0-9a-fA-F]+$")
SUFFIX = {"": 1.0, "K": 1e3, "M": 1e6, "B": 1e9}
FREQ_HZ = 997.0
MIN_DATA_BYTES = 10240

CATS = [
    ("denoise", ("nnnoiseless", "rnn_", "denois", "rnnoise")),
    ("opus", ("opus_", "celt_", "silk_", "libopus")),
    ("resample", ("rubato", "resamp", "sinc", "polyphase")),
    ("mixer", ("mixer",)),
    ("ui", ("ratatui", "tui::", "unicode_width", "unicode-width", "textwrap",
             "crossterm", "grapheme", "paragraph", "smawk", "linewrap",
             "wrap_single_line", "optimal_fit", "split_points", "cellwidth",
             "str_width", "buffer::diff", "buffer::reset")),
    ("runtime", ("tokio", "parking_lot", "std::", "crossbeam", "futures",
                 "mio", "rayon", "async")),
    ("audioio", ("cpal", "alsa", "pipewire", "pulseaudio", "spa_", "jack")),
    ("libc", ("memcpy", "memmove", "memcmp", "memset", "malloc", "free",
              "__mem", "libc-", "libm-", "libgcc")),
    ("kernel", ("[k]",)),
]


def die(msg) -> NoReturn:
    print(f"compare_profiles: ERROR: {msg}", file=sys.stderr)
    sys.exit(1)


def warn(msg):
    print(f"compare_profiles: WARN: {msg}", file=sys.stderr)


def split_argv(argv):
    raw = {}
    rest = []
    i = 0
    while i < len(argv):
        tok = argv[i]
        name, eq, val = tok.partition("=")
        m = DATASET_ARG.match(name)
        if m:
            idx = int(m.group(1))
            field = m.group(2) or "tag"
            if not eq:
                i += 1
                if i >= len(argv):
                    die(f"{tok} needs a value")
                val = argv[i]
            slot = raw.setdefault(idx, {})
            if field in slot:
                die(f"duplicate {tok}")
            slot[field] = val
        else:
            rest.append(tok)
        i += 1
    return raw, rest


def resolve_datasets(raw, data_dir):
    if not raw:
        die("no datasets given; pass at least --dataset0 TAG")
    out = []
    for idx in sorted(raw):
        r = raw[idx]
        tag = r.get("tag")
        name = r.get("name", tag if tag else f"dataset{idx}")
        quiet = r.get("quiet")
        music = r.get("music")
        if quiet is None and tag:
            quiet = os.path.join(data_dir, f"{tag}_quiet.data")
        if music is None and tag:
            music = os.path.join(data_dir, f"{tag}_music.data")
        if quiet is None or music is None:
            die(f"dataset {idx}: need quiet+music files "
                f"(or a --dataset{idx} TAG shorthand)")
        stat_quiet = r.get("stat-quiet")
        stat_music = r.get("stat-music")
        stat_quiet_explicit = stat_quiet is not None
        stat_music_explicit = stat_music is not None
        if stat_quiet is None and tag:
            stat_quiet = os.path.join(data_dir, f"{tag}_quiet.stat")
        if stat_music is None and tag:
            stat_music = os.path.join(data_dir, f"{tag}_music.stat")
        binpath = r.get("bin")
        if binpath is None:
            for cand in (os.path.join(data_dir, f"insanity-{name}"),
                         os.path.join("/tmp", f"insanity-{name}")):
                if os.path.isfile(cand):
                    binpath = cand
                    break
            if binpath is None:
                die(f"dataset {idx} ({name}): binary not found; "
                    f"pass --dataset{idx}-bin PATH")
        out.append({"idx": idx, "name": name,
                    "quiet": quiet, "music": music, "bin": binpath,
                    "stat-quiet": stat_quiet, "stat-music": stat_music,
                    "stat-quiet-explicit": stat_quiet_explicit,
                    "stat-music-explicit": stat_music_explicit})
    return out


def check_dataset(ds):
    for mode in ("quiet", "music"):
        path = ds[mode]
        if not os.path.isfile(path):
            die(f"[{ds['name']}] {mode} file missing: {path}")
        if os.path.getsize(path) <= MIN_DATA_BYTES:
            die(f"[{ds['name']}] {mode} file suspiciously small: {path}")
    if not os.path.isfile(ds["bin"]):
        die(f"[{ds['name']}] binary missing: {ds['bin']} "
            f"(perf would report [unknown] symbols)")
    if not os.access(ds["bin"], os.X_OK):
        warn(f"[{ds['name']}] binary not executable: {ds['bin']}")
    newest_data = max(os.path.getmtime(ds["quiet"]),
                      os.path.getmtime(ds["music"]))
    if os.path.getmtime(ds["bin"]) > newest_data:
        warn(f"[{ds['name']}] binary is newer than its captures; "
             f"symbols may misattribute")


def run_flat_report(perf, dsname, path):
    try:
        p = subprocess.run(
            [perf, "report", "-i", path, "--stdio",
             "-g", "none", "--no-children", "--percent-limit", "0"],
            capture_output=True, text=True)
    except OSError as e:
        die(f"[{dsname}] failed to run perf: {e}")
    if p.returncode != 0:
        tail = "\n".join(p.stderr.splitlines()[-5:])
        die(f"[{dsname}] perf report failed on {path}:\n{tail}")
    return p.stdout


def run_exact_samples(perf, dsname, path):
    try:
        p = subprocess.run(
            [perf, "report", "--stats", "-i", path],
            capture_output=True, text=True)
    except OSError as e:
        die(f"[{dsname}] failed to run perf: {e}")
    if p.returncode != 0:
        tail = "\n".join(p.stderr.splitlines()[-5:])
        die(f"[{dsname}] perf report --stats failed on {path}:\n{tail}")
    sections = {}
    section = None
    for line in p.stdout.splitlines():
        m = STATS_SECTION.match(line)
        if m:
            section = m.group(1).strip()
            continue
        m = STATS_SAMPLES.match(line)
        if m:
            sections[section] = int(m.group(1))
    for name, count in sections.items():
        if name is not None and "cycles" in name:
            return name, count
    if None in sections:
        return "", sections[None]
    die(f"[{dsname}] perf report --stats on {path}: no SAMPLE count found")


STAT_EVENTS = ["task-clock", "voluntary-ctxt-switches",
               "involuntary-ctxt-switches", "page-faults", "minor-faults",
               "major-faults", "cycles", "instructions", "cache-misses",
               "branch-misses"]


def parse_stat(text):
    vals = {}
    for line in text.splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        fields = line.split(";")
        if len(fields) < 3:
            continue
        raw, unit, event = fields[0].strip(), fields[1].strip(), fields[2].strip()
        if raw in ("<not counted>", "<not supported>", ""):
            vals[event.split(":")[0]] = None
            continue
        try:
            vals[event.split(":")[0]] = (float(raw), unit)
        except ValueError:
            continue
    return vals


def parse_count(num, suffix):
    return int(float(num) * SUFFIX.get(suffix.upper(), 1.0))


def parse_report(text):
    samples = None
    lost = 0
    entries = []
    for line in text.splitlines():
        if samples is None:
            m = SAMPLES_LINE.match(line)
            if m:
                samples = parse_count(m.group(1), m.group(2))
                continue
        m = LOST_LINE.match(line)
        if m:
            lost = int(m.group(1))
            continue
        m = SYMBOL_LINE.match(line)
        if m:
            entries.append((float(m.group(1)), m.group(3), m.group(4).strip()))
    return samples, lost, entries


def categorize(sym, dso):
    s = (sym + " " + dso).lower()
    for cat, keys in CATS:
        if any(k in s for k in keys):
            return cat
    return "other"


def analyze(entries):
    cats = {}
    syms = {}
    for pct, dso, sym in entries:
        cat = categorize(sym, dso)
        cats[cat] = cats.get(cat, 0.0) + pct
        key = (sym, cat)
        syms[key] = syms.get(key, 0.0) + pct
    return cats, syms


def collect(perf, datasets, modes, stat_modes):
    stats = {}
    for mode in stat_modes:
        for ds in datasets:
            path = ds[f"stat-{mode}"]
            try:
                with open(path) as f:
                    text = f.read()
            except OSError as e:
                die(f"[{ds['name']}] {mode} stat file unreadable: {path}: {e}")
            vals = parse_stat(text)
            if not vals:
                die(f"[{ds['name']}] {mode} stat file has no counters: {path}")
            stats[(ds["idx"], mode)] = vals
    jobs = [(ds, mode, ds[mode]) for ds in datasets for mode in modes]
    texts = {}
    exact = {}
    with concurrent.futures.ThreadPoolExecutor(max_workers=2 * len(jobs)) as ex:
        futs = {}
        for ds, mode, path in jobs:
            futs[ex.submit(run_flat_report, perf, ds["name"], path)] = ("report", ds, mode)
            futs[ex.submit(run_exact_samples, perf, ds["name"], path)] = ("stats", ds, mode)
        for fut in concurrent.futures.as_completed(futs):
            kind, ds, mode = futs[fut]
            if kind == "report":
                texts[(ds["idx"], mode)] = fut.result()
            else:
                exact[(ds["idx"], mode)] = fut.result()
    results = {}
    for ds in datasets:
        for mode in modes:
            samples, lost, entries = parse_report(texts[(ds["idx"], mode)])
            event, exact_samples = exact[(ds["idx"], mode)]
            samples = exact_samples or samples
            if not entries:
                die(f"[{ds['name']}] {mode}: no symbols parsed; "
                    f"capture may be corrupt")
            total = sum(p for p, _, _ in entries)
            hexed = sum(p for p, _, s in entries if HEX_SYMBOL.match(s))
            if total > 0 and hexed / total > 0.5:
                warn(f"[{ds['name']}] {mode}: "
                     f"{100.0 * hexed / total:.0f}% of samples are raw "
                     f"addresses; is the exact binary in place?")
            cats, syms = analyze(entries)
            results[(ds["idx"], mode)] = {"samples": samples,
                                          "lost": lost,
                                          "event": event,
                                          "stat": stats.get((ds["idx"], mode)),
                                          "cats": cats, "syms": syms}
    events = {results[k]["event"] for k in results if results[k]["event"]}
    if len(events) > 1:
        warn(f"captures use different sample events: {sorted(events)}; "
             f"shares may not be comparable")
    return results


def cpu_of(pct, samples):
    if not samples:
        return None
    return pct / 100.0 * samples / FREQ_HZ


def fmt_rel(d, base):
    if base is None or base <= 0:
        if d is None or d == 0:
            return "+0%"
        return "n/a"
    return f"{100.0 * d / base:+.0f}%"


def flagged(d, base, min_abs, min_rel):
    if d is None or base is None:
        return False
    if abs(d) < min_abs:
        return False
    if base > 0:
        return abs(100.0 * d / base) >= min_rel
    return True


def show_mode(mode, datasets, base_pos, results, top_n, min_abs, min_rel):
    print(f"== {mode} ==")
    base = datasets[base_pos]
    base_n = results[(base["idx"], mode)]["samples"]
    sanity = []
    for ds in datasets:
        r = results[(ds["idx"], mode)]
        n = r["samples"]
        cpu = f"{n / FREQ_HZ:.1f} CPU-s" if n else "samples unknown"
        cell = (f"[{ds['idx']}]{ds['name']}: "
                f"{n if n else '?'} samples (~{cpu})")
        if ds is not base and n and base_n:
            delta = n - base_n
            cell += f" Δ{delta:+d}/{fmt_rel(delta, base_n)}"
        if r["lost"]:
            cell += f" LOST={r['lost']}"
            warn(f"[{ds['name']}] {mode}: {r['lost']} lost samples; "
                 f"totals and shares may skew")
        cell += ", bin OK"
        sanity.append(cell)
    print("sanity: " + " | ".join(sanity))
    others = [ds for ds in datasets if ds is not base]
    cats = set()
    for ds in datasets:
        cats.update(results[(ds["idx"], mode)]["cats"])
    rows = []
    for cat in cats:
        pcts = [results[(ds["idx"], mode)]["cats"].get(cat, 0.0)
                for ds in datasets]
        if all(v == 0 for v in pcts):
            continue
        cpus = [cpu_of(p, results[(ds["idx"], mode)]["samples"])
                for ds, p in zip(datasets, pcts)]
        bcpu = cpus[base_pos]
        deltas = []
        peak = 0.0
        for pos, ds in enumerate(datasets):
            if ds is base:
                continue
            dcpu = (None if cpus[pos] is None or bcpu is None
                    else cpus[pos] - bcpu)
            if dcpu is not None:
                peak = max(peak, abs(dcpu))
            deltas.append((ds, pcts[pos], cpus[pos], dcpu,
                           fmt_rel(dcpu, bcpu),
                           flagged(dcpu, bcpu, min_abs, min_rel)))
        rows.append((peak, cat, pcts, cpus, deltas))
    rows.sort(key=lambda r: -r[0])
    name_w = max([len("category")] + [len(c) for _, c, _, _, _ in rows])
    pct_w = max(10, max(len(f"[{ds['idx']}]{ds['name']}%")
                        for ds in datasets))
    cpu_w = max([7] + [len(f"{c:.2f}") for _, _, _, cpus, _ in rows
                       for c in cpus if c is not None])
    abs_w = 10
    cells = [" ".join([f"[{o['idx']}]{o['name']}%".rjust(pct_w),
                       "CPU-s".rjust(cpu_w),
                       "Δabs".rjust(abs_w), "Δrel".rjust(9)])
             for o in others]
    print(f"{'category'.ljust(name_w)} "
          f"{f'[{base['idx']}]{base['name']}%'.rjust(pct_w)} "
          f"{'CPU-s'.rjust(cpu_w)} " +
          " ".join(cells))
    for _, cat, pcts, cpus, deltas in rows:
        bcpu = cpus[base_pos]
        line = (f"{cat.ljust(name_w)} {pcts[base_pos]:{pct_w}.2f} "
                f"{bcpu:{cpu_w}.2f}" if bcpu is not None
                else f"{cat.ljust(name_w)} {pcts[base_pos]:{pct_w}.2f} "
                f"{'?'.rjust(cpu_w)}")
        for ds, pct, cpu, dcpu, rel, flag in deltas:
            star = "*" if flag else " "
            line += (f" {pct:{pct_w}.2f} "
                     f"{cpu:{cpu_w}.2f}" if cpu is not None else
                     f" {pct:{pct_w}.2f} {'?'.rjust(cpu_w)}")
            line += (f" {dcpu:+{abs_w}.2f} "
                     f"{rel.rjust(8)}{star}" if dcpu is not None else
                     f" {'?'.rjust(abs_w)} {rel.rjust(8)}{star}")
        print(line)
    print(f"top movers vs baseline [{base['idx']}]{base['name']}:")
    bsyms = results[(base["idx"], mode)]["syms"]
    bsamples = results[(base["idx"], mode)]["samples"]
    for ds in others:
        csyms = results[(ds["idx"], mode)]["syms"]
        csamples = results[(ds["idx"], mode)]["samples"]
        moves = []
        for key in set(bsyms) | set(csyms):
            sym, cat = key
            cur = cpu_of(csyms.get(key, 0.0), csamples)
            ref = cpu_of(bsyms.get(key, 0.0), bsamples)
            dcpu = None if cur is None or ref is None else cur - ref
            moves.append((abs(dcpu) if dcpu is not None else -1.0,
                          dcpu, sym, cat, cur, ref))
        moves.sort(key=lambda m: -m[0])
        print(f"  [{ds['idx']}]{ds['name']}:")
        for _, dcpu, sym, cat, cur, ref in moves[:top_n]:
            d = f"{dcpu:+.2f}" if dcpu is not None else "?"
            c = f"{cur:.2f}" if cur is not None else "?"
            b = f"{ref:.2f}" if ref is not None else "?"
            print(f"    {d} CPU-s {sym[:70]} ({cat}) {c} vs {b}")
    print("verdict:", end="")
    verdicts = []
    for ds in others:
        hits = []
        bvals = results[(base["idx"], mode)]["cats"]
        cvals = results[(ds["idx"], mode)]["cats"]
        bsamples = results[(base["idx"], mode)]["samples"]
        csamples = results[(ds["idx"], mode)]["samples"]
        for cat in cats:
            bcpu = cpu_of(bvals.get(cat, 0.0), bsamples)
            ccpu = cpu_of(cvals.get(cat, 0.0), csamples)
            dcpu = None if bcpu is None or ccpu is None else ccpu - bcpu
            if flagged(dcpu, bcpu, min_abs, min_rel):
                hits.append(f"{cat} {dcpu:+.2f} CPU-s "
                            f"({fmt_rel(dcpu, bcpu)})")
        if hits:
            verdicts.append(f"[{ds['idx']}]{ds['name']}: " +
                            "; ".join(hits))
        else:
            verdicts.append(f"[{ds['idx']}]{ds['name']}: all categories "
                            f"within noise (±{min_abs:g} CPU-s, "
                            f"±{min_rel:g}%)")
    print(" " + " | ".join(verdicts))


def fmt_stat(value):
    if value is None:
        return "?"
    num, unit = value
    if unit == "msec":
        text = f"{num:,.2f}"
    elif num == int(num):
        text = f"{int(num):,}"
    else:
        text = f"{num:,.2f}"
    return f"{text} {unit}".rstrip() if unit else text


def show_stat(mode, datasets, base_pos, results, min_rel):
    print(f"-- counters ({mode}) --")
    base = datasets[base_pos]
    others = [ds for ds in datasets if ds is not base]
    events = [e for e in STAT_EVENTS
              if any(results[(ds["idx"], mode)]["stat"].get(e) is not None
                     for ds in datasets)]
    if not events:
        print("no counters parsed")
        return
    name_w = max([len("counter")] + [len(e) for e in events])
    val_w = 18
    abs_w = 14
    cells = [" ".join([f"[{o['idx']}]{o['name']}".rjust(val_w),
                       "Δabs".rjust(abs_w), "Δrel".rjust(9)])
             for o in others]
    print(f"{'counter'.ljust(name_w)} "
          f"{f'[{base['idx']}]{base['name']}'.rjust(val_w)} " +
          " ".join(cells))
    for event in events:
        bval = results[(base["idx"], mode)]["stat"].get(event)
        line = f"{event.ljust(name_w)} {fmt_stat(bval).rjust(val_w)}"
        for ds in others:
            cval = results[(ds["idx"], mode)]["stat"].get(event)
            if bval is None or cval is None:
                line += f" {'?'.rjust(val_w)} {'?'.rjust(abs_w)} {'?'.rjust(9)} "
                continue
            delta = cval[0] - bval[0]
            rel = fmt_rel(delta, bval[0])
            counter_hit = (bval[0] > 0
                           and abs(100.0 * delta / bval[0]) >= min_rel)
            star = "*" if counter_hit else " "
            line += f" {fmt_stat(cval).rjust(val_w)}"
            line += f" {f'{delta:+,.2f}'.rjust(abs_w)}"
            line += f" {rel.rjust(8)}{star}"
        print(line)


def main(argv):
    raw, rest = split_argv(argv)
    ap = argparse.ArgumentParser(
        description="Compare perf.data captures by absolute CPU and top "
                    "symbol movers. Dataset 0 is the baseline unless "
                    "--baseline says otherwise.")
    ap.add_argument("--data-dir", default="/tmp",
                    help="directory for TAG shorthand expansion "
                         "(default: /tmp)")
    ap.add_argument("--baseline", type=int, default=0,
                    help="dataset index to compare against (default: 0)")
    ap.add_argument("--top", type=int, default=10,
                    help="top movers shown per dataset (default: 10)")
    ap.add_argument("--mode", choices=("both", "quiet", "music"),
                    default="both", help="which captures to compare")
    ap.add_argument("--min-abs", type=float, default=0.5,
                    help="flag threshold in absolute CPU-seconds "
                         "(default: 0.5)")
    ap.add_argument("--min-rel", type=float, default=10.0,
                    help="flag threshold in relative percent")
    args = ap.parse_args(rest)
    datasets = resolve_datasets(raw, args.data_dir)
    idxs = [ds["idx"] for ds in datasets]
    if args.baseline not in idxs:
        die(f"--baseline {args.baseline} matches no dataset "
            f"(have: {idxs})")
    if shutil.which("perf") is None:
        die("perf not found on PATH (run inside nix develop)")
    for ds in datasets:
        check_dataset(ds)
    modes = ("quiet", "music") if args.mode == "both" else (args.mode,)
    stat_modes = set()
    for mode in modes:
        missing = [ds["name"] for ds in datasets
                   if not (ds.get(f"stat-{mode}") and
                           os.path.isfile(ds[f"stat-{mode}"]))]
        explicit_missing = [ds["name"] for ds in datasets
                            if ds.get(f"stat-{mode}-explicit")
                            and not (ds.get(f"stat-{mode}") and
                                     os.path.isfile(ds[f"stat-{mode}"]))]
        if explicit_missing:
            die(f"{mode} stat file explicitly passed but missing for: "
                f"{', '.join(explicit_missing)}")
        if not missing:
            stat_modes.add(mode)
        elif len(missing) != len(datasets):
            die(f"{mode} stat counters missing for: {', '.join(missing)}; "
                f"recapture with the current profiler or drop the .stat files")
    results = collect("perf", datasets, modes, stat_modes)
    base_pos = idxs.index(args.baseline)
    for mode in modes:
        show_mode(mode, datasets, base_pos, results,
                  args.top, args.min_abs, args.min_rel)
        if mode in stat_modes:
            show_stat(mode, datasets, base_pos, results,
                      args.min_rel)


if __name__ == "__main__":
    main(sys.argv[1:])
