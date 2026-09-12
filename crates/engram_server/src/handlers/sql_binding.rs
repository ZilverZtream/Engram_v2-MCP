//! Static T-SQL identifier binding. Execution and type checking are separate.
use super::access_layer_tools::SqlValidationIssue;
use engram_graph::GraphStore;
use sqlparser::{dialect::MsSqlDialect, parser::Parser};

pub(super) struct Binding {
    pub complete: bool,
    pub tables: Vec<String>,
    pub issues: Vec<SqlValidationIssue>,
}

/// Parse T-SQL, then bind supported query scopes against indexed evidence.
pub(super) fn bind(sql: &str, graph: &GraphStore, pid: &str) -> Result<Binding, String> {
    let mut result = Binding {
        complete: false,
        tables: Vec::new(),
        issues: Vec::new(),
    };
    let statements = match Parser::parse_sql(&MsSqlDialect {}, sql) {
        Ok(statements) => statements,
        Err(error) => {
            result.issues.push(issue(
                "fail",
                "sql_syntax",
                format!("T-SQL parsing failed: {error}"),
            ));
            return Ok(result);
        }
    };
    let [statement] = statements.as_slice() else {
        return Ok(result);
    };
    super::sql_scopes::bind(statement, graph, pid)
}

fn issue(severity: &str, category: &str, message: String) -> SqlValidationIssue {
    SqlValidationIssue {
        severity: severity.into(),
        category: category.into(),
        message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engram_core::RelPath;
    use engram_graph::Node;

    #[test]
    fn joins_predicates_and_ambiguity_are_bound_to_the_declared_tables() {
        let tmp = tempfile::tempdir().unwrap();
        let graph = GraphStore::open(&tmp.path().join("graph.redb")).unwrap();
        let mut nodes = Vec::new();
        for (table, columns) in [
            ("orders", vec!["id", "customer_id"]),
            ("customers", vec!["id", "name"]),
        ] {
            for (id, kind, name) in
                std::iter::once((format!("table:{table}"), "db_table", table.to_string())).chain(
                    columns.into_iter().map(|column| {
                        (
                            format!("column:{table}:{column}"),
                            "db_column",
                            column.into(),
                        )
                    }),
                )
            {
                nodes.push(Node {
                    node_id: id,
                    node_type: kind.into(),
                    name,
                    namespace: "memory".into(),
                    language: "sql".into(),
                    file_path: RelPath::new("schema.sql"),
                    start_line: 1,
                    end_line: 1,
                    generation: 1,
                    metadata: if kind == "db_table" { Some(serde_json::json!({"ddl": if table == "orders" { "CREATE TABLE dbo.orders (id int, customer_id int)" } else { "CREATE TABLE dbo.customers (id int, name nvarchar(100))" }})) } else { None },
                });
            }
        }
        graph.upsert_nodes("test", &nodes).unwrap();
        for (sql, expected) in [
            (
                "SELECT o.id, c.name FROM orders o JOIN customers c ON c.id = o.customer_id WHERE c.name = 'example'",
                None,
            ),
            (
                "SELECT o.id FROM orders o WHERE o.missing = @value",
                Some("unknown_column"),
            ),
            (
                "SELECT id FROM orders o JOIN customers c ON c.id = o.customer_id",
                Some("ambiguous_column"),
            ),
            (
                "SELECT c.name FROM orders o JOIN customers c ON c.missing = o.customer_id",
                Some("unknown_column"),
            ),
            ("SELECT z.id FROM orders o", Some("unknown_qualifier")),
            (
                "SELECT o.id FROM orders o JOIN customers o ON o.id = o.id",
                Some("duplicate_alias"),
            ),
            ("SELECT id FROM orders WHERE id IN (1, 2, 3)", None),
            ("SELECT COUNT(id) FROM orders WHERE id > 0", None),
            (
                "SELECT o.id FROM orders o JOIN customers c",
                Some("missing_join_condition"),
            ),
            (
                "WITH x(a,a) AS (SELECT id, customer_id FROM orders) SELECT 1 FROM x",
                Some("duplicate_output"),
            ),
            ("SELECT id FROM dbo.orders", None),
            ("SELECT id FROM [dbo].[orders]", None),
            (
                "SELECT o.id FROM dbo.orders o WHERE o.customer_id = @id",
                None,
            ),
            (
                "INSERT INTO orders (id, customer_id) VALUES (1, @customer)",
                None,
            ),
            ("INSERT INTO orders (id) SELECT id FROM customers", None),
            (
                "INSERT INTO orders (id) VALUES (1, 2)",
                Some("insert_arity"),
            ),
            (
                "INSERT INTO orders (id, id) VALUES (1, 2)",
                Some("duplicate_insert_column"),
            ),
            (
                "INSERT INTO orders (missing) VALUES (1)",
                Some("unknown_column"),
            ),
            (
                "INSERT INTO orders (id) VALUES (missing)",
                Some("unknown_column"),
            ),
            (
                "UPDATE orders SET customer_id = @customer WHERE id = 1",
                None,
            ),
            (
                "UPDATE orders SET customer_id = missing WHERE id = 1",
                Some("unknown_column"),
            ),
            (
                "UPDATE orders SET missing = 1 WHERE id = 1",
                Some("unknown_column"),
            ),
            (
                "UPDATE orders SET id = 1, id = 2",
                Some("duplicate_assignment"),
            ),
            ("DELETE FROM orders WHERE id = @id", None),
            (
                "DELETE FROM orders WHERE missing = @id",
                Some("unknown_column"),
            ),
            (
                "SELECT id FROM orders WHERE id IN (SELECT id, name FROM customers)",
                Some("subquery_arity"),
            ),
            (
                "SELECT (SELECT id, name FROM customers) FROM orders",
                Some("subquery_arity"),
            ),
            (
                "WITH x AS (SELECT id, id FROM orders) SELECT id FROM x",
                Some("duplicate_output"),
            ),
            ("WITH x AS (SELECT id FROM orders) SELECT id FROM x", None),
            (
                "WITH x(order_id) AS (SELECT id FROM orders) SELECT order_id FROM x",
                None,
            ),
            ("SELECT x.id FROM (SELECT id FROM orders) x", None),
            (
                "SELECT o.id FROM orders o WHERE EXISTS (SELECT 1 FROM customers c WHERE c.id = o.customer_id)",
                None,
            ),
            (
                "SELECT id AS result_id FROM orders ORDER BY result_id",
                None,
            ),
            ("SELECT id FROM orders ORDER BY 1", None),
            ("SELECT id FROM orders ORDER BY 2", Some("order_ordinal")),
            (
                "SELECT id AS result_id FROM orders WHERE result_id > 0",
                Some("unknown_column"),
            ),
            (
                "WITH x AS (SELECT id FROM orders) SELECT missing FROM x",
                Some("unknown_column"),
            ),
            (
                "SELECT id FROM orders WHERE id IN (SELECT missing FROM customers)",
                Some("unknown_column"),
            ),
            (
                "SELECT o.id FROM orders o JOIN customers c ON z.id = o.id JOIN customers z ON z.id = c.id",
                Some("unknown_qualifier"),
            ),
            (
                "SELECT id FROM orders ORDER BY missing",
                Some("unknown_column"),
            ),
        ] {
            let result = bind(sql, &graph, "test").unwrap();
            assert!(result.complete, "{sql}");
            match expected {
                Some(category) => assert!(
                    result.issues.iter().any(|issue| issue.category == category),
                    "{sql}"
                ),
                None => assert!(result.issues.is_empty(), "{sql}: {:?}", result.issues),
            }
        }
        for sql in [
            "SELECT wrong.* FROM orders",
            "SELECT COUNT(wrong.*) FROM orders",
            "WITH orders AS (SELECT 1 FROM orders) SELECT 1 FROM orders",
            "WITH x AS (SELECT id FROM y), y AS (SELECT id FROM orders) SELECT id FROM x",
            "UPDATE o SET id = 1 FROM orders o",
            "EXEC missing_procedure @id = 1",
            "UPDATE orders SET id = DEFAULT",
            "INSERT INTO orders (id) VALUES (DEFAULT)",
            "SELECT id FROM missing_schema.orders",
        ] {
            assert!(!bind(sql, &graph, "test").unwrap().complete, "{sql}");
        }
    }
}
