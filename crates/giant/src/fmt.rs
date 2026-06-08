//! Small shared formatting helpers used by porcelains' renderers.

/// Human-readable elapsed time: `ms` under a second, `s` under a minute,
/// `m` beyond. Stable across the build renderer and the task renderer.
pub fn format_duration(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.2}s", ms as f64 / 1000.0)
    } else {
        format!("{:.1}m", ms as f64 / 60_000.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_duration_chooses_right_unit() {
        assert_eq!(format_duration(0), "0ms");
        assert_eq!(format_duration(7), "7ms");
        assert_eq!(format_duration(999), "999ms");
        assert_eq!(format_duration(1_000), "1.00s");
        assert_eq!(format_duration(1_240), "1.24s");
        assert_eq!(format_duration(59_999), "60.00s");
        assert_eq!(format_duration(60_000), "1.0m");
        assert_eq!(format_duration(192_000), "3.2m");
    }
}
