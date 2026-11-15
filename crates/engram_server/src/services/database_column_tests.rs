use super::*;

#[test]
fn shared_view_and_trigger_declarations_preserve_events_and_source_citations() {
    let source = "CREATE TABLE [dbo].[Orders] ([Id] INT NOT NULL);\nGO\nCREATE VIEW [dbo].[OrderTotals] ([Id]) AS SELECT o.Id FROM [dbo].[Orders] o;\nGO\nCREATE TRIGGER [dbo].[AuditOrders] ON [dbo].[Orders] AFTER INSERT, UPDATE, DELETE AS BEGIN INSERT [dbo].[Audit] (Id) SELECT Id FROM inserted; END;\nGO\nGRANT SELECT ON [dbo].[OrderTotals] TO [reader];\n";
    let files = vec![("schema.sql".into(), source.into())];
    let catalog = source_catalog(&files);
    let views = project_catalog_views(&catalog);
    assert_eq!(views.len(), 1);
    assert_eq!(views[0].name, "OrderTotals");
    assert_eq!(views[0].qualified_name, "[dbo].[OrderTotals]");
    assert_eq!(views[0].source_tables, ["Orders"]);
    assert_eq!(
        (
            views[0].source_citation.start_line,
            views[0].source_citation.end_line
        ),
        (3, 3)
    );
    let triggers = project_catalog_triggers(&catalog);
    assert_eq!(triggers.len(), 1);
    assert_eq!(triggers[0].event_types, ["INSERT", "UPDATE", "DELETE"]);
    assert_eq!(triggers[0].trigger_type, "AFTER");
    assert_eq!(
        triggers[0].qualified_target_table.as_deref(),
        Some("[dbo].[Orders]")
    );
    assert!(triggers[0].body_summary.contains("Audit"));
    for (kind, expected) in [
        (
            "views_sourced_from",
            serde_json::to_value(&views[0].source_citation).unwrap(),
        ),
        (
            "triggers_on_table",
            serde_json::to_value(&triggers[0].source_citation).unwrap(),
        ),
    ] {
        let request = serde_json::from_value(
            serde_json::json!({"project_id":"p","query_type":{"kind":kind,"table":"dbo.Orders"}}),
        )
        .unwrap();
        let result = catalog.query(&request).unwrap();
        assert_eq!(result["results"][0]["object"]["citation"], expected);
        assert_eq!(
            expected["source_hash"],
            blake3::hash(source.as_bytes()).to_hex().to_string()
        );
    }
    let empty =
        super::super::full_project_migration_service::build_sp_catalog_public(&files, &[], 1);
    let report = build_database_intelligence(&empty, &files, &HashSet::new());
    let markdown = render_database_intelligence_markdown(&report);
    assert!(markdown.contains("[dbo].[OrderTotals]") && markdown.contains("schema.sql:3-3"));
    assert!(markdown.contains("[dbo].[AuditOrders]") && markdown.contains("schema.sql:5-5"));
    assert!(!markdown.contains("GRANT SELECT"));
}

#[test]
fn shared_post_deploy_targets_preserve_mixed_operations_without_keyword_tables() {
    let source = "INSERT [dbo].[Settings] (Id) VALUES (1), (2);\nMERGE [dbo].[Settings] AS target USING [dbo].[Defaults] AS incoming ON target.Id=incoming.Id WHEN MATCHED THEN UPDATE SET Id=incoming.Id WHEN NOT MATCHED THEN INSERT (Id) VALUES (incoming.Id);\nALTER TABLE [dbo].[Settings] ADD [Active] bit NULL;\nCREATE NONCLUSTERED INDEX [IX_Settings] ON [dbo].[Settings] ([Id]);\nDROP INDEX [IX_Settings] ON [dbo].[Settings];\n";
    let analysis = analyze_sql_script("deployment.sql", source).unwrap();
    assert_eq!(analysis.script_type, ScriptType::Mixed);
    assert_eq!(analysis.tables_affected, ["Settings"]);
    for operation in ["INSERT", "MERGE", "UPDATE", "ALTER", "INDEX"] {
        assert!(analysis.operations.iter().any(|value| value == operation));
    }
    assert!(analysis.row_count_estimate.is_some());
    let canonical = engram_index::sql_parser::analyze_write_operations(source);
    assert!(
        canonical
            .iter()
            .any(|reference| reference.operation == "INSERT" && reference.table == "Settings")
    );
    let deleted = analyze_sql_script("delete.sql", "DELETE [dbo].[Settings] WHERE Id=1;").unwrap();
    assert_eq!(deleted.tables_affected, ["Settings"]);
    assert_eq!(deleted.operations, ["DELETE"]);
}

#[test]
fn combined_database_report_preserves_selected_procedures_chains_and_script_exclusion() {
    let files = vec![
        ("read.sql".into(), "CREATE PROCEDURE [dbo].[ReadOrders] AS BEGIN SELECT Id FROM dbo.Orders; EXEC dbo.SaveOrders; END\nGO\nGRANT EXECUTE ON dbo.ReadOrders TO reader;".into()),
        ("save.sql".into(), "CREATE PROCEDURE [dbo].[SaveOrders] @Id INT AS BEGIN INSERT dbo.Orders (Id) VALUES (@Id); END\nGO\n".into()),
        ("seed.sql".into(), "INSERT dbo.Settings (Id) VALUES (1);".into()),
    ];
    let selected =
        super::super::full_project_migration_service::build_sp_catalog_public(&files, &[], 10);
    let report = build_database_intelligence(&selected, &files, &HashSet::new());
    assert_eq!(
        report
            .sp_logic
            .iter()
            .map(|row| row.sp_name.as_str())
            .collect::<Vec<_>>(),
        selected
            .procedures
            .iter()
            .map(|row| row.name.as_str())
            .collect::<Vec<_>>()
    );
    assert!(
        report
            .sp_call_chains
            .iter()
            .any(|row| row.chain == ["ReadOrders", "SaveOrders"])
    );
    assert_eq!(report.sql_scripts.len(), 1);
    assert_eq!(report.sql_scripts[0].file_path, "seed.sql");
    assert_eq!(report.sql_scripts[0].tables_affected, ["Settings"]);
    assert!(
        report
            .sp_logic
            .iter()
            .all(|row| !row.parameters.iter().any(|value| value.contains("GRANT")))
    );
}

#[test]
fn source_indexes_attach_to_exact_table_names_with_their_own_citations() {
    let source = "CREATE TABLE [dbo].[Orders] ([Id] INT NOT NULL);\nGO\nCREATE NONCLUSTERED INDEX [IX_Orders_Id] ON [dbo].[Orders] ([Id] ASC);\nGO\nCREATE TABLE [audit].[Orders] ([Id] INT NOT NULL);";
    let tables = parse_create_tables(&[("indexes.sql".into(), source.into())]);
    assert_eq!(tables.len(), 2);
    let dbo = tables
        .iter()
        .find(|table| table.qualified_name.as_deref() == Some("[dbo].[Orders]"))
        .unwrap();
    let audit = tables
        .iter()
        .find(|table| table.qualified_name.as_deref() == Some("[audit].[Orders]"))
        .unwrap();
    assert_eq!(dbo.index_evidence.len(), 1);
    assert!(audit.indexes.is_empty());
    assert!(audit.index_evidence.is_empty());
    let index = &dbo.index_evidence[0];
    assert_eq!(index.name, "[IX_Orders_Id]");
    assert_eq!(index.binding, "exact_source_name");
    assert_eq!(dbo.indexes, [index.declaration.clone()]);
    assert_eq!(index.source_citation.path, "indexes.sql");
    assert_eq!(
        (
            index.source_citation.start_line,
            index.source_citation.end_line
        ),
        (3, 3)
    );
    assert_eq!(
        index.source_citation.source_hash,
        blake3::hash(source.as_bytes()).to_hex().to_string()
    );
    let intel = DatabaseIntelligence {
        schema: SchemaReport {
            tables,
            ..Default::default()
        },
        ..Default::default()
    };
    let markdown = render_database_intelligence_markdown(&intel);
    assert!(markdown.contains("[IX_Orders_Id]"));
    assert!(markdown.contains("indexes.sql:3-3"));
}

#[test]
fn unbound_index_spelling_is_a_visible_gap_without_a_guessed_default_schema() {
    let source = "CREATE TABLE [dbo].[Orders] ([Id] INT NOT NULL);\nGO\nCREATE INDEX [IX_Unqualified] ON [Orders] ([Id]);";
    let (tables, gaps) =
        parse_schema_tables_with_coverage(&[("unqualified.sql".into(), source.into())]);
    assert!(tables[0].indexes.is_empty());
    assert_eq!(gaps.len(), 1);
    assert!(gaps[0].contains("[IX_Unqualified]"));
    assert!(gaps[0].contains("no exact matching table declaration"));
}

#[test]
fn shared_table_inventory_preserves_composite_keys_and_source_citation() {
    let source = "-- Declared schema, not deployed-state evidence.\nCREATE TABLE [sales].[Lines] (\n[TenantId] INT,\n[LineId] INT,\n[ParentId] INT NULL,\nCONSTRAINT PK_Lines PRIMARY KEY CLUSTERED ([TenantId] ASC, [LineId] DESC),\nCONSTRAINT FK_Parents FOREIGN KEY ([TenantId], [ParentId]) REFERENCES [sales].[Parents] ([TenantId], [Id]) ON DELETE CASCADE ON UPDATE NO ACTION,\nCONSTRAINT CK_Parent CHECK (ParentId > 0)\n);";
    let tables = parse_create_tables(&[("lines.sql".into(), source.into())]);
    assert_eq!(tables.len(), 1);
    let table = &tables[0];
    assert_eq!(table.name, "Lines");
    assert_eq!(table.qualified_name.as_deref(), Some("[sales].[Lines]"));
    assert_eq!(table.primary_key, ["TenantId", "LineId"]);
    assert!(!table.columns[0].nullable);
    assert!(!table.columns[1].nullable);
    assert_eq!(table.foreign_keys.len(), 2);
    let pairs: Vec<_> = table
        .foreign_keys
        .iter()
        .map(|key| (key.column.as_str(), key.references_column.as_str()))
        .collect();
    assert_eq!(pairs, [("TenantId", "TenantId"), ("ParentId", "Id")]);
    assert!(
        table
            .foreign_keys
            .iter()
            .all(|key| key.references_table == "Parents"
                && key.on_delete.as_deref() == Some("CASCADE")
                && key.on_update.as_deref() == Some("NO ACTION"))
    );
    assert_eq!(table.check_constraints.len(), 1);
    assert!(table.parsing_warnings.is_empty());
    let citation = table.source_citation.as_ref().unwrap();
    assert_eq!(citation.path, "lines.sql");
    assert_eq!(citation.start_line, 2);
    assert_eq!(citation.end_line, source.lines().count() as u64);
    assert_eq!(
        citation.source_hash,
        blake3::hash(source.as_bytes()).to_hex().to_string()
    );
    let intel = DatabaseIntelligence {
        schema: SchemaReport {
            tables,
            ..Default::default()
        },
        ..Default::default()
    };
    let markdown = render_database_intelligence_markdown(&intel);
    assert!(markdown.contains("[sales].[Lines]"));
    assert!(markdown.contains("lines.sql:2-9"));
}

#[test]
fn warning_only_database_report_remains_visible_without_recognized_objects() {
    let intel = DatabaseIntelligence {
        warnings: vec!["INCOMPLETE schema inventory: SQL source could not be tokenized".into()],
        ..Default::default()
    };
    let markdown = render_database_intelligence_markdown(&intel);
    assert!(markdown.contains("INCOMPLETE schema inventory"));
    assert!(markdown.contains("SQL source could not be tokenized"));
}

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
