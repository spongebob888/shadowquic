use shadowquic::config::parse_bps;

#[test]
fn parse_plain_integer() {
    assert_eq!(parse_bps("3123").unwrap(), 3123);
    assert_eq!(parse_bps("0").unwrap(), 0);
    assert_eq!(parse_bps("  42  ").unwrap(), 42);
}

#[test]
fn parse_kilo_suffix() {
    assert_eq!(parse_bps("1K").unwrap(), 1024);
    assert_eq!(parse_bps("1k").unwrap(), 1024);
    assert_eq!(parse_bps("1.5K").unwrap(), 1536);
    assert_eq!(parse_bps("30K").unwrap(), 30 * 1024);
}

#[test]
fn parse_mega_suffix() {
    assert_eq!(parse_bps("1M").unwrap(), 1024 * 1024);
    assert_eq!(parse_bps("1m").unwrap(), 1024 * 1024);
    assert_eq!(parse_bps("1.5M").unwrap(), 1572864);
    assert_eq!(parse_bps("20M").unwrap(), 20 * 1024 * 1024);
}

#[test]
fn parse_giga_suffix() {
    assert_eq!(parse_bps("1G").unwrap(), 1024_u64 * 1024 * 1024);
    assert_eq!(parse_bps("0.5G").unwrap(), 512_u64 * 1024 * 1024);
    assert_eq!(parse_bps("2G").unwrap(), 2_u64 * 1024 * 1024 * 1024);
}

#[test]
fn parse_rounding() {
    assert_eq!(parse_bps("1.25K").unwrap(), 1280);
    assert_eq!(
        parse_bps("1.1K").unwrap(),
        (1.1_f64 * 1024.0).round() as u64
    );
    assert_eq!(
        parse_bps("1.9M").unwrap(),
        (1.9_f64 * 1024.0 * 1024.0).round() as u64
    );
}

#[test]
fn reject_invalid_inputs() {
    assert!(parse_bps("").is_err());
    assert!(parse_bps("   ").is_err());
    assert!(parse_bps("abc").is_err());
    assert!(parse_bps("20T").is_err());
    assert!(parse_bps("M").is_err());
    assert!(parse_bps("-1M").is_err());
    assert!(parse_bps("1.2.3M").is_err());
}
