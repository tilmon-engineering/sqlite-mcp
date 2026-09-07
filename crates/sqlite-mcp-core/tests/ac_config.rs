use sqlite_mcp_core::Config;

#[test]
fn example_config_loads() {
    let text = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../config.example.toml"
    ))
    .unwrap();
    let config: Config = toml::from_str(&text).unwrap();
    config.validate().unwrap();
    assert_eq!(config.max_handles, 32);
    assert_eq!(config.queue_capacity, 16);
    assert_eq!(config.query_timeout_ms, 30_000);
}

#[test]
fn config_validation_and_unknown_keys() {
    let mk = |f: fn(&mut Config)| {
        let mut c = Config::default();
        f(&mut c);
        c
    };
    assert!(mk(|c| c.max_handles = 0).validate().is_err());
    assert!(mk(|c| c.queue_capacity = 0).validate().is_err());
    assert!(mk(|c| c.query_timeout_ms = 0).validate().is_err());
    assert!(mk(|c| c.max_handles = 1025).validate().is_err());
    assert!(mk(|c| c.queue_capacity = 4097).validate().is_err());
    assert!(toml::from_str::<Config>("unknown_key = 1").is_err());
}
