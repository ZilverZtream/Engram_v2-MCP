//! Allocation-free admission for the locked Lance 2.0 manifest codec.
//!
//! This recognizes only the current managed flat-table wire layout from
//! lance-table/protos/table.proto and lance-file/protos/file.proto. It does not
//! interpret schema/data semantics: the official codec still does that next.
//! Counts bound protobuf Vec/map expansion before either table-open or decode.
use super::{Budget, MAX_MANIFEST_BYTES};
use std::io::Read;
use std::path::Path;

pub(super) const MAX_FRAGMENTS: usize = 4096;
pub(super) const MAX_DATA_FILES: usize = 4096;
pub(super) const MAX_SCHEMA_FIELDS: usize = 128;
const MAX_MAPS: usize = 256;
const MAX_SCALARS: usize = 65_536;
const MAX_WIRE_FIELDS: usize = 65_536;
const MAX_REWRITE_GROUPS: usize = 4096;

#[derive(Default)]
struct Counts {
    fragments: usize,
    files: usize,
    fields: usize,
    maps: usize,
    scalars: usize,
    wire: usize,
    groups: usize,
}

struct SchemaIds {
    ids: [i32; MAX_SCHEMA_FIELDS],
    len: usize,
}

impl Default for SchemaIds {
    fn default() -> Self {
        Self {
            ids: [0; MAX_SCHEMA_FIELDS],
            len: 0,
        }
    }
}

fn count(value: &mut usize, max: usize, name: &str) -> anyhow::Result<()> {
    *value += 1;
    anyhow::ensure!(
        *value <= max,
        "Native manifest {name} structural budget exceeded; history preserved"
    );
    Ok(())
}

struct Wire<'a> {
    bytes: &'a [u8],
}

impl<'a> Wire<'a> {
    fn varint(&mut self) -> anyhow::Result<u64> {
        let mut value = 0;
        for shift in (0..70).step_by(7) {
            let (&byte, rest) = self
                .bytes
                .split_first()
                .ok_or_else(|| anyhow::anyhow!("Truncated native manifest varint"))?;
            self.bytes = rest;
            anyhow::ensure!(
                shift != 63 || byte <= 1,
                "Overflowing native manifest varint"
            );
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        anyhow::bail!("Overflowing native manifest varint")
    }

    fn bytes(&mut self, wire: u64) -> anyhow::Result<&'a [u8]> {
        anyhow::ensure!(wire == 2, "Unsupported native manifest wire type");
        let length = usize::try_from(self.varint()?)?;
        anyhow::ensure!(
            length <= self.bytes.len(),
            "Truncated native manifest message"
        );
        let (value, rest) = self.bytes.split_at(length);
        self.bytes = rest;
        Ok(value)
    }

    fn scalar(&mut self, wire: u64) -> anyhow::Result<u64> {
        anyhow::ensure!(wire == 0, "Unsupported native manifest scalar wire type");
        self.varint()
    }

    fn next(
        &mut self,
        counts: &mut Counts,
        budget: &mut Budget<'_>,
    ) -> anyhow::Result<Option<(u64, u64)>> {
        if self.bytes.is_empty() {
            return Ok(None);
        }
        budget.charge()?;
        count(&mut counts.wire, MAX_WIRE_FIELDS, "wire field")?;
        let key = self.varint()?;
        anyhow::ensure!(
            key >> 3 != 0 && key >> 3 <= 0x1fff_ffff,
            "Invalid native manifest field number"
        );
        Ok(Some((key >> 3, key & 7)))
    }
}

fn map(bytes: &[u8], counts: &mut Counts, budget: &mut Budget<'_>) -> anyhow::Result<()> {
    count(&mut counts.maps, MAX_MAPS, "metadata entry")?;
    let mut input = Wire { bytes };
    while let Some((field, wire)) = input.next(counts, budget)? {
        anyhow::ensure!(matches!(field, 1 | 2), "Unknown native manifest map field");
        input.bytes(wire)?;
    }
    Ok(())
}

fn scalars(
    bytes: &[u8],
    allowed: &[u64],
    counts: &mut Counts,
    budget: &mut Budget<'_>,
) -> anyhow::Result<()> {
    let mut input = Wire { bytes };
    while let Some((field, wire)) = input.next(counts, budget)? {
        anyhow::ensure!(
            allowed.contains(&field),
            "Unknown native manifest scalar field"
        );
        input.scalar(wire)?;
    }
    Ok(())
}

fn strings(
    bytes: &[u8],
    allowed: &[u64],
    counts: &mut Counts,
    budget: &mut Budget<'_>,
) -> anyhow::Result<()> {
    let mut input = Wire { bytes };
    while let Some((field, wire)) = input.next(counts, budget)? {
        anyhow::ensure!(
            allowed.contains(&field),
            "Unknown native manifest string field"
        );
        input.bytes(wire)?;
    }
    Ok(())
}

fn flat_logical_type(bytes: &[u8]) -> anyhow::Result<()> {
    // vector_schema currently emits only these flat types. In particular do
    // not hand arbitrary logical-type strings to Lance's recursive parser.
    let supported = if matches!(bytes, b"string" | b"uint64") {
        true
    } else if bytes.len() <= 64 {
        bytes
            .strip_prefix(b"fixed_size_list:float:")
            .is_some_and(|size| {
                !size.is_empty()
                    && size.len() <= 10
                    && size.iter().all(u8::is_ascii_digit)
                    && std::str::from_utf8(size)
                        .ok()
                        .and_then(|size| size.parse::<i32>().ok())
                        .is_some_and(|size| size > 0)
            })
    } else {
        false
    };
    anyhow::ensure!(
        supported,
        "Unsupported native manifest flat logical type; history preserved"
    );
    Ok(())
}

fn schema_field(
    bytes: &[u8],
    counts: &mut Counts,
    schema: &mut SchemaIds,
    budget: &mut Budget<'_>,
) -> anyhow::Result<()> {
    // A file may contain both its manifest schema and an Overwrite transaction
    // schema. Their IDs/parents are independent; their total allocation is not.
    count(
        &mut counts.fields,
        MAX_SCHEMA_FIELDS * 2,
        "total schema field",
    )?;
    anyhow::ensure!(
        schema.len < MAX_SCHEMA_FIELDS,
        "Native manifest schema field structural budget exceeded; history preserved"
    );
    let mut id = 0;
    let mut parent = 0;
    let mut logical_type = false;
    let mut input = Wire { bytes };
    while let Some((field, wire)) = input.next(counts, budget)? {
        match field {
            1 | 6 | 7 | 12 | 13 => {
                input.scalar(wire)?;
            }
            3 => id = input.scalar(wire)?,
            4 => parent = input.scalar(wire)?,
            2 | 9 => {
                input.bytes(wire)?;
            }
            5 => {
                flat_logical_type(input.bytes(wire)?)?;
                logical_type = true;
            }
            10 => map(input.bytes(wire)?, counts, budget)?,
            // Dictionary payloads are not part of Engram's managed schema.
            _ => anyhow::bail!("Unsupported native manifest schema field; history preserved"),
        }
    }
    anyhow::ensure!(
        id <= i32::MAX as u64 && !schema.ids[..schema.len].contains(&(id as i32)),
        "Invalid or duplicate native manifest schema field id; history preserved"
    );
    // int32 -1 is normally sign-extended to ten bytes. Accept the equivalent
    // five-byte representation too; both decode to the official sentinel.
    let top_level = parent == u64::MAX || parent == u32::MAX as u64;
    anyhow::ensure!(
        top_level
            || (parent <= i32::MAX as u64 && schema.ids[..schema.len].contains(&(parent as i32))),
        "Native manifest schema field parent is missing or not previously declared; history preserved"
    );
    anyhow::ensure!(
        logical_type,
        "Missing native manifest flat logical type; history preserved"
    );
    schema.ids[schema.len] = id as i32;
    schema.len += 1;
    Ok(())
}

fn data_file(bytes: &[u8], counts: &mut Counts, budget: &mut Budget<'_>) -> anyhow::Result<()> {
    count(&mut counts.files, MAX_DATA_FILES, "data file")?;
    let mut input = Wire { bytes };
    while let Some((field, wire)) = input.next(counts, budget)? {
        match field {
            1 => {
                input.bytes(wire)?;
            }
            2 | 3 => {
                if wire == 2 {
                    let mut packed = Wire {
                        bytes: input.bytes(wire)?,
                    };
                    while !packed.bytes.is_empty() {
                        budget.charge()?;
                        count(&mut counts.scalars, MAX_SCALARS, "column scalar")?;
                        packed.varint()?;
                    }
                } else {
                    count(&mut counts.scalars, MAX_SCALARS, "column scalar")?;
                    input.scalar(wire)?;
                }
            }
            4..=6 => {
                input.scalar(wire)?;
            }
            _ => anyhow::bail!("Unsupported native manifest data file field; history preserved"),
        }
    }
    Ok(())
}

fn add_rows(total: &mut usize, rows: usize) -> anyhow::Result<()> {
    *total = total.checked_add(rows).ok_or_else(|| {
        anyhow::anyhow!("Native manifest fragment row sum overflow; history preserved")
    })?;
    Ok(())
}

fn deletion_rows(
    bytes: &[u8],
    rows: &mut u64,
    counts: &mut Counts,
    budget: &mut Budget<'_>,
) -> anyhow::Result<()> {
    let mut input = Wire { bytes };
    while let Some((field, wire)) = input.next(counts, budget)? {
        match field {
            1..=3 => {
                input.scalar(wire)?;
            }
            4 => *rows = input.scalar(wire)?,
            _ => anyhow::bail!("Unsupported native manifest deletion field; history preserved"),
        }
    }
    Ok(())
}

fn fragment(bytes: &[u8], counts: &mut Counts, budget: &mut Budget<'_>) -> anyhow::Result<usize> {
    count(&mut counts.fragments, MAX_FRAGMENTS, "fragment")?;
    let mut physical = 0;
    let mut deleted = 0;
    let mut has_deletion = false;
    let mut input = Wire { bytes };
    while let Some((field, wire)) = input.next(counts, budget)? {
        match field {
            1 => {
                input.scalar(wire)?;
            }
            4 => physical = input.scalar(wire)?,
            2 => data_file(input.bytes(wire)?, counts, budget)?,
            3 => {
                has_deletion = true;
                deletion_rows(input.bytes(wire)?, &mut deleted, counts, budget)?;
            }
            5 | 7 | 9 => {
                input.bytes(wire)?;
            }
            // External row/version metadata requires separate reference proof.
            _ => anyhow::bail!("Unsupported native manifest fragment field; history preserved"),
        }
    }
    // The locked codec maps physical=0/deleted=0 to unknown (None), not
    // known zero. Match Fragment::num_rows without its unchecked subtraction.
    if physical == 0 {
        return Ok(0);
    }
    let physical = usize::try_from(physical)?;
    let deleted = usize::try_from(deleted)?;
    anyhow::ensure!(
        deleted <= physical,
        "Native manifest deleted rows exceed physical rows; history preserved"
    );
    let rows = if has_deletion && deleted == 0 {
        0
    } else {
        physical - deleted
    };
    Ok(rows)
}

fn repeated_scalar(
    input: &mut Wire<'_>,
    wire: u64,
    counts: &mut Counts,
    budget: &mut Budget<'_>,
) -> anyhow::Result<()> {
    if wire == 2 {
        let mut packed = Wire {
            bytes: input.bytes(wire)?,
        };
        while !packed.bytes.is_empty() {
            budget.charge()?;
            count(&mut counts.scalars, MAX_SCALARS, "transaction scalar")?;
            packed.varint()?;
        }
    } else {
        count(&mut counts.scalars, MAX_SCALARS, "transaction scalar")?;
        input.scalar(wire)?;
    }
    Ok(())
}

fn update_map(bytes: &[u8], counts: &mut Counts, budget: &mut Budget<'_>) -> anyhow::Result<()> {
    let mut input = Wire { bytes };
    while let Some((field, wire)) = input.next(counts, budget)? {
        match field {
            1 => map(input.bytes(wire)?, counts, budget)?,
            2 => {
                input.scalar(wire)?;
            }
            _ => anyhow::bail!("Unsupported native transaction map field; history preserved"),
        }
    }
    Ok(())
}

fn update_config(bytes: &[u8], counts: &mut Counts, budget: &mut Budget<'_>) -> anyhow::Result<()> {
    let mut input = Wire { bytes };
    while let Some((field, wire)) = input.next(counts, budget)? {
        match field {
            1 | 3 => map(input.bytes(wire)?, counts, budget)?,
            2 => {
                count(&mut counts.maps, MAX_MAPS, "transaction map deletion")?;
                input.bytes(wire)?;
            }
            6..=8 => update_map(input.bytes(wire)?, counts, budget)?,
            // Managed code never changes per-field metadata or external bases.
            _ => anyhow::bail!("Unsupported native transaction config field; history preserved"),
        }
    }
    Ok(())
}

fn rewrite_group(bytes: &[u8], counts: &mut Counts, budget: &mut Budget<'_>) -> anyhow::Result<()> {
    count(&mut counts.groups, MAX_REWRITE_GROUPS, "rewrite group")?;
    let mut input = Wire { bytes };
    let mut rows = [0; 2];
    while let Some((field, wire)) = input.next(counts, budget)? {
        anyhow::ensure!(
            matches!(field, 1 | 2),
            "Unsupported native rewrite group field"
        );
        add_rows(
            &mut rows[field as usize - 1],
            fragment(input.bytes(wire)?, counts, budget)?,
        )?;
    }
    Ok(())
}

// Only operations emitted by managed create/merge/delete/compaction/policy
// migration are admitted. Unknown/manual index/blob/WAL/schema operations defer.
fn operation(
    kind: u64,
    bytes: &[u8],
    counts: &mut Counts,
    schema: &mut SchemaIds,
    rows: &mut [usize; 3],
    budget: &mut Budget<'_>,
) -> anyhow::Result<()> {
    if kind == 110 {
        return update_config(bytes, counts, budget);
    }
    let mut input = Wire { bytes };
    while let Some((field, wire)) = input.next(counts, budget)? {
        match (kind, field) {
            (100, 1) | (101, 1) | (102, 1) | (104, 1 | 2) | (108, 2 | 3) => {
                add_rows(
                    &mut rows[field as usize - 1],
                    fragment(input.bytes(wire)?, counts, budget)?,
                )?;
            }
            (101, 2) | (108, 1 | 4 | 6) => repeated_scalar(&mut input, wire, counts, budget)?,
            (101, 3) => {
                input.bytes(wire)?;
            }
            (102, 2) => schema_field(input.bytes(wire)?, counts, schema, budget)?,
            (102, 3 | 4) => map(input.bytes(wire)?, counts, budget)?,
            (104, 3) => rewrite_group(input.bytes(wire)?, counts, budget)?,
            (107, 1) | (108, 7) => {
                input.scalar(wire)?;
            }
            _ => anyhow::bail!("Unsupported native transaction operation field; history preserved"),
        }
    }
    Ok(())
}

fn transaction(bytes: &[u8], counts: &mut Counts, budget: &mut Budget<'_>) -> anyhow::Result<()> {
    let mut input = Wire { bytes };
    let mut schema = SchemaIds::default();
    // Protobuf can merge repeated occurrences of the same oneof message. Keep
    // each operation's vector totals across occurrences, rather than resetting
    // the overflow check at each wire envelope.
    let mut rows = [[0; 3]; 7];
    while let Some((field, wire)) = input.next(counts, budget)? {
        match field {
            1 => {
                input.scalar(wire)?;
            }
            2 => {
                input.bytes(wire)?;
            }
            3 => anyhow::ensure!(
                input.bytes(wire)?.is_empty(),
                "Native transaction tag requires separate maintenance"
            ),
            4 => map(input.bytes(wire)?, counts, budget)?,
            100 | 101 | 102 | 104 | 107 | 108 | 110 => {
                let slot = match field {
                    100 => 0,
                    101 => 1,
                    102 => 2,
                    104 => 3,
                    107 => 4,
                    108 => 5,
                    110 => 6,
                    _ => unreachable!(),
                };
                operation(
                    field,
                    input.bytes(wire)?,
                    counts,
                    &mut schema,
                    &mut rows[slot],
                    budget,
                )?
            }
            _ => anyhow::bail!("Unsupported native transaction field; history preserved"),
        }
    }
    Ok(())
}

fn section(bytes: &[u8], offset: u64, manifest_start: usize) -> anyhow::Result<&[u8]> {
    let start = usize::try_from(offset)?;
    let header = start
        .checked_add(4)
        .filter(|end| *end <= manifest_start)
        .ok_or_else(|| anyhow::anyhow!("Invalid native manifest section offset"))?;
    let length = u32::from_le_bytes(bytes[start..header].try_into()?) as usize;
    let end = header
        .checked_add(length)
        .filter(|end| *end <= manifest_start)
        .ok_or_else(|| anyhow::anyhow!("Invalid native manifest section length"))?;
    Ok(&bytes[header..end])
}

fn validate(bytes: &[u8], budget: &mut Budget<'_>) -> anyhow::Result<()> {
    anyhow::ensure!(
        (20..=MAX_MANIFEST_BYTES as usize).contains(&bytes.len()),
        "Invalid bounded native manifest size"
    );
    let footer = &bytes[bytes.len() - 16..];
    anyhow::ensure!(
        &footer[12..] == lance_table::format::MAGIC
            && i16::from_le_bytes(footer[8..10].try_into()?) == lance_table::format::MAJOR_VERSION
            // The actual locked writer (lance-table/io/commit.rs) imports
            // lance_file::format 0/2, not lance_table::format's legacy 0/1.
            && i16::from_le_bytes(footer[10..12].try_into()?) == 2,
        "Unsupported native manifest footer; history preserved"
    );
    let position = i64::from_le_bytes(footer[..8].try_into()?);
    anyhow::ensure!(position >= 0, "Invalid native manifest position");
    let start = usize::try_from(position)?;
    let end = bytes.len() - 16;
    anyhow::ensure!(
        start <= end.saturating_sub(4),
        "Invalid native manifest position"
    );
    let length = u32::from_le_bytes(bytes[start..start + 4].try_into()?) as usize;
    anyhow::ensure!(
        length == end - start - 4,
        "Invalid native manifest message length"
    );
    let mut input = Wire {
        bytes: &bytes[start + 4..end],
    };
    let mut counts = Counts::default();
    let mut schema = SchemaIds::default();
    let mut rows = 0;
    while let Some((field, wire)) = input.next(&mut counts, budget)? {
        match field {
            1 => schema_field(input.bytes(wire)?, &mut counts, &mut schema, budget)?,
            2 => add_rows(
                &mut rows,
                fragment(input.bytes(wire)?, &mut counts, budget)?,
            )?,
            3 | 9 | 10 | 11 | 14 => {
                input.scalar(wire)?;
            }
            4 => anyhow::ensure!(
                input.scalar(wire)? == 0,
                "Unsupported native auxiliary metadata"
            ),
            5 | 16 => map(input.bytes(wire)?, &mut counts, budget)?,
            6 => anyhow::ensure!(
                section(bytes, input.scalar(wire)?, start)?.is_empty(),
                "Native index metadata requires separate maintenance; history preserved"
            ),
            7 => scalars(input.bytes(wire)?, &[1, 2], &mut counts, budget)?,
            8 => anyhow::ensure!(
                input.bytes(wire)?.is_empty(),
                "Native tag requires separate maintenance"
            ),
            12 => {
                input.bytes(wire)?;
            }
            13 => strings(input.bytes(wire)?, &[1, 2, 3, 4], &mut counts, budget)?,
            15 => strings(input.bytes(wire)?, &[1, 2], &mut counts, budget)?,
            // Dataset::open may decode/cache an inline transaction from its
            // prefetched tail. Admit its nested allocations before table-open,
            // sharing the same counters with the manifest itself.
            21 => {
                transaction(
                    section(bytes, input.scalar(wire)?, start)?,
                    &mut counts,
                    budget,
                )?;
            }
            _ => anyhow::bail!("Unsupported native manifest field; history preserved"),
        }
    }
    Ok(())
}

pub(super) fn check_file(
    path: &Path,
    expected_size: u64,
    budget: &mut Budget<'_>,
) -> anyhow::Result<()> {
    budget.charge()?;
    anyhow::ensure!(
        expected_size <= MAX_MANIFEST_BYTES,
        "Native manifest exceeds 512KiB; history preserved"
    );
    let file = std::fs::File::open(path)?;
    anyhow::ensure!(
        file.metadata()?.len() == expected_size,
        "Native manifest changed during inspection"
    );
    // Bounded even if a cooperative-store violation changes the file size.
    let mut bytes = Vec::with_capacity(expected_size as usize);
    file.take(MAX_MANIFEST_BYTES + 1).read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() as u64 == expected_size,
        "Native manifest changed during inspection"
    );
    validate(&bytes, budget)
}
