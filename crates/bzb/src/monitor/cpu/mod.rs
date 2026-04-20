/// Raw tick counters for one CPU core sampled from the kernel.
#[derive(Debug, Clone, Copy, Default)]
pub struct CoreSample {
    pub user: u64,
    pub system: u64,
    pub idle: u64,
    pub nice: u64,
}

impl CoreSample {
    pub fn total(&self) -> u64 {
        self.user + self.system + self.idle + self.nice
    }
    pub fn active(&self) -> u64 {
        self.user + self.system + self.nice
    }
}

/// Convert a pair of samples (previous, current) into 0–100% usage.
pub fn usage_percent(prev: CoreSample, curr: CoreSample) -> u8 {
    let total = curr.total().saturating_sub(prev.total());
    if total == 0 {
        return 0;
    }
    let active = curr.active().saturating_sub(prev.active());
    ((active * 100) / total).min(100) as u8
}

#[cfg(target_os = "macos")]
pub use macos::sample;
#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "linux")]
pub use linux::sample;
#[cfg(target_os = "linux")]
mod linux;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_total_delta_is_zero_usage() {
        let s = CoreSample {
            user: 10,
            system: 5,
            idle: 100,
            nice: 0,
        };
        assert_eq!(usage_percent(s, s), 0);
    }

    #[test]
    fn half_active_half_idle_is_fifty() {
        let prev = CoreSample::default();
        let curr = CoreSample {
            user: 50,
            system: 0,
            idle: 50,
            nice: 0,
        };
        assert_eq!(usage_percent(prev, curr), 50);
    }

    #[test]
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn sample_returns_non_empty_on_real_hardware() {
        let v = sample();
        assert!(!v.is_empty(), "expected at least one CPU core");
    }
}
