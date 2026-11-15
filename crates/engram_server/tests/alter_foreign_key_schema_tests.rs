//! Source-declared post-deployment relationships are discoverable without claiming execution.
use anyhow::{Result, ensure};
use engram_server::{QuerySchemaRequest, services::schema_query::Catalog};
use serde_json::{Value, json};

const SCHEMA: &str = "CREATE TABLE [dbo].[team_member] ([member_id] int NOT NULL, [cost_center_id] int NOT NULL);\nGO\nCREATE TABLE [dbo].[cost_center] ([cost_center_id] int NOT NULL PRIMARY KEY);\nGO\n";
const POST: &str = "IF EXISTS (SELECT 1 FROM sys.foreign_keys WHERE name = 'FK_team_member_cost_center')\nBEGIN\n PRINT 'Already exists';\nEND\nELSE\nBEGIN\n ALTER TABLE [dbo].[team_member]\n ADD CONSTRAINT [FK_team_member_cost_center]\n FOREIGN KEY ([cost_center_id])\n REFERENCES [dbo].[cost_center] ([cost_center_id])\nEND\nGO\n";

fn request(query_type: Value) -> Result<QuerySchemaRequest> {
    Ok(serde_json::from_value(
        json!({"project_id":"owned-schema", "query_type":query_type}),
    )?)
}
fn query(catalog: &Catalog, query_type: Value) -> Result<Value> {
    catalog.query(&request(query_type)?)
}

#[test]
fn post_deployment_foreign_key_is_cited_in_table_and_incoming_queries_without_changing_baseline()
-> Result<()> {
    let mut catalog = Catalog::default();
    // Source order is not migration order: collect the script before table definitions.
    catalog.add_source("Scripts/Post/costCenter.sql", POST)?;
    catalog.add_source("Tables.sql", SCHEMA)?;
    let table = query(&catalog, json!({"kind":"table","name":"dbo.team_member"}))?;
    ensure!(
        table["results"][0]["table"]["foreign_keys"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let rows = table["results"][0]["alter_table_foreign_keys"]
        .as_array()
        .unwrap();
    ensure!(rows.len() == 1, "{table}");
    let row = &rows[0];
    ensure!(row["table"] == "[dbo].[team_member]");
    ensure!(row["foreign_key"]["columns"] == json!(["cost_center_id"]));
    ensure!(row["foreign_key"]["references_table"] == "[dbo].[cost_center]");
    ensure!(row["foreign_key"]["referenced_columns"] == json!(["cost_center_id"]));
    ensure!(row["application_status"] == "not_established");
    ensure!(row["citation"]["path"] == "Scripts/Post/costCenter.sql");
    ensure!(row["citation"]["start_line"] == 7 && row["citation"]["end_line"] == 10);
    ensure!(row["citation"]["source_hash"] == blake3::hash(POST.as_bytes()).to_hex().to_string());
    let incoming = query(
        &catalog,
        json!({"kind":"tables_that_reference_foreign_key","table":"dbo.cost_center","column":"cost_center_id"}),
    )?;
    ensure!(incoming["total_matches"] == 1 && incoming["results"][0] == *row);
    ensure!(
        !catalog.gaps.is_empty(),
        "Execution/order coverage is still incomplete"
    );
    Ok(())
}

#[test]
fn composite_and_multiple_alter_foreign_keys_preserve_actions_qualified_names_and_pagination()
-> Result<()> {
    let mut catalog = Catalog::default();
    catalog.add_source("schema.sql", "CREATE TABLE dbo.Child (Tenant int, Id int); GO\nCREATE TABLE crm.Parent (Tenant int, Id int); GO\nCREATE TABLE other.Parent (Tenant int, Id int); GO\n")?;
    catalog.add_source("post.sql", "ALTER TABLE dbo.Child WITH CHECK ADD\nCONSTRAINT FK_Parent FOREIGN KEY (Tenant, Id) REFERENCES crm.Parent(Tenant, Id) ON DELETE CASCADE ON UPDATE NO ACTION,\nCONSTRAINT FK_Second FOREIGN KEY (Id) REFERENCES crm.Parent(Id);\nALTER TABLE dbo.Child WITH NOCHECK ADD FOREIGN KEY (Id) REFERENCES other.Parent(Id);\n")?;
    let mut req = request(
        json!({"kind":"tables_that_reference_foreign_key","table":"crm.Parent","column":"Id"}),
    )?;
    req.limit = Some(1);
    let first = catalog.query(&req)?;
    ensure!(
        first["total_matches"] == 2 && first["next_offset"] == 1,
        "{first}"
    );
    ensure!(first["results"][0]["foreign_key"]["columns"] == json!(["Tenant", "Id"]));
    ensure!(first["results"][0]["foreign_key"]["on_delete"] == "CASCADE");
    ensure!(first["results"][0]["foreign_key"]["on_update"] == "NO ACTION");
    req.offset = Some(1);
    let second = catalog.query(&req)?;
    ensure!(second["results"][0]["foreign_key"]["columns"] == json!(["Id"]));
    let other = query(
        &catalog,
        json!({"kind":"tables_that_reference_foreign_key","table":"other.Parent","column":"Id"}),
    )?;
    ensure!(other["total_matches"] == 1);
    ensure!(other["results"][0]["foreign_key"]["references_table"] == "[other].[Parent]");
    Ok(())
}

#[test]
fn comments_strings_and_procedure_bodies_do_not_become_schema_foreign_keys() -> Result<()> {
    let mut catalog = Catalog::default();
    catalog.add_source("schema.sql", SCHEMA)?;
    catalog.add_source("not_declarations.sql", "-- ALTER TABLE dbo.team_member ADD FOREIGN KEY (cost_center_id) REFERENCES dbo.cost_center(cost_center_id);\nSELECT 'ALTER TABLE dbo.team_member ADD FOREIGN KEY (cost_center_id) REFERENCES dbo.cost_center(cost_center_id)';\nGO\nCREATE PROCEDURE dbo.ChangeRelationship AS\nALTER TABLE dbo.team_member ADD FOREIGN KEY (cost_center_id) REFERENCES dbo.cost_center(cost_center_id);\nGO\nALTER TABLE dbo.team_member ALTER COLUMN cost_center_id bigint;\n")?;
    let incoming = query(
        &catalog,
        json!({"kind":"tables_that_reference_foreign_key","table":"dbo.cost_center","column":"cost_center_id"}),
    )?;
    ensure!(incoming["total_matches"] == 0, "{incoming}");
    ensure!(catalog.gaps.iter().any(|gap| gap.contains("ALTER TABLE")));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn public_schema_lookup_withholds_changed_script_until_source_refresh() -> Result<()> {
    use engram_core::Config;
    use engram_server::{AppState, Engram};
    let owned = tempfile::tempdir()?;
    let root = owned.path().join("source");
    std::fs::create_dir(&root)?;
    std::fs::write(root.join("schema.sql"), SCHEMA)?;
    std::fs::write(root.join("post.sql"), POST)?;
    let (state, _) = AppState::new(Config {
        data_dir: owned.path().join("data"),
        allowed_roots: vec![root.clone()],
        embedding_backend: "fts_only".into(),
        llm_backend: "none".into(),
        ..Default::default()
    })?;
    let engram = Engram::new(state);
    let indexed = engram.handle_index_project(serde_json::from_value(json!({"directory":root,"project_name":"Post deployment schema","project_type":"general","wait":true}))?).await?;
    let text = &indexed.content[0].as_text().unwrap().text;
    let pid = text
        .split("project_id: ")
        .nth(1)
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .trim();
    let make_request = || -> Result<QuerySchemaRequest> {
        Ok(serde_json::from_value(
            json!({"project_id":pid,"query_type":{"kind":"tables_that_reference_foreign_key","table":"dbo.cost_center","column":"cost_center_id"}}),
        )?)
    };
    async fn call(e: &Engram, req: QuerySchemaRequest) -> Result<Value> {
        let result = e.handle_query_schema(req).await?;
        Ok(serde_json::from_str(
            &result.content[0].as_text().unwrap().text,
        )?)
    }
    let first = call(&engram, make_request()?).await?;
    ensure!(first["total_matches"] == 1, "{first}");
    let changed = POST.replace(
        "FOREIGN KEY ([cost_center_id])",
        "FOREIGN KEY ([member_id])",
    );
    std::fs::write(root.join("post.sql"), &changed)?;
    let stale = call(&engram, make_request()?).await?;
    ensure!(stale["total_matches"] == 0);
    ensure!(
        stale["coverage"]["gaps"]
            .as_array()
            .unwrap()
            .iter()
            .any(|gap| gap.as_str().unwrap().contains("source differs"))
    );
    engram
        .handle_update_project(serde_json::from_value(
            json!({"project_id":pid,"wait":true,"only_paths":["post.sql"]}),
        )?)
        .await?;
    let refreshed = call(&engram, make_request()?).await?;
    ensure!(refreshed["total_matches"] == 1);
    ensure!(refreshed["results"][0]["foreign_key"]["columns"] == json!(["member_id"]));
    ensure!(
        refreshed["results"][0]["citation"]["source_hash"]
            == blake3::hash(changed.as_bytes()).to_hex().to_string()
    );
    ensure!(refreshed["coverage"]["database_execution"] == false);
    drop(engram);
    owned.close()?;
    Ok(())
}
