//! Agents must recover actual source lines, and unchanged legacy markup must
//! migrate its position contract without rewriting unrelated source/knowledge.
use anyhow::{Result, ensure};
use engram_core::{Config, MemorySection, RelPath};
use engram_index::{IngestStats, SourceExtractionExpectation, SourceExtractionIdentity};
use engram_server::{AppState, Engram};
use serde_json::json;
use std::future::Future;

#[test]
fn source_lines_are_one_based_in_control_and_layout_output() -> Result<()> {
    let owned = tempfile::tempdir()?;
    let button = r#"<table><tr><td><asp:TextBox ID="txtUploadName" runat="server" CssClass="upload" Text="Upload" /></td></tr></table>"#;
    for (source, expected) in [
        (button.to_owned(), 1),
        (format!("\n{button}"), 2),
        (format!("\u{feff}Åäö {button}"), 1),
        (
            format!("<%@ Page Language=\"C#\" %>\n<%-- comment --%>\n{button}"),
            3,
        ),
        (
            format!(
                "\u{feff}<%@ Page Language=\"C#\" %>\r\n<%-- Åäö --%>\r\n<asp:Panel ID=\"parent\" runat=\"server\">\r\n{button}\r\n</asp:Panel>"
            ),
            4,
        ),
    ] {
        let (symbols, _) = engram_index::webforms::extract_webforms(
            owned.path(),
            &RelPath::new("Home.aspx"),
            &source,
        );
        for kind in ["control", "control_layout"] {
            let found = symbols
                .iter()
                .find(|s| s.kind == kind && s.name == "txtUploadName")
                .ok_or_else(|| anyhow::anyhow!("Missing {kind} for {source:?}"))?;
            ensure!(
                (found.start_line, found.end_line) == (expected, expected),
                "{kind}: actual source line{expected}, got{}..{}",
                found.start_line,
                found.end_line
            );
            ensure!(
                source
                    .lines()
                    .nth(expected as usize - 1)
                    .unwrap()
                    .contains("txtUploadName")
            );
        }
    }
    owned.close()?;
    Ok(())
}

#[test]
fn only_webforms_requires_the_new_produced_position_contract() -> Result<()> {
    let expected = SourceExtractionExpectation::for_paths(2000, ["Home.aspx", "Source.rs"])?;
    let legacy = json!({"source_index_version":engram_index::SOURCE_INDEX_VERSION,
        "source_extraction_identity":SourceExtractionIdentity::for_chunk_target(2000)});
    for path in [
        "Home.aspx",
        "part.ASCX",
        "Site.master",
        "Service.asmx",
        "Handler.ashx",
        "Service.svc",
        "Global.asax",
    ] {
        ensure!(
            !expected.metadata_is_compatible(&legacy, path),
            "Legacy markup must not silently retain bad positions"
        );
        let mut current = legacy.clone();
        current["webforms_position_contract"] = json!(engram_index::webforms::POSITION_CONTRACT);
        ensure!(expected.metadata_is_compatible(&current, path));
    }
    for path in ["Source.rs", "style.css", "page.html", "view.cshtml"] {
        ensure!(
            expected.metadata_is_compatible(&legacy, path),
            "Unrelated extraction must remain available"
        );
    }
    let mut produced = IngestStats {
        source_extraction_identity: Some(SourceExtractionIdentity::for_chunk_target(2000)),
        files: 1,
        all_files: vec![RelPath::new("Empty.ascx")],
        ..Default::default()
    };
    ensure!(expected.verify_produced(&produced).is_err());
    produced
        .webforms_position_contracts
        .insert("Empty.ascx".into(), 0);
    ensure!(expected.verify_produced(&produced).is_err());
    produced.webforms_position_contracts.insert(
        "Empty.ascx".into(),
        engram_index::webforms::POSITION_CONTRACT,
    );
    expected.verify_produced(&produced)?;
    produced.webforms_position_contracts.insert(
        "NotProduced.aspx".into(),
        engram_index::webforms::POSITION_CONTRACT,
    );
    ensure!(expected.verify_produced(&produced).is_err());
    let mut empty = IngestStats::default();
    empty.webforms_position_contracts = produced.webforms_position_contracts;
    ensure!(expected.verify_produced(&empty).is_err());
    Ok(())
}

async fn settle(app: &Engram) -> Result<()> {
    loop {
        let jobs = app
            .state
            .active_jobs
            .write()
            .await
            .drain()
            .map(|(_, j)| j)
            .collect::<Vec<_>>();
        if jobs.is_empty() {
            return Ok(());
        }
        for job in jobs {
            job.await?;
        }
    }
}

fn owned_case<F, Fut>(run: F) -> Result<()>
where
    F: FnOnce(std::path::PathBuf) -> Fut,
    Fut: Future<Output = Result<()>>,
{
    let owned = tempfile::tempdir()?;
    let result = {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?;
        runtime.block_on(run(owned.path().to_owned()))
    }; // All application/database/worker handles close before teardown.
    let cleanup = owned.close();
    result?;
    cleanup?;
    Ok(())
}

#[test]
fn unchanged_markup_migrates_once_and_preserves_other_source_and_authored_knowledge() -> Result<()>
{
    owned_case(|owned| async move {
        let root = owned.join("repo");
        std::fs::create_dir_all(&root)?;
        drop(git2::Repository::init(&root)?);
        let source = "<%@ Page Language=\"C#\" %>\r\n<asp:Panel ID=\"parent\" runat=\"server\">\r\n<%-- comment --%>\r\n<asp:Button ID=\"btnUpload\" runat=\"server\" CssClass=\"upload\" />\r\n</asp:Panel>\r\n";
        std::fs::write(root.join("Home.aspx"), source)?;
        std::fs::write(root.join("Empty.ascx"), "")?;
        std::fs::write(root.join("Source.rs"), "pub fn unchanged_companion() {}\n")?;
        let (state, _) = AppState::new(Config {
            data_dir: owned.join("data"),
            allowed_roots: vec![root.clone()],
            embedding_backend: "fts_only".into(),
            llm_backend: "none".into(),
            ..Default::default()
        })?;
        let app = Engram::new(state.clone());
        let result = app
            .handle_index_project(serde_json::from_value(json!({"directory":root,
            "project_name":"WebForms position migration","project_type":"general","wait":true}))?)
            .await;
        settle(&app).await?;
        result.map_err(|e| anyhow::anyhow!("{e}"))?;
        let pid = state.registry.list_projects()?[0].project_id.clone();
        let note: MemorySection =
            serde_json::from_value(json!({"section_id":"decision","title":"Retained knowledge",
            "content":"Keep this authored decision through markup migration.","updated_at_ms":1}))?;
        state.registry.put_memory_section(&pid, &note)?;
        let before = state.registry.capture_source_head(&pid)?;
        let companion = state.graph.get_node(&pid, "file:Source.rs")?.unwrap();
        for path in ["Home.aspx", "Empty.ascx"] {
            let mut node = state
                .graph
                .get_node(&pid, &format!("file:{path}"))?
                .unwrap();
            ensure!(
                node.metadata.as_ref().unwrap()["webforms_position_contract"]
                    == engram_index::webforms::POSITION_CONTRACT
            );
            node.metadata
                .as_mut()
                .unwrap()
                .as_object_mut()
                .unwrap()
                .remove("webforms_position_contract");
            state.graph.upsert_nodes(&pid, &[node])?;
        }
        let mut control = state
            .graph
            .get_node(&pid, "control:Home.aspx:btnUpload")?
            .unwrap();
        control.start_line = 3;
        control.end_line = 3;
        state.graph.upsert_nodes(&pid, &[control])?;
        let result = app
            .handle_update_project(serde_json::from_value(
                json!({"project_id":pid,"wait":true}),
            )?)
            .await;
        settle(&app).await?;
        result.map_err(|e| anyhow::anyhow!("{e}"))?;
        let migrated = state.registry.capture_source_head(&pid)?;
        ensure!(
            !state.registry.source_head_is_current(&before)?,
            "Contract-incompatible unchanged markup must publish a migrated view"
        );
        let control = state
            .graph
            .get_node(&pid, "control:Home.aspx:btnUpload")?
            .unwrap();
        ensure!(
            (control.start_line, control.end_line) == (4, 4),
            "Published source position must inspect btnUpload"
        );
        let recovered=app.handle_get_full_method_body(serde_json::from_value(json!({"project_id":pid,
            "file_path":"Home.aspx","line_start":4,"line_end":4,"output_json":true,"context_lines":0}))?).await
            .map_err(|e|anyhow::anyhow!("{e}"))?;
        let recovered: serde_json::Value =
            serde_json::from_str(&recovered.content[0].as_text().unwrap().text)?;
        ensure!(
            recovered["source_code"]
                .as_str()
                .unwrap()
                .contains("btnUpload")
        );
        ensure!(recovered["source_file_hash"] == blake3::hash(source.as_bytes()).to_hex().as_str());
        for path in ["Home.aspx", "Empty.ascx"] {
            let node = state
                .graph
                .get_node(&pid, &format!("file:{path}"))?
                .unwrap();
            ensure!(
                node.metadata.as_ref().unwrap()["webforms_position_contract"]
                    == engram_index::webforms::POSITION_CONTRACT
            );
        }
        let after = state.graph.get_node(&pid, "file:Source.rs")?.unwrap();
        ensure!(
            after.generation == companion.generation && after.metadata == companion.metadata,
            "Unchanged non-WebForms source must be inherited without rewriting its metadata"
        );
        ensure!(
            state
                .registry
                .get_memory_section(&pid, "decision")?
                .unwrap()
                .content
                == note.content
        );
        ensure!(std::fs::read(root.join("Home.aspx"))? == source.as_bytes());
        let result = app
            .handle_update_project(serde_json::from_value(
                json!({"project_id":pid,"wait":true}),
            )?)
            .await;
        settle(&app).await?;
        result.map_err(|e| anyhow::anyhow!("{e}"))?;
        ensure!(
            state.registry.source_head_is_current(&migrated)?,
            "A second ordinary update must not repeat migration"
        );
        Ok(())
    })
}

#[test]
fn exclusive_refresh_preserves_outside_changes_deletions_and_unreadable_source() -> Result<()> {
    owned_case(|owned| async move {
        let root = owned.join("repo");
        std::fs::create_dir_all(&root)?;
        drop(git2::Repository::init(&root)?);
        std::fs::write(
            root.join("Home.aspx"),
            "<asp:Button ID=\"btnUpload\" runat=\"server\" />\n",
        )?;
        std::fs::write(root.join("Outside.rs"), "pub fn outside_unrelated() {}\n")?;
        std::fs::write(root.join("Deleted.rs"), "pub fn deleted_unrelated() {}\n")?;
        let (state, _) = AppState::new(Config {
            data_dir: owned.join("data"),
            allowed_roots: vec![root.clone()],
            embedding_backend: "fts_only".into(),
            llm_backend: "none".into(),
            ..Default::default()
        })?;
        let app = Engram::new(state.clone());
        let result=app.handle_index_project(serde_json::from_value(json!({"directory":root,"project_name":"Exclusive source refresh","project_type":"general","wait":true}))?).await;
        settle(&app).await?;
        result.map_err(|e| anyhow::anyhow!("{e}"))?;
        let pid = state.registry.list_projects()?[0].project_id.clone();
        ensure!(!state.registry.source_identity_needs_verification(&pid)?);
        let outside = state.graph.get_node(&pid, "file:Outside.rs")?.unwrap();
        let deleted = state.graph.get_node(&pid, "file:Deleted.rs")?.unwrap();
        let original_head = state.registry.capture_source_head(&pid)?;
        let runtime = state.get_project_cached(&pid).unwrap();
        let _old_view = runtime
            .search
            .pin_source_generation(&pid, original_head.generation())?;
        let original_docs =
            runtime
                .search
                .count_docs_in_generation(&pid, "memory", original_head.generation())?;
        ensure!(original_docs >= 3);
        std::fs::write(
            root.join("Outside.rs"),
            "pub fn outside_changed_on_disk() {}\n",
        )?;
        std::fs::remove_file(root.join("Deleted.rs"))?;
        std::fs::write(root.join("New.rs"), "pub fn new_unrelated() {}\n")?;
        std::fs::write(root.join(".gitignore"), "Ignored.aspx\n")?;
        std::fs::write(
            root.join("Ignored.aspx"),
            "<asp:Button ID=\"ignored\" runat=\"server\" />",
        )?;
        #[cfg(windows)]
        let outside_guard = {
            use std::os::windows::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .read(true)
                .share_mode(0)
                .open(root.join("Outside.rs"))?
        };
        let result = app
            .handle_update_project(serde_json::from_value(
                json!({"project_id":pid,"wait":true,"only_paths":["Home.aspx"]}),
            )?)
            .await;
        settle(&app).await?;
        let result = result.map_err(|e| anyhow::anyhow!("{e}"))?;
        let report = &result.content[0].as_text().unwrap().text;
        ensure!(
            report.contains("update_scope: only_paths")
                && report.contains("outside_scope_disk_check: not_run")
                && report.contains("history_enrichment: skipped")
        );
        let after = state.graph.get_node(&pid, "file:Outside.rs")?.unwrap();
        ensure!(after.generation == outside.generation && after.metadata == outside.metadata);
        let after = state.graph.get_node(&pid, "file:Deleted.rs")?.unwrap();
        ensure!(after.generation == deleted.generation && after.metadata == deleted.metadata);
        ensure!(state.graph.get_node(&pid, "file:New.rs")?.is_none());
        ensure!(
            runtime
                .search
                .count_docs_in_generation(&pid, "memory", original_head.generation())?
                == original_docs,
            "A retained source reader must remain usable after exclusive publication"
        );
        let published = state.registry.capture_source_head(&pid)?;
        ensure!(
            runtime
                .search
                .count_docs_in_generation(&pid, "memory", published.generation())?
                == original_docs,
            "The new source view must include all inherited outside chunks"
        );
        let hits=app.handle_grep_project(serde_json::from_value(json!({"project_id":pid,"pattern":"outside_unrelated","path_prefix":"Outside.rs","freshness":"off","output_json":true}))?).await.map_err(|e|anyhow::anyhow!("{e}"))?;
        let hits: serde_json::Value =
            serde_json::from_str(&hits.content[0].as_text().unwrap().text)?;
        ensure!(
            hits["matches"].as_array().unwrap().len() == 1,
            "Outside indexed chunks must remain inherited"
        );
        #[cfg(windows)]
        drop(outside_guard);
        let before = state.registry.capture_source_head(&pid)?;
        for request in [
            json!({"project_id":pid,"only_paths":[],"wait":true}),
            json!({"project_id":pid,"only_paths":["Home.aspx"],"wait":false}),
            json!({"project_id":pid,"only_paths":["Home.aspx"],"reindex_paths":["Home.aspx"],"wait":true}),
            json!({"project_id":pid,"only_paths":["Home.aspx"],"index_antipatterns":true,"wait":true}),
            json!({"project_id":pid,"only_paths":["Home.aspx","Missing.aspx"],"wait":true}),
            json!({"project_id":pid,"only_paths":["Ignored.aspx"],"wait":true}),
            json!({"project_id":pid,"only_paths":["../Outside.rs"],"wait":true}),
        ] {
            ensure!(
                app.handle_update_project(serde_json::from_value(request)?)
                    .await
                    .is_err()
            );
            settle(&app).await?;
            ensure!(
                state.registry.source_head_is_current(&before)?,
                "Invalid selection must publish nothing"
            );
        }
        let mut unpublished = outside.clone();
        unpublished.generation = before.generation() + 1;
        state.graph.upsert_nodes(&pid, &[unpublished])?;
        let result = app
            .handle_update_project(serde_json::from_value(
                json!({"project_id":pid,"only_paths":["Home.aspx"],"wait":true}),
            )?)
            .await;
        ensure!(result.is_err() && state.registry.source_head_is_current(&before)?);
        state.graph.upsert_nodes(&pid, &[outside])?;
        let home = state.graph.get_node(&pid, "file:Home.aspx")?.unwrap();
        let mut future = home.clone();
        future.generation = before.generation() + 2;
        state.graph.upsert_nodes(&pid, &[future])?;
        let result = app
            .handle_update_project(serde_json::from_value(json!({"project_id":pid,
            "only_paths":["Home.aspx"],"wait":true}))?)
            .await;
        ensure!(result.is_err() && state.registry.source_head_is_current(&before)?);
        state.graph.upsert_nodes(&pid, &[home])?;
        state
            .registry
            .set_meta(&pid, "source_identity_verified_v1", "0")?;
        let unverified = state.registry.capture_source_head(&pid)?;
        let result = app
            .handle_update_project(serde_json::from_value(
                json!({"project_id":pid,"only_paths":["Home.aspx"],"wait":true}),
            )?)
            .await;
        ensure!(result.is_err() && state.registry.source_head_is_current(&unverified)?);
        ensure!(
            state.registry.source_identity_needs_verification(&pid)?,
            "Scoped refresh must not certify inherited bytes"
        );
        settle(&app).await?;
        Ok(())
    })
}
