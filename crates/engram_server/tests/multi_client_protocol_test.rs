#![cfg(windows)]
#![allow(clippy::unwrap_used)]
//! Exercise the actual proxy executable against an owned live named pipe.
use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct OwnedProxy {
    child: Child,
    reader: Option<std::thread::JoinHandle<()>>,
}
impl Drop for OwnedProxy {
    fn drop(&mut self) {
        drop(self.child.stdin.take());
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() { let _ = reader.join(); }
    }
}

async fn proxy_case(protocol: u32, daemon: bool) -> anyhow::Result<(Vec<u8>, Vec<u8>, bool, String)> {
    use fs2::FileExt;
    use tokio::net::windows::named_pipe::ServerOptions;
    let temporary = tempfile::tempdir()?;
    let root = temporary.path();
    let data = root.join("data");
    std::fs::create_dir(&data)?;
    let pipe = format!(r"\\.\pipe\engram-protocol-{}", uuid::Uuid::new_v4());
    let server = ServerOptions::new().first_pipe_instance(true).create(&pipe)?;
    // Hold the election lock so the explicit in-process client takes run_proxy.
    let lock = std::fs::File::create(data.join(".engram.lock"))?;
    lock.lock_exclusive()?;
    std::fs::write(data.join(".engram.primary"), format!(
        "pid={}\nstarted_at_ms=1\nprotocol_version={protocol}\nsocket={pipe}\nversion=999.0.0\n",
        std::process::id(),
    ))?;
    let config = root.join("config.yaml");
    std::fs::write(&config, format!(
        "data_dir: {}\nallowed_roots: [{}]\nembedding_backend: fts_only\nmulti_client: true\nmulti_client_daemon: {daemon}\nmulti_client_connect_timeout_secs: 5\n",
        serde_json::to_string(&data)?, serde_json::to_string(root)?,
    ))?;
    let exe = std::env::var_os("ENGRAM_PROTOCOL_TEST_EXE")
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_engram_server").into());
    let child = Command::new(exe).env("ENGRAM_CONFIG_PATH", &config)
        .env("ENGRAM_MULTI_CLIENT", "true").env("ENGRAM_MULTI_CLIENT_DAEMON", daemon.to_string())
        .env("TEMP", root).env("TMP", root)
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    let mut owned = OwnedProxy { child, reader: None };
    let request = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n";
    let response = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n";
    let mut stdout = owned.child.stdout.take().unwrap();
    let (sender, receiver) = std::sync::mpsc::channel();
    owned.reader = Some(std::thread::spawn(move || {
        let mut received = vec![0; response.len()];
        let result = stdout.read_exact(&mut received).map(|_| received);
        let _ = sender.send(result);
    }));
    let _ = owned.child.stdin.as_mut().unwrap().write_all(request);
    let mut forwarded = Vec::new();
    if tokio::time::timeout(Duration::from_secs(5), server.connect()).await.is_ok_and(|r| r.is_ok()) {
        let mut server = server;
        let mut buffer = [0; 4096];
        if let Ok(Ok(count)) = tokio::time::timeout(Duration::from_secs(5), server.read(&mut buffer)).await {
            forwarded.extend_from_slice(&buffer[..count]);
            if count > 0 { server.write_all(response).await?; }
        }
        // Wait for the exact response before closing stdin: the proxy ends on
        // either direction's EOF and must have a chance to forward the reply.
        let reply = receiver.recv_timeout(Duration::from_secs(5)).ok().and_then(Result::ok).unwrap_or_default();
        drop(server);
        drop(owned.child.stdin.take());
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while owned.child.try_wait()?.is_none() && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let success = owned.child.try_wait()?.is_some_and(|s| s.success());
        let mut errors = String::new();
        drop(owned.child.stdin.take());
        if owned.child.try_wait()?.is_none() { owned.child.kill()?; owned.child.wait()?; }
        owned.child.stderr.take().unwrap().read_to_string(&mut errors)?;
        drop(owned); drop(lock); temporary.close()?;
        return Ok((forwarded, reply, success, errors));
    }
    drop(server);
    drop(owned.child.stdin.take());
    let status = owned.child.wait()?;
    let mut errors = String::new();
    owned.child.stderr.take().unwrap().read_to_string(&mut errors)?;
    drop(owned); drop(lock); temporary.close()?;
    Ok((forwarded, vec![], status.success(), errors))
}

#[tokio::test]
async fn shared_daemon_refuses_incompatible_protocol_before_forwarding() -> anyhow::Result<()> {
    let (forwarded, response, success, errors) = proxy_case(999, true).await?;
    assert!(forwarded.is_empty(), "incompatible endpoint received {} bytes", forwarded.len());
    assert!(response.is_empty() && !success);
    assert!(errors.contains("protocol v999") && errors.contains("v1") && errors.contains("Restart"), "{errors}");
    Ok(())
}

#[tokio::test]
async fn shared_daemon_accepts_compatible_protocol_across_crate_versions() -> anyhow::Result<()> {
    let (forwarded, response, success, errors) = proxy_case(1, true).await?;
    assert_eq!(forwarded, b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n");
    assert_eq!(response, b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n");
    assert!(success, "{errors}");
    Ok(())
}

#[tokio::test]
async fn in_process_proxy_still_refuses_incompatible_protocol() -> anyhow::Result<()> {
    let (forwarded, _, success, errors) = proxy_case(999, false).await?;
    assert!(forwarded.is_empty() && !success);
    assert!(errors.contains("protocol v999"), "{errors}");
    Ok(())
}
