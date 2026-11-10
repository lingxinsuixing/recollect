use rc_net::ingest::{assemble, decode_body, decode_body_limited, DecodeError};

fn line(session: &str, device: &str, body: &str) -> String {
    format!(
        r#"{{"format_version":1,"sessions":[{{"session_key":"{session}","device_id":"{device}","source_agent":"qoder","turns":[{{"role":"human","body":"{body}","created_at_ms":1}}]}}]}}"#
    )
}

fn gzip(bytes: &[u8]) -> Vec<u8> {
    use flate2::write::GzEncoder;
    use std::io::Write;
    let mut enc = GzEncoder::new(Vec::new(), flate2::Compression::fast());
    enc.write_all(bytes).unwrap();
    enc.finish().unwrap()
}

#[test]
fn one_exchange_file_per_line_and_line_numbers_survive_rejects() {
    let body = format!(
        "{}\nnot json\n{}\n",
        line("s-1", "dev-A", "one"),
        line("s-2", "dev-A", "two"),
    );
    let batch = assemble(body.as_bytes(), "dev-A");
    assert_eq!(batch.lines_in, 3);
    assert_eq!(batch.lines_accepted, 2);
    assert_eq!(batch.file.sessions.len(), 2);
    assert_eq!(batch.file.sessions[0].session_key, "s-1");
    assert_eq!(batch.file.sessions[1].session_key, "s-2");
    assert_eq!(batch.line_of_session, vec![1, 3], "会话下标映射回行号错了");
    assert_eq!(batch.devices, vec!["dev-A", "dev-A"]);
    assert_eq!(batch.rejects.len(), 1);
    assert_eq!(batch.rejects[0].line, 2);
    assert_eq!(batch.rejects[0].turn, None);
    assert_eq!(batch.rejects[0].code, "json_malformed");
}

#[test]
fn device_mismatch_is_a_line_reject_and_never_folds_into_the_batch() {
    let body = format!(
        "{}\n{}\n",
        line("s-1", "dev-other", "x"),
        line("s-2", "dev-A", "y"),
    );
    let batch = assemble(body.as_bytes(), "dev-A");
    assert_eq!(batch.lines_accepted, 1);
    assert_eq!(batch.file.sessions.len(), 1, "外来设备的会话被并进了批");
    assert_eq!(batch.file.sessions[0].session_key, "s-2");
    assert_eq!(batch.rejects[0].code, "device_mismatch");
    assert_eq!(batch.rejects[0].turns, 1);
    assert_eq!(batch.rejected_turns(), 1);
    assert_eq!(batch.signature_rejected_turns(), 1);
}

#[test]
fn duplicate_session_key_is_refused_on_the_second_line() {
    let body = format!(
        "{}\n{}\n",
        line("s-1", "dev-A", "x"),
        line("s-1", "dev-A", "y"),
    );
    let batch = assemble(body.as_bytes(), "dev-A");
    assert_eq!(batch.lines_in, 2);
    assert_eq!(batch.lines_accepted, 1);
    assert_eq!(batch.file.sessions.len(), 1);
    assert_eq!(batch.file.sessions[0].turns.len(), 1);
    assert_eq!(batch.rejects[0].code, "duplicate_session");
    assert_eq!(batch.rejects[0].line, 2);
    assert_eq!(batch.rejects[0].detail, "s-1");
}

#[test]
fn a_line_must_hold_exactly_one_session() {
    let two = r#"{"format_version":1,"sessions":[{"session_key":"a","device_id":"dev-A","source_agent":"q","turns":[]},{"session_key":"b","device_id":"dev-A","source_agent":"q","turns":[]}]}"#;
    let batch = assemble(two.as_bytes(), "dev-A");
    assert_eq!(batch.lines_in, 1);
    assert_eq!(batch.lines_accepted, 0);
    assert_eq!(batch.rejects[0].code, "line_malformed");
    assert_eq!(batch.rejects[0].detail, "sessions");
}

#[test]
fn line_reject_details_never_carry_body_or_path() {
    // 字段类型错：serde 的原串会把值抄出来（这里是一段像路径的文本），detail 只许留类别+位置。
    let bad = r#"{"format_version":1,"sessions":[{"session_key":"s","device_id":"dev-A","source_agent":"q","turns":[{"role":"human","body":"CANARY-INGEST-77","created_at_ms":"C:/Users/x/.ssh/id_rsa"}]}]}"#;
    let batch = assemble(bad.as_bytes(), "dev-A");
    assert_eq!(batch.rejects.len(), 1);
    let detail = &batch.rejects[0].detail;
    assert!(detail.starts_with("malformed json:"), "{detail}");
    assert!(!detail.contains("CANARY-INGEST-77"), "detail 带上了正文：{detail}");
    assert!(!detail.contains("id_rsa"), "detail 带上了值：{detail}");

    // 未知字段：键名是路径形状 ⇒ 必须换成占位符，只回"unknown field"。
    let unknown = r#"{"format_version":1,"sessions":[{"session_key":"s","device_id":"dev-A","source_agent":"q","turns":[{"role":"human","body":"x","created_at_ms":1,"C:/Users/x/.ssh/id_rsa":null}]}]}"#;
    let batch = assemble(unknown.as_bytes(), "dev-A");
    assert_eq!(batch.rejects[0].code, "json_malformed");
    assert_eq!(batch.rejects[0].detail, "unknown field: (redacted)");
}

#[test]
fn whitespace_only_lines_are_skipped_not_rejected() {
    let body = format!("\n   \n{}\n\r\n", line("s-1", "dev-A", "x"));
    let batch = assemble(body.as_bytes(), "dev-A");
    assert_eq!(batch.lines_in, 1);
    assert_eq!(batch.lines_accepted, 1);
    assert!(batch.rejects.is_empty(), "{:?}", batch.rejects);
}

#[test]
fn gzip_round_trip_matches_identity_bytes() {
    let plain = format!("{}\n", line("s-1", "dev-A", "x"));
    let wire = gzip(plain.as_bytes());
    assert_eq!(decode_body(Some("gzip"), &wire).unwrap(), plain.as_bytes());
    assert_eq!(decode_body(None, plain.as_bytes()).unwrap(), plain.as_bytes());
    assert_eq!(decode_body(Some("identity"), plain.as_bytes()).unwrap(), plain.as_bytes());
}

#[test]
fn decompressed_size_cap_is_enforced_while_decoding() {
    let plain = vec![b'a'; 64];
    let wire = gzip(&plain);
    assert!(wire.len() < plain.len(), "压缩没生效，这条用例测不到解压路径");
    assert_eq!(
        decode_body_limited(Some("gzip"), &wire, 32),
        Err(DecodeError::TooLarge),
        "解压上限没在解码途中截住"
    );
    assert_eq!(decode_body_limited(Some("gzip"), &wire, 64).unwrap(), plain);
    assert_eq!(decode_body_limited(None, &plain, 63), Err(DecodeError::TooLarge));
}

#[test]
fn unknown_content_encoding_is_refused() {
    assert_eq!(
        decode_body_limited(Some("br"), b"x", 16),
        Err(DecodeError::UnsupportedEncoding)
    );
}
