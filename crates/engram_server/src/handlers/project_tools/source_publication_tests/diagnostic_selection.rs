use super::*;
use anyhow::ensure;
use redb::ReadableTable;

#[test]
fn project_info_preserves_legacy_unpublished_and_unknown_project_semantics() -> anyhow::Result<()> {
    let owned = tempfile::tempdir()?;
    let outcome = {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        runtime.block_on(async {
            let engram = fixture_at_with_initial_head(owned.path(), "fts_only", false);
            let request = || ProjectIdRequest {
                project_id: PROJECT.into(),
            };
            let legacy = engram.handle_project_info(request()).await?;
            ensure!(
                legacy.content[0]
                    .as_text()
                    .unwrap()
                    .text
                    .ends_with("active_generation: 1")
            );
            ensure!(
                engram
                    .state
                    .registry
                    .get_meta(PROJECT, "active_generation")?
                    .is_none(),
                "Legacy diagnostic must not invent stored authority"
            );
            let record = engram.state.registry.get_project(PROJECT)?.unwrap();
            engram.state.registry.delete_all_for_project(PROJECT)?;
            engram.state.registry.register_source_project(&record)?;
            let unpublished = engram.handle_project_info(request()).await?;
            ensure!(
                unpublished.content[0]
                    .as_text()
                    .unwrap()
                    .text
                    .ends_with("active_generation: 0")
            );
            ensure!(
                engram
                    .state
                    .registry
                    .get_meta(PROJECT, "active_generation")?
                    .as_deref()
                    == Some("0")
            );
            let missing = engram
                .handle_project_info(ProjectIdRequest {
                    project_id: "never-registered".into(),
                })
                .await;
            ensure!(
                missing
                    .as_ref()
                    .err()
                    .is_some_and(|error| error.message == "Unknown project")
            );
            ensure!(
                engram
                    .state
                    .registry
                    .get_project("never-registered")?
                    .is_none()
            );
            Ok::<(), anyhow::Error>(())
        })
    };
    let cleanup = owned.close();
    match (outcome, cleanup) {
        (Err(error), Err(cleanup)) => {
            Err(error.context(format!("Owned fixture cleanup failed: {cleanup}")))
        }
        (Err(error), _) => Err(error),
        (Ok(()), cleanup) => cleanup.map_err(Into::into),
    }
}

#[test]
fn project_info_never_invents_a_generation_when_source_authority_is_unavailable()
-> anyhow::Result<()> {
    let owned = tempfile::tempdir()?;
    let outcome = {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        runtime.block_on(async {
            let mut engram = fixture_at(owned.path(), "fts_only");
            // Publish through the real repair flow, so head presence is required.
            engram.handle_repair_project(serde_json::from_value(json!({
                "project_id": PROJECT, "scope": "full", "max_commits": 0, "index_antipatterns": false
            }))?).await?;
            finish_owned_jobs(&engram).await;
            let generation = engram.state.registry.active_source_generation(PROJECT)?;
            let request = || ProjectIdRequest { project_id: PROJECT.into() };
            let valid = engram.handle_project_info(request()).await?;
            ensure!(valid.content[0].as_text().unwrap().text.ends_with(&format!("active_generation: {generation}")), "Diagnostic lost the published generation");
            for damaged in [Some("broken-head"), Some("18446744073709551616"), None] {
                if let Some(raw) = damaged {
                    engram.state.registry.set_meta(PROJECT, "active_generation", raw)?;
                } else {
                    let config = (*engram.state.cfg).clone();
                    drop(engram);
                    {
                        let database = redb::Database::open(config.data_dir.join("registry/registry.redb"))?;
                        let transaction = database.begin_write()?;
                        {
                            let mut table = transaction.open_table(redb::TableDefinition::<&str, &[u8]>::new("meta"))?;
                            ensure!(table.get(format!("{PROJECT}\0source_head_required_v1").as_str())?
                                .is_some_and(|value| value.value() == b"1"));
                            ensure!(table.remove(format!("{PROJECT}\0active_generation").as_str())?.is_some());
                        }
                        transaction.commit()?;
                    }
                    let (state, _) = AppState::new(config)?;
                    engram = Engram::new(state);
                }
                let raw_before = engram.state.registry.get_meta(PROJECT, "active_generation")?;
                let authority_error = engram.get_active_generation(PROJECT).await.unwrap_err();
                let diagnostic = engram.handle_project_info(request()).await;
                ensure!(diagnostic.as_ref().err().is_some_and(|error| error.message == authority_error.message),
                    "project_info fabricated a generation instead of preserving source authority failure for {damaged:?}: {diagnostic:?}");
                ensure!(engram.state.registry.get_meta(PROJECT, "active_generation")? == raw_before,
                    "Read-only diagnostic changed damaged authority");
                engram.state.registry.set_meta(PROJECT, "active_generation", &generation.to_string())?;
                let restored = engram.handle_project_info(request()).await?;
                ensure!(restored.content[0].as_text().unwrap().text.ends_with(&format!("active_generation: {generation}")));
            }
            Ok::<(), anyhow::Error>(())
        })
    };
    let cleanup = owned.close();
    match (outcome, cleanup) {
        (Err(error), Err(cleanup)) => {
            Err(error.context(format!("Owned fixture cleanup failed: {cleanup}")))
        }
        (Err(error), _) => Err(error),
        (Ok(()), cleanup) => cleanup.map_err(Into::into),
    }
}
