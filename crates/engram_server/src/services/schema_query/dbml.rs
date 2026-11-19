//! Literal LINQ-to-SQL mapping attributes, not deployed database definitions.
use super::{Citation, Column, Table, input_name};
use quick_xml::{events::Event, name::ResolveResult, reader::NsReader};
use std::collections::BTreeMap;

const NS: &[u8] = b"http://schemas.microsoft.com/linqtosql/dbml/2007";

fn boolean(attributes: &BTreeMap<String, String>, name: &str) -> anyhow::Result<Option<bool>> {
    match attributes.get(name).map(String::as_str) {
        None => Ok(None),
        Some("true" | "1") => Ok(Some(true)),
        Some("false" | "0") => Ok(Some(false)),
        _ => anyhow::bail!("Invalid DBML {name} boolean"),
    }
}

pub(super) fn tables(path: &str, source: &str) -> anyhow::Result<Vec<Table>> {
    // Match the existing DBML source reader: positions exclude a UTF-8 BOM,
    // while the citation hash covers the complete original file bytes.
    let xml = source.strip_prefix('\u{feff}').unwrap_or(source);
    let starts: Vec<_> = std::iter::once(0)
        .chain(xml.match_indices('\n').map(|(i, _)| i + 1))
        .collect();
    let line = |position: usize| starts.partition_point(|start| *start <= position) as u64;
    let hash = blake3::hash(source.as_bytes()).to_hex().to_string();
    let mut reader = NsReader::from_str(xml);
    reader.config_mut().expand_empty_elements = true;
    let mut depth = 0usize;
    let mut seen_root = false;
    let mut current: Option<Table> = None;
    let mut in_type = false;
    let mut tables = Vec::new();
    loop {
        let position = reader.buffer_position() as usize;
        let (namespace, event) = reader.read_resolved_event()?;
        let dbml = matches!(namespace, ResolveResult::Bound(ns) if ns.as_ref() == NS);
        match event {
            Event::Start(element) => {
                anyhow::ensure!(depth < 64, "DBML nesting limit exceeded");
                let mut attributes = BTreeMap::new();
                for attribute in element.attributes() {
                    let attribute = attribute?;
                    attributes.insert(
                        std::str::from_utf8(attribute.key.as_ref())?.to_owned(),
                        attribute.unescape_value()?.into_owned(),
                    );
                }
                if depth == 0 {
                    anyhow::ensure!(
                        !seen_root && dbml && element.local_name().as_ref() == b"Database",
                        "Expected one DBML Database root"
                    );
                    seen_root = true;
                } else if depth == 1 && dbml && element.local_name().as_ref() == b"Table" {
                    let name = attributes
                        .get("Name")
                        .ok_or_else(|| anyhow::anyhow!("DBML table Name missing"))?;
                    let key = input_name(name)?;
                    current = Some(Table {
                        name: key.display(), source_kind: Some("dbml_mapping"), key,
                        columns: vec![], primary_key: vec![], foreign_keys: vec![],
                        constraints: vec![], constraint_indexes: vec![],
                        citation: Citation {path:path.into(),start_line:line(position),
                            end_line:line(position),source_hash:hash.clone()},
                        gaps: vec!["Direct table/type Column mapping attributes only: DbType is retained literally; CanBeNull and IsPrimaryKey describe the model. Missing CanBeNull remains unknown. Derived-type columns, deployed types/nullability/keys, associations, defaults, computed/generated semantics, indexes and triggers are not established by this mapping.".into()],
                    });
                } else if depth == 2
                    && dbml
                    && element.local_name().as_ref() == b"Type"
                    && current.is_some()
                {
                    in_type = true;
                } else if depth == 3
                    && dbml
                    && element.local_name().as_ref() == b"Column"
                    && in_type
                {
                    let table = current
                        .as_mut()
                        .ok_or_else(|| anyhow::anyhow!("DBML column outside table"))?;
                    let name = attributes
                        .get("Name")
                        .filter(|name| !name.is_empty())
                        .ok_or_else(|| anyhow::anyhow!("DBML column Name missing"))?
                        .clone();
                    let primary_key = boolean(&attributes, "IsPrimaryKey")?.unwrap_or(false);
                    if primary_key {
                        table.primary_key.push(name.clone());
                    }
                    table.columns.push(Column {
                        name,
                        data_type: attributes
                            .get("DbType")
                            .filter(|value| !value.is_empty())
                            .cloned(),
                        nullable: boolean(&attributes, "CanBeNull")?,
                        primary_key,
                        default_value: None,
                        computed_expression: None,
                        identity: None,
                        declaration: xml[position..reader.buffer_position() as usize].to_owned(),
                    });
                }
                depth += 1;
            }
            Event::End(element) => {
                depth = depth
                    .checked_sub(1)
                    .ok_or_else(|| anyhow::anyhow!("Unbalanced DBML"))?;
                if depth == 2 && dbml && element.local_name().as_ref() == b"Type" {
                    in_type = false;
                }
                if depth == 1 && dbml && element.local_name().as_ref() == b"Table" {
                    if let Some(mut table) = current.take() {
                        table.citation.end_line =
                            line((reader.buffer_position() as usize).saturating_sub(1));
                        tables.push(table);
                    }
                }
            }
            Event::DocType(_) => anyhow::bail!("DBML DTD declarations are unsupported"),
            Event::Eof => break,
            _ => {}
        }
    }
    anyhow::ensure!(
        seen_root && depth == 0 && current.is_none(),
        "Incomplete DBML Database"
    );
    Ok(tables)
}
