use shadowquic::config::parse_duration_str;

#[cfg(test)]
mod tests {
    use super::parse_duration_str;

    #[test]
    fn duration_plain_integer() {
        assert_eq!(parse_duration_str("3123").unwrap(), Some(3123));
        assert_eq!(parse_duration_str("0").unwrap(), Some(0));
        assert_eq!(parse_duration_str("  42  ").unwrap(), Some(42));
    }

    #[test]
    fn duration_days_suffix() {
        assert_eq!(parse_duration_str("1d").unwrap(), Some(86_400_000));
        assert_eq!(parse_duration_str("0.1d").unwrap(), Some(8_640_000));
        assert_eq!(parse_duration_str("2d").unwrap(), Some(172_800_000));
    }

    #[test]
    fn duration_hours_suffix() {
        assert_eq!(parse_duration_str("1h").unwrap(), Some(3_600_000));
        assert_eq!(parse_duration_str("0.5h").unwrap(), Some(1_800_000));
        assert_eq!(parse_duration_str("2h").unwrap(), Some(7_200_000));
    }

    #[test]
    fn duration_minutes_suffix() {
        assert_eq!(parse_duration_str("1m").unwrap(), Some(60_000));
        assert_eq!(parse_duration_str("2.5m").unwrap(), Some(150_000));
        assert_eq!(parse_duration_str("0.5m").unwrap(), Some(30_000));
        assert_eq!(parse_duration_str("10m").unwrap(), Some(600_000));
        assert_eq!(parse_duration_str("1.25m").unwrap(), Some(75_000));
    }

    #[test]
    fn duration_seconds_suffix() {
        assert_eq!(parse_duration_str("30s").unwrap(), Some(30000));
        assert_eq!(parse_duration_str("1s").unwrap(), Some(1000));
        assert_eq!(parse_duration_str("1.5s").unwrap(), Some(1500));
    }

    #[test]
    fn duration_millis_suffix() {
        assert_eq!(parse_duration_str("500ms").unwrap(), Some(500));
        assert_eq!(parse_duration_str("1ms").unwrap(), Some(1));
    }

    #[test]
    fn duration_empty_is_none() {
        assert_eq!(parse_duration_str("").unwrap(), None);
        assert_eq!(parse_duration_str("   ").unwrap(), None);
    }

    #[test]
    fn duration_max_u32_ok() {
        assert_eq!(parse_duration_str("4294967295").unwrap(), Some(u32::MAX));
    }

    #[test]
    fn duration_reject_invalid() {
        assert!(parse_duration_str("-1").is_err());
        assert!(parse_duration_str("4294967296").is_err());
        assert!(parse_duration_str("5000000s").is_err());
        assert!(parse_duration_str("abc").is_err());
        assert!(parse_duration_str("1.2.3s").is_err());
        assert!(parse_duration_str("s").is_err());
        assert!(parse_duration_str("ms").is_err());
        assert!(parse_duration_str("30w").is_err());
    }
}
