use super::CoreSample;

/// Per-core `cpuN` lines of `/proc/stat`; the aggregate `cpu` line is skipped.
pub fn sample() -> Vec<CoreSample> {
    let Ok(contents) = std::fs::read_to_string("/proc/stat") else {
        return Vec::new();
    };
    contents
        .lines()
        .take_while(|line| line.starts_with("cpu"))
        .filter(|line| !line.starts_with("cpu "))
        .map(|line| {
            let mut fields = line
                .split_whitespace()
                .skip(1)
                .map(|f| f.parse::<u64>().unwrap_or(0));
            let mut next = || fields.next().unwrap_or(0);
            // Field order in /proc/stat: user nice system idle.
            CoreSample {
                user: next(),
                nice: next(),
                system: next(),
                idle: next(),
            }
        })
        .collect()
}
