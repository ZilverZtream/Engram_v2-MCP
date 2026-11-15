use super::*;

#[test]
fn ordinary_sql_server_column_sizes_defaults_and_computed_values_are_preserved() {
    let sql = vec![(
        "orders.sql".into(),
        r#"
CREATE TABLE [dbo].[Orders] (
    [Id] INT IDENTITY (1, 1) NOT NULL,
    [Label] NVARCHAR (100) DEFAULT ('Open') NOT NULL,
    [Enabled] BIT CONSTRAINT DF_Enabled DEFAULT ((1)) NOT NULL,
    [Amount] DECIMAL (12, 2) DEFAULT ((0)) NOT NULL,
    [CreatedAt] DATETIME DEFAULT (GETDATE()) NOT NULL,
    [Display] AS ([Label] + ' order')
);
"#
        .into(),
    )];
    let tables = parse_create_tables(&sql);
    let table = &tables[0];
    assert_eq!(table.columns.len(), 6);
    assert!(table.parsing_warnings.is_empty());
    let column = |name: &str| table.columns.iter().find(|col| col.name == name).unwrap();
    assert_eq!(column("Label").data_type, "NVARCHAR(100)");
    assert_eq!(column("Amount").data_type.replace(' ', ""), "DECIMAL(12,2)");
    assert_eq!(column("Enabled").default_value.as_deref(), Some("((1))"));
    assert_eq!(column("Amount").default_value.as_deref(), Some("((0))"));
    assert_eq!(
        column("CreatedAt").default_value.as_deref(),
        Some("(GETDATE())")
    );
    assert!(!column("Label").nullable);
    assert!(column("Display").is_computed);
    assert!(
        column("Display")
            .computed_expression
            .as_ref()
            .unwrap()
            .contains("' order'")
    );
}

#[test]
fn unparsed_declarations_remain_in_json_markdown_and_report_warnings() {
    let sql = vec![(
        "review.sql".into(),
        "CREATE TABLE Review ([Id] INT NOT NULL, [UnparsedColumn] INT REVIEW_REQUIRED_OPTIONS);"
            .into(),
    )];
    let catalog = StoredProcedureCatalog {
        procedures: vec![],
        total_procedures: 0,
        procedures_with_params: 0,
        procedures_called_from_code: 0,
        uncalled_procedures: vec![],
    };
    let intel = build_database_intelligence(&catalog, &sql, &HashSet::new());
    assert_eq!(intel.schema.tables[0].columns.len(), 1);
    let warnings = &intel.schema.tables[0].parsing_warnings;
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("[UnparsedColumn] INT REVIEW_REQUIRED_OPTIONS"));
    assert!(intel.warnings.contains(&warnings[0]));
    assert!(
        serde_json::to_string(&intel)
            .unwrap()
            .contains("UnparsedColumn")
    );
    let markdown = render_database_intelligence_markdown(&intel);
    assert!(markdown.contains("INCOMPLETE schema declaration"));
    assert!(markdown.contains("REVIEW_REQUIRED_OPTIONS"));
}
