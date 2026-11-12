use super::*;

const SQL: &str = r#"
-- CREATE TABLE fake (bad int);
CREATE TABLE [dbo].[Orders] (
  [TenantID] int NOT NULL,
  [OrderID] int NOT NULL,
  [CustomerID] int NULL,
  [Note] nvarchar(80) CONSTRAINT DF_Note DEFAULT (concat('a,b', ')')) NULL,
  [UnknownNullability] int,
  [Total] AS ([OrderID] * 2) PERSISTED,
  CONSTRAINT PK_Orders PRIMARY KEY CLUSTERED ([TenantID] ASC, [OrderID] ASC),
  CONSTRAINT FK_Customer FOREIGN KEY ([TenantID], [CustomerID]) REFERENCES [crm].[Customers]([TenantID], [CustomerID]) ON DELETE CASCADE ON UPDATE NO ACTION
);
GO
CREATE TABLE [crm].[Customers] ([TenantID] int NOT NULL, [CustomerID] int NOT NULL);
GO
CREATE UNIQUE NONCLUSTERED INDEX [IX_Orders] ON [dbo].[Orders] ([CustomerID] ASC);
GO
CREATE VIEW [dbo].[OrderView] AS SELECT * FROM [dbo].[Orders];
GO
CREATE TRIGGER [dbo].[OrderAudit] ON [dbo].[Orders] AFTER INSERT, UPDATE AS SELECT 1;
GO
CREATE PROCEDURE [dbo].[ReadOrders] @id int AS
SELECT * FROM [dbo].[Orders] WHERE OrderID=@id;
SELECT 'CREATE TABLE string_fake (id int)';
CREATE TABLE #temporary (id int);
GO
"#;

fn request(q: Value) -> QuerySchemaRequest {
    serde_json::from_value(json!({"project_id":"p","query_type":q})).unwrap()
}
fn query(c: &Catalog, q: Value) -> Value {
    c.query(&request(q)).unwrap()
}

#[test]
fn schema_queries_preserve_sql_server_columns_keys_defaults_and_relations() {
    let mut c = Catalog::default();
    c.add_source("schema.sql", SQL).unwrap();
    let table = query(&c, json!({"kind":"table","name":"dbo.Orders"}));
    let t = &table["results"][0]["table"];
    assert_eq!(t["columns"].as_array().unwrap().len(), 6, "{}", table);
    assert_eq!(t["primary_key"], json!(["TenantID", "OrderID"]));
    assert_eq!(t["columns"][0]["nullable"], false);
    assert!(
        t["columns"][3]["default_value"]
            .as_str()
            .unwrap()
            .contains("'a,b'")
    );
    assert!(t["columns"][4]["nullable"].is_null());
    assert_eq!(
        t["foreign_keys"][0]["referenced_columns"],
        json!(["TenantID", "CustomerID"])
    );
    assert_eq!(
        t["foreign_keys"][0]["references_table"],
        "[crm].[Customers]"
    );
    assert_eq!(t["foreign_keys"][0]["on_delete"], "CASCADE");
    assert_eq!(t["foreign_keys"][0]["on_update"], "NO ACTION");
    assert!(t["gaps"].as_array().unwrap().is_empty(), "{}", t);
    assert_eq!(
        query(&c, json!({"kind":"indexes_on_table","table":"dbo.Orders"}))["total_matches"],
        2,
        "Standalone index and primary-key index both matter"
    );
    for kind in [
        "triggers_on_table",
        "views_sourced_from",
        "computed_columns",
    ] {
        assert_eq!(
            query(&c, json!({"kind":kind,"table":"dbo.Orders"}))["total_matches"],
            1,
            "{kind}"
        );
    }
    assert_eq!(
        query(&c, json!({"kind":"nullable_columns","table":"dbo.Orders"}))["total_matches"],
        2
    );
    assert_eq!(
        query(
            &c,
            json!({"kind":"column","table":"dbo.Orders","column":"Note"})
        )["total_matches"],
        1
    );
    assert_eq!(
        query(&c, json!({"kind":"stored_proc","sp_name":"dbo.ReadOrders"}))["total_matches"],
        1
    );
    assert_eq!(
        query(
            &c,
            json!({"kind":"tables_that_reference_foreign_key","table":"crm.Customers","column":"CustomerID"})
        )["total_matches"],
        1
    );
    assert_eq!(
        query(
            &c,
            json!({"kind":"tables_with_column","column_name_pattern":"customer"})
        )["total_matches"],
        2
    );
    assert_eq!(
        query(&c, json!({"kind":"all_tables"}))["total_matches"],
        2,
        "Comments, strings and procedure-local tables must not enter the schema"
    );
}

#[test]
fn qualified_and_duplicate_definitions_never_silently_select_one() {
    let mut c = Catalog::default();
    c.add_source(
        "one.sql",
        "CREATE TABLE dbo.Orders(id int NULL); CREATE TABLE audit.Orders(id int NOT NULL);",
    )
    .unwrap();
    assert_eq!(
        query(&c, json!({"kind":"table","name":"Orders"}))["status"],
        "ambiguous"
    );
    assert_eq!(
        query(&c, json!({"kind":"table","name":"[audit].[Orders]"}))["total_matches"],
        1
    );
    c.add_source("two.sql", "CREATE TABLE dbo.Orders(id int NULL);")
        .unwrap();
    assert_eq!(
        query(&c, json!({"kind":"table","name":"dbo.Orders"}))["status"],
        "ambiguous"
    );
    assert_eq!(
        query(&c, json!({"kind":"table","name":"unknown.Orders"}))["status"],
        "not_found"
    );
}

#[test]
fn query_controls_and_pagination_are_explicit() {
    let mut c = Catalog::default();
    c.add_source(
        "one.sql",
        "CREATE TABLE dbo.A(id int); CREATE TABLE dbo.B(id int);",
    )
    .unwrap();
    let mut req = request(json!({"kind":"all_tables"}));
    req.limit = Some(1);
    let first = c.query(&req).unwrap();
    assert_eq!(first["next_offset"], 1);
    assert_eq!(first["total_matches"], 2);
    req.offset = Some(1);
    assert!(c.query(&req).unwrap()["next_offset"].is_null());
    req.limit = Some(0);
    assert!(validate(&req).is_err());
    assert!(
        validate(&request(
            json!({"kind":"table","name":"dbo.A; DROP TABLE B"})
        ))
        .is_err()
    );
    assert!(
        validate(&request(
            json!({"kind":"column","table":"A","column":"A.id"})
        ))
        .is_err()
    );
}

#[test]
fn unqualified_references_and_alterations_disclose_their_limits() {
    let mut c = Catalog::default();
    c.add_source("one.sql","CREATE TABLE dbo.A(id int); ALTER TABLE dbo.A ADD x int; CREATE TABLE dbo.B(a int REFERENCES A(id));").unwrap();
    assert_eq!(c.gaps.len(), 1);
    let result = query(
        &c,
        json!({"kind":"tables_that_reference_foreign_key","table":"dbo.A","column":"id"}),
    );
    assert_eq!(
        result["results"][0]["binding"],
        "unqualified_candidate_default_schema_unknown"
    );
}

#[test]
fn inline_keys_are_indexes_and_table_links_do_not_expand_procedure_bodies() {
    let mut c = Catalog::default();
    c.add_source(
        "keys.sql",
        "CREATE TABLE dbo.Keys(id int PRIMARY KEY, email varchar(80) UNIQUE);",
    )
    .unwrap();
    assert_eq!(
        query(&c, json!({"kind":"indexes_on_table","table":"dbo.Keys"}))["total_matches"],
        2
    );
    c.add_source("schema.sql", SQL).unwrap();
    let table = query(&c, json!({"kind":"table","name":"dbo.Orders"}));
    let link = &table["results"][0]["stored_procedures"][0]["object"];
    assert_eq!(link["declaration_included"], false);
    assert!(link.get("declaration").is_none());
    let recovery: QuerySchemaRequest =
        serde_json::from_value(link["recovery"]["arguments"].clone()).unwrap();
    assert!(
        c.query(&recovery).unwrap()["results"][0]["declaration"]
            .as_str()
            .unwrap()
            .contains("#temporary")
    );
}
