use super::CoreSample;

/// Parse `/proc/stat` lines starting with `cpuN` (per-core) into CoreSamples.
/// Skips the aggregate `cpu` line.
pub fn sample() -> Vec<CoreSample> {
    let Ok(contents) = std::fs::read_to_string("/proc/stat") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in contents.lines() {
        if !line.starts_with("cpu") {
            break;
        }
        // Skip aggregate "cpu " line (second char is space).
        let rest = &line[3..];
        if rest.starts_with(' ') {
            continue;
        }
        let mut parts = line.split_whitespace();
        parts.next(); // cpuN label
        let user: u64 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        let nice: u64 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        let system: u64 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        let idle: u64 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        out.push(CoreSample {
            user,
            system,
            idle,
            nice,
        });
    }
    out
}
