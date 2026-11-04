//! Observing resources must not create them or keep an idle project resident.
use super::*;
use anyhow::{Result, ensure};
use serde_json::{Value, json};
use std::path::Path;

fn state(root: &Path) -> Result<crate::state::AppState> {
    let data = root.join("data");
    std::fs::create_dir_all(&data)?;
    Ok(crate::state::AppState::new(engram_core::Config {
        data_dir: data,
        allowed_roots: vec![root.to_path_buf()],
        embedding_backend: "fts_only".into(),
        ..Default::default()
    })?
    .0)
}

async fn observe(engram: &Engram, project: Option<&str>) -> Result<Value> {
    let result = engram
        .handle_get_memory_budget(serde_json::from_value(json!({
            "project_id":project,"output_json":true
        }))?)
        .await?;
    Ok(serde_json::from_str(
        &result.content[0].as_text().unwrap().text,
    )?)
}

async fn indexed(root: &Path) -> Result<(Engram, String)> {
    let project = root.join("repo");
    std::fs::create_dir(&project)?;
    drop(git2::Repository::init(&project)?);
    std::fs::write(project.join("source.rs"), "pub fn Answer() -> i32 { 42 }\n")?;
    let engram = Engram::new(state(root)?);
    let result = engram.handle_index_project(serde_json::from_value(json!({
        "directory":project,"project_name":"Memory observation","project_type":"rust","wait":true
    }))?).await?;
    let text = &result.content[0].as_text().unwrap().text;
    let pid = text
        .split("project_id: ")
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("missingprojectID"))?
        .lines()
        .next()
        .unwrap()
        .trim()
        .to_owned();
    Ok((engram, pid))
}

#[test]
fn inspecting_a_cold_project_preserves_budget_fields_and_does_not_create_runtime_storage()
-> Result<()> {
    let owned = tempfile::tempdir()?;
    let outcome = {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        runtime.block_on(async {
            let engram = Engram::new(state(owned.path())?);
            let budget = engram.state.memory_budget.breakdown();
            let ordinary = observe(&engram, None).await?;
            ensure!(
                ordinary["budget"] == budget.budget && ordinary["total_used"] == budget.total_used
            );
            ensure!(
                ordinary["accounting_scope"]
                    .as_str()
                    .unwrap()
                    .contains("not process private memory")
            );
            for _ in 0..3 {
                let cold = observe(&engram, Some("cold-project")).await?;
                ensure!(cold["project_retention"]["status"] == "not_open_or_not_cached");
                ensure!(
                    engram
                        .state
                        .peek_project_cached("cold-project")
                        .unwrap()
                        .is_none()
                );
                ensure!(!owned.path().join("data/projects/cold-project").exists());
                ensure!(
                    engram
                        .state
                        .registry
                        .source_identity_epoch("cold-project")?
                        .is_none()
                );
                ensure!(cold["total_used"] == ordinary["total_used"]);
            }
            ensure!(
                engram
                    .handle_get_memory_budget(serde_json::from_value(
                        json!({"project_id":"../outside","output_json":true})
                    )?)
                    .await
                    .is_err()
            );
            Ok::<_, anyhow::Error>(())
        })
    };
    owned.close()?;
    outcome
}

fn pin_count(value: &Value, generation: u64) -> u64 {
    value["project_retention"]["search"]["source_catalog"]["project_views"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|v| v["namespace"] == "memory" && v["generation"] == generation)
        .map(|v| v["external_pin_references"].as_u64().unwrap())
        .sum()
}

#[test]
fn reader_ownership_is_observed_and_released_without_diagnostic_pins_or_cache_work() -> Result<()> {
    let owned = tempfile::tempdir()?;
    let outcome = {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        runtime.block_on(async {
            let (engram, pid) = indexed(owned.path()).await?;
            let generation = project_service::get_active_generation(&engram.state, &pid).await?;
            let project = engram.state.peek_project_cached(&pid).unwrap().unwrap();
            let source = owned.path().join("repo/source.rs");
            let original = std::fs::read(&source)?;
            let reader = project.search.pin_source_generation(&pid, generation)?;
            let held = observe(&engram, Some(&pid)).await?;
            ensure!(
                pin_count(&held, generation) >= 1,
                "Actual reader must be visible: {held}"
            );
            for _ in 0..3 {
                let repeated = observe(&engram, Some(&pid)).await?;
                ensure!(
                    pin_count(&repeated, generation) == pin_count(&held, generation),
                    "Observation must not pin another reader"
                );
                ensure!(
                    repeated["project_retention"]["search"]["native_cache"]
                        == held["project_retention"]["search"]["native_cache"]
                );
            }
            drop(reader);
            let released = observe(&engram, Some(&pid)).await?;
            ensure!(
                pin_count(&released, generation) < pin_count(&held, generation),
                "Reader release must be visible without getter cleanup"
            );
            ensure!(std::fs::read(source)? == original);
            ensure!(
                project_service::get_active_generation(&engram.state, &pid).await? == generation
            );
            let native = &released["project_retention"]["search"]["native_cache"];
            if cfg!(feature = "vector") {
                ensure!(
                    native["status"] == "compiled"
                        && native["approximate_session_items"].is_number()
                );
            } else {
                ensure!(native["status"] == "not_compiled");
            }
            Ok::<_, anyhow::Error>(())
        })
    };
    owned.close()?;
    outcome
}

#[test]
fn repeated_inspection_does_not_protect_the_oldest_runtime_from_normal_lru_eviction() -> Result<()>
{
    let owned = tempfile::tempdir()?;
    let outcome = {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        runtime.block_on(async {
            let (engram, pid) = indexed(owned.path()).await?;
            let template = engram.state.peek_project_cached(&pid).unwrap().unwrap();
            // Cache eviction is tested with shared immutable engine ownership;
            // no duplicate project trees or additional database instances.
            for n in 0..crate::state::MAX_CACHED_PROJECTS {
                let mut project = template.clone();
                project.info.project_id = format!("observed-{n}");
                engram.state.put_project_cached(project).await;
                tokio::time::sleep(std::time::Duration::from_millis(2)).await;
            }
            for _ in 0..3 {
                ensure!(
                    observe(&engram, Some("observed-0")).await?["project_retention"]["status"]
                        == "observed"
                );
            }
            let mut next = template.clone();
            next.info.project_id = "next-runtime".into();
            engram.state.put_project_cached(next).await;
            ensure!(
                engram
                    .state
                    .peek_project_cached("observed-0")
                    .unwrap()
                    .is_none(),
                "Diagnostic polling must not refresh eviction priority"
            );
            ensure!(
                engram
                    .state
                    .peek_project_cached("observed-1")
                    .unwrap()
                    .is_some()
            );
            ensure!(
                engram
                    .state
                    .peek_project_cached("next-runtime")
                    .unwrap()
                    .is_some()
            );
            Ok::<_, anyhow::Error>(())
        })
    };
    owned.close()?;
    outcome
}
