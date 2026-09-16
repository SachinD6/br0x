//! Memory sampler: reads Linux pressure without GTK.
//! Pure parse plus one thin file read, kept separate for tests.

use crate::tab::SysState;

/// Parse mem used percent from `/proc/meminfo` text.
/// Uses MemTotal and MemAvailable. Returns 0.0 on parse failure
/// so a bad sample never triggers a mass park.
pub fn parse_mem_used_percent(meminfo: &str) -> f64 {
    let total = field_kb(meminfo, "MemTotal");
    let avail = field_kb(meminfo, "MemAvailable");
    match (total, avail) {
        (Some(t), Some(a)) if t > 0 && a <= t => (t - a) as f64 / t as f64 * 100.0,
        _ => 0.0,
    }
}

fn field_kb(meminfo: &str, name: &str) -> Option<u64> {
    meminfo.lines().find_map(|line| {
        let rest = line.strip_prefix(name)?;
        let num: String = rest.chars().filter(|c| c.is_ascii_digit()).collect();
        num.parse().ok()
    })
}

/// Read current pressure. `tab_count` comes from the shell tab strip.
pub fn sample(tab_count: usize) -> SysState {
    let text = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
    SysState { tab_count, mem_used_percent: parse_mem_used_percent(&text) }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str =
        "MemTotal:        7480420 kB\nMemFree:          829112 kB\nMemAvailable:    1803996 kB\n";

    #[test]
    fn parses_used_percent() {
        let pct = parse_mem_used_percent(SAMPLE);
        assert!(pct > 70.0 && pct < 80.0, "got {pct}");
    }

    #[test]
    fn bad_input_returns_zero() {
        assert_eq!(parse_mem_used_percent("garbage"), 0.0);
    }
}
