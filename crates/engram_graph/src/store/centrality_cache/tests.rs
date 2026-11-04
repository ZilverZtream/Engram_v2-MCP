use super::*;

fn scores(value: f32) -> HashMap<String, f32> {
    HashMap::from([("unchanged-source-node".into(), value)])
}

fn rows(store: &GraphStore, project: &str) -> anyhow::Result<usize> {
    let tx = store.db.begin_read()?;
    let table = tx.open_table(CENTRALITY)?;
    let prefix = format!("{project}\0");
    let mut count = 0;
    for row in table.range(prefix.as_str()..)? {
        let (key, _) = row?;
        if !key.value().starts_with(&prefix) {
            break;
        }
        count += 1;
    }
    Ok(count)
}

#[test]
fn pagerank_retention_hundreds_of_edits_keep_one_durable_record_and_reopen_correctly()
-> anyhow::Result<()> {
    let fixture = tempfile::tempdir()?;
    let path = fixture.path().join("graph.redb");
    let store = GraphStore::open(&path)?;
    for generation in 1..=300 {
        store.set_cached_centrality("project", generation, &scores(generation as f32))?;
        assert_eq!(
            rows(&store, "project")?,
            1,
            "Derived cache storage must not accumulate every source generation"
        );
        assert_eq!(
            store.get_cached_centrality("project", generation)?.unwrap(),
            scores(generation as f32)
        );
        assert!(
            store
                .get_cached_centrality("project", generation - 1)?
                .is_none()
        );
    }
    drop(store);
    let reopened = GraphStore::open(&path)?;
    assert_eq!(
        reopened.get_cached_centrality("project", 300)?.unwrap(),
        scores(300.0)
    );
    assert!(reopened.get_cached_centrality("project", 299)?.is_none());
    assert_eq!(rows(&reopened, "project")?, 1);
    drop(reopened);
    fixture.close()?;
    Ok(())
}

#[test]
fn pagerank_retention_unknown_or_truncated_current_format_is_preserved() -> anyhow::Result<()> {
    let fixture = tempfile::tempdir()?;
    let store = GraphStore::open(&fixture.path().join("graph.redb"))?;
    for bytes in [
        b"EGPR\0".as_slice(),
        b"EGPR\0\0\0\x02future_version".as_slice(),
    ] {
        {
            let tx = store.db.begin_write()?;
            tx.open_table(CENTRALITY)?
                .insert("project\0current_v1", bytes)?;
            tx.commit()?;
        }
        assert!(store.get_cached_centrality("project", 1).is_err());
        assert!(
            store
                .set_cached_centrality("project", 1, &scores(1.0))
                .is_err()
        );
        let tx = store.db.begin_read()?;
        let table = tx.open_table(CENTRALITY)?;
        assert_eq!(table.get("project\0current_v1")?.unwrap().value(), bytes);
    }
    drop(store);
    fixture.close()?;
    Ok(())
}

#[test]
fn pagerank_retention_late_older_persistence_cannot_replace_newer_scores() -> anyhow::Result<()> {
    let fixture = tempfile::tempdir()?;
    let store = GraphStore::open(&fixture.path().join("graph.redb"))?;
    store.set_cached_centrality("project", u64::MAX, &scores(9.0))?;
    store.set_cached_centrality("project", u64::MAX - 1, &scores(1.0))?;
    assert_eq!(
        store.get_cached_centrality("project", u64::MAX)?.unwrap(),
        scores(9.0)
    );
    assert!(
        store
            .get_cached_centrality("project", u64::MAX - 1)?
            .is_none()
    );
    store.set_cached_centrality("project", u64::MAX, &scores(8.0))?;
    assert_eq!(
        store.get_cached_centrality("project", u64::MAX)?.unwrap(),
        scores(8.0)
    );
    assert_eq!(rows(&store, "project")?, 1);
    drop(store);
    fixture.close()?;
    Ok(())
}

#[test]
fn pagerank_retention_legacy_cleanup_is_bounded_project_scoped_and_does_not_decode_old_maps()
-> anyhow::Result<()> {
    let fixture = tempfile::tempdir()?;
    let store = GraphStore::open(&fixture.path().join("graph.redb"))?;
    {
        let tx = store.db.begin_write()?;
        {
            let mut table = tx.open_table(CENTRALITY)?;
            for generation in 0..100 {
                let key = format!("project\0{generation}");
                table.insert(
                    key.as_str(),
                    b"obsolete unreadable derived cache".as_slice(),
                )?;
            }
            table.insert("project-extra\0old", b"preserve other project".as_slice())?;
            table.insert(
                "project\0future_cache_kind",
                b"preserve unknown cache format".as_slice(),
            )?;
        }
        tx.commit()?;
    }
    assert!(store.get_cached_centrality("project", 99)?.is_none());
    for expected_rows in [70, 38, 6, 2] {
        store.set_cached_centrality("project", 200, &scores(2.0))?;
        assert_eq!(rows(&store, "project")?, expected_rows);
        assert_eq!(rows(&store, "project-extra")?, 1);
        assert_eq!(
            store.get_cached_centrality("project", 200)?.unwrap(),
            scores(2.0)
        );
    }
    drop(store);
    fixture.close()?;
    Ok(())
}
