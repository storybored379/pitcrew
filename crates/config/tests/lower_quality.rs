use pitcrew_config::Config;

#[test]
fn lower_quality_is_disabled_unless_a_positive_explicit_floor_is_configured() {
    assert!(
        Config::parse("version=1")
            .unwrap()
            .lower_quality
            .minimum_resolution
            .is_none()
    );
    assert_eq!(
        Config::parse("version=1\n[lower_quality]\nminimum_resolution=1080")
            .unwrap()
            .lower_quality
            .minimum_resolution
            .unwrap()
            .get(),
        1080
    );
    for value in ["0", "-1", "\"1080p\"", "4294967296"] {
        assert!(
            Config::parse(&format!(
                "version=1\n[lower_quality]\nminimum_resolution={value}"
            ))
            .is_err()
        );
    }
}
