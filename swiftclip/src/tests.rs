use crate::{model::*, platform::AltTap, storage::Database, sync};
use std::{
    io::{Read, Write},
    net::TcpListener,
    path::PathBuf,
};

#[test]
fn right_alt_tap_and_repeat() {
    let mut alt = AltTap::default();
    assert!(alt.event(0xa5, true, false).swallow);
    assert!(!alt.event(0xa5, true, false).summon);
    let released = alt.event(0xa5, false, false);
    assert!(released.swallow && released.summon);
    assert!(!alt.event(0xa5, false, false).summon);
}
#[test]
fn right_alt_chord_and_altgr_are_preserved() {
    let mut alt = AltTap::default();
    alt.event(0xa5, true, false);
    let chord = alt.event(b'A' as u32, true, false);
    assert!(chord.replay && !chord.swallow);
    assert!(!alt.event(0xa5, false, false).summon);
    assert!(!alt.event(0xa5, true, true).swallow);
    assert!(!alt.event(0xa5, false, false).summon);
}
#[test]
fn exact_hash_and_newline_equivalence_are_separate() {
    assert_eq!(key("中文\r\n🙂"), key("中文\n🙂"));
    assert_ne!(hash("中文\r\n🙂"), hash("中文\n🙂"));
    assert_ne!(key("text "), key("text"));
    assert!(sync::verify_text("你好🙂", Some(&hash("你好🙂").to_lowercase()), 10).is_ok());
    assert!(sync::verify_text("你好🙂", None, 9).is_err());
    assert!(sync::verify_text("x", Some("invalid"), 1024).is_err());
}
#[test]
fn server_prefix_and_names_are_validated() {
    assert_eq!(
        sync::base_url("https://example.com/clipboard")
            .unwrap()
            .join("SyncClipboard.json")
            .unwrap()
            .as_str(),
        "https://example.com/clipboard/SyncClipboard.json"
    );
    assert!(sync::base_url("https://user:pass@example.com").is_err());
    assert!(sync::base_url("file:///etc").is_err());
    for name in ["..", "../x", "%2f", "a\\b", "https://x", "x?y", "x\n"] {
        assert!(!sync::valid_name(name));
    }
    assert!(sync::valid_name("中文 text.txt"));
}
fn temp_dir(label: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "swiftclip-test-{}-{}-{}",
        std::process::id(),
        label,
        now()
    ));
    std::fs::create_dir_all(&p).unwrap();
    p
}
#[tokio::test]
async fn history_protects_conflict_and_searches_literal_symbols() {
    let dir = temp_dir("storage");
    let db = Database::open(&dir.join("history.sqlite3")).unwrap();
    let s = Settings {
        history_limit: 1,
        ..Default::default()
    };
    db.add("已收藏".into(), "local", true, s.clone())
        .await
        .unwrap();
    let id = db
        .add("远端冲突 %_中文🙂".into(), "remote", true, s.clone())
        .await
        .unwrap();
    db.add("应清理".into(), "local", false, s.clone())
        .await
        .unwrap();
    let (rows, total, matched) = db.query("%_".into(), false, 50).await.unwrap();
    assert_eq!(total, 2);
    assert_eq!(matched, 1);
    assert_eq!(rows[0].id, id);
    assert!(rows[0].pinned);
    let record = db.record(id).await.unwrap();
    assert_eq!(record.content, "远端冲突 %_中文🙂");
    db.clear(false).await.unwrap();
    assert_eq!(db.query(String::new(), false, 50).await.unwrap().1, 2);
}
#[test]
fn privacy_gate_and_config_roundtrip() {
    let dir = temp_dir("config");
    let s = Settings {
        privacy: true,
        sync_enabled: true,
        ..Default::default()
    };
    assert!(!s.listening() && !s.syncing());
    s.save(&dir).unwrap();
    assert!(Settings::load(&dir).unwrap().privacy);
    let s = Settings {
        history_enabled: false,
        ..Default::default()
    };
    assert!(!s.listening());
}
fn server(
    responses: Vec<(&'static str, Vec<u8>)>,
) -> (String, std::thread::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/prefix/", listener.local_addr().unwrap());
    let h = std::thread::spawn(move || {
        let mut requests = vec![];
        for (content_type, body) in responses {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(3)))
                .unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 2048];
            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = stream.read(&mut buffer).unwrap();
                if n == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..n]);
            }
            requests.push(String::from_utf8_lossy(&request).to_string());
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(header.as_bytes()).unwrap();
            let _ = stream.write_all(&body);
        }
        requests
    });
    (url, h)
}
#[tokio::test]
async fn http_receives_long_text_and_checks_both_hashes() {
    use sha2::{Digest, Sha256};
    let text = "中文🙂\n保留换行";
    let mut bytes = vec![0xef, 0xbb, 0xbf];
    bytes.extend_from_slice(text.as_bytes());
    let metadata = serde_json::json!({"type":"Text","text":"中文","hasData":true,"dataName":"long.txt","hash":hash(text),"transferDataHash":format!("{:X}",Sha256::digest(&bytes))});
    let (url, handle) = server(vec![
        ("application/json", serde_json::to_vec(&metadata).unwrap()),
        ("text/plain", bytes),
    ]);
    let s = Settings {
        server_url: url,
        username: "test-user".into(),
        ..Default::default()
    };
    let remote = sync::fetch(&sync::client().unwrap(), &s, "test-password")
        .await
        .unwrap();
    assert_eq!(remote.text.as_deref(), Some(text));
    let reqs = handle.join().unwrap();
    assert!(reqs[0].starts_with("GET /prefix/SyncClipboard.json "));
    assert!(reqs[1].starts_with("GET /prefix/file/long.txt "));
    assert!(
        reqs[0]
            .to_ascii_lowercase()
            .contains("authorization: basic ")
    );
}
#[tokio::test]
async fn http_rejects_incomplete_invalid_and_oversized_payloads() {
    for payload in [
        serde_json::json!({"type":"Text","text":"hello","hasData":false,"hash":"BAD"}),
        serde_json::json!({"Type":"Text","text":"hello","hasData":false}),
        serde_json::json!({"type":"Text","text":"hello","hasData":true,"dataName":"../secret"}),
    ] {
        let (url, h) = server(vec![(
            "application/json",
            serde_json::to_vec(&payload).unwrap(),
        )]);
        assert!(
            sync::fetch(
                &sync::client().unwrap(),
                &Settings {
                    server_url: url,
                    ..Default::default()
                },
                ""
            )
            .await
            .is_err()
        );
        h.join().unwrap();
    }
    let (url, h) = server(vec![("application/json", vec![b' '; 70000])]);
    assert!(
        sync::fetch(
            &sync::client().unwrap(),
            &Settings {
                server_url: url,
                max_sync_bytes: 8,
                ..Default::default()
            },
            ""
        )
        .await
        .is_err()
    );
    h.join().unwrap();
}

#[test]
fn configurable_hotkey_requires_a_modifier_and_letter() {
    assert!(crate::platform::parse_hotkey("Ctrl+Alt+J").is_ok());
    assert!(crate::platform::parse_hotkey("J").is_err());
    assert!(crate::platform::parse_hotkey("Alt+Win+J").is_err());
}

#[test]
fn clipboard_write_uses_crlf_without_doubling_existing_crlf() {
    assert_eq!(
        crate::platform::windows_text("first\nsecond\r\nthird"),
        "first\r\nsecond\r\nthird"
    );
    assert_eq!(crate::platform::windows_text("  空白 \t"), "  空白 \t");
}

#[test]
fn clipboard_budget_rejects_overload_and_releases_failed_sends() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let backend = Backend {
        tx,
        data_dir: PathBuf::new(),
        done: Default::default(),
        clipboard_budget: Default::default(),
    };
    backend
        .clipboard_budget
        .store(32 * 1048576, std::sync::atomic::Ordering::Relaxed);
    assert!(!backend.clipboard(1, "new".into(), false));
    assert!(rx.try_recv().is_err());
    backend
        .clipboard_budget
        .store(0, std::sync::atomic::Ordering::Relaxed);
    drop(rx);
    assert!(!backend.clipboard(2, "text".into(), false));
    assert_eq!(
        backend
            .clipboard_budget
            .load(std::sync::atomic::Ordering::Relaxed),
        0
    );
}
#[test]
fn newer_database_schema_is_not_downgraded() {
    let dir = temp_dir("newer-schema");
    let path = dir.join("history.sqlite3");
    let c = rusqlite::Connection::open(&path).unwrap();
    c.execute_batch("PRAGMA user_version=2").unwrap();
    drop(c);
    assert!(Database::open(&path).is_err());
    let c = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(
        c.query_row("PRAGMA user_version", [], |r| r.get::<_, i32>(0))
            .unwrap(),
        2
    );
}
