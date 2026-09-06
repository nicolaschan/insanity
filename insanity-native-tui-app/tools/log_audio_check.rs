use insanity_native_tui_app::audio::buffer_starved;

struct Interval {
    stamp: String,
    gaps: usize,
    late: usize,
    underruns: usize,
    plc: usize,
    clips: usize,
    fills: usize,
    fill_avg_ns: u64,
    occupancies: Vec<(String, usize)>,
}

fn parse_value(parts: &[&str], key: &str) -> Option<usize> {
    parts
        .iter()
        .find_map(|p| p.strip_prefix(&format!("{key}=")))
        .and_then(|v| v.parse().ok())
}

fn parse_value_u64(parts: &[&str], key: &str) -> Option<u64> {
    parts
        .iter()
        .find_map(|p| p.strip_prefix(&format!("{key}=")))
        .and_then(|v| v.parse().ok())
}

fn parse_interval(line: &str) -> Option<Interval> {
    let (prefix, message) = line.split_once("] audio ")?;
    let stamp = prefix.to_string();
    let parts: Vec<&str> = message.split_whitespace().collect();
    let peers_raw = parts
        .iter()
        .find_map(|p| p.strip_prefix("peers=["))
        .unwrap_or("")
        .strip_suffix(']')
        .unwrap_or("");
    let mut occupancies = Vec::new();
    if !peers_raw.is_empty() {
        for entry in peers_raw.split(' ') {
            let (id, len) = entry.split_at(entry.rfind(':')?);
            occupancies.push((id.to_string(), len[1..].parse().ok()?));
        }
    }
    Some(Interval {
        stamp,
        gaps: parse_value(&parts, "gaps")?,
        late: parse_value(&parts, "late")?,
        underruns: parse_value(&parts, "underruns")?,
        plc: parse_value(&parts, "plc")?,
        clips: parse_value(&parts, "clips")?,
        fills: parse_value(&parts, "fills")?,
        fill_avg_ns: parse_value_u64(&parts, "fill_avg_ns")?,
        occupancies,
    })
}

fn parse_jitter_chunks(line: &str) -> Option<usize> {
    line.split_once("jitter_chunks=")?
        .1
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

fn underruns_per_fill(iv: &Interval) -> f64 {
    iv.underruns as f64 / iv.fills.max(1) as f64
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mut path: Option<String> = None;
    let mut capacity_override: Option<usize> = None;
    while let Some(arg) = args.next() {
        if arg == "--capacity" {
            capacity_override = args.next().and_then(|v| v.parse().ok());
        } else {
            path = Some(arg);
        }
    }
    let path = path.unwrap_or_else(|| {
        eprintln!("usage: log_audio_check <insanity.log> [--capacity N]");
        std::process::exit(2);
    });
    let text = std::fs::read_to_string(&path).expect("read log");
    let mut capacity = capacity_override;
    if capacity.is_none() {
        for line in text.lines() {
            if line.contains("Audio formats:")
                && let Some(n) = parse_jitter_chunks(line)
            {
                capacity = Some(n);
                break;
            }
        }
    }
    let intervals: Vec<Interval> = text
        .lines()
        .filter(|l| l.contains("] audio gaps="))
        .filter_map(parse_interval)
        .collect();
    if intervals.is_empty() {
        eprintln!("log_audio_check: no audio metric lines in {path}");
        std::process::exit(2);
    }
    let mut starved_intervals = 0usize;
    for (i, iv) in intervals.iter().enumerate() {
        let mut flags = Vec::new();
        if i == 0 {
            flags.push("BASELINE");
        }
        if let Some(cap) = capacity
            && buffer_starved(iv.underruns, &iv.occupancies, cap)
        {
            flags.push("STARVED");
            starved_intervals += 1;
        }
        if iv.gaps > 0 {
            flags.push("GAPS");
        }
        if iv.clips > 0 {
            flags.push("CLIPS");
        }
        eprintln!(
            "{} underruns_per_fill={:.2} gaps={} late={} plc={} clips={} fills={} fill_avg_ns={} occ={:?} {}",
            iv.stamp,
            underruns_per_fill(iv),
            iv.gaps,
            iv.late,
            iv.plc,
            iv.clips,
            iv.fills,
            iv.fill_avg_ns,
            iv.occupancies
                .iter()
                .map(|(_, n)| *n)
                .collect::<Vec<usize>>(),
            flags.join(",")
        );
    }
    if starved_intervals > 0 {
        eprintln!("log_audio_check: STARVED in {starved_intervals} interval(s)");
        std::process::exit(1);
    }
    eprintln!("log_audio_check: ok, {} interval(s)", intervals.len());
}

#[cfg(test)]
mod tests {
    use super::{parse_interval, parse_jitter_chunks, underruns_per_fill};

    #[test]
    fn parses_audio_line() {
        let line = "[2026-09-04 10:18:50][INFO][insanity_native_tui_app::connection_manager] audio gaps=0 late=0 underruns=291264 plc=291264 clips=495 fills=234 fill_avg_ns=256299 peers=[36f72d6a:3]";
        let iv = parse_interval(line).expect("parse");
        assert_eq!(iv.gaps, 0);
        assert_eq!(iv.underruns, 291264);
        assert_eq!(iv.plc, 291264);
        assert_eq!(iv.clips, 495);
        assert_eq!(iv.fills, 234);
        assert_eq!(iv.fill_avg_ns, 256299);
        assert_eq!(iv.occupancies, vec![("36f72d6a".to_string(), 3)]);
    }

    #[test]
    fn summary_reports_underruns_per_fill() {
        let iv = parse_interval("[2026-09-04 10:18:50][INFO][m] audio gaps=1 late=0 underruns=6 plc=5760 clips=0 fills=3 fill_avg_ns=9 peers=[]").expect("parse");
        assert_eq!(format!("{:.2}", underruns_per_fill(&iv)), "2.00");
    }

    #[test]
    fn parses_empty_peers_and_formats_line() {
        let line = "[2026-09-04 10:18:30][INFO][x] audio gaps=0 late=0 underruns=0 plc=0 clips=0 fills=0 fill_avg_ns=0 peers=[]";
        let iv = parse_interval(line).expect("parse");
        assert!(iv.occupancies.is_empty());
        assert!(parse_interval("nope").is_none());
        let fmt =
            "Audio formats: input channels=2 output channels=2 output rate=48000 jitter_chunks=10";
        assert_eq!(parse_jitter_chunks(fmt), Some(10));
        assert_eq!(parse_jitter_chunks("Audio formats: input channels=2"), None);
    }
}
