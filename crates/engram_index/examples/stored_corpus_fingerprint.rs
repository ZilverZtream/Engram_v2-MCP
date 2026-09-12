//! Read-only fingerprints of stored documents, grouped by namespace and path.
//! Opens only an existing index; never creates a writer or prints document text.
use std::collections::BTreeMap;
use tantivy::schema::{Document, Value};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("hash-file") {
        anyhow::ensure!(args.len() == 3, "usage: hash-file PATH");
        println!("{}", blake3::hash(&std::fs::read(&args[2])?).to_hex());
        return Ok(());
    }
    anyhow::ensure!(args.len() == 3, "usage: INDEX_DIRECTORY PROJECT_ID");
    let index = tantivy::Index::open_in_dir(&args[1])?;
    let schema = index.schema();
    let pid = schema.get_field("project_id")?;
    let ns = schema.get_field("namespace")?;
    let path = schema.get_field("path")?;
    let reader = index.reader()?;
    let searcher = reader.searcher();
    let mut groups: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    for (ordinal, segment) in searcher.segment_readers().iter().enumerate() {
        for id in segment.doc_ids_alive() {
            let doc: tantivy::TantivyDocument =
                searcher.doc(tantivy::DocAddress::new(ordinal as u32, id))?;
            let text = |field| {
                doc.get_first(field)
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_owned()
            };
            if text(pid) != args[2] {
                continue;
            }
            // Canonical field ordering avoids segment merge/iteration order effects.
            let canonical: serde_json::Value = serde_json::from_str(&doc.to_json(&schema))?;
            let hash = blake3::hash(&serde_json::to_vec(&canonical)?)
                .to_hex()
                .to_string();
            groups.entry((text(ns), text(path))).or_default().push(hash);
        }
    }
    let rows: Vec<_> = groups
        .into_iter()
        .map(|((namespace, path), mut hashes)| {
            hashes.sort();
            serde_json::json!({"namespace":namespace,"path":path,"documents":hashes.len(),
            "blake3":blake3::hash(hashes.join("\n").as_bytes()).to_hex().to_string()})
        })
        .collect();
    println!("{}", serde_json::to_string(&rows)?);
    Ok(())
}
