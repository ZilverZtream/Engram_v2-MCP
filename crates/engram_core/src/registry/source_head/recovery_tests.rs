use super::*;

fn project() -> ProjectRecord {
    ProjectRecord {
        project_id: "project".into(), project_name: "Recovery source".into(),
        project_type: "general".into(), directory: "source".into(),
        created_at_ms: 1, updated_at_ms: 1, reindex_required_since_ms: None,
    }
}

fn fixture() -> (tempfile::TempDir, Registry) {
    let directory = tempfile::tempdir().unwrap();
    let registry = Registry::open(&directory.path().join("registry.redb")).unwrap();
    registry.register_source_project(&project()).unwrap();
    let head = registry.capture_source_head("project").unwrap();
    assert!(registry.publish_source_head(&head, 1).unwrap());
    (directory, registry)
}

fn damage(registry: &Registry, key: &str, value: Option<&[u8]>) {
    let tx = registry.db.begin_write().unwrap();
    {
        let mut meta = tx.open_table(META).unwrap();
        if let Some(value) = value { meta.insert(key, value).unwrap(); }
        else { meta.remove(key).unwrap(); }
    }
    tx.commit().unwrap();
}

#[test]
fn damaged_and_missing_required_heads_recover_without_exposing_staging_or_losing_history() {
    for raw in [Some(b"broken-head".as_slice()), Some(b"\xff".as_slice()), None] {
        let (_directory, registry) = fixture();
        registry.set_meta("project", "last_git_oid", "history-kept").unwrap();
        registry.set_meta("project", "human-note", "keep this").unwrap();
        damage(&registry, &head_key("project"), raw);
        assert!(registry.capture_source_head("project").is_err());
        let recovery = registry.capture_source_recovery("project").unwrap();
        let reservation = registry.reserve_source_recovery(&recovery, 12).unwrap().unwrap();
        assert!(reservation.generation() > 12);
        registry.set_meta("unrelated", HEAD, "1").unwrap();
        assert!(registry.active_source_generation("project").is_err(), "reserved source is unpublished");
        assert!(registry.publish_source_recovery(&reservation).unwrap());
        assert_eq!(registry.active_source_generation("project").unwrap(), reservation.generation());
        assert!(registry.capture_source_head("project").is_ok());
        assert!(registry.capture_source_recovery("project").is_err());
        assert!(!registry.source_identity_needs_verification("project").unwrap());
        assert_eq!(registry.get_meta("project", "last_git_oid").unwrap().as_deref(), Some("history-kept"));
        assert_eq!(registry.get_meta("project", "human-note").unwrap().as_deref(), Some("keep this"));
        assert_eq!(registry.get_project("project").unwrap().unwrap().directory, "source");
    }
}

#[test]
fn abandoned_recovery_reservations_are_durable_and_never_reuse_a_legacy_high_generation() {
    let (directory, registry) = fixture();
    damage(&registry, &head_key("project"), Some(b"broken"));
    let recovery = registry.capture_source_recovery("project").unwrap();
    assert!(registry.reserve_source_recovery(&recovery, u64::MAX - 1).is_err(),
        "reservation must leave room to publish its final revision");
    let abandoned = registry.reserve_source_recovery(&recovery, 50_000).unwrap().unwrap();
    assert!(abandoned.generation() > 50_000);
    drop(registry);
    let registry = Registry::open(&directory.path().join("registry.redb")).unwrap();
    let retry = registry.capture_source_recovery("project").unwrap();
    let reserved = registry.reserve_source_recovery(&retry, 1).unwrap().unwrap();
    assert!(reserved.generation() > abandoned.generation());
    assert!(registry.publish_source_recovery(&reserved).unwrap());
    assert!(!registry.publish_source_recovery(&abandoned).unwrap());
    assert_eq!(registry.active_source_generation("project").unwrap(), reserved.generation());
    // A numeric value outside ordinary metadata's byte budget must also be a
    // floor, even when the caller's physical observation contains no source rows.
    damage(&registry, &head_key("project"), Some(b"00000000000000000000070000"));
    let recovery = registry.capture_source_recovery("project").unwrap();
    assert!(registry.reserve_source_recovery(&recovery, 0).unwrap().unwrap().generation() > 70_000);
}

#[test]
fn stale_recovery_cannot_overwrite_a_concurrent_head_identity_or_exact_admission_metadata() {
    for change in ["head", "raw-head", "identity", "required", "verified", "legacy", "recreated"] {
        let (_directory, registry) = fixture();
        registry.set_meta("project", HEAD, "broken").unwrap();
        let token = registry.capture_source_recovery("project").unwrap();
        let reservation = registry.reserve_source_recovery(&token, 100).unwrap().unwrap();
        match change {
            "head" => registry.set_meta("project", HEAD, "99").unwrap(),
            "raw-head" => damage(&registry, &head_key("project"), Some(b"other-damage")),
            "identity" => {
                let mut changed = project();
                changed.directory = "other-source".into();
                registry.put_project(&changed).unwrap();
                registry.put_project(&project()).unwrap();
            }
            "required" => damage(&registry, &flag_key("project", REQUIRED), None),
            "verified" => damage(&registry, &flag_key("project", IDENTITY_VERIFIED), Some(b"1")),
            "legacy" => damage(&registry, &flag_key("project", LEGACY_ATTESTED), Some(b"1")),
            "recreated" => {
                registry.delete_all_for_project("project").unwrap();
                registry.register_source_project(&project()).unwrap();
                registry.set_meta("project", HEAD, "broken").unwrap();
            }
            _ => unreachable!(),
        }
        let current = registry.get_meta("project", HEAD).unwrap();
        assert!(registry.reserve_source_recovery(&token, 100).unwrap().is_none(), "{change}");
        assert!(!registry.publish_source_recovery(&reservation).unwrap(), "{change}");
        assert_eq!(registry.get_meta("project", HEAD).unwrap(), current, "{change}");
    }
}

#[test]
fn recovery_refuses_missing_corrupt_or_regressed_authority_without_initializing_it() {
    let sequence_key = super::super::reindex::REVISION_SEQUENCE;
    for (field, value) in [
        (REVISION, None), (REVISION, Some(b"broken".as_slice())), (REVISION, Some(b"0".as_slice())),
        (IDENTITY_EPOCH, None), (IDENTITY_EPOCH, Some(b"broken".as_slice())),
        (IDENTITY_EPOCH, Some(b"999999".as_slice())),
        (sequence_key, None), (sequence_key, Some(b"broken".as_slice())), (sequence_key, Some(b"1".as_slice())),
        (REQUIRED, Some(b"0".as_slice())), (IDENTITY_VERIFIED, Some(b"x".as_slice())),
        (LEGACY_ATTESTED, Some(b"0".as_slice())),
    ] {
        let (_directory, registry) = fixture();
        damage(&registry, &head_key("project"), Some(b"broken"));
        let token = registry.capture_source_recovery("project").unwrap();
        let reservation = registry.reserve_source_recovery(&token, 100).unwrap().unwrap();
        let key = if field == sequence_key { field.to_owned() } else { flag_key("project", field) };
        damage(&registry, &key, value);
        assert!(registry.capture_source_recovery("project").is_err(), "{field}");
        assert!(registry.reserve_source_recovery(&token, 100).is_err(), "{field}");
        assert!(registry.publish_source_recovery(&reservation).is_err(), "{field}");
        let tx = registry.db.begin_read().unwrap();
        let meta = tx.open_table(META).unwrap();
        assert_eq!(meta.get(key.as_str()).unwrap().as_ref().map(|v| v.value()), value, "{field}");
    }
    let (_directory, registry) = fixture();
    damage(&registry, &head_key("project"), None);
    damage(&registry, &flag_key("project", REQUIRED), None);
    assert!(registry.capture_source_head("project").is_err(),
        "Independent managed authority must never fall back to legacy when the old presence flag is also lost");
    assert!(registry.source_generation_metadata("project").is_err());
    let token = registry.capture_source_recovery("project").unwrap();
    let recovered = registry.reserve_source_recovery(&token, 500).unwrap().unwrap();
    assert!(recovered.generation() > 500);
    assert!(registry.publish_source_recovery(&recovered).unwrap());
}
