//! One cancellable process/IO owner. Sync extraction callers never own blocked
//! pipe threads, and shutdown does not depend on descendants closing pipe ends.
use std::{io, path::{Path, PathBuf}, sync::{Arc, LazyLock, Mutex, atomic::{AtomicBool, Ordering}, mpsc}, thread::JoinHandle, time::{Duration, Instant}};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio_util::sync::CancellationToken;
mod stdio;

const SHUTDOWN: Duration = Duration::from_secs(8);
const MAX_REQUEST: usize = 32 * 1024 * 1024;
/// Encode directly into a bounded buffer. Escaped JSON can be much larger
/// than the raw source; reject it before allocating beyond the wire budget or
/// submitting any bytes to a healthy parser child.
pub(super) fn encode_request<T: serde::Serialize>(request: &T) -> serde_json::Result<String> {
    struct Payload(Vec<u8>);
    impl io::Write for Payload {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let required = self.0.len().checked_add(bytes.len())
                .filter(|length| *length <= MAX_REQUEST)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput,
                    "VB request exceeds transport budget"))?;
            if required > self.0.capacity() {
                let capacity = self.0.capacity().saturating_mul(2)
                    .max(4096).max(required).min(MAX_REQUEST);
                self.0.try_reserve_exact(capacity - self.0.len())
                    .map_err(|error| io::Error::other(format!("VB request allocation refused: {error}")))?;
            }
            self.0.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> { Ok(()) }
    }
    let mut payload = Payload(Vec::new());
    serde_json::to_writer(&mut payload, request)?;
    String::from_utf8(payload.0)
        .map_err(|error| serde_json::Error::io(io::Error::new(io::ErrorKind::InvalidData, error)))
}

static TEARDOWN_FAULTED: AtomicBool = AtomicBool::new(false);
static RETIRING: LazyLock<Mutex<Vec<Owner>>> = LazyLock::new(|| Mutex::new(Vec::new()));

struct Request { payload: String, limit: usize, timeout: Duration, reply: mpsc::SyncSender<io::Result<String>> }
struct Owner {
    thread: Option<JoinHandle<()>>, done: mpsc::Receiver<io::Result<()>>, result: Option<io::Result<()>>,
    // Retirement owns these until the actual process/IO thread has joined.
    compilation_holds: Vec<Arc<super::compilation::CachedCharge>>,
}

impl Owner {
    fn finish(&mut self, wait: Duration) -> io::Result<bool> {
        let deadline = Instant::now() + wait;
        if self.result.is_none() {
            match self.done.recv_timeout(wait) {
                Ok(result) => self.result = Some(result),
                Err(mpsc::RecvTimeoutError::Timeout) => return Ok(false),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    TEARDOWN_FAULTED.store(true, Ordering::Release);
                    self.result = Some(Err(io::Error::other("VB IO owner ended without teardown acknowledgement")));
                }
            }
        }
        while self.thread.as_ref().is_some_and(|thread| !thread.is_finished()) {
            if Instant::now() >= deadline { return Ok(false); }
            std::thread::sleep(Duration::from_millis(1));
        }
        if let Some(thread) = self.thread.take() {
            if thread.join().is_err() {
                TEARDOWN_FAULTED.store(true, Ordering::Release);
                return Err(io::Error::other("VB IO owner panicked"));
            }
        }
        self.result.take().unwrap_or(Ok(())).map(|_| true)
    }
}

fn reap_retiring() -> io::Result<()> {
    let mut retiring = RETIRING.lock().map_err(|_| io::Error::other("VB retiring-owner coordinator unavailable"))?;
    let mut n = 0;
    while n < retiring.len() {
        match retiring[n].finish(Duration::ZERO) {
            Ok(true) => { retiring.swap_remove(n); }
            Ok(false) => n += 1,
            Err(error) => { TEARDOWN_FAULTED.store(true, Ordering::Release); return Err(error); }
        }
    }
    if !retiring.is_empty() { return Err(io::Error::other("VB previous IO owner is still retiring; no new child may start")); }
    if TEARDOWN_FAULTED.load(Ordering::Acquire) { return Err(io::Error::other("VB process teardown was not established; inspect the recorded child before restarting parser service")); }
    Ok(())
}

pub(super) fn lifecycle_ready() -> bool {
    reap_retiring().is_ok()
}

pub(super) struct Client {
    commands: tokio::sync::mpsc::Sender<Request>, cancel: CancellationToken,
    owner: Option<Owner>, running: Arc<AtomicBool>, pid: u32,
}

impl Client {
    pub fn spawn(binary: &Path) -> io::Result<Self> {
        reap_retiring()?;
        // Missing companions are an ordinary degraded mode. Do not construct
        // a runtime/owner/three pipes for every file just to rediscover absence.
        // A bare command name keeps Command's existing PATH-search semantics.
        if binary.is_absolute() || binary.components().count() > 1 {
            if !std::fs::metadata(binary)?.is_file() {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "VB companion is not a regular executable file"));
            }
        }
        let (commands, receiver) = tokio::sync::mpsc::channel(1);
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let (done_tx, done) = mpsc::sync_channel(1);
        let cancel = CancellationToken::new();
        let worker_cancel = cancel.clone();
        let running = Arc::new(AtomicBool::new(true));
        let worker_running = running.clone();
        let binary = binary.to_owned();
        let thread = std::thread::Builder::new().name("vb-parser-io".into()).spawn(move || {
            let result = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(runtime) => {
                    let result = runtime.block_on(run(binary, receiver, worker_cancel, started_tx));
                    drop(runtime);
                    result
                }
                Err(error) => { let _ = started_tx.send(Err(io::Error::other(error.to_string()))); Ok(()) }
            };
            worker_running.store(false, Ordering::Release);
            let _ = done_tx.send(result);
        })?;
        let mut client = Self { commands, cancel, owner: Some(Owner { thread: Some(thread), done, result: None,
            compilation_holds: Vec::new() }), running, pid: 0 };
        match started_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(pid)) => { client.pid = pid; Ok(client) }
            Ok(Err(error)) => { let _ = client.stop(); Err(error) }
            Err(error) => { let _ = client.stop(); Err(io::Error::new(io::ErrorKind::TimedOut, format!("VB owner startup did not complete: {error}"))) }
        }
    }

    pub fn pid(&self) -> u32 { self.pid }
    pub fn is_running(&self) -> bool { self.running.load(Ordering::Acquire) }

    pub(super) fn retain_compilation_holds(&mut self, holds: Vec<Arc<super::compilation::CachedCharge>>) {
        if let Some(owner) = self.owner.as_mut() { owner.compilation_holds = holds; }
    }

    pub fn request(&mut self, payload: String, limit: usize, timeout: Duration) -> io::Result<String> {
        if payload.len() > MAX_REQUEST { return Err(io::Error::new(io::ErrorKind::InvalidInput, "VB request exceeds transport budget")); }
        if limit == 0 || limit > 64 * 1024 * 1024 { return Err(io::Error::new(io::ErrorKind::InvalidInput, "VB response budget is invalid")); }
        if !self.is_running() { return Err(io::Error::new(io::ErrorKind::BrokenPipe, "VB IO owner has stopped")); }
        let timeout = timeout.min(Duration::from_secs(300));
        let (reply, response) = mpsc::sync_channel(1);
        self.commands.try_send(Request { payload, limit, timeout, reply })
            .map_err(|error| io::Error::new(io::ErrorKind::WouldBlock, format!("VB IO owner unavailable: {error}")))?;
        match response.recv_timeout(timeout + Duration::from_secs(1)) {
            Ok(result) => {
                if result.is_err() { self.running.store(false, Ordering::Release); self.cancel.cancel(); }
                result
            }
            Err(error) => {
                self.cancel.cancel();
                Err(io::Error::new(io::ErrorKind::TimedOut, format!("VB request owner did not complete: {error}")))
            }
        }
    }

    pub fn stop(&mut self) -> io::Result<()> {
        self.running.store(false, Ordering::Release);
        self.cancel.cancel();
        let Some(owner) = self.owner.as_mut() else { return Ok(()); };
        if owner.finish(SHUTDOWN)? { self.owner = None; return Ok(()); }
        Err(io::Error::new(io::ErrorKind::TimedOut, format!("VB IO owner for pid {} has not acknowledged teardown; no replacement child is permitted", self.pid)))
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        if let Err(error) = self.stop() {
            tracing::error!(pid=self.pid, error=%error, "VB IO teardown incomplete; retain owner until completion, do not respawn");
        }
        if let Some(owner) = self.owner.take() {
            let mut retiring = RETIRING.lock().unwrap_or_else(|poisoned| {
                TEARDOWN_FAULTED.store(true, Ordering::Release);
                poisoned.into_inner()
            });
            retiring.push(owner);
        }
    }
}

async fn read_frame<R: tokio::io::AsyncBufRead + Unpin>(reader: &mut R, limit: usize) -> io::Result<String> {
    let mut line = String::new();
    reader.take(limit as u64 + 1).read_line(&mut line).await?;
    if line.len() > limit || !line.ends_with('\n') { return Err(io::Error::new(io::ErrorKind::InvalidData, "VB response exceeds frame budget or is incomplete")); }
    Ok(line)
}

async fn stderr_frames(stderr: Arc<tokio::sync::Mutex<stdio::ErrorOutput>>) {
    let mut stderr = stderr.lock().await;
    let mut chunk = [0u8; 1024];
    let mut line = Vec::with_capacity(4096);
    let mut truncated = false;
    loop {
        match stderr.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(n) => for byte in &chunk[..n] {
                if *byte == b'\n' {
                    tracing::warn!(message=%String::from_utf8_lossy(&line), truncated, "vb_sidecar_stderr");
                    line.clear(); truncated = false;
                } else if line.len() < 4096 { line.push(*byte); }
                else { truncated = true; }
            }
        }
    }
}

async fn run(binary: PathBuf, mut requests: tokio::sync::mpsc::Receiver<Request>, cancel: CancellationToken,
    started: mpsc::SyncSender<io::Result<u32>>) -> io::Result<()> {
    let process = tokio::select! {
        _ = cancel.cancelled() => Err(io::Error::new(io::ErrorKind::Interrupted, "VB startup cancelled")),
        result = tokio::time::timeout(Duration::from_secs(5), stdio::spawn(&binary)) => result.unwrap_or_else(|_| Err(io::Error::new(io::ErrorKind::TimedOut, "VB pipe/process startup deadline exceeded"))),
    };
    let stdio::Process { mut child, mut stdin, stdout, stderr } = match process {
        Ok(process) => process,
        Err(error) => { let _ = started.send(Err(io::Error::new(error.kind(), error.to_string()))); return Ok(()); }
    };
    let pid = child.id().unwrap_or(0);
    let mut stdout = BufReader::new(stdout);
    let stderr = Arc::new(tokio::sync::Mutex::new(stderr));
    let stderr_owner = tokio::spawn(stderr_frames(stderr.clone()));
    let _ = started.send(Ok(pid));
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = child.wait() => break,
            request = requests.recv() => {
                let Some(request) = request else { break; };
                let operation = async {
                    stdin.write_all(request.payload.as_bytes()).await?;
                    stdin.write_all(b"\n").await?;
                    stdin.flush().await?;
                    read_frame(&mut stdout, request.limit).await
                };
                let result = tokio::select! {
                    _ = cancel.cancelled() => Err(io::Error::new(io::ErrorKind::Interrupted, "VB request cancelled")),
                    result = tokio::time::timeout(request.timeout, operation) => result.unwrap_or_else(|_| Err(io::Error::new(io::ErrorKind::TimedOut, "VB request deadline exceeded"))),
                };
                let failed = result.is_err();
                let _ = request.reply.send(result);
                if failed { break; }
            }
        }
    }
    // Cancel the reader but retain its endpoint in the owner. Mio deliberately
    // leaves pending writes on drop, so Windows must sever every connection
    // before dropping endpoints; descendant-held ends cannot defer completion.
    stderr_owner.abort();
    let _ = stderr_owner.await;
    let stderr_endpoint = stderr.lock().await;
    let disconnected = stdio::disconnect(&stdin, stdout.get_ref(), &stderr_endpoint);
    drop(stderr_endpoint);
    drop(stdin); drop(stdout); drop(stderr);
    let stopped = stop_child(&mut child, pid).await;
    if let Err(error) = disconnected {
        TEARDOWN_FAULTED.store(true, Ordering::Release);
        tracing::error!(pid, error=%error, "VB pipe disconnection was not established; parser service is fenced");
        return Err(error);
    }
    stopped
}

async fn stop_child(child: &mut tokio::process::Child, pid: u32) -> io::Result<()> {
    let cleanup = async {
        if child.try_wait()?.is_none() { child.start_kill()?; }
        child.wait().await.map(|_| ())
    };
    match tokio::time::timeout(Duration::from_secs(5), cleanup).await {
        Ok(Ok(())) => Ok(()),
        result => {
            TEARDOWN_FAULTED.store(true, Ordering::Release);
            let error = match result { Ok(Err(error)) => error, _ => io::Error::new(io::ErrorKind::TimedOut, "VB child kill/reap deadline exceeded") };
            tracing::error!(pid, error=%error, "VB owned process exit was not established; parser service is fenced");
            Err(error)
        }
    }
}
