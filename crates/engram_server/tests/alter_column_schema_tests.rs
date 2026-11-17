//! Source migration columns stay discoverable without becoming effective schema.
use anyhow::{Result, ensure};
use engram_server::{QuerySchemaRequest, services::schema_query::Catalog};
use serde_json::{Value, json};

const BASELINE: &str =
    "CREATE TABLE dbo.Customer (Id int NOT NULL, Name varchar(20) NULL, Obsolete int);\nGO\n";
const CHANGES: &str = "IF NOT EXISTS (SELECT 1 FROM sys.columns WHERE name = 'ApprovalCode')\nBEGIN\nALTER TABLE dbo.Customer ADD\n ApprovalCode nvarchar(40) NULL,\n Enabled bit NOT NULL CONSTRAINT DF_Customer_Enabled DEFAULT (1);\nEND\nGO\nALTER TABLE dbo.Customer ALTER COLUMN Name nvarchar(200) NOT NULL;\nGO\nALTER TABLE dbo.Customer DROP COLUMN Obsolete;\nGO\n";

fn request(project: &str, query_type: Value) -> Result<QuerySchemaRequest> {
    Ok(serde_json::from_value(
        json!({"project_id":project,"query_type":query_type,"limit":100,"max_tokens":65536}),
    )?)
}
fn query(catalog: &Catalog, query_type: Value) -> Result<Value> {
    catalog.query(&request("owned-schema", query_type)?)
}

#[test]
fn table_and_new_column_queries_preserve_cited_operations_and_unchanged_create_baseline()
-> Result<()> {
    let mut catalog = Catalog::default();
    catalog.add_source("Post.sql", CHANGES)?;
    catalog.add_source("Customer.sql", BASELINE)?;
    let report = query(&catalog, json!({"kind":"table","name":"dbo.Customer"}))?;
    let original = &report["results"][0]["table"]["columns"];
    ensure!(original.as_array().unwrap().len() == 3);
    ensure!(original[1]["data_type"] == "VARCHAR(20)" && original[1]["nullable"] == true);
    let rows = report["results"][0]["alter_table_columns"]
        .as_array()
        .unwrap();
    ensure!(rows.len() == 4, "{report}");
    ensure!(rows[0]["column_name"] == "ApprovalCode" && rows[0]["operation"] == "add_column");
    ensure!(
        rows[0]["column"]["data_type"] == "NVARCHAR(40)" && rows[0]["column"]["nullable"] == true
    );
    ensure!(rows[0]["citation"]["start_line"] == 3 && rows[0]["citation"]["end_line"] == 4);
    ensure!(
        rows[1]["column"]["default_value"]
            .as_str()
            .unwrap()
            .contains('1')
    );
    ensure!(rows[2]["operation"] == "alter_column" && rows[2]["column"]["nullable"] == false);
    ensure!(
        rows[2]["column"]["data_type"] == "NVARCHAR(200)" && rows[2]["citation"]["start_line"] == 8
    );
    ensure!(
        rows[3]["operation"] == "drop_column"
            && rows[3]["column_name"] == "Obsolete"
            && rows[3]["column"].is_null()
    );
    for row in rows {
        ensure!(row["application_status"] == "not_established");
        ensure!(
            row["citation"]["source_hash"] == blake3::hash(CHANGES.as_bytes()).to_hex().to_string()
        );
    }
    let column = query(
        &catalog,
        json!({"kind":"column","table":"dbo.Customer","column":"ApprovalCode"}),
    )?;
    ensure!(column["total_matches"] == 1 && column["results"][0] == rows[0]);
    let changed = query(
        &catalog,
        json!({"kind":"column","table":"dbo.Customer","column":"Name"}),
    )?;
    ensure!(
        changed["total_matches"] == 2,
        "CREATE baseline plus separate operation: {changed}"
    );
    Ok(())
}

#[test]
fn qualified_targets_and_non_schema_sql_do_not_invent_or_merge_column_changes() -> Result<()> {
    let mut catalog = Catalog::default();
    catalog.add_source(
        "base.sql",
        &format!("{BASELINE}CREATE TABLE crm.Customer (Id int);\nGO\n"),
    )?;
    catalog.add_source("changes.sql", "-- ALTER TABLE dbo.Customer ADD Hidden int;\nSELECT 'ALTER TABLE dbo.Customer ADD Quoted int';\nGO\nCREATE PROCEDURE dbo.Migrate AS\nALTER TABLE dbo.Customer ADD Internal int;\nGO\nALTER TABLE crm.Customer ADD Segment nvarchar(80);\nALTER TABLE dbo.Customer ADD CONSTRAINT CK_Customer CHECK (Id > 0);\n")?;
    let dbo = query(&catalog, json!({"kind":"table","name":"dbo.Customer"}))?;
    ensure!(
        dbo["results"][0]["alter_table_columns"]
            .as_array()
            .unwrap()
            .is_empty(),
        "{dbo}"
    );
    let crm = query(&catalog, json!({"kind":"table","name":"crm.Customer"}))?;
    ensure!(
        crm["results"][0]["alter_table_columns"]
            .as_array()
            .unwrap()
            .len()
            == 1
    );
    ensure!(crm["results"][0]["alter_table_columns"][0]["column_name"] == "Segment");
    let ambiguous = query(&catalog, json!({"kind":"table","name":"Customer"}))?;
    ensure!(ambiguous["status"] == "ambiguous", "{ambiguous}");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn indexed_column_operations_withhold_changed_source_until_refresh() -> Result<()> {
    use engram_core::Config;
    use engram_server::{AppState, Engram};
    struct Fixture {
        server: Engram,
        owned: tempfile::TempDir,
    }
    let parent = std::env::var_os("ENGRAM_TEST_FIXTURE_ROOT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let owned = tempfile::Builder::new().prefix("ac-").tempdir_in(parent)?;
    let root = owned.path().join("source");
    std::fs::create_dir(&root)?;
    std::fs::write(root.join("Customer.sql"), BASELINE)?;
    std::fs::write(root.join("Post.sql"), CHANGES)?;
    std::fs::write(
        root.join("AGENTS.md"),
        "Preserve approval for customer exports. Exception: previews are read-only.\n",
    )?;
    let (state, _) = AppState::new(Config {
        data_dir: owned.path().join("data"),
        allowed_roots: vec![root.clone()],
        embedding_backend: "fts_only".into(),
        llm_backend: "none".into(),
        watch_debounce_secs: 3600,
        ..Default::default()
    })?;
    let fixture = Fixture {
        server: Engram::new(state),
        owned,
    };
    let indexed = fixture.server.handle_index_project(serde_json::from_value(json!({"directory":root,"project_name":"Owned column migrations","project_type":"general","wait":true}))?).await?;
    let project = indexed.content[0]
        .as_text()
        .unwrap()
        .text
        .split("project_id: ")
        .nth(1)
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .trim();
    async fn read(server: &Engram, project: &str) -> Result<Value> {
        let result = server
            .handle_query_schema(request(
                project,
                json!({"kind":"column","table":"dbo.Customer","column":"ApprovalCode"}),
            )?)
            .await?;
        Ok(serde_json::from_str(
            &result.content[0].as_text().unwrap().text,
        )?)
    }
    let first = read(&fixture.server, project).await?;
    ensure!(
        first["total_matches"] == 1 && first.to_string().contains("previews are read-only"),
        "{first}"
    );
    let edited = CHANGES.replace("nvarchar(40)", "nvarchar(120)");
    std::fs::write(root.join("Post.sql"), &edited)?;
    let stale = read(&fixture.server, project).await?;
    ensure!(
        stale["total_matches"] == 0
            && stale["coverage"]["gaps"]
                .to_string()
                .contains("source differs"),
        "{stale}"
    );
    fixture
        .server
        .handle_update_project(serde_json::from_value(
            json!({"project_id":project,"wait":true,"only_paths":["Post.sql"]}),
        )?)
        .await?;
    let fresh = read(&fixture.server, project).await?;
    ensure!(
        fresh["results"][0]["column"]["data_type"] == "NVARCHAR(120)",
        "{fresh}"
    );
    ensure!(
        fresh["results"][0]["citation"]["source_hash"]
            == blake3::hash(edited.as_bytes()).to_hex().to_string()
    );
    ensure!(fresh["coverage"]["database_execution"] == false);
    let Fixture { server, owned } = fixture;
    drop(server);
    owned.close()?;
    Ok(())
}
