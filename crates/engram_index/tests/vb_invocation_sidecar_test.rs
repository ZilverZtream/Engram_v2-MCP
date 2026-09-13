#![allow(clippy::unwrap_used)]

use sha2::{Digest, Sha256};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn sidecar_path() -> Option<PathBuf> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()?
        .parent()?
        .join("tools/vb_roslyn_sidecar/publish_out")
        .join(if cfg!(windows) {
            "vb_roslyn_sidecar.exe"
        } else {
            "vb_roslyn_sidecar"
        });
    path.exists().then_some(path)
}

#[test]
fn roslyn_invocation_query_handles_multiline_named_arguments_and_ignores_text() {
    let Some(binary) = sidecar_path() else { return };
    let source = r#"Class Sample
    Private Sub Save()
        Dim fake = "AuditTrail.Record(category := EventPrefix.Fake)"
        ' AuditTrail.Record(category := EventPrefix.Comment)
        AuditTrail.Record(
            value := "a""b",
            category := EventPrefix.Widget)
    End Sub
End Class
"#;
    let mut child = Command::new(binary)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    writeln!(stdin, "{}", serde_json::json!({
        "cmd": "invocations",
        "path": "Sample.vb",
        "source": source,
        "request_id": "fixture-request"
    }))
    .unwrap();
    stdin.flush().unwrap();
    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();
    let response: serde_json::Value = serde_json::from_str(&line).unwrap();
    child.kill().unwrap();

    let report = &response["invocation_report"];
    assert_eq!(report["request_id"], "fixture-request");
    assert_eq!(report["scope"], "full_source");
    assert_eq!(report["parse_status"], "complete");
    assert_eq!(report["parse_error_count"], 0);
    assert_eq!(report["truncated"], false);
    assert_eq!(
        report["source_sha256"],
        format!("{:x}", Sha256::digest(source.as_bytes()))
    );
    let calls = report["invocations"].as_array().unwrap();
    assert_eq!(calls.len(), 1, "{response:#}");
    assert_eq!(calls[0]["start_line"], 5);
    assert_eq!(calls[0]["end_line"], 7);
    let arguments = calls[0]["arguments"].as_array().unwrap();
    assert_eq!(arguments[0]["name"], "value");
    assert_eq!(arguments[0]["classification"], "string_literal");
    assert_eq!(arguments[1]["name"], "category");
    assert_eq!(arguments[1]["classification"], "member_access");
    assert_eq!(arguments[1]["start_line"], 7);
}
