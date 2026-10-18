//! Storage service registration must survive narrowing rustic_backend's features.
use opendal::Operator;

#[test]
fn default_services_are_registered() {
    for (scheme, options) in [
        ("fs", vec![("root", "/tmp")]),
        (
            "s3",
            vec![
                ("bucket", "test-bucket"),
                ("region", "us-east-1"),
                ("access_key_id", "test"),
                ("secret_access_key", "test"),
            ],
        ),
    ] {
        Operator::via_iter(
            scheme,
            options
                .into_iter()
                .map(|(key, value)| (key.to_string(), value.to_string())),
        )
        .unwrap_or_else(|error| panic!("{scheme}: {error}"));
    }
}

#[cfg(feature = "full-backends")]
#[test]
fn optional_services_are_registered() {
    Operator::via_iter("memory", std::iter::empty::<(String, String)>()).unwrap();
    Operator::via_iter("http", [("endpoint".to_string(), "https://example.com".to_string())]).unwrap();
}
