//! Storage service registration must survive narrowing rustic_backend's features.
use opendal::Operator;

#[test]
fn s3_is_registered() {
    let options = [
        ("bucket", "test-bucket"),
        ("region", "us-east-1"),
        ("access_key_id", "test"),
        ("secret_access_key", "test"),
    ];
    Operator::via_iter(
        "s3",
        options
            .into_iter()
            .map(|(key, value)| (key.to_string(), value.to_string())),
    )
    .unwrap();
}
