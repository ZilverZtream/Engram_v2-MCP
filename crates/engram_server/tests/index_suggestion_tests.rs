use engram_server::services::{
    index_suggestions::{QuerySource, Suggestions},
    schema_query::{Catalog, Citation},
};
use serde_json::Value;

fn source(path: &str, sql: &str) -> QuerySource {
    QuerySource {
        sql: sql.into(),
        citation: Citation {
            path: path.into(),
            start_line: 1,
            end_line: sql.lines().count() as u64,
            source_hash: blake3::hash(sql.as_bytes()).to_hex().to_string(),
        },
        exact_lines: true,
    }
}
const SCHEMA: &str = r#"CREATE TABLE [dbo].[Orders] (
 [Id] int NOT NULL PRIMARY KEY, [CustomerId] int NOT NULL,
 [TenantId] int NOT NULL, [Created] datetime2 NOT NULL,
 [Active] bit NOT NULL, [Title] nvarchar(80) NULL);
GO
CREATE TABLE [crm].[Customers] ([Id] int NOT NULL PRIMARY KEY, [Name] nvarchar(100) NULL);
GO
"#;
fn analyze(schema: &str, queries: &[QuerySource]) -> Value {
    let mut catalog = Catalog::default();
    catalog.add_source("schema.sql", schema).unwrap();
    let mut engine = Suggestions::new(&catalog);
    for query in queries {
        engine.add_source(query);
    }
    engine.finish()
}

#[test]
fn index_suggestions_use_where_join_order_and_rank_independent_sites() {
    let sql = "SELECT o.Id, o.Title FROM dbo.Orders o JOIN crm.Customers c ON c.Id=o.CustomerId WHERE o.TenantId=@tenant ORDER BY o.Created DESC;";
    let result = analyze(
        SCHEMA,
        &[
            source("a.sql", sql),
            source("b.sql", sql),
            source("c.sql", "SELECT Id FROM dbo.Orders WHERE Active=1"),
        ],
    );
    let rows = result["suggestions"].as_array().unwrap();
    assert_eq!(rows.len(), 2, "{result}");
    assert_eq!(rows[0]["table"], "[dbo].[Orders]");
    assert_eq!(rows[0]["static_query_sites"], 2);
    assert_eq!(
        rows[0]["key_columns"]
            .as_array()
            .unwrap()
            .iter()
            .map(|k| k["column"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["TenantId", "CustomerId", "Created"]
    );
    assert_eq!(rows[0]["key_columns"][2]["ascending"], false);
    assert!(
        rows[0]["create_index"]
            .as_str()
            .unwrap()
            .contains("[Created] DESC")
    );
    assert!(
        rows[0]["include_columns"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("Title"))
    );
    assert_eq!(result["covered_pattern_count"], 1); // Customers.Id already has its PK.
    assert!(
        rows[1]["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w.as_str().unwrap().contains("BIT cardinality"))
    );
    assert!(
        rows[1]["filtered_alternative"]
            .as_str()
            .unwrap()
            .contains("WHERE [Active] = 1")
    );
    assert!(rows[1]["estimated_selectivity"].is_null());
}

#[test]
fn index_suggestions_distinguish_leading_keys_include_and_filter_predicates() {
    let schema = format!(
        "{SCHEMA}CREATE INDEX IX_Wrong ON dbo.Orders(TenantId, CustomerId) INCLUDE (Active);\nGO\nCREATE INDEX IX_Filtered ON dbo.Orders(CustomerId) INCLUDE(Title) WHERE Active=1;\nGO\n"
    );
    let result = analyze(
        &schema,
        &[source(
            "q.sql",
            "SELECT Title FROM dbo.Orders WHERE CustomerId=@id AND Active=1;",
        )],
    );
    // The filter fixes Active, so the eligible CustomerId index covers access.
    assert!(
        result["suggestions"].as_array().unwrap().is_empty(),
        "{result}"
    );
    assert_eq!(result["covered_pattern_count"], 1);
    let alone = analyze(
        &schema,
        &[source(
            "q.sql",
            "SELECT Title FROM dbo.Orders WHERE CustomerId=@id;",
        )],
    );
    assert_eq!(alone["suggestions"].as_array().unwrap().len(), 1, "{alone}");
    let covered_schema = format!(
        "{schema}CREATE INDEX IX_Right ON dbo.Orders(CustomerId, Active) INCLUDE(Title);\nGO\n"
    );
    let covered = analyze(
        &covered_schema,
        &[source(
            "q.sql",
            "SELECT Title FROM dbo.Orders WHERE CustomerId=@id AND Active=1;",
        )],
    );
    assert!(
        covered["suggestions"].as_array().unwrap().is_empty(),
        "{covered}"
    );
    assert!(
        covered["covered_patterns"][0]["projected_columns_not_in_index"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn index_suggestions_respect_alias_scopes_ctes_and_ambiguity() {
    let nested = "WITH recent AS (SELECT CustomerId AS Customer FROM dbo.Orders WHERE TenantId=@t) SELECT r.Customer FROM recent r WHERE r.Customer=@c;";
    let result = analyze(SCHEMA, &[source("q.sql", nested)]);
    assert_eq!(
        result["suggestions"].as_array().unwrap().len(),
        2,
        "{result}"
    );
    assert!(
        result["coverage"]["gaps"].as_array().unwrap().is_empty(),
        "{result}"
    );
    let duplicate = format!("{SCHEMA}CREATE TABLE audit.Orders(Id int, CustomerId int);\nGO\n");
    let ambiguous = analyze(
        &duplicate,
        &[source(
            "q.sql",
            "SELECT Id FROM Orders WHERE CustomerId=@id;",
        )],
    );
    assert!(ambiguous["suggestions"].as_array().unwrap().is_empty());
    assert!(
        ambiguous["coverage"]["gaps"]
            .as_array()
            .unwrap()
            .iter()
            .any(|g| g.as_str().unwrap().contains("ambiguous"))
    );
    let bad = analyze(
        SCHEMA,
        &[source(
            "q.sql",
            "SELECT o.Id FROM dbo.Orders o JOIN crm.Customers c ON c.Id=o.Id WHERE Id=@id;",
        )],
    );
    assert!(bad["suggestions"].as_array().unwrap().is_empty(), "{bad}");
}

#[test]
fn index_suggestions_parse_procedures_and_do_not_analyze_comments_or_strings() {
    let sql = "-- SELECT * FROM dbo.Orders WHERE Active=1;\nCREATE PROCEDURE dbo.ReadOrders @customer int AS BEGIN\n SELECT Title FROM dbo.Orders WHERE CustomerId=@customer;\n SELECT 'SELECT Id FROM dbo.Orders WHERE Active=1';\nEND;\nGO\n";
    let result = analyze(SCHEMA, &[source("read.sql", sql)]);
    assert_eq!(
        result["suggestions"].as_array().unwrap().len(),
        1,
        "{result}"
    );
    let row = &result["suggestions"][0];
    assert_eq!(row["key_columns"][0]["column"], "CustomerId");
    assert_eq!(row["query_sites"][0]["citation"]["start_line"], 3);
    let mut inline = source(
        "Page.vb",
        "SELECT Title FROM dbo.Orders WHERE CustomerId=@id",
    );
    inline.exact_lines = false;
    inline.citation.start_line = 21;
    inline.citation.end_line = 26;
    let inline_result = analyze(SCHEMA, &[inline]);
    assert_eq!(
        inline_result["suggestions"][0]["query_sites"][0]["citation"]["start_line"],
        21
    );
    assert_eq!(
        inline_result["suggestions"][0]["query_sites"][0]["span_kind"],
        "containing_indexed_method_or_statement"
    );
}

#[test]
fn index_suggestions_withhold_unsupported_expressions_and_invalid_key_types() {
    let result = analyze(
        SCHEMA,
        &[source(
            "q.sql",
            "SELECT Id FROM dbo.Orders WHERE LOWER(Title)=@title;",
        )],
    );
    assert!(result["suggestions"].as_array().unwrap().is_empty());
    assert!(!result["coverage"]["gaps"].as_array().unwrap().is_empty());
    let schema = "CREATE TABLE dbo.Notes(Id int, Body nvarchar(max));";
    let invalid = analyze(
        schema,
        &[source(
            "q.sql",
            "SELECT Id FROM dbo.Notes WHERE Body=@text;",
        )],
    );
    assert_eq!(invalid["suggestions"][0]["status"], "ddl_withheld");
    assert!(invalid["suggestions"][0]["create_index"].is_null());
}

#[test]
fn index_suggestions_reuse_unique_and_permuted_equality_keys() {
    let singleton = analyze(
        SCHEMA,
        &[source(
            "q.sql",
            "SELECT Title FROM dbo.Orders WHERE Id=@id AND Active=1;",
        )],
    );
    assert!(
        singleton["suggestions"].as_array().unwrap().is_empty(),
        "{singleton}"
    );
    let schema =
        format!("{SCHEMA}CREATE INDEX IX_Existing ON dbo.Orders(TenantId,CustomerId);\nGO\n");
    let reordered = analyze(
        &schema,
        &[source(
            "q.sql",
            "SELECT Title FROM dbo.Orders WHERE CustomerId=@id AND TenantId=@tenant;",
        )],
    );
    assert!(
        reordered["suggestions"].as_array().unwrap().is_empty(),
        "{reordered}"
    );
    assert_eq!(reordered["covered_pattern_count"], 1);
    let range = analyze(
        SCHEMA,
        &[source(
            "q.sql",
            "SELECT Title FROM dbo.Orders WHERE Id>@id AND CustomerId=@customer;",
        )],
    );
    assert_eq!(range["suggestions"].as_array().unwrap().len(), 1, "{range}");
}

#[test]
fn index_suggestions_analyze_schema_bound_view_bodies_with_source_lines() {
    let query = "CREATE VIEW dbo.OrderCustomers\nWITH SCHEMABINDING\nAS\nSELECT dbo.Orders.Title FROM dbo.Orders INNER JOIN crm.Customers ON crm.Customers.Id=dbo.Orders.CustomerId;\nGO\n";
    let result = analyze(SCHEMA, &[source("view.sql", query)]);
    assert!(
        result["coverage"]["gaps"].as_array().unwrap().is_empty(),
        "{result}"
    );
    assert_eq!(
        result["suggestions"].as_array().unwrap().len(),
        1,
        "{result}"
    );
    assert_eq!(
        result["suggestions"][0]["query_sites"][0]["citation"]["start_line"],
        4
    );
    assert_eq!(
        result["suggestions"][0]["key_columns"][0]["column"],
        "CustomerId"
    );
}
