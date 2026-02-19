#[cfg(test)]
mod audit_regressions {
    mod dup_content_does_not_overwrite;
    mod update_project_keeps_full_snapshot;
    mod vector_store_does_not_bloat_on_repeat;
}
