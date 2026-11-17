use borealis::scheduler::events::parse_duration;

#[test]
fn duration_unit_conversion_rejects_overflow() {
    for (unit, factor) in [('m', 60u64), ('h', 3600)] {
        let largest = u64::MAX / factor;
        assert_eq!(
            parse_duration(&format!("{largest}{unit}"))
                .unwrap()
                .as_secs(),
            largest * factor
        );
        assert!(parse_duration(&format!("{}{unit}", largest + 1)).is_err());
        assert!(parse_duration(&format!("{}{unit}", u64::MAX)).is_err());
    }
    assert_eq!(
        parse_duration(&format!("{}s", u64::MAX)).unwrap().as_secs(),
        u64::MAX
    );
}
