#![allow(clippy::unwrap_used)]
//! External audit 2026-08-29 P0-3: the first get_change_set after a daemon
//! restart took 38 s on the pilot corpus — the project runtime opened on the first
//! user's call. A fresh AppState over an existing data dir has no runtime
//! cached; the warm-up opens selected registered projects so the first call is
//! served from a warm daemon.

use engram_core::{ProjectRecord, RepoRule, config::Config};
use engram_server::actors::warmup::warm_all_projects;
use engram_server::state::AppState;
use engram_server::tools::Engram;
use rmcp::handler::server::tool::Parameters;

#[test]
fn a_fresh_daemon_warms_a_registered_project_runtime() {
    const NEWER_LEGACY_PROJECTS: usize = 7;
    let tmp = tempfile::TempDir::new().unwrap();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let root = tmp.path().join("proj");
    std::fs::create_dir_all(root.join("Site/App_Code")).unwrap();
    std::fs::write(
        root.join("Site/App_Code/a.vb"),
        "Public Class a\n    Public Function Go() As Integer\n        Return 1\n    End Function\nEnd Class\n",
    )
    .unwrap();
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    let cfg = || Config {
        allowed_roots: vec![tmp.path().to_path_buf()],
        data_dir: data_dir.clone(),
        max_project_files: Some(20),
        max_project_bytes: Some(1024 * 1024),
        embedding_backend: "fts_only".into(),
        llm_backend: "none".into(),
        max_parse_concurrency: 1,
        ..Default::default()
    };
    let pid = runtime.block_on(async {
        let pid = {
            let (state, _rx) = AppState::new(cfg()).unwrap();
            let engram = Engram::new(state.clone());
            engram
                .index_project(Parameters(engram_server::IndexProjectRequest {
                    directory: root.to_string_lossy().to_string(),
                    project_name: "WarmupFixture".into(),
                    project_type: engram_server::models::ProjectType::DotnetWebformsVb,
                    wait: true,
                    dedupe_by_directory: false,
                }))
                .await
                .unwrap();
            let pid = state.registry.list_projects().unwrap()[0]
                .project_id
                .clone();
            assert!(
                matches!(state.registry.source_snapshot_requirement(&pid).unwrap(),
            (head, engram_core::registry::SourceSnapshotRequirement::Managed) if head > 0)
            );
            // More than a full cache of newer registrations must not crowd out this
            // real indexed VB project, nor open their optional native/index stores.
            let updated = state
                .registry
                .get_project(&pid)
                .unwrap()
                .unwrap()
                .updated_at_ms;
            for number in 0..NEWER_LEGACY_PROJECTS {
                let id = format!("legacy-{number}");
                let legacy_root = tmp.path().join(&id);
                std::fs::create_dir_all(&legacy_root).unwrap();
                std::fs::write(legacy_root.join("untouched.cs"), "class Unconverted {}\n").unwrap();
                state
                    .registry
                    .put_project(&ProjectRecord {
                        project_id: id.clone(),
                        project_name: id.clone(),
                        directory: legacy_root.to_string_lossy().into_owned(),
                        project_type: "general".into(),
                        created_at_ms: 0,
                        updated_at_ms: updated + number as u64 + 1,
                        reindex_required_since_ms: None,
                    })
                    .unwrap();
                state
                    .registry
                    .set_meta(
                        &id,
                        "active_generation",
                        if number == 0 { "malformed" } else { "42" },
                    )
                    .unwrap();
                state
                    .registry
                    .put_repo_rule(
                        &id,
                        &RepoRule {
                            rule_id: "preserve-policy".into(),
                            file_pattern: "**/*.cs".into(),
                            rule_text: format!("Preserve original policy for {id}"),
                            priority: 7,
                            updated_at_ms: 17,
                            introduced_at: None,
                            provenance: None,
                        },
                    )
                    .unwrap();
            }
            // wait=true indexes synchronously; these non-Git roots create no jobs.
            assert!(state.registry.list_jobs(None).unwrap().is_empty());
            drop(engram);
            pid
        };
        pid
    });
    // Join the public producer's optional blocking cache work before reopening stores.
    drop(runtime);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        // A restarted daemon: same data dir, empty runtime cache.
        let (fresh, _rx2) = AppState::new(cfg()).unwrap();
        assert!(
            fresh.get_project_cached(&pid).is_none(),
            "a fresh state has no runtime cached"
        );
        let before = serde_json::to_value(fresh.registry.list_projects().unwrap()).unwrap();
        let policies: Vec<_> = (0..NEWER_LEGACY_PROJECTS)
            .map(|number| {
                let id = format!("legacy-{number}");
                let rules = fresh.registry.list_repo_rules(&id).unwrap();
                (id, rules)
            })
            .collect();
        let n = warm_all_projects(&fresh).await;
        assert_eq!(
            n, 1,
            "the older managed project warms despite more than five newer legacy registrations"
        );
        assert!(
            fresh.get_project_cached(&pid).is_some(),
            "the runtime is cached after warm-up"
        );
        assert_eq!(
            serde_json::to_value(fresh.registry.list_projects().unwrap()).unwrap(),
            before,
            "startup skips must preserve every registration"
        );
        for (id, rules) in policies {
            assert!(
                !data_dir.join("projects").join(&id).exists(),
                "an unproven/erroring registration must not create index or native directories"
            );
            assert!(!fresh.projects.contains_key(&id));
            assert!(!fresh.node_snapshot_cache.contains_key(&id));
            assert!(!fresh.setting_prior_cache.contains_key(&id));
            assert!(!fresh.co_change_cache.contains_key(&id));
            assert!(!fresh.ui_catalog_cache.contains_key(&id));
            assert_eq!(
                fresh.registry.list_repo_rules(&id).unwrap(),
                rules,
                "unconverted policy remains available through Registry metadata APIs"
            );
            assert_eq!(
                fresh
                    .registry
                    .get_meta(&id, "active_generation")
                    .unwrap()
                    .as_deref(),
                Some(if id == "legacy-0" { "malformed" } else { "42" })
            );
        }
        assert!(fresh.registry.list_jobs(None).unwrap().is_empty());
        // Skipping optional startup work does not disable the explicit repair
        // route. Only after this real producer publishes may source be queried.
        let repair_id = format!("legacy-{}", NEWER_LEGACY_PROJECTS - 1);
        let engram = Engram::new(fresh.clone());
        let repaired = engram
            .handle_repair_project(
                serde_json::from_value(serde_json::json!({
                    "project_id": repair_id, "scope": "full", "wipe_and_reindex": false,
                    "max_commits": 0, "index_antipatterns": false,
                }))
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(!repaired.is_error.unwrap_or(false));
        assert!(
            matches!(fresh.registry.source_snapshot_requirement(&repair_id).unwrap(),
            (head, engram_core::registry::SourceSnapshotRequirement::Managed) if head > 42)
        );
        let recalled = engram
            .handle_search_memory(
                serde_json::from_value(serde_json::json!({
                    "project_id": repair_id, "query": "Unconverted", "semantic": false,
                    "include_content": true, "max_results": 1,
                }))
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(!recalled.is_error.unwrap_or(false));
        let text = recalled
            .content
            .iter()
            .filter_map(|c| c.as_text())
            .map(|c| c.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("untouched.cs") && text.contains("class Unconverted {}"),
            "{text}"
        );
        assert!(fresh.registry.list_jobs(None).unwrap().is_empty());
    });
    // Runtime shutdown joins remaining owned tasks before Windows fixture removal.
    drop(runtime);
    tmp.close().unwrap();
}
