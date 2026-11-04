use super::*;
use anyhow::{Result, ensure};
use std::sync::atomic::{AtomicUsize, Ordering};

struct AdmissionProbe;

impl Drop for AdmissionProbe {
    fn drop(&mut self) {
        NATIVE_OPEN_PROBE.with(|probe| *probe.borrow_mut() = None);
        PATH_BYTE_LIMIT.with(|limit| limit.set(MAX_RETAINED_PATH_BYTES));
    }
}

fn varint(mut value: u64, output: &mut Vec<u8>) {
    while value >= 128 {
        output.push((value as u8 & 0x7f) | 0x80);
        value >>= 7;
    }
    output.push(value as u8);
}

// Preserve a real SDK-written manifest's prefix, schema and footer format;
// inject additional physical protobuf content and update only its envelope.
fn append_message(original: &[u8], extra: &[u8]) -> Vec<u8> {
    let footer = original.len() - 16;
    let start = i64::from_le_bytes(original[footer..footer + 8].try_into().unwrap()) as usize;
    let mut bytes = original[..footer].to_vec();
    bytes.extend_from_slice(extra);
    let length = u32::try_from(bytes.len() - start - 4).unwrap();
    bytes[start..start + 4].copy_from_slice(&length.to_le_bytes());
    bytes.extend_from_slice(&original[footer..]);
    bytes
}

fn inline_transaction(original: &[u8], operation: u64, body: &[u8]) -> Vec<u8> {
    let footer = original.len() - 16;
    let start = i64::from_le_bytes(original[footer..footer + 8].try_into().unwrap()) as usize;
    let mut transaction = vec![0x08, 0x01, 0x12, 36];
    transaction.extend_from_slice(b"00000000-0000-4000-8000-000000000001");
    varint((operation << 3) | 2, &mut transaction);
    varint(body.len() as u64, &mut transaction);
    transaction.extend_from_slice(body);
    // Put a valid length-prefixed transaction before the manifest and point
    // its field21 at that section. The official SDK may decode this during
    // latest-open; do not merely corrupt an unused manifest suffix.
    let mut bytes = original[..start].to_vec();
    let transaction_offset = bytes.len();
    bytes.extend_from_slice(&(transaction.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&transaction);
    let manifest_start = bytes.len();
    let mut manifest = original[start + 4..footer].to_vec();
    varint((21 << 3) | 0, &mut manifest);
    varint(transaction_offset as u64, &mut manifest);
    bytes.extend_from_slice(&(manifest.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&manifest);
    bytes.extend_from_slice(&(manifest_start as i64).to_le_bytes());
    bytes.extend_from_slice(&original[footer + 8..]);
    bytes
}

fn reason(outcome: NativeHistoryPruneOutcome) -> Result<String> {
    match outcome {
        NativeHistoryPruneOutcome::Deferred { reason } => Ok(reason),
        other => anyhow::bail!("Untrusted manifest must defer native open/prune, got {other:?}"),
    }
}

#[test]
fn excessive_or_malformed_owned_manifest_is_refused_before_native_open_without_changes()
-> Result<()> {
    let owned = tempfile::tempdir()?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let result: Result<()> = runtime.block_on(async {
        let index = super::super::tests::engine(owned.path()).await?;
        let cancel = CancellationToken::new();
        index
            .index_docs(
                "project",
                &[super::super::tests::document(
                    1,
                    "owned.rs",
                    "needle owned source",
                )],
                &cancel,
            )
            .await?;
        let table = super::super::tests::table(&index).await?;
        let root = owned
            .path()
            .join("vectors")
            .join(format!("{}.lance", table.name()));
        let dataset = table.dataset().unwrap().get().await?;
        let path =
            ManifestNamingScheme::V2.manifest_path(dataset.refs.base(), dataset.manifest().version);
        let latest = root.join("_versions").join(path.filename().unwrap());
        drop(dataset);
        drop(table);
        let original = std::fs::read(&latest)?;
        let opens = Arc::new(AtomicUsize::new(0));
        NATIVE_OPEN_PROBE.with(|probe| *probe.borrow_mut() = Some(opens.clone()));
        let _probe = AdmissionProbe;

        let fragments = [0x12, 0x00].repeat(structural::MAX_FRAGMENTS + 1);
        let mut fields = Vec::new();
        for id in 1000..1000 + structural::MAX_SCHEMA_FIELDS {
            let mut field = vec![0x18];
            varint(id as u64, &mut field);
            field.push(0x20);
            varint(u64::MAX, &mut field);
            field.extend_from_slice(b"\x2a\x06string");
            fields.push(0x0a);
            varint(field.len() as u64, &mut fields);
            fields.extend_from_slice(&field);
        }
        let mut missing_parent_field = vec![0x18];
        varint(1000, &mut missing_parent_field);
        missing_parent_field.push(0x20);
        varint(i32::MAX as u64, &mut missing_parent_field);
        missing_parent_field.extend_from_slice(b"\x2a\x06string");
        let mut missing_parent = vec![0x0a];
        varint(missing_parent_field.len() as u64, &mut missing_parent);
        missing_parent.extend_from_slice(&missing_parent_field);
        let mut missing_transaction_parent = vec![0x12];
        varint(
            missing_parent_field.len() as u64,
            &mut missing_transaction_parent,
        );
        missing_transaction_parent.extend_from_slice(&missing_parent_field);
        let mut nested_type_field = vec![0x18];
        varint(1000, &mut nested_type_field);
        nested_type_field.push(0x20);
        varint(u64::MAX, &mut nested_type_field);
        let nested_type = format!("{}float{}", "fixed_size_list:".repeat(32), ":1".repeat(32));
        nested_type_field.push(0x2a);
        varint(nested_type.len() as u64, &mut nested_type_field);
        nested_type_field.extend_from_slice(nested_type.as_bytes());
        let mut nested_type_message = vec![0x0a];
        varint(nested_type_field.len() as u64, &mut nested_type_message);
        nested_type_message.extend_from_slice(&nested_type_field);
        let deleted_over_physical = [0x12, 6, 0x20, 1, 0x1a, 2, 0x20, 2];
        let mut huge_rows = vec![0x20];
        varint(usize::MAX as u64 / 2 + 1, &mut huge_rows);
        let mut overflowing_fragments = Vec::new();
        for _ in 0..2 {
            overflowing_fragments.push(0x12);
            varint(huge_rows.len() as u64, &mut overflowing_fragments);
            overflowing_fragments.extend_from_slice(&huge_rows);
        }
        // Append uses field1 for its independent fragment vector, RewriteGroup
        // uses field1 for old fragments. Neither may wrap its own row total.
        let mut transaction_fragments = Vec::new();
        for _ in 0..2 {
            transaction_fragments.push(0x0a);
            varint(huge_rows.len() as u64, &mut transaction_fragments);
            transaction_fragments.extend_from_slice(&huge_rows);
        }
        let mut rewrite = vec![0x1a];
        varint(transaction_fragments.len() as u64, &mut rewrite);
        rewrite.extend_from_slice(&transaction_fragments);
        let files = [0x12, 0x00].repeat(structural::MAX_DATA_FILES + 1);
        let mut file_fragment = vec![0x12];
        varint(files.len() as u64, &mut file_fragment);
        file_fragment.extend_from_slice(&files);
        let mut invalid_footer = original.clone();
        let offset = invalid_footer.len() - 16;
        invalid_footer[offset..offset + 8].copy_from_slice(&(-1i64).to_le_bytes());
        let cases = [
            ("fragment", append_message(&original, &fragments)),
            ("schema field", append_message(&original, &fields)),
            ("data file", append_message(&original, &file_fragment)),
            (
                "fragment",
                inline_transaction(
                    &original,
                    108,
                    &[0x1a, 0].repeat(structural::MAX_FRAGMENTS + 1),
                ),
            ),
            ("parent", append_message(&original, &missing_parent)),
            (
                "deleted rows exceed",
                append_message(&original, &deleted_over_physical),
            ),
            (
                "row sum overflow",
                append_message(&original, &overflowing_fragments),
            ),
            (
                "row sum overflow",
                inline_transaction(&original, 100, &transaction_fragments),
            ),
            (
                "row sum overflow",
                inline_transaction(&original, 104, &rewrite),
            ),
            (
                "logical type",
                append_message(&original, &nested_type_message),
            ),
            (
                "parent",
                inline_transaction(&original, 102, &missing_transaction_parent),
            ),
            ("wire type", append_message(&original, &[0x0b])),
            (
                "Unsupported native manifest field",
                append_message(&original, &[0xf8, 0x01, 0x00]),
            ),
            ("Truncated", append_message(&original, &[0x12, 0xff])),
            ("position", invalid_footer),
        ];
        for (expected, bytes) in cases {
            std::fs::write(&latest, bytes)?;
            let before = super::super::tests::inventory(&root)?;
            // Directly poll the real worker entry point on this test's thread:
            // the probe observes precisely its pre-open admission boundary.
            let refusal = reason(index.prune_vector_history_inner("project", &cancel).await?)?;
            ensure!(
                refusal.contains(expected),
                "Expected {expected} refusal, got {refusal}"
            );
            ensure!(
                opens.load(Ordering::SeqCst) == 0,
                "Untrusted protobuf reached native table open"
            );
            ensure!(
                super::super::tests::inventory(&root)? == before,
                "Refusal must preserve the complete owned native inventory"
            );
            std::fs::write(&latest, &original)?;
        }
        // Restore normal bytes and continue through the same real maintenance
        // path: refusing malformed inputs must not poison later valid use.
        ensure!(matches!(
            index.prune_vector_history_inner("project", &cancel).await?,
            NativeHistoryPruneOutcome::Pruned { .. }
        ));
        ensure!(opens.load(Ordering::SeqCst) == 1);
        ensure!(index.count_vectors("project").await? == 1);
        drop(index);
        Ok(())
    });
    drop(runtime);
    let cleanup = owned.close();
    result?;
    cleanup?;
    Ok(())
}

#[test]
fn retained_inventory_path_budget_refuses_before_native_open_preserving_unknown_files() -> Result<()>
{
    let owned = tempfile::tempdir()?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let result: Result<()> = runtime.block_on(async {
        let index = super::super::tests::engine(owned.path()).await?;
        let cancel = CancellationToken::new();
        index
            .index_docs(
                "project",
                &[super::super::tests::document(
                    1,
                    "owned.rs",
                    "needle owned source",
                )],
                &cancel,
            )
            .await?;
        let table = super::super::tests::table(&index).await?;
        let root = owned
            .path()
            .join("vectors")
            .join(format!("{}.lance", table.name()));
        drop(table);
        // Real filesystem input exceeds a deliberately small fixture budget,
        // while remaining far below the production entry-count limit.
        for i in 0..64 {
            std::fs::write(
                root.join("data")
                    .join(format!("unknown-{i:03}-{}.lance", "x".repeat(80))),
                b"human bytes",
            )?;
        }
        let before = super::super::tests::inventory(&root)?;
        let opens = Arc::new(AtomicUsize::new(0));
        NATIVE_OPEN_PROBE.with(|probe| *probe.borrow_mut() = Some(opens.clone()));
        PATH_BYTE_LIMIT.with(|limit| limit.set(4096));
        let _probe = AdmissionProbe;
        let refusal = reason(index.prune_vector_history_inner("project", &cancel).await?)?;
        ensure!(refusal.contains("path byte budget"), "Got {refusal}");
        ensure!(
            opens.load(Ordering::SeqCst) == 0 && super::super::tests::inventory(&root)? == before
        );
        drop(index);
        Ok(())
    });
    drop(runtime);
    let cleanup = owned.close();
    result?;
    cleanup?;
    Ok(())
}
