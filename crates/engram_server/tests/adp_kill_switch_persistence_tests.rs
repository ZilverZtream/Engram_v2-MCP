#![allow(clippy::unwrap_used)]
//! ADP1-3b9d — ADP kill-switch persistence across AppState restart.
//!
//! Proves that the ADP kill-switch set via the registry at runtime survives a
//! process restart (AppState drop + recreate with a fresh Config that has
//! adp_kill_switch=false).  The OR(config, registry) logic in AppState::new must
//! load the persisted registry value and keep the kill-switch active.

use engram_core::Config;
use engram_server::state::AppState;
use std::sync::atomic::Ordering;

fn make_cfg(data_dir: &std::path::Path, kill_switch: bool) -> Config {
    Config {
        data_dir: data_dir.to_path_buf(),
        embedding_backend: "fts_only".into(),
        allowed_roots: vec![data_dir.to_path_buf()],
        adp_kill_switch: kill_switch,
        ..Default::default()
    }
}

/// kill-switch activated at runtime via `registry.set_adp_kill_switch(true)`
/// must survive a simulated restart even when the new Config has `adp_kill_switch=false`.
///
/// Sequence:
/// 1. Start AppState1 with config kill_switch=false.
/// 2. Activate kill-switch at runtime via registry.
/// 3. Drop AppState1 (simulates process exit).
/// 4. Restart AppState2 with config kill_switch=false (config NOT updated).
/// 5. Assert AppState2.adp_kill_switch.load() == true (registry wins over config).
#[tokio::test]
async fn kill_switch_persists_across_restart_when_registry_set() {
    let tmp = tempfile::TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();

    // Step 1: Create AppState with kill_switch=false.
    let cfg1 = make_cfg(&data_dir, false);
    let (state1, _rx1) = AppState::new(cfg1).unwrap();
    assert!(
        !state1.adp_kill_switch.load(Ordering::SeqCst),
        "precondition — kill_switch must be false initially"
    );

    // Step 2: Activate kill-switch at runtime via registry.
    tokio::task::spawn_blocking({
        let reg = state1.registry.clone();
        move || {
            reg.set_adp_kill_switch(true)
                .expect("set_adp_kill_switch must succeed");
        }
    })
    .await
    .unwrap();

    // Step 3: Drop AppState1 (simulates process exit).
    drop(state1);

    // Step 4: Recreate AppState with adp_kill_switch=false in config.
    // The registry still has kill_switch=true from step 2.
    let cfg2 = make_cfg(&data_dir, false);
    let (state2, _rx2) = AppState::new(cfg2).unwrap();

    // Step 5: Assert the kill-switch loaded from registry overrides the config.
    assert!(
        state2.adp_kill_switch.load(Ordering::SeqCst),
        "kill-switch set at runtime (registry) must survive restart \
         even when new config has adp_kill_switch=false — OR(config, registry) logic must prevail"
    );
}

/// when config has adp_kill_switch=true, it gets persisted to registry
/// on startup and survives restart with config=false.
#[tokio::test]
async fn kill_switch_from_config_persists_to_registry_and_survives_restart() {
    let tmp = tempfile::TempDir::new().unwrap();
    let data_dir = tmp.path().join("data2");
    std::fs::create_dir_all(&data_dir).unwrap();

    // Start with config kill_switch=true — AppState::new must persist it to registry.
    let cfg1 = make_cfg(&data_dir, true);
    let (state1, _rx1) = AppState::new(cfg1).unwrap();
    assert!(
        state1.adp_kill_switch.load(Ordering::SeqCst),
        "precondition — kill_switch must be true when config sets it"
    );

    // Verify it was persisted to the registry on startup.
    let persisted = tokio::task::spawn_blocking({
        let reg = state1.registry.clone();
        move || reg.get_adp_kill_switch().unwrap_or(false)
    })
    .await
    .unwrap();
    assert!(
        persisted,
        "config kill_switch=true must be persisted to registry on AppState startup"
    );

    drop(state1);

    // Restart with config kill_switch=false — registry still has kill_switch=true.
    let cfg2 = make_cfg(&data_dir, false);
    let (state2, _rx2) = AppState::new(cfg2).unwrap();

    assert!(
        state2.adp_kill_switch.load(Ordering::SeqCst),
        "registry-persisted kill_switch must prevail over config=false on restart"
    );
}

/// when neither config nor registry has kill_switch set, restart must not
/// enable the kill-switch spuriously — proves no false positives.
#[tokio::test]
async fn kill_switch_false_in_both_sources_stays_false_after_restart() {
    let tmp = tempfile::TempDir::new().unwrap();
    let data_dir = tmp.path().join("data3");
    std::fs::create_dir_all(&data_dir).unwrap();

    // First start: both config and registry = false (registry has no entry yet).
    let cfg1 = make_cfg(&data_dir, false);
    let (state1, _rx1) = AppState::new(cfg1).unwrap();
    assert!(
        !state1.adp_kill_switch.load(Ordering::SeqCst),
        "precondition: kill_switch must be false when neither source sets it"
    );
    drop(state1);

    // Restart: config still false, registry was never set → still false.
    let cfg2 = make_cfg(&data_dir, false);
    let (state2, _rx2) = AppState::new(cfg2).unwrap();
    assert!(
        !state2.adp_kill_switch.load(Ordering::SeqCst),
        "kill_switch must remain false when neither config nor registry set it \
         — OR(false, false) must not spuriously enable the kill-switch"
    );
}

/// the get/set_adp_kill_switch registry methods round-trip correctly.
/// Structural: proves the persistence layer itself works independently of AppState.
#[test]
fn registry_kill_switch_round_trips() {
    use engram_core::Registry;

    let tmp = tempfile::TempDir::new().unwrap();
    let reg = Registry::open(&tmp.path().join("r.redb")).expect("Registry::open");

    // Default: not set → get returns None/false.
    let initial = reg.get_adp_kill_switch().unwrap_or(false);
    assert!(
        !initial,
        "kill_switch must default to false (no registry entry)"
    );

    // Set to true.
    reg.set_adp_kill_switch(true).expect("set must succeed");
    let after_set = reg.get_adp_kill_switch().unwrap_or(false);
    assert!(
        after_set,
        "kill_switch must read back true after set_adp_kill_switch(true)"
    );

    // Set to false again.
    reg.set_adp_kill_switch(false)
        .expect("set to false must succeed");
    let after_clear = reg.get_adp_kill_switch().unwrap_or(true);
    assert!(
        !after_clear,
        "kill_switch must read back false after set_adp_kill_switch(false)"
    );
}
