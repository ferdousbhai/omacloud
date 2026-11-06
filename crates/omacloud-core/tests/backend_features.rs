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

/// Upstream's Dropbox service has neither: it wants an app secret, and overwrites
/// on every write. Without the vendored copy, this fails only at someone's browser.
#[test]
fn dropbox_is_the_patched_service() -> anyhow::Result<()> {
    let op = Operator::via_iter(
        "dropbox",
        [
            ("root".to_string(), "/test".to_string()),
            ("client_id".to_string(), "app".to_string()),
            ("refresh_token".to_string(), "token".to_string()),
        ],
    )?;
    assert!(op.info().capability().write_with_if_not_exists);
    Ok(())
}
