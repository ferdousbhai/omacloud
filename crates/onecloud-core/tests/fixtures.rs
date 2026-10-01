//! Signed objects for the coordinator service's tests, which verify them in
//! TypeScript. Keeps the two implementations of each signed format in step.
//!
//!   ONECLOUD_FIXTURES=$PWD/service/test/fixtures.json cargo test -p onecloud-core --test fixtures

use std::collections::BTreeMap;

use anyhow::Result;
use ed25519_dalek::SigningKey;
use onecloud_core::{
    Head, JoinRequest, MasterKey, auth,
    devices::{Action, DeviceChain, DeviceEntry, genesis, public_hex},
    epoch::{EpochRecord, EpochSecret, Grant},
};
use serde_json::json;

fn key(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

#[test]
fn write_fixtures() -> Result<()> {
    let Ok(path) = std::env::var("ONECLOUD_FIXTURES") else {
        return Ok(()); // only when asked to
    };
    let (root, a, b) = (key(1), key(2), key(3));
    let mut chain = DeviceChain::new(&public_hex(&root));
    let e1 = genesis(&root, &a, "laptop ünïcode \"quoted\"");
    chain.push(e1.clone())?;
    let e2 = DeviceEntry::next(
        &a,
        chain.last(),
        Action::Add {
            device: public_hex(&b),
            name: "desktop".into(),
        },
    );
    chain.push(e2.clone())?;
    let secret = EpochSecret {
        password: "pw".into(),
        key: MasterKey::new(),
        storage: None,
    };
    let snaps: BTreeMap<String, String> = [
        ("bb".to_string(), "22".to_string()),
        ("aa".into(), "11".into()),
    ]
    .into_iter()
    .collect();
    let rec0 = EpochRecord::next(&a, None, 0, &chain, &secret, snaps)?;
    let h1 = Head::next_in(&a, None, "snap1", 2, 0);
    let h2 = Head::next_in(&b, Some(&h1), "snap2", 2, 0);
    let e3 = DeviceEntry::next(
        &a,
        chain.last(),
        Action::Revoke {
            device: public_hex(&b),
        },
    );
    chain.push(e3.clone())?;
    let grant = Grant::new(&a, 0, &public_hex(&b), &secret)?;
    let join = JoinRequest::new(&b, "desktop", &public_hex(&root));
    let body = br#"{"x":1}"#;
    let authz = auth::header(
        &a,
        "POST",
        "/v1/accounts/abc/heads?x=1",
        1_790_000_000,
        body,
    );

    // a recovery code as people type it: any case, spaces, dashes
    let code = "abcd-EFGH-2345-6789-jkmn-pqrs-tuvw";
    let code_root = public_hex(&onecloud_core::devices::root_key(code)?);
    let out = json!({
        "recovery": {
            "code": code,
            "root": code_root,
            "fingerprint": onecloud_core::fingerprint(&code_root),
        },
        "root": public_hex(&root),
        "a": public_hex(&a),
        "b": public_hex(&b),
        "devices": [e1, e2, e3],
        "device_hashes": [e1.hash(), e2.hash(), e3.hash()],
        "heads": [h1, h2],
        "head_hashes": [h1.hash(), h2.hash()],
        "epoch": rec0,
        "epoch_hash": rec0.hash(),
        "grant": grant,
        "join": join,
        "auth": {
            "method": "POST",
            "path": "/v1/accounts/abc/heads?x=1",
            "ts": 1_790_000_000u64,
            "body": std::str::from_utf8(body)?,
            "header": authz,
        },
    });
    std::fs::write(path, serde_json::to_vec_pretty(&out)?)?;
    Ok(())
}
