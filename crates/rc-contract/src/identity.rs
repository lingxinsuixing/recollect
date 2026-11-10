use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;

pub fn normalize_body(raw: &str) -> String {
    let lf = raw.replace("\r\n", "\n").replace('\r', "\n");
    let nfc: String = lf.chars().nfc().collect();
    nfc.trim().to_string()
}

pub const UID_PREFIX: &[u8] = b"rc.turn.v1";
const SEP: u8 = 0x1f;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn compute_turn_uid(device_id: &str, session_key: &str, role: &str, body: &str, created_at_ms: i64) -> String {
    let mut h = Sha256::new();
    h.update(UID_PREFIX);
    h.update([SEP]);
    h.update(device_id.as_bytes());
    h.update([SEP]);
    h.update(session_key.as_bytes());
    h.update([SEP]);
    h.update(role.as_bytes());
    h.update([SEP]);
    h.update(body.as_bytes());
    h.update([SEP]);
    h.update(created_at_ms.to_string().as_bytes());
    hex(&h.finalize())
}

pub fn compute_body_fp(body: &str) -> String {
    hex(&Sha256::digest(body.as_bytes()))
}
