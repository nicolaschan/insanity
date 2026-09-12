struct Interval {
    stamp: String,
    gaps: usize,
    late: usize,
    underruns: usize,
    plc: usize,
    clips: usize,
    fills: usize,
    fill_avg_ns: u64,
    peers: usize,
}

fn field(message: &str, key: &str) -> usize {
    message
        .split_whitespace()
        .filter_map(|token| token.split_once('='))
        .find(|(k, _)| *k == key)
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0)
}

fn parse_interval(line: &str) -> Option<Interval> {
    let (prefix, _) = line.split_once("] audio ")?;
    let (_, message) = line.split_once("] audio ")?;
    Some(Interval {
        stamp: prefix.to_string(),
        gaps: field(message, "gaps"),
        late: field(message, "late"),
        underruns: field(message, "underruns"),
        plc: field(message, "plc"),
        clips: field(message, "clips"),
        fills: field(message, "fills"),
        fill_avg_ns: field(message, "fill_avg_ns") as u64,
        peers: field(message, "peers"),
    })
}

fn underruns_per_fill(iv: &Interval) -> f64 {
    iv.underruns as f64 / iv.fills.max(1) as f64
}

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().unwrap_or_else(|| {
        eprintln!("usage: log_audio_check <insanity.log>");
        std::process::exit(2);
    });
    let text = std::fs::read_to_string(&path).expect("read log");
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
        if iv.underruns > 0 && iv.gaps == 0 && iv.late == 0 {
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
            "{} underruns_per_fill={:.2} gaps={} late={} plc={} clips={} fills={} fill_avg_ns={} peers={} {}",
            iv.stamp,
            underruns_per_fill(iv),
            iv.gaps,
            iv.late,
            iv.plc,
            iv.clips,
            iv.fills,
            iv.fill_avg_ns,
            iv.peers,
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
    use super::{parse_interval, underruns_per_fill};

    #[test]
    fn parses_audio_line() {
        let line = "[2026-09-04 10:18:50][INFO][insanity_native_tui_app::connection_manager] audio gaps=0 late=0 underruns=291264 plc=291264 clips=495 fills=234 fill_avg_ns=256299 peers=1";
        let iv = parse_interval(line).expect("parse");
        assert_eq!(iv.gaps, 0);
        assert_eq!(iv.underruns, 291264);
        assert_eq!(iv.plc, 291264);
        assert_eq!(iv.clips, 495);
        assert_eq!(iv.fills, 234);
        assert_eq!(iv.fill_avg_ns, 256299);
        assert_eq!(iv.peers, 1);
    }

    #[test]
    fn summary_reports_underruns_per_fill() {
        let iv = parse_interval("[2026-09-04 10:18:50][INFO][m] audio gaps=1 late=0 underruns=6 plc=5760 clips=0 fills=3 fill_avg_ns=9 peers=0").expect("parse");
        assert_eq!(format!("{:.2}", underruns_per_fill(&iv)), "2.00");
    }

    #[test]
    fn parses_empty_peers_and_rejects_garbage() {
        let line = "[2026-09-04 10:18:30][INFO][x] audio gaps=0 late=0 underruns=0 plc=0 clips=0 fills=0 fill_avg_ns=0 peers=0";
        let iv = parse_interval(line).expect("parse");
        assert_eq!(iv.peers, 0);
        assert!(parse_interval("nope").is_none());
    }
}
