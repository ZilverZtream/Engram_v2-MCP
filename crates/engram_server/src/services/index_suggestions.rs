//! Static query access patterns compared with source-declared indexes.
//! Suggestions are review inputs, never measured optimizer advice or migrations.
use super::schema_query::{self as schema, Catalog, Citation, Name};
use serde::Serialize;
use serde_json::{Value, json};
use sqlparser::{ast::*, dialect::MsSqlDialect, parser::Parser, tokenizer::Token};
use std::{
    collections::{BTreeMap, BTreeSet},
    ops::ControlFlow,
};

#[derive(Clone)]
pub struct QuerySource {
    pub sql: String,
    pub citation: Citation,
    /// False for indexed inline SQL whose available citation is its containing method.
    pub exact_lines: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
struct Key {
    column: String,
    ascending: Option<bool>,
}
#[derive(Clone)]
struct Index {
    name: String,
    keys: Vec<Key>,
    include: BTreeSet<String>,
    filter: Option<BTreeSet<String>>,
    filter_supported: bool,
    unique: bool,
    citation: Citation,
}
#[derive(Clone, PartialEq, Eq)]
struct Origin {
    table: usize,
    column: String,
    relation: String,
}
type Columns = BTreeMap<String, Option<Origin>>;
type Scope = BTreeMap<String, Columns>;
#[derive(Default)]
struct Access {
    equals: Vec<String>,
    ranges: Vec<String>,
    order: Vec<Key>,
    projected: BTreeSet<String>,
    filters: BTreeSet<String>,
    reasons: BTreeSet<String>,
}
#[derive(Clone)]
struct Candidate {
    table: usize,
    equals: BTreeSet<String>,
    keys: Vec<Key>,
    projected: BTreeSet<String>,
    filters: BTreeSet<String>,
    reasons: BTreeSet<String>,
    sites: BTreeMap<String, Value>,
    site_ids: BTreeSet<String>,
}

pub struct Suggestions<'a> {
    catalog: &'a Catalog,
    indexes: BTreeMap<usize, Vec<Index>>,
    incomplete_indexes: BTreeSet<usize>,
    candidates: BTreeMap<(usize, Vec<Key>), Candidate>,
    pub gaps: Vec<String>,
    pub truncated: bool,
    queries: usize,
    covered: Vec<Value>,
    covered_count: usize,
}

fn quote(s: &str) -> String {
    format!("[{}]", s.replace(']', "]]"))
}
fn parts(name: &ObjectName) -> Option<Name> {
    Some(Name(
        name.0
            .iter()
            .map(|p| p.as_ident().map(|i| i.value.clone()))
            .collect::<Option<Vec<_>>>()?,
    ))
}
fn column_parts(expr: &Expr) -> Option<Vec<String>> {
    match expr {
        Expr::Identifier(id) if !id.value.starts_with('@') => Some(vec![id.value.to_lowercase()]),
        Expr::CompoundIdentifier(ids) => {
            Some(ids.iter().map(|id| id.value.to_lowercase()).collect())
        }
        Expr::Nested(expr) => column_parts(expr),
        _ => None,
    }
}
fn scalar(expr: &Expr) -> bool {
    match expr {
        Expr::Value(_) => true,
        Expr::Identifier(id) => id.value.starts_with('@'),
        Expr::UnaryOp { expr, .. } => scalar(expr),
        _ => false,
    }
}
fn literal(expr: &Expr) -> bool {
    matches!(expr,Expr::Value(v) if !matches!(v.value,sqlparser::ast::Value::Placeholder(_)))
}
fn atom(column: &str, operator: &str, value: &Expr) -> String {
    format!("{} {operator} {value}", quote(column))
}
fn push_unique(values: &mut Vec<String>, value: String) {
    if !values.contains(&value) {
        values.push(value);
    }
}
fn index_atoms(expr: &Expr) -> Option<BTreeSet<String>> {
    match expr {
        Expr::Nested(e) => index_atoms(e),
        Expr::BinaryOp {
            left,
            op: BinaryOperator::And,
            right,
        } => {
            let mut a = index_atoms(left)?;
            a.extend(index_atoms(right)?);
            Some(a)
        }
        Expr::BinaryOp {
            left,
            op: BinaryOperator::Eq,
            right,
        } if literal(right) => {
            let names = column_parts(left)?;
            (names.len() == 1).then(|| BTreeSet::from([atom(&names[0], "=", right)]))
        }
        Expr::IsNull(e) | Expr::IsNotNull(e) => {
            let names = column_parts(e)?;
            (names.len() == 1).then(|| {
                BTreeSet::from([format!(
                    "{} IS {}NULL",
                    quote(&names[0]),
                    if matches!(expr, Expr::IsNotNull(_)) {
                        "NOT "
                    } else {
                        ""
                    }
                )])
            })
        }
        _ => None,
    }
}

impl<'a> Suggestions<'a> {
    pub fn new(catalog: &'a Catalog) -> Self {
        let mut result = Self {
            catalog,
            indexes: BTreeMap::new(),
            incomplete_indexes: BTreeSet::new(),
            candidates: BTreeMap::new(),
            gaps: vec![],
            truncated: false,
            queries: 0,
            covered: vec![],
            covered_count: 0,
        };
        for (id, table) in catalog.tables.iter().enumerate() {
            for constraint in &table.constraint_indexes {
                let declaration = constraint["declaration"].as_str().unwrap_or("");
                result.add_index(id, declaration, table.citation.clone());
            }
        }
        for object in &catalog.objects {
            if object.kind != "index" {
                continue;
            }
            let Some(target) = &object.target else {
                result.gap(format!("{}: index target unavailable", object.name));
                continue;
            };
            let ids: Vec<_> = catalog
                .tables
                .iter()
                .enumerate()
                .filter(|(_, t)| t.key.lookup(target))
                .map(|(i, _)| i)
                .collect();
            if ids.len() == 1 {
                result.add_index(ids[0], &object.declaration, object.citation.clone());
            } else {
                result.gap(format!(
                    "{}: index target is missing or ambiguous",
                    object.name
                ));
                for id in ids {
                    result.incomplete_indexes.insert(id);
                }
            }
        }
        result
    }
    fn gap(&mut self, message: String) {
        if self.gaps.len() < 64 {
            if !self.gaps.contains(&message) {
                self.gaps.push(message);
            }
        } else {
            self.truncated = true;
        }
    }
    fn add_index(&mut self, table: usize, sql: &str, citation: Citation) {
        let parsed = (|| -> anyhow::Result<Index> {
            let t = schema::tokens(sql)?;
            let marker = (0..t.len())
                .find(|&i| {
                    schema::kw(&t, i, "KEY")
                        || schema::kw(&t, i, "UNIQUE")
                        || schema::kw(&t, i, "INDEX")
                })
                .ok_or_else(|| anyhow::anyhow!("Index key missing"))?;
            // For standalone indexes, ON separates the index name from its table/key.
            let after = (0..t.len())
                .find(|&i| schema::kw(&t, i, "ON"))
                .map(|i| i + 1)
                .unwrap_or(marker + 1);
            let open = (after..t.len()).find(|&i| t[i].token == Token::LParen);
            let mut keys = vec![];
            if let Some(open) = open {
                let end =
                    schema::close(&t, open).ok_or_else(|| anyhow::anyhow!("Unclosed index key"))?;
                for segment in schema::segments(&t[open + 1..end]) {
                    let (name, n) = schema::name_at(segment, 0)
                        .ok_or_else(|| anyhow::anyhow!("Unsupported index expression"))?;
                    anyhow::ensure!(
                        name.0.len() == 1
                            && (n == segment.len()
                                || (n + 1 == segment.len()
                                    && (schema::kw(segment, n, "ASC")
                                        || schema::kw(segment, n, "DESC")))),
                        "Unsupported index key expression"
                    );
                    keys.push(Key {
                        column: name.0[0].to_lowercase(),
                        ascending: Some(!schema::kw(segment, n, "DESC")),
                    });
                }
            } else {
                let (name, end) = schema::name_at(&t, 0)
                    .ok_or_else(|| anyhow::anyhow!("Inline key column missing"))?;
                anyhow::ensure!(
                    end == 1 && name.0.len() == 1,
                    "Unsupported inline constraint"
                );
                keys.push(Key {
                    column: name.0[0].to_lowercase(),
                    ascending: Some(true),
                });
            }
            anyhow::ensure!(!keys.is_empty(), "Index key is empty");
            let mut include = BTreeSet::new();
            if let Some(i) = (0..t.len()).find(|&i| schema::kw(&t, i, "INCLUDE")) {
                let end = schema::close(&t, i + 1)
                    .ok_or_else(|| anyhow::anyhow!("Unsupported INCLUDE list"))?;
                for segment in schema::segments(&t[i + 2..end]) {
                    let (name, n) = schema::name_at(segment, 0)
                        .ok_or_else(|| anyhow::anyhow!("Unsupported INCLUDE column"))?;
                    anyhow::ensure!(
                        name.0.len() == 1 && n == segment.len(),
                        "Unsupported INCLUDE column"
                    );
                    include.insert(name.0[0].to_lowercase());
                }
            }
            let (filter, filter_supported) =
                if let Some(i) = (0..t.len()).find(|&i| schema::kw(&t, i, "WHERE")) {
                    let sql = schema::text(&t[i + 1..]);
                    let parsed = Parser::new(&MsSqlDialect {})
                        .try_with_sql(&sql)?
                        .parse_expr()?;
                    let atoms = index_atoms(&parsed);
                    let supported = atoms.is_some();
                    (atoms, supported)
                } else {
                    (None, true)
                };
            Ok(Index {
                name: sql.into(),
                keys,
                include,
                filter,
                filter_supported,
                unique: (0..t.len())
                    .any(|i| schema::kw(&t, i, "PRIMARY") || schema::kw(&t, i, "UNIQUE")),
                citation,
            })
        })();
        match parsed {
            Ok(index) => self.indexes.entry(table).or_default().push(index),
            Err(error) => {
                self.incomplete_indexes.insert(table);
                self.gap(format!(
                    "{}: index declaration unverified: {error}",
                    self.catalog.tables[table].name
                ));
            }
        }
    }
    pub fn add_source(&mut self, source: &QuerySource) {
        let tokens = match schema::tokens(source.sql.trim_start_matches('\u{feff}')) {
            Ok(t) => t,
            Err(e) => {
                self.gap(format!(
                    "{}:{}: SQL tokenization failed: {e}",
                    source.citation.path, source.citation.start_line
                ));
                return;
            }
        };
        let mut start = 0;
        for end in 0..=tokens.len() {
            let batch_end = end == tokens.len()
                || (schema::kw(&tokens, end, "GO")
                    && (end == 0 || tokens[end - 1].span.end.line < tokens[end].span.start.line)
                    && (end + 1 == tokens.len()
                        || tokens[end + 1].span.start.line > tokens[end].span.end.line));
            if !batch_end {
                continue;
            }
            let batch = &tokens[start..end];
            start = end + 1;
            if batch.is_empty() {
                continue;
            }
            let statements = match Parser::new(&MsSqlDialect {})
                .with_tokens_with_locations(body_parse_tokens(batch))
                .parse_statements()
            {
                Ok(s) => s,
                Err(error) => {
                    if (0..batch.len()).any(|i| {
                        schema::kw(batch, i, "SELECT")
                            || schema::kw(batch, i, "UPDATE")
                            || schema::kw(batch, i, "DELETE")
                    }) {
                        self.gap(format!(
                            "{}:{}: SQL batch unparsed; its access patterns withheld: {error}",
                            source.citation.path, batch[0].span.start.line
                        ));
                    }
                    continue;
                }
            };
            let mut roots = RootQueries::default();
            let _ = statements.visit(&mut roots);
            if roots.truncated {
                self.truncated = true;
                self.gap("Statement inventory exceeds2048 query/write roots".into());
            }
            for query in roots.queries {
                self.query(&query, &Scope::new(), &[], source, 0);
            }
            for statement in roots.writes {
                self.write_query(&statement, source);
            }
        }
    }
    fn write_query(&mut self, statement: &Statement, source: &QuerySource) {
        let sql = match statement {
            Statement::Update {
                table,
                selection,
                from: None,
                ..
            } if table.joins.is_empty() => format!(
                "SELECT 1 FROM {table}{}",
                selection
                    .as_ref()
                    .map(|e| format!(" WHERE {e}"))
                    .unwrap_or_default()
            ),
            Statement::Delete(delete) if delete.using.is_none() && delete.tables.is_empty() => {
                let (FromTable::WithFromKeyword(from) | FromTable::WithoutKeyword(from)) =
                    &delete.from;
                if from.len() != 1 {
                    self.gap("Multi-target DELETE access patterns withheld".into());
                    return;
                }
                format!(
                    "SELECT 1 FROM {}{}",
                    from[0],
                    delete
                        .selection
                        .as_ref()
                        .map(|e| format!(" WHERE {e}"))
                        .unwrap_or_default()
                )
            }
            _ => {
                self.gap(format!(
                    "{}: joined/unsupported write access patterns withheld",
                    source.citation.path
                ));
                return;
            }
        };
        if let Ok(mut statements) = Parser::parse_sql(&MsSqlDialect {}, &sql) {
            if let Statement::Query(query) = statements.remove(0) {
                let mut own = source.clone();
                own.citation = self.citation(
                    source,
                    statement.span().start.line,
                    statement.span().end.line,
                );
                own.exact_lines = false;
                self.query(&query, &Scope::new(), &[], &own, 0);
            }
        }
    }
    fn citation(&self, source: &QuerySource, start: u64, end: u64) -> Citation {
        let mut c = source.citation.clone();
        if source.exact_lines && start > 0 {
            c.start_line += start - 1;
            c.end_line = source.citation.start_line + end.saturating_sub(1);
        }
        c
    }
    fn query(
        &mut self,
        query: &Query,
        virtuals: &Scope,
        outer: &[Scope],
        source: &QuerySource,
        depth: usize,
    ) -> Columns {
        if depth >= 12 || self.queries >= 2048 {
            self.truncated = true;
            self.gap("Query scope expansion reached depth12 or2048 queries".into());
            return Columns::new();
        }
        self.queries += 1;
        let mut virtuals = virtuals.clone();
        if let Some(with) = &query.with {
            if with.recursive {
                self.gap(format!(
                    "{}: recursive CTE access patterns withheld",
                    source.citation.path
                ));
                return Columns::new();
            }
            for cte in &with.cte_tables {
                let mut columns = self.query(&cte.query, &virtuals, outer, source, depth + 1);
                if !cte.alias.columns.is_empty() {
                    self.gap(
                        "Explicit CTE column lists need ordinal lineage; outer CTE uses withheld"
                            .into(),
                    );
                    columns.clear();
                }
                virtuals.insert(cte.alias.name.value.to_lowercase(), columns);
            }
        }
        self.set_expr(
            &query.body,
            query.order_by.as_ref(),
            &virtuals,
            outer,
            source,
            depth,
        )
    }
    fn set_expr(
        &mut self,
        body: &SetExpr,
        order: Option<&OrderBy>,
        virtuals: &Scope,
        outer: &[Scope],
        source: &QuerySource,
        depth: usize,
    ) -> Columns {
        if depth >= 12 {
            self.truncated = true;
            self.gap("Set-query depth12 reached".into());
            return Columns::new();
        }
        match body {
            SetExpr::Select(select) => self.select(select, order, virtuals, outer, source, depth),
            SetExpr::Query(query) => self.query(query, virtuals, outer, source, depth + 1),
            SetExpr::SetOperation { left, right, .. } => {
                let a = self.set_expr(left, None, virtuals, outer, source, depth + 1);
                let b = self.set_expr(right, None, virtuals, outer, source, depth + 1);
                if order.is_some() {
                    self.gap("Set-operation ORDER BY lineage is unassessed".into());
                }
                a.into_iter()
                    .map(|(k, v)| {
                        let value = if b.get(&k) == Some(&v) { v } else { None };
                        (k, value)
                    })
                    .collect()
            }
            _ => Columns::new(),
        }
    }
    fn relation(
        &mut self,
        factor: &TableFactor,
        virtuals: &Scope,
        outer: &[Scope],
        source: &QuerySource,
        depth: usize,
    ) -> Option<(String, Columns)> {
        match factor {
            TableFactor::Table {
                name,
                alias,
                args: None,
                version: None,
                ..
            } => {
                let name = parts(name)?;
                let qualifier = alias
                    .as_ref()
                    .map(|a| a.name.value.to_lowercase())
                    .unwrap_or_else(|| name.0.join(".").to_lowercase());
                if alias.as_ref().is_some_and(|a| !a.columns.is_empty()) {
                    self.gap("Table column aliases require ordinal lineage".into());
                    return None;
                }
                if name.0.len() == 1 {
                    if let Some(columns) = virtuals.get(&name.0[0].to_lowercase()) {
                        return Some((
                            qualifier.clone(),
                            columns
                                .iter()
                                .map(|(c, o)| {
                                    (
                                        c.clone(),
                                        o.clone().map(|mut o| {
                                            o.relation = qualifier.clone();
                                            o
                                        }),
                                    )
                                })
                                .collect(),
                        ));
                    }
                }
                let tables: Vec<_> = self
                    .catalog
                    .tables
                    .iter()
                    .enumerate()
                    .filter(|(_, t)| t.key.lookup(&name))
                    .collect();
                if tables.len() != 1 {
                    self.gap(format!(
                        "{}: table {} is missing or ambiguous",
                        source.citation.path,
                        name.display()
                    ));
                    return None;
                }
                let (id, table) = tables[0];
                if !table.gaps.is_empty() {
                    self.gap(format!(
                        "{}: table has unparsed columns; patterns withheld",
                        table.name
                    ));
                    return None;
                }
                let columns = table
                    .columns
                    .iter()
                    .map(|c| {
                        (
                            c.name.to_lowercase(),
                            Some(Origin {
                                table: id,
                                column: c.name.to_lowercase(),
                                relation: qualifier.clone(),
                            }),
                        )
                    })
                    .collect();
                Some((qualifier, columns))
            }
            TableFactor::Derived {
                subquery,
                alias: Some(alias),
                lateral: false,
            } => {
                let qualifier = alias.name.value.to_lowercase();
                let mut columns = self.query(subquery, virtuals, outer, source, depth + 1);
                if !alias.columns.is_empty() {
                    self.gap("Derived column alias list unassessed".into());
                    columns.clear();
                }
                for origin in columns.values_mut().flatten() {
                    origin.relation = qualifier.clone();
                }
                Some((qualifier, columns))
            }
            _ => {
                self.gap(format!(
                    "{}: unsupported FROM relation; query patterns withheld",
                    source.citation.path
                ));
                None
            }
        }
    }
    fn resolve(expr: &Expr, scope: &Scope, outer: &[Scope]) -> Option<Origin> {
        let names = column_parts(expr)?;
        let column = names.last()?;
        for frame in std::iter::once(scope).chain(outer.iter().rev()) {
            if names.len() > 1 {
                let qualifier = names[..names.len() - 1].join(".");
                let relations: Vec<_> = frame
                    .iter()
                    .filter(|(name, _)| {
                        **name == qualifier
                            || (!qualifier.contains('.')
                                && name.rsplit('.').next() == Some(qualifier.as_str()))
                    })
                    .collect();
                if !relations.is_empty() {
                    return (relations.len() == 1)
                        .then(|| relations[0].1.get(column).cloned().flatten())
                        .flatten();
                }
            } else {
                let matches: Vec<_> = frame
                    .values()
                    .filter_map(|columns| columns.get(column))
                    .collect();
                if !matches.is_empty() {
                    return (matches.len() == 1).then(|| matches[0].clone()).flatten();
                }
            }
        }
        None
    }
    fn select(
        &mut self,
        select: &Select,
        order: Option<&OrderBy>,
        virtuals: &Scope,
        outer: &[Scope],
        source: &QuerySource,
        depth: usize,
    ) -> Columns {
        let mut scope = Scope::new();
        let mut valid = true;
        let mut join_predicates = vec![];
        for from in &select.from {
            for relation in
                std::iter::once(&from.relation).chain(from.joins.iter().map(|j| &j.relation))
            {
                match self.relation(relation, virtuals, outer, source, depth) {
                    Some((name, columns)) => {
                        if scope.insert(name, columns).is_some() {
                            valid = false;
                        }
                    }
                    None => valid = false,
                }
            }
            for join in &from.joins {
                let constraint = match &join.join_operator {
                    JoinOperator::Join(c)
                    | JoinOperator::Inner(c)
                    | JoinOperator::Left(c)
                    | JoinOperator::LeftOuter(c)
                    | JoinOperator::Right(c)
                    | JoinOperator::RightOuter(c)
                    | JoinOperator::FullOuter(c)
                    | JoinOperator::CrossJoin(c) => Some(c),
                    _ => None,
                };
                match constraint {
                    Some(JoinConstraint::On(expr)) => join_predicates.push(expr),
                    Some(JoinConstraint::None) => {}
                    _ => {
                        valid = false;
                        self.gap("Unsupported JOIN constraint; scope patterns withheld".into());
                    }
                }
            }
        }
        let mut access = BTreeMap::<(usize, String), Access>::new();
        let mut outputs = Columns::new();
        for item in &select.projection {
            let (expr, alias) = match item {
                SelectItem::UnnamedExpr(e) => (Some(e), None),
                SelectItem::ExprWithAlias { expr, alias } => {
                    (Some(expr), Some(alias.value.to_lowercase()))
                }
                _ => (None, None),
            };
            if let Some(expr) = expr {
                let origin = Self::resolve(expr, &scope, outer);
                if let Some(o) = &origin {
                    access
                        .entry((o.table, o.relation.clone()))
                        .or_default()
                        .projected
                        .insert(o.column.clone());
                }
                if let Some(name) =
                    alias.or_else(|| column_parts(expr).and_then(|p| p.last().cloned()))
                {
                    if outputs.insert(name.clone(), origin).is_some() {
                        outputs.insert(name, None);
                    }
                }
            } else {
                // Wildcard lineage is safe only when column names are unique in this scope.
                for columns in scope.values() {
                    for (name, origin) in columns {
                        if let Some(o) = origin {
                            access
                                .entry((o.table, o.relation.clone()))
                                .or_default()
                                .projected
                                .insert(o.column.clone());
                        }
                        if outputs.insert(name.clone(), origin.clone()).is_some() {
                            outputs.insert(name.clone(), None);
                        }
                    }
                }
            }
        }
        if let Some(expr) = &select.selection {
            valid &= self.predicate(expr, "WHERE", &scope, outer, &mut access);
        }
        for expr in &join_predicates {
            valid &= self.predicate(expr, "JOIN", &scope, outer, &mut access);
        }
        if let Some(order) = order {
            if let OrderByKind::Expressions(items) = &order.kind {
                for item in items {
                    let origin = match &item.expr {
                        Expr::Identifier(id) if outputs.contains_key(&id.value.to_lowercase()) => {
                            outputs[&id.value.to_lowercase()].clone()
                        }
                        _ => Self::resolve(&item.expr, &scope, outer),
                    };
                    if let Some(o) = origin {
                        let a = access.entry((o.table, o.relation)).or_default();
                        a.order.push(Key {
                            column: o.column,
                            ascending: Some(item.options.asc.unwrap_or(true)),
                        });
                        a.reasons.insert("ORDER BY".into());
                    } else {
                        valid = false;
                        self.gap(
                            "ORDER BY expression/ordinal or ambiguous alias unassessed".into(),
                        );
                    }
                }
            } else {
                valid = false;
                self.gap("ORDER BY ALL is unassessed".into());
            }
        }
        // Nested query scopes keep their own aliases and may resolve outer correlations.
        let mut nested = RootQueries::default();
        let _ = select.projection.visit(&mut nested);
        let _ = select.selection.visit(&mut nested);
        let _ = select.having.visit(&mut nested);
        for expr in join_predicates {
            let _ = expr.visit(&mut nested);
        }
        let mut parent = outer.to_vec();
        parent.push(scope.clone());
        for query in nested.queries {
            self.query(&query, virtuals, &parent, source, depth + 1);
        }
        if valid {
            let span = select.span();
            let end = order
                .and_then(|o| {
                    if let OrderByKind::Expressions(items) = &o.kind {
                        items.iter().map(|i| i.expr.span().end.line).max()
                    } else {
                        None
                    }
                })
                .unwrap_or(span.end.line)
                .max(span.end.line);
            let citation = self.citation(source, span.start.line, end);
            let query_text = format!(
                "{select}{}",
                order.map(|o| format!(" {o}")).unwrap_or_default()
            );
            for ((table, _), a) in access {
                let mut keys = vec![];
                for column in a.equals.iter().chain(a.ranges.iter()) {
                    if !keys.iter().any(|k: &Key| k.column == *column) {
                        keys.push(Key {
                            column: column.clone(),
                            ascending: None,
                        });
                    }
                }
                for key in &a.order {
                    if let Some(existing) = keys.iter_mut().find(|k| k.column == key.column) {
                        existing.ascending = key.ascending;
                    } else {
                        keys.push(key.clone());
                    }
                }
                if keys.is_empty() {
                    continue;
                }
                let signature = (table, keys.clone());
                if self.candidates.len() >= 512 && !self.candidates.contains_key(&signature) {
                    self.truncated = true;
                    continue;
                }
                let site_id = format!(
                    "{}:{}:{}:{}",
                    citation.path,
                    citation.start_line,
                    citation.end_line,
                    blake3::hash(query_text.as_bytes())
                );
                let entry = self
                    .candidates
                    .entry(signature)
                    .or_insert_with(|| Candidate {
                        table,
                        equals: a.equals.iter().cloned().collect(),
                        keys,
                        projected: BTreeSet::new(),
                        filters: a.filters.clone(),
                        reasons: BTreeSet::new(),
                        sites: BTreeMap::new(),
                        site_ids: BTreeSet::new(),
                    });
                entry.equals.retain(|column| a.equals.contains(column));
                entry.projected.extend(a.projected);
                entry.filters = entry.filters.intersection(&a.filters).cloned().collect();
                entry.reasons.extend(a.reasons);
                entry.site_ids.insert(site_id.clone());
                if entry.sites.len() < 16 {
                    entry.sites.insert(site_id,json!({"citation":citation,"span_kind":if source.exact_lines{"query_source_span"}else{"containing_indexed_method_or_statement"},"query":query_text}));
                }
            }
        } else {
            self.gap(format!(
                "{}:{}: unresolved/unsupported access expressions; this query's index DDL withheld",
                source.citation.path,
                select.span().start.line
            ));
        }
        outputs
    }
    fn predicate(
        &mut self,
        expr: &Expr,
        reason: &str,
        scope: &Scope,
        outer: &[Scope],
        access: &mut BTreeMap<(usize, String), Access>,
    ) -> bool {
        match expr {
            Expr::Nested(e) => self.predicate(e, reason, scope, outer, access),
            Expr::BinaryOp {
                left,
                op: BinaryOperator::And,
                right,
            } => {
                let a = self.predicate(left, reason, scope, outer, access);
                let b = self.predicate(right, reason, scope, outer, access);
                a && b
            }
            Expr::BinaryOp { left, op, right }
                if matches!(
                    op,
                    BinaryOperator::Eq
                        | BinaryOperator::Lt
                        | BinaryOperator::LtEq
                        | BinaryOperator::Gt
                        | BinaryOperator::GtEq
                ) =>
            {
                let l = Self::resolve(left, scope, outer);
                let r = Self::resolve(right, scope, outer);
                if (l.is_none() && !scalar(left)) || (r.is_none() && !scalar(right)) {
                    return false;
                }
                for origin in [l.as_ref(), r.as_ref()].into_iter().flatten() {
                    let a = access
                        .entry((origin.table, origin.relation.clone()))
                        .or_default();
                    push_unique(
                        if *op == BinaryOperator::Eq {
                            &mut a.equals
                        } else {
                            &mut a.ranges
                        },
                        origin.column.clone(),
                    );
                    a.reasons.insert(reason.into());
                }
                if reason == "WHERE" && *op == BinaryOperator::Eq {
                    if let Some(o) = l.filter(|_| literal(right)) {
                        access
                            .entry((o.table, o.relation))
                            .or_default()
                            .filters
                            .insert(atom(&o.column, "=", right));
                    }
                    if let Some(o) = r.filter(|_| literal(left)) {
                        access
                            .entry((o.table, o.relation))
                            .or_default()
                            .filters
                            .insert(atom(&o.column, "=", left));
                    }
                }
                true
            }
            Expr::InList {
                expr,
                list,
                negated: false,
            } if list.iter().all(scalar) => self.record_range(expr, reason, scope, outer, access),
            Expr::Between {
                expr,
                low,
                high,
                negated: false,
            } if scalar(low) && scalar(high) => {
                self.record_range(expr, reason, scope, outer, access)
            }
            Expr::IsNull(e) | Expr::IsNotNull(e) => {
                let Some(o) = Self::resolve(e, scope, outer) else {
                    return false;
                };
                let a = access.entry((o.table, o.relation)).or_default();
                push_unique(&mut a.ranges, o.column.clone());
                a.reasons.insert(reason.into());
                if reason == "WHERE" {
                    a.filters.insert(format!(
                        "{} IS {}NULL",
                        quote(&o.column),
                        if matches!(expr, Expr::IsNotNull(_)) {
                            "NOT "
                        } else {
                            ""
                        }
                    ));
                }
                true
            }
            Expr::Exists { .. } => true, // Inner query is analyzed in its own scope.
            _ => false,
        }
    }
    fn record_range(
        &self,
        expr: &Expr,
        reason: &str,
        scope: &Scope,
        outer: &[Scope],
        access: &mut BTreeMap<(usize, String), Access>,
    ) -> bool {
        let Some(o) = Self::resolve(expr, scope, outer) else {
            return false;
        };
        let a = access.entry((o.table, o.relation)).or_default();
        push_unique(&mut a.ranges, o.column);
        a.reasons.insert(reason.into());
        true
    }
    pub fn finish(mut self) -> Value {
        let candidates = std::mem::take(&mut self.candidates);
        let mut suggestions = vec![];
        for (_, candidate) in candidates {
            let table = &self.catalog.tables[candidate.table];
            let existing = self
                .indexes
                .get(&candidate.table)
                .into_iter()
                .flatten()
                .find(|i| index_covers(i, &candidate));
            if let Some(index) = existing {
                self.covered_count += 1;
                if self.covered.len() < 20 {
                    let available: BTreeSet<_> = index
                        .keys
                        .iter()
                        .map(|k| k.column.clone())
                        .chain(index.include.iter().cloned())
                        .collect();
                    self.covered.push(json!({"table":table.name,"query_keys":candidate.keys,"index":index.name,"index_citation":index.citation,"projected_columns_not_in_index":candidate.projected.difference(&available).collect::<Vec<_>>(),"interpretation":"Source-declared eligible leading-key access path; no optimizer-use or clustered-key inclusion claim."}));
                }
                continue;
            }
            let mut warnings=vec!["Validate against deployed indexes, actual plans, row counts and write workload before applying. Static site frequency is not runtime frequency. Generated keys use SQL Server 2016+ nonclustered size limits; engine version, SET options and deployment compatibility are unverified.".to_string()];
            let mut bytes = 0usize;
            let mut supported =
                !self.incomplete_indexes.contains(&candidate.table) && candidate.keys.len() <= 16;
            for key in &candidate.keys {
                let column = table
                    .columns
                    .iter()
                    .find(|c| c.name.eq_ignore_ascii_case(&key.column));
                match column.and_then(|c| {
                    key_width(c.data_type.as_deref()?, c.computed_expression.is_some())
                }) {
                    Some(size) => bytes += size,
                    None => supported = false,
                }
                if column
                    .and_then(|c| c.data_type.as_deref())
                    .is_some_and(|s| s.eq_ignore_ascii_case("BIT"))
                {
                    warnings.push(format!("{} has BIT cardinality (plus NULL when allowed); it may have low selectivity. Distribution is unknown.",quote(&key.column)));
                }
            }
            supported &= bytes <= 1700;
            let includes: Vec<_> = candidate
                .projected
                .iter()
                .filter(|c| !candidate.keys.iter().any(|k| &k.column == *c))
                .filter(|name| {
                    table.columns.iter().any(|c| {
                        c.name.eq_ignore_ascii_case(name)
                            && c.computed_expression.is_none()
                            && c.data_type.as_deref().is_some_and(include_type)
                    })
                })
                .take(8)
                .cloned()
                .collect();
            if !supported {
                warnings.push("DDL withheld: key type/width, key count or source index inventory is unsupported/incomplete.".into());
            }
            let column_name = |name: &str| {
                table
                    .columns
                    .iter()
                    .find(|c| c.name.eq_ignore_ascii_case(name))
                    .map(|c| c.name.as_str())
                    .unwrap_or(name)
                    .to_string()
            };
            let keys = candidate
                .keys
                .iter()
                .map(|k| {
                    format!(
                        "{} {}",
                        quote(&column_name(&k.column)),
                        if k.ascending == Some(false) {
                            "DESC"
                        } else {
                            "ASC"
                        }
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            let digest = blake3::hash(format!("{}:{keys}", table.name).as_bytes()).to_hex();
            let name = format!("IX_Engram_{}", &digest[..12]);
            let include = if includes.is_empty() {
                String::new()
            } else {
                format!(
                    " INCLUDE ({})",
                    includes
                        .iter()
                        .map(|c| quote(&column_name(c)))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            };
            let create = format!(
                "CREATE NONCLUSTERED INDEX {} ON {} ({keys}){include}",
                quote(&name),
                table.name
            );
            let filtered = if supported && !candidate.filters.is_empty() {
                Some(format!(
                    "CREATE NONCLUSTERED INDEX {} ON {} ({keys}){include} WHERE {};",
                    quote(&(name.clone() + "_filtered")),
                    table.name,
                    candidate
                        .filters
                        .iter()
                        .map(|filter| {
                            for c in &table.columns {
                                let prefix = quote(&c.name.to_lowercase());
                                if let Some(rest) = filter.strip_prefix(&prefix) {
                                    return format!("{}{rest}", quote(&c.name));
                                }
                            }
                            filter.clone()
                        })
                        .collect::<Vec<_>>()
                        .join(" AND ")
                ))
            } else {
                None
            };
            suggestions.push(json!({"status":if supported{"review_candidate"}else{"ddl_withheld"},"table":table.name,"table_citation":table.citation,"key_columns":candidate.keys.iter().map(|k|json!({"column":column_name(&k.column),"ascending":k.ascending})).collect::<Vec<_>>(),"include_columns":includes.iter().map(|c|column_name(c)).collect::<Vec<_>>(),"create_index":supported.then(||format!("{create};")),"filtered_alternative":filtered,"filtered_guidance":"Alternative only for the cited constant/null predicates; parameterized or different predicates may not use it. Measure selectivity first.","index_type":"nonclustered","source_declared_indexes_compared":self.indexes.get(&candidate.table).map_or(0,Vec::len),"index_comparison_complete":!self.incomplete_indexes.contains(&candidate.table),"covering_guidance":"INCLUDE lists contain at most8 directly projected eligible columns; expressions, implicit clustered keys and full covering behavior are not proved.","static_query_sites":candidate.site_ids.len(),"query_sites_truncated":candidate.site_ids.len()>candidate.sites.len(),"reasons":candidate.reasons,"query_sites":candidate.sites.into_values().collect::<Vec<_>>(),"estimated_runtime_frequency":null,"estimated_selectivity":null,"estimated_speedup":null,"warnings":warnings}));
        }
        suggestions.sort_by(|a, b| {
            b["static_query_sites"]
                .as_u64()
                .cmp(&a["static_query_sites"].as_u64())
                .then_with(|| a["table"].as_str().cmp(&b["table"].as_str()))
                .then_with(|| {
                    a["key_columns"]
                        .to_string()
                        .cmp(&b["key_columns"].to_string())
                })
        });
        json!({"status":"static_index_candidates","suggestions":suggestions,"covered_patterns":self.covered,"covered_pattern_count":self.covered_count,"covered_patterns_truncated":self.covered_count>self.covered.len(),"coverage":{"queries_analyzed":self.queries,"gaps":self.gaps,"truncated":self.truncated,"schema_gaps":self.catalog.gaps,"semantics":"Source declarations and static query patterns, not deployed schema or executed migrations. LINQ/provider translation, dynamic SQL, runtime frequency, data distribution, optimizer choices and actual benefit remain unknown. Unqualified table names use a unique source match; default schema/collation are unverified. Procedure parameters and view header options are omitted when parsing query bodies; their definitions are not validated."}})
    }
}

/// Analyze the existing AST of the body without interpreting T-SQL procedure
/// parameter declarations or view options. Token spans stay in original source.
fn body_parse_tokens(
    tokens: &[sqlparser::tokenizer::TokenWithSpan],
) -> Vec<sqlparser::tokenizer::TokenWithSpan> {
    if !schema::kw(tokens, 0, "CREATE") && !schema::kw(tokens, 0, "ALTER") {
        return tokens.to_vec();
    }
    let mut kind = 1;
    if schema::kw(tokens, kind, "OR") && schema::kw(tokens, kind + 1, "ALTER") {
        kind += 2;
    }
    if !["PROC", "PROCEDURE", "VIEW"]
        .iter()
        .any(|k| schema::kw(tokens, kind, k))
    {
        return tokens.to_vec();
    }
    let Some((_, after_name)) = schema::name_at(tokens, kind + 1) else {
        return tokens.to_vec();
    };
    let mut depth = 0;
    for i in after_name..tokens.len() {
        if depth == 0
            && schema::kw(tokens, i, "AS")
            && [
                "BEGIN", "SELECT", "WITH", "UPDATE", "DELETE", "INSERT", "SET", "DECLARE", "IF",
                "RETURN",
            ]
            .iter()
            .any(|k| schema::kw(tokens, i + 1, k))
        {
            let mut normalized = tokens[..after_name].to_vec();
            if schema::kw(tokens, 0, "ALTER") {
                if let Token::Word(word) = &mut normalized[0].token {
                    word.value = "CREATE".into();
                    word.keyword = sqlparser::keywords::Keyword::CREATE;
                }
            }
            normalized.extend_from_slice(&tokens[i..]);
            return normalized;
        }
        match tokens[i].token {
            Token::LParen => depth += 1,
            Token::RParen => depth -= 1,
            _ => {}
        }
    }
    tokens.to_vec()
}

fn index_covers(index: &Index, candidate: &Candidate) -> bool {
    if !index.filter_supported
        || index
            .filter
            .as_ref()
            .is_some_and(|f| !f.is_subset(&candidate.filters))
    {
        return false;
    }
    // An eligible filter fixing a column to one value makes that column
    // redundant in the access key. IS NOT NULL does not fix one value.
    let needed: Vec<_> = candidate
        .keys
        .iter()
        .filter(|key| {
            !index.filter.as_ref().is_some_and(|atoms| {
                atoms.iter().any(|a| {
                    a.starts_with(&format!("{} = ", quote(&key.column)))
                        || a == &format!("{} IS NULL", quote(&key.column))
                })
            })
        })
        .collect();
    if index.unique
        && index
            .keys
            .iter()
            .all(|key| candidate.equals.contains(&key.column))
    {
        return true;
    }
    if index.keys.len() < needed.len() {
        return false;
    }
    let equal_count = needed
        .iter()
        .take_while(|key| candidate.equals.contains(&key.column))
        .count();
    let expected: BTreeSet<_> = needed
        .iter()
        .take(equal_count)
        .map(|key| &key.column)
        .collect();
    let actual: BTreeSet<_> = index
        .keys
        .iter()
        .take(equal_count)
        .map(|key| &key.column)
        .collect();
    if expected != actual {
        return false;
    }
    let mut directions = vec![];
    for (have, need) in index
        .keys
        .iter()
        .skip(equal_count)
        .zip(needed.iter().skip(equal_count))
    {
        if have.column != need.column {
            return false;
        }
        if let Some(asc) = need.ascending {
            directions.push(have.ascending == Some(asc));
        }
    }
    directions.iter().all(|x| *x) || directions.iter().all(|x| !*x)
}

fn include_type(s: &str) -> bool {
    let s = s.to_ascii_lowercase();
    ![
        "text",
        "ntext",
        "image",
        "xml",
        "geography",
        "geometry",
        "sql_variant",
    ]
    .iter()
    .any(|t| s == *t)
}
fn key_width(s: &str, computed: bool) -> Option<usize> {
    if computed {
        return None;
    }
    let s = s.to_ascii_lowercase().replace(' ', "");
    let size = match s.as_str() {
        "bit" | "tinyint" => 1,
        "smallint" => 2,
        "int" | "real" | "smallmoney" | "smalldatetime" => 4,
        "bigint" | "float" | "money" | "datetime" | "rowversion" | "timestamp" => 8,
        "date" => 3,
        "uniqueidentifier" => 16,
        _ => 0,
    };
    if size > 0 {
        return Some(size);
    }
    if ["decimal", "numeric"].iter().any(|t| s.starts_with(t)) {
        return Some(17);
    }
    if ["datetime2", "datetimeoffset", "time"]
        .iter()
        .any(|t| s.starts_with(t))
    {
        return Some(10);
    }
    for (prefix, multiplier) in [
        ("varchar(", 1),
        ("char(", 1),
        ("nvarchar(", 2),
        ("nchar(", 2),
        ("varbinary(", 1),
        ("binary(", 1),
    ] {
        if let Some(n) = s
            .strip_prefix(prefix)
            .and_then(|s| s.strip_suffix(')'))
            .and_then(|s| s.parse::<usize>().ok())
        {
            return n.checked_mul(multiplier);
        }
    }
    None
}
#[derive(Default)]
struct RootQueries {
    depth: usize,
    queries: Vec<Query>,
    writes: Vec<Statement>,
    truncated: bool,
    in_write: usize,
}
impl Visitor for RootQueries {
    type Break = ();
    fn pre_visit_query(&mut self, q: &Query) -> ControlFlow<()> {
        if self.depth == 0 && self.in_write == 0 {
            if self.queries.len() + self.writes.len() >= 2048 {
                self.truncated = true;
                return ControlFlow::Break(());
            }
            self.queries.push(q.clone());
        }
        self.depth += 1;
        ControlFlow::Continue(())
    }
    fn post_visit_query(&mut self, _: &Query) -> ControlFlow<()> {
        self.depth -= 1;
        ControlFlow::Continue(())
    }
    fn pre_visit_statement(&mut self, s: &Statement) -> ControlFlow<()> {
        if matches!(s, Statement::Update { .. } | Statement::Delete(_)) {
            if self.depth == 0 && self.in_write == 0 {
                if self.queries.len() + self.writes.len() >= 2048 {
                    self.truncated = true;
                    return ControlFlow::Break(());
                }
                self.writes.push(s.clone());
            }
            self.in_write += 1;
        }
        ControlFlow::Continue(())
    }
    fn post_visit_statement(&mut self, s: &Statement) -> ControlFlow<()> {
        if matches!(s, Statement::Update { .. } | Statement::Delete(_)) {
            self.in_write -= 1;
        }
        ControlFlow::Continue(())
    }
}
