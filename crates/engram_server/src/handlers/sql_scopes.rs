//! Static identifier scopes. This does not compile or execute SQL.
use super::{access_layer_tools::SqlValidationIssue, sql_binding::Binding};
use engram_graph::GraphStore;
use sqlparser::ast::*;
use sqlparser::{dialect::MsSqlDialect, parser::Parser};
use std::{collections::HashMap, ops::ControlFlow};

#[derive(Clone, Default)]
struct Relation {
    qualifier: String,
    base: Option<String>,
    columns: Option<Vec<String>>,
    known: bool,
}

#[derive(Default)]
struct References {
    columns: Vec<Vec<String>>,
    queries: Vec<(Query, bool)>,
    scalar_queries: std::collections::HashSet<usize>,
    depth: usize,
    unsupported: bool,
}
impl Visitor for References {
    type Break = ();
    fn pre_visit_query(&mut self, query: &Query) -> ControlFlow<()> {
        if self.depth == 0 {
            self.queries.push((
                query.clone(),
                self.scalar_queries
                    .contains(&(query as *const Query as usize)),
            ));
        }
        self.depth += 1;
        ControlFlow::Continue(())
    }
    fn post_visit_query(&mut self, _: &Query) -> ControlFlow<()> {
        self.depth -= 1;
        ControlFlow::Continue(())
    }
    fn pre_visit_expr(&mut self, expr: &Expr) -> ControlFlow<()> {
        if self.depth != 0 {
            return ControlFlow::Continue(());
        }
        match expr {
            Expr::Subquery(query)
            | Expr::InSubquery {
                subquery: query, ..
            } => {
                self.scalar_queries
                    .insert(query.as_ref() as *const Query as usize);
            }
            Expr::Identifier(id) if !id.value.starts_with('@') => {
                self.columns.push(vec![id.value.to_lowercase()])
            }
            Expr::CompoundIdentifier(ids) => self
                .columns
                .push(ids.iter().map(|id| id.value.to_lowercase()).collect()),
            Expr::QualifiedWildcard(..) => self.unsupported = true,
            Expr::Function(f) => {
                if let FunctionArguments::List(args) = &f.args {
                    if args.args.iter().any(|arg| {
                        !matches!(
                            arg,
                            FunctionArg::Unnamed(
                                FunctionArgExpr::Expr(_) | FunctionArgExpr::Wildcard
                            )
                        )
                    }) {
                        self.unsupported = true;
                    }
                }
            }
            _ => {}
        }
        ControlFlow::Continue(())
    }
}

struct Context<'a> {
    graph: &'a GraphStore,
    pid: &'a str,
    result: Binding,
    depth: usize,
    queries_bound: usize,
}

pub(super) fn bind(
    statement: &Statement,
    graph: &GraphStore,
    pid: &str,
) -> Result<Binding, String> {
    let mut context = Context {
        graph,
        pid,
        result: Binding {
            complete: true,
            tables: Vec::new(),
            issues: Vec::new(),
        },
        depth: 0,
        queries_bound: 0,
    };
    context.statement(statement)?;
    context
        .result
        .issues
        .sort_by(|a, b| (&a.category, &a.message).cmp(&(&b.category, &b.message)));
    context
        .result
        .issues
        .dedup_by(|a, b| a.category == b.category && a.message == b.message);
    Ok(context.result)
}

impl Context<'_> {
    fn statement(&mut self, statement: &Statement) -> Result<(), String> {
        match statement {
            Statement::Query(query) => {
                self.query(query, &HashMap::new(), &[])?;
            }
            Statement::Update {
                table,
                assignments,
                from,
                selection,
                returning,
                or,
                limit,
            } => {
                if from.is_some()
                    || returning.is_some()
                    || or.is_some()
                    || limit.is_some()
                    || !table.joins.is_empty()
                    || !matches!(table.relation, TableFactor::Table { .. })
                {
                    self.partial("UPDATE FROM, joined targets, returning/conflict/limit clauses need additional validation");
                    return Ok(());
                }
                let mut projection = Vec::new();
                let mut targets = std::collections::HashSet::new();
                for assignment in assignments {
                    let AssignmentTarget::ColumnName(target) = &assignment.target else {
                        self.partial("Tuple assignment targets need additional binding");
                        return Ok(());
                    };
                    let identity = target
                        .0
                        .iter()
                        .filter_map(|part| part.as_ident().map(|id| id.value.to_lowercase()))
                        .collect::<Vec<_>>();
                    if !targets.insert(identity) {
                        self.issue(
                            "fail",
                            "duplicate_assignment",
                            format!("Column '{target}' is assigned more than once"),
                        );
                    }
                    projection.push(target.to_string());
                    projection.push(self.value_expression(&assignment.value));
                }
                let predicate = selection
                    .as_ref()
                    .map(|expr| format!(" WHERE {expr}"))
                    .unwrap_or_default();
                self.synthetic_query(&format!(
                    "SELECT {} FROM {table}{predicate}",
                    projection.join(", ")
                ))?;
            }
            Statement::Delete(delete) => {
                let (FromTable::WithFromKeyword(from) | FromTable::WithoutKeyword(from)) =
                    &delete.from;
                if !delete.tables.is_empty()
                    || delete.using.is_some()
                    || delete.returning.is_some()
                    || delete.limit.is_some()
                    || !delete.order_by.is_empty()
                    || from.len() != 1
                    || !from[0].joins.is_empty()
                    || !matches!(from[0].relation, TableFactor::Table { .. })
                {
                    self.partial(
                        "Joined/multiple DELETE targets or extra clauses need additional binding",
                    );
                    return Ok(());
                }
                let predicate = delete
                    .selection
                    .as_ref()
                    .map(|expr| format!(" WHERE {expr}"))
                    .unwrap_or_default();
                self.synthetic_query(&format!("SELECT 1 FROM {}{predicate}", from[0]))?;
            }
            Statement::Insert(insert) => {
                let TableObject::TableName(table) = &insert.table else {
                    self.partial("INSERT table functions need additional binding");
                    return Ok(());
                };
                if insert.columns.is_empty()
                    || insert.table_alias.is_some()
                    || insert.or.is_some()
                    || insert.on.is_some()
                    || insert.returning.is_some()
                    || !insert.assignments.is_empty()
                    || insert.partitioned.is_some()
                    || !insert.after_columns.is_empty()
                    || insert.insert_alias.is_some()
                    || insert.settings.is_some()
                    || insert.format_clause.is_some()
                    || insert.overwrite
                    || insert.replace_into
                    || insert.ignore
                    || insert.priority.is_some()
                {
                    self.partial("INSERT needs explicit target columns and no unsupported conflict/partition/returning clauses");
                    return Ok(());
                }
                let mut targets = std::collections::HashSet::new();
                for column in &insert.columns {
                    if !targets.insert(column.value.to_lowercase()) {
                        self.issue(
                            "fail",
                            "duplicate_insert_column",
                            format!("INSERT column '{column}' is listed more than once"),
                        );
                    }
                }
                self.synthetic_query(&format!(
                    "SELECT {} FROM {table}",
                    insert
                        .columns
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                ))?;
                let Some(source) = &insert.source else {
                    self.partial("INSERT defaults require column/default contracts");
                    return Ok(());
                };
                if let SetExpr::Values(values) = source.body.as_ref() {
                    if source.with.is_some()
                        || source.order_by.is_some()
                        || source.limit_clause.is_some()
                        || source.fetch.is_some()
                    {
                        self.partial("VALUES source has unsupported clauses");
                    }
                    for row in &values.rows {
                        if row.len() != insert.columns.len() {
                            self.issue(
                                "fail",
                                "insert_arity",
                                "INSERT value count differs from target column count".into(),
                            );
                        }
                        let projection = row
                            .iter()
                            .map(|expr| self.value_expression(expr))
                            .collect::<Vec<_>>()
                            .join(", ");
                        self.synthetic_query(&format!("SELECT {projection}"))?;
                    }
                } else {
                    let outputs = self.query(source, &HashMap::new(), &[])?;
                    if !outputs.is_empty() && outputs.len() != insert.columns.len() {
                        self.issue(
                            "fail",
                            "insert_arity",
                            "INSERT SELECT projection differs from target column count".into(),
                        );
                    }
                }
            }
            _ => self.partial("Statement/procedure contracts require additional validation"),
        }
        Ok(())
    }
    // These SELECTs are built exclusively from parsed AST nodes. They are
    // never executed; they reuse exactly the same identifier-scope binder.
    fn synthetic_query(&mut self, sql: &str) -> Result<(), String> {
        let statements = Parser::parse_sql(&MsSqlDialect {}, sql)
            .map_err(|error| format!("Internal DML binding projection: {error}"))?;
        let [Statement::Query(query)] = statements.as_slice() else {
            return Err("Internal DML projection was not a query".into());
        };
        self.query(query, &HashMap::new(), &[])?;
        Ok(())
    }
    fn value_expression(&mut self, expr: &Expr) -> String {
        if matches!(expr, Expr::Identifier(id) if id.value.eq_ignore_ascii_case("DEFAULT")) {
            self.partial("DEFAULT values require column/default contracts");
            "NULL".into()
        } else {
            expr.to_string()
        }
    }
    fn issue(&mut self, severity: &str, category: &str, message: String) {
        self.result.issues.push(SqlValidationIssue {
            severity: severity.into(),
            category: category.into(),
            message,
        });
    }
    fn partial(&mut self, message: &str) {
        self.result.complete = false;
        self.issue("info", "unsupported_binding", message.into());
    }
    fn query(
        &mut self,
        query: &Query,
        inherited: &HashMap<String, Relation>,
        outer: &[Vec<Relation>],
    ) -> Result<Vec<Option<String>>, String> {
        if self.depth >= 16 || self.queries_bound >= 512 {
            self.partial("Query binding capped at depth 16 and 512 total scopes");
            return Ok(Vec::new());
        }
        self.queries_bound += 1;
        self.depth += 1;
        let result = self.query_inner(query, inherited, outer);
        self.depth -= 1;
        result
    }
    fn query_inner(
        &mut self,
        query: &Query,
        inherited: &HashMap<String, Relation>,
        outer: &[Vec<Relation>],
    ) -> Result<Vec<Option<String>>, String> {
        let mut ctes = inherited.clone();
        if let Some(with) = &query.with {
            if with.recursive {
                self.partial("Recursive CTE binding is not supported");
                return Ok(Vec::new());
            }
            let mut local_names = std::collections::HashSet::new();
            // Reserve every local name before binding definitions. A forward
            // or self reference must not accidentally resolve to a base table.
            for cte in &with.cte_tables {
                let name = cte.alias.name.value.to_lowercase();
                ctes.insert(
                    name.clone(),
                    Relation {
                        qualifier: name,
                        ..Default::default()
                    },
                );
            }
            for cte in &with.cte_tables {
                let name = cte.alias.name.value.to_lowercase();
                if !local_names.insert(name.clone()) {
                    self.issue("fail", "duplicate_cte", format!("Duplicate CTE '{name}'"));
                }
                let outputs = self.query(&cte.query, &ctes, outer)?;
                let columns = self.output_names(outputs, &cte.alias);
                ctes.insert(
                    name.clone(),
                    Relation {
                        qualifier: name,
                        columns,
                        known: true,
                        base: None,
                    },
                );
            }
        }
        if query.limit_clause.is_some()
            || query.fetch.is_some()
            || !query.locks.is_empty()
            || query.for_clause.is_some()
            || query.settings.is_some()
            || query.format_clause.is_some()
            || !query.pipe_operators.is_empty()
        {
            self.partial("Query limit/locking/format clauses need additional validation");
        }
        let SetExpr::Select(select) = query.body.as_ref() else {
            self.partial("This query body requires additional binding (set operations or VALUES)");
            return Ok(Vec::new());
        };
        if select.into.is_some()
            || !select.named_window.is_empty()
            || !select.lateral_views.is_empty()
            || select.exclude.is_some()
            || select.connect_by.is_some()
            || select.value_table_mode.is_some()
        {
            self.partial("SELECT INTO, named windows, lateral or dialect extensions need additional validation");
        }
        let mut relations = Vec::new();
        for from in &select.from {
            relations.push(self.relation(&from.relation, &ctes)?);
            for join in &from.joins {
                relations.push(self.relation(&join.relation, &ctes)?);
                let constraint = match &join.join_operator {
                    JoinOperator::Join(c)
                    | JoinOperator::Inner(c)
                    | JoinOperator::Left(c)
                    | JoinOperator::LeftOuter(c)
                    | JoinOperator::Right(c)
                    | JoinOperator::RightOuter(c)
                    | JoinOperator::FullOuter(c)
                    | JoinOperator::CrossJoin(c) => Some(c),
                    _ => {
                        self.partial("This join operator requires additional binding");
                        None
                    }
                };
                if let Some(constraint) = constraint {
                    match constraint {
                        JoinConstraint::On(expr) => {
                            let mut scopes = vec![relations.clone()];
                            scopes.extend_from_slice(outer);
                            let mut refs = References::default();
                            let _ = expr.visit(&mut refs);
                            self.references(refs, &scopes, &ctes)?;
                        }
                        JoinConstraint::None => {
                            if !matches!(join.join_operator, JoinOperator::CrossJoin(_)) {
                                self.issue(
                                    "fail",
                                    "missing_join_condition",
                                    "This JOIN requires an ON condition".into(),
                                );
                            }
                        }
                        _ => self.partial("Natural/USING joins require additional binding"),
                    }
                }
            }
        }
        let mut names = std::collections::HashSet::new();
        for relation in &relations {
            if !names.insert(&relation.qualifier) {
                self.issue(
                    "fail",
                    "duplicate_alias",
                    format!("Duplicate table qualifier '{}'", relation.qualifier),
                );
            }
        }
        let mut scopes = vec![relations.clone()];
        scopes.extend_from_slice(outer);
        let mut refs = References::default();
        let _ = select.projection.visit(&mut refs);
        let _ = select.top.visit(&mut refs);
        let _ = select.selection.visit(&mut refs);
        let _ = select.prewhere.visit(&mut refs);
        let _ = select.group_by.visit(&mut refs);
        let _ = select.having.visit(&mut refs);
        let _ = select.qualify.visit(&mut refs);
        let _ = select.cluster_by.visit(&mut refs);
        let _ = select.distribute_by.visit(&mut refs);
        let _ = select.sort_by.visit(&mut refs);
        self.references(refs, &scopes, &ctes)?;
        let mut outputs = Vec::new();
        for item in &select.projection {
            match item {
                SelectItem::ExprWithAlias { alias, .. } => {
                    outputs.push(Some(alias.value.to_lowercase()))
                }
                SelectItem::UnnamedExpr(Expr::Identifier(id)) => {
                    outputs.push(Some(id.value.to_lowercase()))
                }
                SelectItem::UnnamedExpr(Expr::CompoundIdentifier(ids)) => {
                    outputs.push(ids.last().map(|id| id.value.to_lowercase()))
                }
                SelectItem::UnnamedExpr(_) => outputs.push(None),
                _ => {
                    self.partial("Wildcard projection expansion is not yet verified");
                    outputs.push(None);
                }
            }
        }
        if let Some(order) = &query.order_by {
            match &order.kind {
                OrderByKind::Expressions(expressions) => {
                    for item in expressions {
                        if let Expr::Identifier(id) = &item.expr {
                            let count = outputs
                                .iter()
                                .flatten()
                                .filter(|name| name.eq_ignore_ascii_case(&id.value))
                                .count();
                            if count > 1 {
                                self.issue(
                                    "fail",
                                    "ambiguous_order_alias",
                                    format!("ORDER BY '{}' has multiple output bindings", id.value),
                                );
                            }
                            if count > 0 {
                                continue;
                            }
                        }
                        if let Expr::Value(value) = &item.expr {
                            if let Value::Number(number, _) = &value.value {
                                if let Ok(ordinal) = number.parse::<usize>() {
                                    if ordinal == 0 || ordinal > outputs.len() {
                                        self.issue("fail", "order_ordinal", format!("ORDER BY position {ordinal} is outside the projection"));
                                    }
                                    continue;
                                }
                            }
                        }
                        let mut refs = References::default();
                        let _ = item.expr.visit(&mut refs);
                        self.references(refs, &scopes, &ctes)?;
                    }
                }
                _ => self.partial("This ORDER BY form requires additional binding"),
            }
        }
        Ok(outputs)
    }
    fn output_names(
        &mut self,
        outputs: Vec<Option<String>>,
        alias: &TableAlias,
    ) -> Option<Vec<String>> {
        if !alias.columns.is_empty() {
            if alias.columns.len() != outputs.len() {
                self.issue(
                    "fail",
                    "projection_arity",
                    format!(
                        "Alias '{}' column count differs from its query projection",
                        alias.name
                    ),
                );
            }
            let names: Vec<_> = alias
                .columns
                .iter()
                .map(|column| column.name.value.to_lowercase())
                .collect();
            let mut unique = std::collections::HashSet::new();
            if names.iter().any(|name| !unique.insert(name)) {
                self.issue(
                    "fail",
                    "duplicate_output",
                    format!("Alias '{}' declares duplicate output names", alias.name),
                );
            }
            return Some(names);
        }
        if outputs.iter().any(Option::is_none) {
            self.partial("Derived/CTE expressions need explicit column aliases");
            return None;
        }
        let names: Vec<_> = outputs.into_iter().flatten().collect();
        let mut unique = std::collections::HashSet::new();
        if names.iter().any(|name| !unique.insert(name)) {
            self.issue(
                "fail",
                "duplicate_output",
                format!(
                    "Derived relation '{}' has duplicate output column names",
                    alias.name
                ),
            );
        }
        Some(names)
    }
    fn relation(
        &mut self,
        factor: &TableFactor,
        ctes: &HashMap<String, Relation>,
    ) -> Result<Relation, String> {
        match factor {
            TableFactor::Derived {
                subquery,
                alias: Some(alias),
                lateral: false,
                ..
            } => {
                let outputs = self.query(subquery, ctes, &[])?;
                let columns = self.output_names(outputs, alias);
                Ok(Relation {
                    qualifier: alias.name.value.to_lowercase(),
                    columns,
                    known: true,
                    base: None,
                })
            }
            TableFactor::Table {
                name,
                alias,
                args,
                with_hints,
                version,
                ..
            } => {
                if args.is_some()
                    || version.is_some()
                    || !with_hints.is_empty()
                    || alias
                        .as_ref()
                        .is_some_and(|alias| !alias.columns.is_empty())
                {
                    self.partial(
                        "Table function/hint/version/column aliases need additional validation",
                    );
                }
                let parts: Vec<_> = name
                    .0
                    .iter()
                    .filter_map(|part| part.as_ident().map(|id| id.value.to_lowercase()))
                    .collect();
                let Some(base) = parts.last().cloned() else {
                    self.partial("Unknown table identity");
                    return Ok(Relation::default());
                };
                let qualifier = alias
                    .as_ref()
                    .map(|alias| alias.name.value.to_lowercase())
                    .unwrap_or_else(|| base.clone());
                if parts.len() == 1 {
                    if let Some(cte) = ctes.get(&base) {
                        let mut cte = cte.clone();
                        if !cte.known {
                            self.partial("Self/forward CTE references require additional binding");
                        }
                        cte.qualifier = qualifier;
                        return Ok(cte);
                    }
                }
                let node = self
                    .graph
                    .get_node(self.pid, &engram_core::ids::NodeId::table(&base).0)
                    .map_err(|error| error.to_string())?;
                let mut known = node.is_some();
                let mut columns = None;
                let mut declared_name = None;
                if let Some(ddl) = node
                    .as_ref()
                    .and_then(|node| node.metadata.as_ref())
                    .and_then(|meta| meta.get("ddl"))
                    .and_then(|value| value.as_str())
                {
                    if let Ok(statements) = Parser::parse_sql(&MsSqlDialect {}, ddl) {
                        if let [Statement::CreateTable(table)] = statements.as_slice() {
                            declared_name = Some(
                                table
                                    .name
                                    .0
                                    .iter()
                                    .filter_map(|part| {
                                        part.as_ident().map(|id| id.value.to_lowercase())
                                    })
                                    .collect::<Vec<_>>(),
                            );
                            columns = Some(
                                table
                                    .columns
                                    .iter()
                                    .map(|column| column.name.value.to_lowercase())
                                    .collect(),
                            );
                        }
                    }
                }
                if parts.len() > 1 && declared_name.as_ref() != Some(&parts) {
                    known = false;
                    columns = None;
                    self.partial(&format!("Qualified table '{}' is not established by its indexed CREATE TABLE declaration", name));
                }
                if !known {
                    self.result.complete = false;
                    if node.is_none() || declared_name.is_some() {
                        self.issue(
                            "warn",
                            "unknown_table",
                            format!("Table '{name}' is not confirmed in indexed schema"),
                        );
                    }
                }
                if !self.result.tables.contains(&name.to_string()) {
                    self.result.tables.push(name.to_string());
                }
                Ok(Relation {
                    qualifier,
                    base: Some(base),
                    columns,
                    known,
                })
            }
            _ => {
                self.partial("Table relation requires additional binding");
                Ok(Relation::default())
            }
        }
    }
    fn references(
        &mut self,
        refs: References,
        scopes: &[Vec<Relation>],
        ctes: &HashMap<String, Relation>,
    ) -> Result<(), String> {
        if refs.unsupported {
            self.partial(
                "Qualified wildcards/named function arguments require additional validation",
            );
        }
        for parts in refs.columns {
            self.column(&parts, scopes)?;
        }
        for (query, scalar) in refs.queries {
            let outputs = self.query(&query, ctes, scopes)?;
            if scalar && !outputs.is_empty() && outputs.len() != 1 {
                self.issue(
                    "fail",
                    "subquery_arity",
                    "Scalar/IN subquery must project exactly one column".into(),
                );
            }
        }
        Ok(())
    }
    fn column(&mut self, parts: &[String], scopes: &[Vec<Relation>]) -> Result<(), String> {
        let (qualifier, column) = match parts {
            [column] => (None, column),
            [qualifier, column] => (Some(qualifier), column),
            _ => {
                self.partial("Multipart column identity requires additional binding");
                return Ok(());
            }
        };
        let mut qualifier_found = false;
        for scope in scopes {
            let candidates: Vec<_> = scope
                .iter()
                .filter(|relation| {
                    qualifier.is_none_or(|qualifier| qualifier == &relation.qualifier)
                })
                .collect();
            qualifier_found |= !candidates.is_empty();
            let mut matches = 0;
            let mut unknown = false;
            for relation in &candidates {
                if !relation.known {
                    unknown = true;
                    continue;
                }
                if let Some(columns) = &relation.columns {
                    matches += columns.iter().filter(|name| *name == column).count();
                } else if let Some(base) = &relation.base {
                    matches += usize::from(
                        self.graph
                            .get_node(self.pid, &engram_core::ids::NodeId::column(base, column).0)
                            .map_err(|error| error.to_string())?
                            .is_some(),
                    );
                } else {
                    unknown = true;
                }
            }
            if matches > 1 {
                self.issue(
                    "fail",
                    "ambiguous_column",
                    format!("Column '{}' has multiple bindings", parts.join(".")),
                );
                return Ok(());
            }
            if unknown {
                self.partial(&format!(
                    "Column '{}' has an incomplete relation scope",
                    parts.join(".")
                ));
                return Ok(());
            }
            if matches == 1 {
                return Ok(());
            }
            if qualifier.is_some() && !candidates.is_empty() {
                break;
            }
        }
        if qualifier.is_some() && !qualifier_found {
            self.issue(
                "fail",
                "unknown_qualifier",
                format!("Unknown table qualifier in '{}'", parts.join(".")),
            );
        } else {
            self.issue(
                "warn",
                "unknown_column",
                format!(
                    "Column '{}' is not confirmed in its relation scopes",
                    parts.join(".")
                ),
            );
        }
        Ok(())
    }
}
