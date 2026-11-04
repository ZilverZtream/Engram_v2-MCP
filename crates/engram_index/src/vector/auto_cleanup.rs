//! Native version removal belongs to explicit reader-exclusive maintenance.
//! The locked Lance default otherwise schedules it during ordinary commits.
use std::collections::HashMap;

const INTERVAL: &str = "lance.auto_cleanup.interval";
const AGE: &str = "lance.auto_cleanup.older_than";

/// Historical ordinary Lance creates used exactly these two default values.
/// Unknown/manual policy is preserved for explicit inspection, never adopted.
pub(crate) fn validate_history_config(config: &HashMap<String, String>, allow_legacy_defaults: bool) -> anyhow::Result<()> {
    anyhow::ensure!(config.len() <= 3 && config.iter().all(|(key, value)| match key.as_str() {
        INTERVAL => allow_legacy_defaults && value == "20",
        AGE => allow_legacy_defaults && value == "14days",
        // Lance >=2.2 persists this even for schemas with no blob columns.
        "lance.blob.version" => value == "2",
        _ => false,
    }), "Unsupported native table configuration; data and history preserved");
    anyhow::ensure!(config.contains_key(INTERVAL) == config.contains_key(AGE),
        "Incomplete native cleanup configuration; data and history preserved");
    Ok(())
}

pub(super) fn write_options() -> lancedb::table::WriteOptions {
    let mut options = lancedb::table::WriteOptions::default();
    let mut params = options.lance_write_params.take().unwrap_or_default();
    params.auto_cleanup = None;
    params.skip_auto_cleanup = true;
    options.lance_write_params = Some(params);
    options
}

/// Called only inside a native worker retaining the mutation/commit authority,
/// after the managed engine has verified the table's ownership receipt/schema.
/// Raw existing unowned tables cannot acquire managed policy through this path.
pub(crate) async fn prepare_managed_write(table: &lancedb::Table) -> anyhow::Result<()> {
    let wrapper = table.dataset().ok_or_else(|| anyhow::anyhow!("Native policy inspection unavailable"))?;
    let needs_migration = {
        let dataset = wrapper.get().await?;
        let manifest = dataset.manifest();
        validate_history_config(&manifest.config, true)?;
        if !manifest.config.contains_key(INTERVAL) { return Ok(()); }
        let metadata = &manifest.schema.metadata;
        // This confirms the physical handle is managed; it never installs or
        // repairs ownership. The engine's route/receipt validation is separate.
        let owner = metadata.get("engram.ownership.table").map(String::as_str);
        let token = metadata.get("engram.ownership.token").map(String::as_str);
        anyhow::ensure!(metadata.get("engram.ownership.version").map(String::as_str) == Some("1")
            && owner == Some(table.name())
            && token.is_some_and(|token| token.len() == 64 && token.bytes().all(|byte| byte.is_ascii_hexdigit()))
            && metadata.get("engram.ownership.project").is_some_and(|project| !project.is_empty()
                && project.len() <= 384 && project.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')))
            && metadata.get("engram.embedding.identity").is_some_and(|identity| !identity.is_empty() && identity.len() <= 1024),
            "Native cleanup migration requires verified managed ownership; unowned table preserved");
        true
    };
    if needs_migration {
        // Crucial: remove the interval, never set it to zero (zero means run
        // on EVERY commit). The locked hook observes the new manifest, finds
        // no interval and returns before any file deletion. Tags, schema,
        // data and all preexisting version files remain untouched.
        table.as_native().ok_or_else(|| anyhow::anyhow!("Native policy migration unavailable"))?
            .delete_config_keys(&[INTERVAL, AGE]).await?;
        let dataset = wrapper.get().await?;
        validate_history_config(&dataset.manifest().config, false)?;
    }
    Ok(())
}
