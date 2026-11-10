use rc_net::error::NetError;
use rc_net::key::HubKey;
use rc_net::protocol::manifest_canonical;
use rc_net::snapshot::{accept_manifest, AcceptOutcome, Manifest};

const SHA_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const SHA_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn hub() -> (tempfile::TempDir, HubKey) {
    let dir = tempfile::tempdir().unwrap();
    let key = HubKey::load_or_create(&dir.path().join("hub.key")).unwrap();
    (dir, key)
}

fn manifest(key: &HubKey, epoch: u64, seq: u64, sha: &str, schema: i32, norm: u32) -> Manifest {
    let signature = key.sign_b64(manifest_canonical(epoch, seq, sha, schema, norm).as_bytes());
    Manifest {
        epoch,
        seq,
        sha256: sha.to_string(),
        schema_version: schema,
        norm_version: norm,
        bytes: 4096,
        created_at_ms: 7,
        signature,
    }
}

#[test]
fn the_first_manifest_is_new() {
    let (_dir, key) = hub();
    let m = manifest(&key, 7, 1, SHA_A, 6, 1);
    assert_eq!(
        accept_manifest(&key.public_b64(), &m, None, 1).unwrap(),
        AcceptOutcome::New
    );
}

#[test]
fn a_forged_signature_is_rejected_before_any_other_field() {
    let (_dir, key) = hub();
    let (_other_dir, other) = hub();
    // schema/norm 也故意是非法值：若实现先比它们，返回的会是 Schema/Norm 而不是 Signature。
    let forged = manifest(&other, 7, 1, SHA_A, 99, 99);
    assert!(matches!(
        accept_manifest(&key.public_b64(), &forged, None, 1),
        Err(NetError::Signature)
    ));
}

#[test]
fn a_schema_outside_the_supported_range_is_rejected() {
    let (_dir, key) = hub();
    for schema in [5, 7] {
        let m = manifest(&key, 7, 1, SHA_A, schema, 1);
        assert!(
            matches!(
                accept_manifest(&key.public_b64(), &m, None, 1),
                Err(NetError::Schema)
            ),
            "schema {schema} 被收下了"
        );
    }
}

#[test]
fn a_different_norm_version_is_rejected() {
    let (_dir, key) = hub();
    let m = manifest(&key, 7, 1, SHA_A, 6, 2);
    assert!(matches!(
        accept_manifest(&key.public_b64(), &m, None, 1),
        Err(NetError::Norm)
    ));
}

#[test]
fn a_malformed_sha_is_rejected_as_malformed_state() {
    let (_dir, key) = hub();
    for bad in ["".to_string(), "zz".to_string(), "A".repeat(64)] {
        let m = manifest(&key, 7, 1, &bad, 6, 1);
        assert!(
            matches!(
                accept_manifest(&key.public_b64(), &m, None, 1),
                Err(NetError::SnapshotState)
            ),
            "{bad:?} 被当成合法 sha"
        );
    }
}

#[test]
fn a_newer_epoch_is_new() {
    let (_dir, key) = hub();
    let current = manifest(&key, 7, 5, SHA_A, 6, 1);
    let next = manifest(&key, 8, 1, SHA_B, 6, 1);
    assert_eq!(
        accept_manifest(&key.public_b64(), &next, Some(&current), 1).unwrap(),
        AcceptOutcome::New
    );
}

#[test]
fn an_older_epoch_is_stale() {
    let (_dir, key) = hub();
    let current = manifest(&key, 8, 1, SHA_A, 6, 1);
    let old = manifest(&key, 7, 9, SHA_B, 6, 1);
    assert!(matches!(
        accept_manifest(&key.public_b64(), &old, Some(&current), 1),
        Err(NetError::StaleEpoch)
    ));
}

#[test]
fn a_newer_seq_in_the_same_epoch_is_new() {
    let (_dir, key) = hub();
    let current = manifest(&key, 8, 1, SHA_A, 6, 1);
    let next = manifest(&key, 8, 2, SHA_B, 6, 1);
    assert_eq!(
        accept_manifest(&key.public_b64(), &next, Some(&current), 1).unwrap(),
        AcceptOutcome::New
    );
}

#[test]
fn an_older_seq_is_stale() {
    let (_dir, key) = hub();
    let current = manifest(&key, 8, 2, SHA_A, 6, 1);
    let old = manifest(&key, 8, 1, SHA_B, 6, 1);
    assert!(matches!(
        accept_manifest(&key.public_b64(), &old, Some(&current), 1),
        Err(NetError::StaleSeq)
    ));
}

#[test]
fn the_same_epoch_seq_and_bytes_is_already_current() {
    let (_dir, key) = hub();
    let current = manifest(&key, 8, 2, SHA_A, 6, 1);
    let same = manifest(&key, 8, 2, SHA_A, 6, 1);
    assert_eq!(
        accept_manifest(&key.public_b64(), &same, Some(&current), 1).unwrap(),
        AcceptOutcome::AlreadyCurrent
    );
}

#[test]
fn the_same_epoch_and_seq_with_other_bytes_is_a_conflict() {
    let (_dir, key) = hub();
    let current = manifest(&key, 8, 2, SHA_A, 6, 1);
    let other = manifest(&key, 8, 2, SHA_B, 6, 1);
    assert!(matches!(
        accept_manifest(&key.public_b64(), &other, Some(&current), 1),
        Err(NetError::EpochSeqConflict)
    ));
}
