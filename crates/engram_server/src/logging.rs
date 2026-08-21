//! Process-wide tracing setup with an optional shared log file.
//!
//! Why this exists: the production deployment used to log by wrapping the
//! binary in `engram_logged.bat` with a `2>> engram.log` shell redirect.
//! cmd.exe opens redirect targets WITHOUT `FILE_SHARE_WRITE`, so the first
//! MCP session's process tree held `engram.log` exclusively and every other
//! Claude Code session's engram spawn died on a sharing violation before
//! `engram_server.exe` even started — the "multi-session fails to start"
//! bug. Logging from inside the process uses Rust's std file open, which on
//! Windows passes `FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE`,
//! so any number of engram processes (daemon + proxies) can append to the
//! same file concurrently.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Rotate `path` to `<path>.1` when it exceeds this size at startup.
pub const ROTATE_AT_BYTES: u64 = 64 * 1024 * 1024;

/// Resolve the effective log-file path: `ENGRAM_LOG_FILE` env var wins,
/// then the config value. Empty env value disables file logging entirely
/// (lets a user switch it off without editing YAML).
pub fn resolve_log_path(cfg_value: Option<&Path>) -> Option<PathBuf> {
    if let Some(env) = std::env::var_os("ENGRAM_LOG_FILE") {
        let s = env.to_string_lossy();
        let trimmed = s.trim();
        if trimmed.is_empty() {
            return None;
        }
        return Some(PathBuf::from(trimmed));
    }
    cfg_value.map(|p| p.to_path_buf())
}

/// Best-effort startup rotation: rename an oversized log to `<name>.1`,
/// replacing any previous `.1`. Failure is non-fatal (e.g. another process
/// is mid-append on a platform without shared delete) — we just keep
/// appending to the big file.
pub fn rotate_if_large(path: &Path, max_bytes: u64) {
    let Ok(meta) = std::fs::metadata(path) else {
        return;
    };
    if meta.len() <= max_bytes {
        return;
    }
    let mut rotated = path.as_os_str().to_owned();
    rotated.push(".1");
    let rotated = PathBuf::from(rotated);
    let _ = std::fs::remove_file(&rotated);
    if let Err(e) = std::fs::rename(path, &rotated) {
        eprintln!(
            "engram_server: could not rotate oversized log {} ({e}); appending anyway",
            path.display()
        );
    }
}

/// Open the log file for shared append, creating parent directories.
pub fn open_shared_append(path: &Path) -> std::io::Result<File> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    File::options().create(true).append(true).open(path)
}

/// Cloneable `Write` over one shared file handle. Every engram process gets
/// its own OS handle in append mode, so concurrent writers interleave at
/// line granularity instead of clobbering each other.
#[derive(Clone)]
pub struct SharedFileWriter(Arc<File>);

impl Write for SharedFileWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        (&*self.0).write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        (&*self.0).flush()
    }
}

/// Initialise tracing: always stderr (MCP hosts capture it), plus the shared
/// log file when configured. Never panics on a bad file path — file logging
/// is best-effort, stderr always works.
pub fn init_tracing(log_file: Option<&Path>) {
    use tracing_subscriber::layer::SubscriberExt as _;
    use tracing_subscriber::util::SubscriberInitExt as _;

    // `from_default_env()` alone yields an (almost) empty filter when RUST_LOG
    // is unset — which is exactly how the MCP host / detached daemon launches
    // us, so the daemon logged NOTHING and every diagnosis had to run blind.
    // Default to INFO when RUST_LOG is absent; honour RUST_LOG when present.
    let env_filter = tracing_subscriber::EnvFilter::builder()
        .with_default_directive(tracing::level_filters::LevelFilter::INFO.into())
        .from_env_lossy();
    let stderr_layer = tracing_subscriber::fmt::layer().with_writer(std::io::stderr);

    let file_layer = log_file.and_then(|path| {
        rotate_if_large(path, ROTATE_AT_BYTES);
        match open_shared_append(path) {
            Ok(file) => {
                let writer = SharedFileWriter(Arc::new(file));
                Some(
                    tracing_subscriber::fmt::layer()
                        .with_writer(move || writer.clone())
                        .with_ansi(false),
                )
            }
            Err(e) => {
                eprintln!(
                    "engram_server: cannot open log file {} ({e}); logging to stderr only",
                    path.display()
                );
                None
            }
        }
    });

    tracing_subscriber::registry()
        .with(env_filter)
        .with(stderr_layer)
        .with(file_layer)
        .init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_prefers_env_over_config() {
        // Serialise env mutation against other tests in this module.
        let _guard = ENV_LOCK.lock().unwrap();
        unsafe {
            std::env::set_var("ENGRAM_LOG_FILE", r"C:\tmp\env.log");
        }
        let got = resolve_log_path(Some(Path::new(r"C:\tmp\cfg.log")));
        unsafe {
            std::env::remove_var("ENGRAM_LOG_FILE");
        }
        assert_eq!(got, Some(PathBuf::from(r"C:\tmp\env.log")));
    }

    #[test]
    fn empty_env_disables_file_logging() {
        let _guard = ENV_LOCK.lock().unwrap();
        unsafe {
            std::env::set_var("ENGRAM_LOG_FILE", "  ");
        }
        let got = resolve_log_path(Some(Path::new(r"C:\tmp\cfg.log")));
        unsafe {
            std::env::remove_var("ENGRAM_LOG_FILE");
        }
        assert_eq!(got, None);
    }

    #[test]
    fn no_env_falls_back_to_config() {
        let _guard = ENV_LOCK.lock().unwrap();
        unsafe {
            std::env::remove_var("ENGRAM_LOG_FILE");
        }
        let got = resolve_log_path(Some(Path::new(r"C:\tmp\cfg.log")));
        assert_eq!(got, Some(PathBuf::from(r"C:\tmp\cfg.log")));
        assert_eq!(resolve_log_path(None), None);
    }

    #[test]
    fn rotate_moves_oversized_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let log = tmp.path().join("engram.log");
        std::fs::write(&log, vec![b'x'; 128]).unwrap();
        rotate_if_large(&log, 64);
        assert!(!log.exists(), "oversized log must be renamed away");
        let rotated = tmp.path().join("engram.log.1");
        assert_eq!(std::fs::metadata(&rotated).unwrap().len(), 128);
    }

    #[test]
    fn rotate_keeps_small_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let log = tmp.path().join("engram.log");
        std::fs::write(&log, b"small").unwrap();
        rotate_if_large(&log, 64);
        assert!(log.exists(), "small log must be left in place");
    }

    #[test]
    fn concurrent_appends_from_two_handles_both_land() {
        // The property the whole module exists for: two independent handles
        // (two processes in production) appending to one file must both
        // succeed — no exclusive-open sharing violation — and lose no bytes.
        let tmp = tempfile::TempDir::new().unwrap();
        let log = tmp.path().join("shared.log");
        let mut a = open_shared_append(&log).unwrap();
        let mut b = open_shared_append(&log).unwrap();
        for i in 0..50 {
            writeln!(a, "a{i}").unwrap();
            writeln!(b, "b{i}").unwrap();
        }
        drop((a, b));
        let text = std::fs::read_to_string(&log).unwrap();
        assert_eq!(text.lines().filter(|l| l.starts_with('a')).count(), 50);
        assert_eq!(text.lines().filter(|l| l.starts_with('b')).count(), 50);
    }

    #[test]
    fn open_creates_parent_dirs() {
        let tmp = tempfile::TempDir::new().unwrap();
        let log = tmp.path().join("nested").join("dir").join("x.log");
        let mut f = open_shared_append(&log).unwrap();
        writeln!(f, "hello").unwrap();
        assert!(log.exists());
    }

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
}
