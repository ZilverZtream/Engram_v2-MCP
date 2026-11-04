//! Parent pipe endpoints must be genuinely cancellation-capable. Tokio 1.49
//! Windows ChildStdio wraps blocking anonymous pipes; do not use it here.
use std::{io, path::Path, process::Stdio};
use tokio::process::{Child, Command};

#[cfg(windows)]
pub(super) type Input = tokio::net::windows::named_pipe::NamedPipeServer;
#[cfg(windows)]
pub(super) type Output = tokio::net::windows::named_pipe::NamedPipeServer;
#[cfg(not(windows))]
pub(super) type Input = tokio::process::ChildStdin;
#[cfg(not(windows))]
pub(super) type Output = tokio::process::ChildStdout;
#[cfg(not(windows))]
pub(super) type ErrorOutput = tokio::process::ChildStderr;
#[cfg(windows)]
pub(super) type ErrorOutput = Output;

pub(super) struct Process { pub child: Child, pub stdin: Input, pub stdout: Output, pub stderr: ErrorOutput }

#[cfg(windows)]
pub(super) fn disconnect(stdin: &Input, stdout: &Output, stderr: &ErrorOutput) -> io::Result<()> {
    let mut failure = None;
    for endpoint in [stdin, stdout, stderr] {
        match endpoint.disconnect() {
            Ok(()) => {}
            // Already-broken/disconnected means there is no connection left
            // whose pending write could depend on a descendant reader.
            Err(error) if matches!(error.raw_os_error(), Some(109 | 233)) => {}
            Err(error) => { if failure.is_none() { failure = Some(error); } }
        }
    }
    failure.map_or(Ok(()), Err)
}

#[cfg(not(windows))]
pub(super) fn disconnect(_stdin: &Input, _stdout: &Output, _stderr: &ErrorOutput) -> io::Result<()> { Ok(()) }

#[cfg(windows)]
fn pair(parent_writes: bool) -> io::Result<(Input, std::fs::File)> {
    use std::{hash::BuildHasher, sync::atomic::{AtomicU64, Ordering}};
    use tokio::net::windows::named_pipe::{PipeMode, ServerOptions};
    static NEXT: AtomicU64 = AtomicU64::new(1);
        let nonce = std::collections::hash_map::RandomState::new()
            .hash_one((std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        let name = format!(r"\\.\pipe\engram-vb-{}-{nonce:016x}", std::process::id());
        // ServerOptions creates FILE_FLAG_OVERLAPPED endpoints registered with
        // IOCP. Only the synchronous CLIENT ends become the child's stdio.
        let server = ServerOptions::new().first_pipe_instance(true).max_instances(1)
            .reject_remote_clients(true).pipe_mode(PipeMode::Byte)
            .access_inbound(!parent_writes).access_outbound(parent_writes).create(&name)?;
        let client = std::fs::OpenOptions::new().read(parent_writes).write(!parent_writes).open(&name)?;
        Ok((server, client))
}

#[cfg(windows)]
pub(super) async fn spawn(binary: &Path) -> io::Result<Process> {
    let (stdin, child_stdin) = pair(true)?;
    let (stdout, child_stdout) = pair(false)?;
    let (stderr, child_stderr) = pair(false)?;
    // Clients opened above may already be connected; connect accepts that case.
    // There is no child yet, so connection failure cannot strand a process.
    tokio::try_join!(stdin.connect(), stdout.connect(), stderr.connect())?;
    let mut command = Command::new(binary);
    command.stdin(Stdio::from(child_stdin)).stdout(Stdio::from(child_stdout))
        .stderr(Stdio::from(child_stderr)).kill_on_drop(true);
    let spawned = command.spawn();
    // Do not retain parent copies of child pipe endpoints in the builder.
    drop(command);
    let child = spawned?;
    Ok(Process { child, stdin, stdout, stderr })
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use anyhow::{Result, ensure};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn windows_vb_iocp_pending_io_disconnects_without_peer_eof_or_blocking_helpers() -> Result<()> {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
        let peers = runtime.block_on(async {
            let (mut writer, unread_peer) = pair(true)?;
            let (mut reader, silent_peer) = pair(false)?;
            writer.connect().await?;
            reader.connect().await?;
            let payload = vec![b'x'; 2 * 1024 * 1024];
            // Mio can acknowledge ownership of a whole buffer while its
            // native write is pending. A subsequent write then waits behind it.
            let queued = tokio::time::timeout(std::time::Duration::from_millis(100), writer.write_all(&payload)).await;
            let pending_write = match queued {
                Ok(Ok(())) => tokio::time::timeout(std::time::Duration::from_millis(100), writer.write_all(b"next")).await,
                other => other,
            };
            let mut byte = [0u8];
            let pending_read = tokio::time::timeout(std::time::Duration::from_millis(30), reader.read(&mut byte)).await;
            // Force both connections closed before any assertions can unwind.
            let write_closed = writer.disconnect();
            let read_closed = reader.disconnect();
            drop(writer); drop(reader);
            write_closed?; read_closed?;
            ensure!(pending_write.is_err(), "A nonreading peer must force an actual pending write");
            ensure!(pending_read.is_err(), "A connected silent peer must force an actual pending read");
            Ok::<_, anyhow::Error>((unread_peer, silent_peer))
        });
        let started = std::time::Instant::now();
        // Both peer Files deliberately remain open IN `peers` until after
        // driver shutdown. Teardown cannot rely on clients closing their ends.
        drop(runtime);
        let peers = peers?;
        let elapsed = started.elapsed();
        drop(peers);
        ensure!(elapsed < std::time::Duration::from_secs(2), "IOCP shutdown waited for stalled peers: {elapsed:?}");
        Ok(())
    }
}

#[cfg(not(windows))]
pub(super) async fn spawn(binary: &Path) -> io::Result<Process> {
    let mut child = Command::new(binary).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped())
        .kill_on_drop(true).spawn()?;
    let pipes = (child.stdin.take(), child.stdout.take(), child.stderr.take());
    match pipes {
        (Some(stdin), Some(stdout), Some(stderr)) => Ok(Process { child, stdin, stdout, stderr }),
        _ => {
            let original = io::Error::other("VB child did not expose all owned pipes");
            let pid = child.id().unwrap_or(0);
            super::stop_child(&mut child, pid).await?;
            Err(original)
        }
    }
}
