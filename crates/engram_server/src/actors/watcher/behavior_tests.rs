//! Product behavior through the real notify callback, actor and source updater.
//! Observation hooks never replace scheduling. The one-shot pause holds already
//! captured source before publication so a later edit genuinely needs follow-up.
#![allow(clippy::unwrap_used)]

use super::*;
use anyhow::{Result, ensure};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Weak};
use tokio::sync::Notify;

static PROBES: LazyLock<Mutex<HashMap<String, Weak<Probe>>>> = LazyLock::new(Mutex::default);
const WAIT: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, Default)]
struct Observations {
    enabled: Option<bool>,
    admissions: usize,
    starts: usize,
    finishes: usize,
    last_cancelled: bool,
    last_error: Option<String>,
    events: HashMap<(String, bool), usize>,
    last_tick: Option<Instant>,
    staged: bool,
}

struct Probe {
    root: PathBuf,
    observations: Mutex<Observations>,
    changed: Notify,
    pause_next: AtomicBool,
    released: AtomicBool,
    release_signal: Notify,
}

impl Probe {
    fn new(root: PathBuf) -> Self {
        Self {
            root,
            observations: Mutex::default(),
            changed: Notify::new(),
            pause_next: AtomicBool::new(false),
            released: AtomicBool::new(true),
            release_signal: Notify::new(),
        }
    }

    fn update(&self, change: impl FnOnce(&mut Observations)) {
        change(&mut self.observations.lock().unwrap());
        self.changed.notify_waiters();
    }

    fn snapshot(&self) -> Observations {
        self.observations.lock().unwrap().clone()
    }

    async fn wait(&self, label: &str, predicate: impl Fn(&Observations) -> bool) -> Result<()> {
        tokio::time::timeout(WAIT, async {
            loop {
                let signal = self.changed.notified();
                tokio::pin!(signal);
                signal.as_mut().enable();
                if predicate(&self.observations.lock().unwrap()) {
                    return;
                }
                signal.await;
            }
        })
        .await
        .map_err(|_| anyhow::anyhow!("Timed out waiting for {label}: {:?}", self.snapshot()))?;
        Ok(())
    }

    async fn saw_write(&self, path: &str, eligible: bool) -> Result<()> {
        self.write_after(path, eligible, 0).await
    }

    fn write_count(&self, path: &str, eligible: bool) -> usize {
        self.snapshot()
            .events
            .get(&(path.to_owned(), eligible))
            .copied()
            .unwrap_or(0)
    }

    async fn write_after(&self, path: &str, eligible: bool, before: usize) -> Result<()> {
        self.wait(path, |state| {
            state
                .events
                .get(&(path.to_owned(), eligible))
                .copied()
                .unwrap_or(0)
                > before
        })
        .await
    }

    async fn tick_after(&self, when: Instant) -> Result<()> {
        self.wait("actor acknowledgement after debounce", |state| {
            state.last_tick.is_some_and(|tick| tick >= when)
        })
        .await
    }

    fn arm_pause(&self) {
        self.released.store(false, Ordering::SeqCst);
        self.pause_next.store(true, Ordering::SeqCst);
        self.update(|state| state.staged = false);
    }

    fn release(&self) {
        self.released.store(true, Ordering::SeqCst);
        self.release_signal.notify_waiters();
    }
}

fn probe(project_id: &str) -> Option<Arc<Probe>> {
    PROBES
        .lock()
        .unwrap()
        .get(project_id)
        .and_then(Weak::upgrade)
}

pub(super) fn ready(project_id: &str, enabled: bool) {
    if let Some(probe) = probe(project_id) {
        probe.update(|state| state.enabled = Some(enabled));
    }
}

pub(super) fn admitted(project_id: &str) {
    if let Some(probe) = probe(project_id) {
        probe.update(|state| state.admissions += 1);
    }
}

pub(super) fn started(project_id: &str) {
    if let Some(probe) = probe(project_id) {
        probe.update(|state| state.starts += 1);
    }
}

pub(super) fn finished(project_id: &str, error: Option<String>, cancelled: bool) {
    if let Some(probe) = probe(project_id) {
        probe.update(|state| {
            state.finishes += 1;
            state.last_cancelled = cancelled;
            state.last_error = error.map(|error| error.chars().take(1_024).collect());
        });
    }
}

pub(super) fn tick() {
    let probes = PROBES
        .lock()
        .unwrap()
        .values()
        .filter_map(Weak::upgrade)
        .collect::<Vec<_>>();
    for probe in probes {
        probe.update(|state| state.last_tick = Some(Instant::now()));
    }
}

pub(super) fn event(project_id: &str, event: &notify::Event, eligible: bool) {
    if !(event.kind.is_modify() || event.kind.is_create() || event.kind.is_remove()) {
        return;
    }
    if let Some(probe) = probe(project_id) {
        for path in &event.paths {
            let Some(relative) = ProjectWatchFilter::relative_to(path, &probe.root) else {
                continue;
            };
            let relative = relative.to_string_lossy().replace('\\', "/");
            // Record only this fixture's named writes, never runtime event volume.
            if matches!(
                relative.as_str(),
                "watched.rs"
                    | "Renamed.rs"
                    | "delete_me.rs"
                    | "generated/noise.rs"
                    | "generated/Renamed.rs"
                    | "runtime-data/storage_poison.rs"
            ) {
                probe.update(|state| {
                    *state.events.entry((relative, eligible)).or_default() += 1;
                });
            }
        }
    }
}

pub(crate) async fn staged(project_id: &str) {
    let Some(probe) = probe(project_id) else {
        return;
    };
    if !probe.pause_next.swap(false, Ordering::SeqCst) {
        return;
    }
    probe.update(|state| state.staged = true);
    loop {
        let signal = probe.release_signal.notified();
        tokio::pin!(signal);
        signal.as_mut().enable();
        if probe.released.load(Ordering::SeqCst) {
            return;
        }
        signal.await;
    }
}

struct Fixture {
    root: PathBuf,
    project_id: String,
    engram: crate::Engram,
    probe: Arc<Probe>,
    shutdown: tokio_util::sync::CancellationToken,
    watcher: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.probe.release();
        self.shutdown.cancel();
        PROBES.lock().unwrap().remove(&self.project_id);
    }
}

impl Fixture {
    async fn new(temporary: &Path, nested_storage: bool) -> Result<Self> {
        let root = temporary.join("repo");
        std::fs::create_dir_all(root.join("generated"))?;
        std::fs::write(
            root.join(".gitignore"),
            "generated/\n!runtime-data/\n!runtime-data/**\n",
        )?;
        std::fs::write(root.join("watched.rs"), source("baselinevalue"))?;
        std::fs::write(root.join("delete_me.rs"), source("deletemevalue"))?;
        {
            let repo = git2::Repository::init(&root)?;
            let mut index = repo.index()?;
            index.add_all(["."], git2::IndexAddOption::DEFAULT, None)?;
            let tree = repo.find_tree(index.write_tree()?)?;
            let signature = git2::Signature::now("Fixture", "fixture@example.invalid")?;
            repo.commit(
                Some("HEAD"),
                &signature,
                &signature,
                "Owned watcher fixture",
                &tree,
                &[],
            )?;
        }
        let data_dir = if nested_storage {
            root.join("runtime-data")
        } else {
            temporary.join("data")
        };
        let (state, receiver) = AppState::new(engram_core::Config {
            data_dir,
            allowed_roots: vec![root.clone()],
            embedding_backend: "fts_only".into(),
            llm_backend: "none".into(),
            watch_debounce_secs: 1,
            max_commits_per_watch: 8,
            max_project_files: nested_storage.then_some(2),
            max_project_bytes: nested_storage.then_some(512),
            ..Default::default()
        })?;
        let engram = crate::Engram::new(state.clone());
        let indexed = engram.handle_index_project(serde_json::from_value(serde_json::json!({
            "directory":root,"project_name":"Owned watcher fixture","project_type":"rust","wait":true
        }))?).await?;
        ensure!(indexed.is_error != Some(true));
        let text = &indexed.content[0].as_text().unwrap().text;
        let project_id = text
            .split("project_id: ")
            .nth(1)
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .trim()
            .to_owned();
        let probe = Arc::new(Probe::new(root.clone()));
        PROBES
            .lock()
            .unwrap()
            .insert(project_id.clone(), Arc::downgrade(&probe));
        let shutdown = tokio_util::sync::CancellationToken::new();
        let watcher = tokio::spawn(run_watcher(state, receiver, shutdown.clone()));
        let fixture = Self {
            root,
            project_id,
            engram,
            probe,
            shutdown,
            watcher: Some(watcher),
        };
        fixture
            .engram
            .handle_watch_project(serde_json::from_value(serde_json::json!({
                "project_id":fixture.project_id,"enabled":true
            }))?)
            .await?;
        fixture
            .probe
            .wait("watch registration", |state| state.enabled == Some(true))
            .await?;
        Ok(fixture)
    }

    fn head(&self) -> Result<u64> {
        Ok(self
            .engram
            .state
            .registry
            .get_meta(&self.project_id, "active_generation")?
            .unwrap()
            .parse()?)
    }

    async fn search(&self, query: &str) -> Result<String> {
        let response = self
            .engram
            .handle_search_memory(serde_json::from_value(serde_json::json!({
                "project_id":self.project_id,"query":query,"semantic":false,"use_mmr":false,
                "include_content":true,"max_results":10
            }))?)
            .await?;
        ensure!(response.is_error != Some(true));
        Ok(response.content[0].as_text().unwrap().text.clone())
    }

    async fn wait_source(&self, query: &str, present: bool, after: u64) -> Result<String> {
        self.wait_source_where(query, after, |text| {
            (!text.contains("result: no_hits") && text.contains(query)) == present
        })
        .await
    }

    async fn wait_source_where(
        &self,
        query: &str,
        after: u64,
        accepted: impl Fn(&str) -> bool,
    ) -> Result<String> {
        tokio::time::timeout(WAIT, async {
            loop {
                let text = self.search(query).await?;
                if self.head()? > after && accepted(&text) {
                    return Ok(text);
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "Source failed to converge for {query}: {:?}",
                self.probe.snapshot()
            )
        })?
    }

    async fn stop(&mut self) -> Result<()> {
        self.probe.release();
        self.shutdown.cancel();
        if let Some(watcher) = self.watcher.take() {
            watcher.await?;
        }
        Ok(())
    }
}

fn source(value: &str) -> String {
    format!("pub fn watched_value() -> &'static str {{ \"{value}\" }}\n")
}

fn owned_runtime(body: impl FnOnce(&Path, &tokio::runtime::Runtime) -> Result<()>) -> Result<()> {
    let temporary = tempfile::tempdir()?;
    // Reverse unwind order shuts workers/databases before deleting this tree.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let result = body(temporary.path(), &runtime);
    drop(runtime);
    temporary.close()?;
    result
}

#[test]
fn real_watcher_ignores_storage_and_gitignored_writes_across_a_source_update() -> Result<()> {
    owned_runtime(|temporary, runtime| {
        runtime.block_on(async {
            let mut fixture = Fixture::new(temporary, true).await?;
            let before = fixture.head()?;
            std::fs::write(
                fixture.root.join("generated/noise.rs"),
                source("gitignoredpoison"),
            )?;
        std::fs::write(fixture.root.join("runtime-data/.ignore"), "[\n")?;
        std::fs::write(
                fixture.root.join("runtime-data/storage_poison.rs"),
            format!("{}{}", source("privateengramstoragepoison"), "// storage-owned bytes\n".repeat(64)),
            )?;
            fixture.probe.saw_write("generated/noise.rs", false).await?;
            fixture
                .probe
                .saw_write("runtime-data/storage_poison.rs", false)
                .await?;
            fixture
                .probe
                .tick_after(Instant::now() + Duration::from_millis(1_600))
                .await?;
            ensure!(
                fixture.probe.snapshot().admissions == 0,
                "Ignored writes admitted work: {:?}",
                fixture.probe.snapshot()
            );
            ensure!(
                fixture.head()? == before,
                "Ignored writes must preserve the source head"
            );
            std::fs::write(
                fixture.root.join("watched.rs"),
                source("legitimatechangedvalue"),
            )?;
            fixture
                .wait_source("legitimatechangedvalue", true, before)
                .await?;
            ensure!(
                fixture
                    .search("privateengramstoragepoison")
                    .await?
                    .contains("result: no_hits"),
                "Engram storage must remain outside the source corpus after a legitimate edit"
            );
            let files = fixture.engram.state.graph.query_nodes(
                &fixture.project_id,
                Some("file"),
                Some("memory"),
                None,
                1_000,
            )?;
            ensure!(
                files
                    .iter()
                    .all(|node| node.file_path.as_str() != "runtime-data/storage_poison.rs"),
                "Storage must not acquire source-file identity"
            );
            ensure!(
                fixture
                    .search("gitignoredpoison")
                    .await?
                    .contains("result: no_hits")
            );
        let published = fixture.head()?;
        let unchanged = fixture.engram.handle_update_project(serde_json::from_value(serde_json::json!({
            "project_id":fixture.project_id,"wait":true,"index_antipatterns":false
        }))?).await?;
        ensure!(unchanged.content[0].as_text().unwrap().text.contains("source_update: no_changes_detected"), "Runtime writes must not turn an unchanged refresh into source work");
        ensure!(fixture.head()? == published, "Unchanged refresh must preserve the source head");
        let forced = fixture.engram.handle_update_project(serde_json::from_value(serde_json::json!({
            "project_id":fixture.project_id,"wait":true,"reindex_paths":["runtime-data/storage_poison.rs"],"index_antipatterns":false
        }))?).await;
        ensure!(forced.is_err(), "Explicit reindex must respect runtime-storage ownership");
        ensure!(fixture.head()? == published, "Rejected reindex must preserve the published source head");
            fixture.stop().await
        })
    })
}

#[test]
fn real_watcher_converges_after_an_edit_during_indexing_rename_and_deletion() -> Result<()> {
    owned_runtime(|temporary, runtime| {
        runtime.block_on(async {
            let mut fixture = Fixture::new(temporary, false).await?;
            ensure!(
                fixture
                    .search("deletemevalue")
                    .await?
                    .contains("delete_me.rs")
            );
            let before = fixture.head()?;
            fixture.probe.arm_pause();
            std::fs::write(fixture.root.join("watched.rs"), source("capturedfirstedit"))?;
            fixture
                .probe
                .wait("captured unpublished source", |state| state.staged)
                .await?;
            ensure!(
                fixture.head()? == before,
                "Staging must not advance the published head"
            );
            let writes_before = fixture.probe.write_count("watched.rs", true);
            std::fs::write(
                fixture.root.join("watched.rs"),
                source("secondeditmustconverge"),
            )?;
            fixture
                .probe
                .write_after("watched.rs", true, writes_before)
                .await?;
            fixture.probe.release();
            fixture
                .wait_source("secondeditmustconverge", true, before)
                .await?;
            ensure!(
                fixture
                    .search("capturedfirstedit")
                    .await?
                    .contains("result: no_hits")
            );
            let before = fixture.head()?;
            std::fs::rename(
                fixture.root.join("watched.rs"),
                fixture.root.join("Renamed.rs"),
            )?;
            fixture
                .wait_source_where("secondeditmustconverge", before, |text| {
                    text.contains("path: Renamed.rs") && !text.contains("path: watched.rs")
                })
                .await?;
            let before = fixture.head()?;
            std::fs::rename(
                fixture.root.join("Renamed.rs"),
                fixture.root.join("generated/Renamed.rs"),
            )?;
            fixture
                .wait_source("secondeditmustconverge", false, before)
                .await?;
            let before = fixture.head()?;
            std::fs::remove_file(fixture.root.join("delete_me.rs"))?;
            fixture.wait_source("deletemevalue", false, before).await?;
            fixture.stop().await
        })
    })
}

#[test]
fn unwatch_cancels_busy_work_and_stays_disabled() -> Result<()> {
    owned_runtime(|temporary, runtime| {
        runtime.block_on(async {
            let mut fixture = Fixture::new(temporary, false).await?;
            let custom_directory = fixture
                .root
                .join("custom area")
                .to_string_lossy()
                .into_owned();
            fixture.engram.state.registry.put_watch(
                &fixture.project_id,
                &engram_core::WatchRecord {
                    watch_id: "custom-retained".into(),
                    directory: custom_directory.clone(),
                    enabled: true,
                    updated_at_ms: 0,
                },
            )?;
            let before = fixture.head()?;
            fixture.probe.arm_pause();
            std::fs::write(
                fixture.root.join("watched.rs"),
                source("cancelledfirstedit"),
            )?;
            fixture
                .probe
                .wait("captured unpublished source", |state| state.staged)
                .await?;
            let writes_before = fixture.probe.write_count("watched.rs", true);
            std::fs::write(
                fixture.root.join("watched.rs"),
                source("cancelledsecondedit"),
            )?;
            fixture
                .probe
                .write_after("watched.rs", true, writes_before)
                .await?;
            fixture
                .probe
                .tick_after(Instant::now() + Duration::from_millis(1_600))
                .await?;
            ensure!(
                fixture.probe.snapshot().admissions == 1,
                "Busy project must not admit another updater"
            );
            fixture
                .engram
                .handle_unwatch_project(serde_json::from_value(
                    serde_json::json!({"project_id":fixture.project_id}),
                )?)
                .await?;
            fixture
                .probe
                .wait("watch disable acknowledgement", |state| {
                    state.enabled == Some(false)
                })
                .await?;
            fixture.probe.release();
            fixture
                .probe
                .wait("cancelled updater exit", |state| state.finishes >= 1)
                .await?;
            ensure!(
                fixture.probe.snapshot().last_cancelled,
                "Unwatch lost cancellation: {:?}",
                fixture.probe.snapshot()
            );
            ensure!(
                fixture.head()? == before,
                "Unwatch must not publish the paused candidate"
            );
            ensure!(
                fixture
                    .search("baselinevalue")
                    .await?
                    .contains("path: watched.rs"),
                "Cancelled staging must preserve the prior indexed source"
            );
            ensure!(
                fixture
                    .search("cancelledfirstedit")
                    .await?
                    .contains("result: no_hits")
            );
            ensure!(
                fixture
                    .engram
                    .state
                    .registry
                    .list_watches(&fixture.project_id)?
                    .iter()
                    .all(|watch| !watch.enabled),
                "Unwatch must remain disabled across actor/server restarts"
            );
            let retained = fixture
                .engram
                .state
                .registry
                .list_watches(&fixture.project_id)?;
            ensure!(
                retained
                    .iter()
                    .any(|watch| watch.watch_id == "custom-retained"
                        && watch.directory == custom_directory
                        && !watch.enabled),
                "Disabling watching must preserve custom configurations"
            );
            fixture
                .probe
                .tick_after(Instant::now() + Duration::from_millis(1_600))
                .await?;
            ensure!(
                fixture.probe.snapshot().admissions == 1,
                "Queued notifications must not restart an unwatched project"
            );
            fixture.stop().await
        })
    })
}
