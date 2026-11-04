//! File metadata scanning preserves validation of every stored node.

use super::*;
use serde::de::{DeserializeSeed, MapAccess, SeqAccess, Visitor};
use std::cell::Cell;

/// Same complete positional wire layout as Node, including fields a metadata
/// caller does not retain. Borrow only while the redb value guard is alive.
/// Keep this DTO synchronized with every Node wire-field change; accepted
/// trailing bytes must not hide an appended field that should be validated.
#[derive(Deserialize)]
struct BorrowedNode<'a> {
    #[serde(borrow)]
    _node_id: &'a str,
    #[serde(borrow)]
    node_type: &'a str,
    #[serde(borrow)]
    _name: &'a str,
    #[serde(borrow)]
    _namespace: &'a str,
    #[serde(borrow)]
    _language: &'a str,
    #[serde(borrow)]
    file_path: &'a str,
    _start_line: u32,
    _end_line: u32,
    generation: u64,
    // Node's optional_json_string stores Option<String>, not a JSON binary DOM.
    #[serde(default, borrow)]
    metadata: Option<&'a str>,
}

struct JsonSeed<'a>(&'a Cell<bool>);
impl<'de> DeserializeSeed<'de> for JsonSeed<'_> {
    type Value = ();
    fn deserialize<D: serde::Deserializer<'de>>(self, input: D) -> Result<(), D::Error> {
        input.deserialize_any(JsonVisitor(self.0))
    }
}
struct JsonVisitor<'a>(&'a Cell<bool>);
impl<'de> Visitor<'de> for JsonVisitor<'_> {
    type Value = ();
    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a JSON value")
    }
    fn visit_unit<E: serde::de::Error>(self) -> Result<(), E> {
        Ok(())
    }
    fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<(), E> {
        Ok(())
    }
    fn visit_i64<E: serde::de::Error>(self, _: i64) -> Result<(), E> {
        Ok(())
    }
    fn visit_u64<E: serde::de::Error>(self, _: u64) -> Result<(), E> {
        Ok(())
    }
    fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<(), E> {
        Ok(())
    }
    fn visit_str<E: serde::de::Error>(self, _: &str) -> Result<(), E> {
        Ok(())
    }
    fn visit_string<E: serde::de::Error>(self, _: String) -> Result<(), E> {
        Ok(())
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<(), A::Error> {
        while sequence.next_element_seed(JsonSeed(self.0))?.is_some() {}
        Ok(())
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        while let Some(JsonKey(reserved)) = map.next_key::<JsonKey>()? {
            if reserved {
                self.0.set(true);
                return Err(serde::de::Error::custom(
                    "Reserved JSON key requires complete value validation",
                ));
            }
            map.next_value_seed(JsonSeed(self.0))?;
        }
        Ok(())
    }
}

/// Decode/validate keys normally (including escapes and surrogate pairs), but
/// retain no owned key strings. serde_json's private map tokens need its own
/// Value decoder, whose interpretation depends on enabled features and order.
struct JsonKey(bool);
impl<'de> Deserialize<'de> for JsonKey {
    fn deserialize<D: serde::Deserializer<'de>>(input: D) -> Result<Self, D::Error> {
        struct KeyVisitor;
        impl Visitor<'_> for KeyVisitor {
            type Value = JsonKey;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a JSON object key")
            }
            fn visit_str<E: serde::de::Error>(self, key: &str) -> Result<JsonKey, E> {
                Ok(JsonKey(matches!(
                    key,
                    "$serde_json::private::RawValue" | "$serde_json::private::Number"
                )))
            }
        }
        input.deserialize_str(KeyVisitor)
    }
}

fn validate_metadata(json: &str) -> anyhow::Result<()> {
    let reserved = Cell::new(false);
    let mut decoder = serde_json::Deserializer::from_str(json);
    let result = JsonSeed(&reserved)
        .deserialize(&mut decoder)
        .and_then(|_| decoder.end());
    if reserved.get() {
        // Delegate the entire original string, not just the reserved field:
        // first/later keys, duplicate keys and trailing input keep Value's exact
        // behavior. Ordinary symbol metadata needs no DOM; reserved keys may.
        serde_json::from_str::<JsonValue>(json)?;
    } else {
        result?;
    }
    Ok(())
}

pub(super) fn scan<T>(
    graph: &GraphStore,
    project: &str,
    retain: impl Fn(RelPath, Option<JsonValue>, u64) -> T,
) -> anyhow::Result<Vec<T>> {
    let prefix = format!("{project}\0");
    let tx = graph.db.begin_read()?;
    let table = tx.open_table(NODES)?;
    let mut output = Vec::new();
    for entry in table.range(prefix.as_str()..)? {
        let (key, value) = entry?;
        if !key.value().starts_with(&prefix) {
            break;
        }
        // Preserve Node's complete bincode validation and trailing-byte policy.
        let node: BorrowedNode<'_> = bincode::deserialize(value.value())?;
        if node.node_type == "file" {
            // Derived RelPath deserialization preserves stored bytes. Calling
            // RelPath::new here would silently normalize legacy stored paths.
            let path =
                serde_json::from_value::<RelPath>(JsonValue::String(node.file_path.to_owned()))?;
            let metadata = node
                .metadata
                .map(serde_json::from_str::<JsonValue>)
                .transpose()?;
            output.push(retain(path, metadata, node.generation));
        } else if let Some(metadata) = node.metadata {
            validate_metadata(metadata)?;
        }
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use std::{
        alloc::{GlobalAlloc, Layout, System},
        cell::Cell,
    };

    struct MeasuredAllocator;
    thread_local! {
        static MEASURING: Cell<bool> = const { Cell::new(false) };
        static ALLOCATED: Cell<u64> = const { Cell::new(0) };
        static REQUESTS: Cell<u64> = const { Cell::new(0) };
    }
    fn allocated(bytes: usize) {
        if MEASURING.try_with(Cell::get).unwrap_or(false) {
            let _ = ALLOCATED.try_with(|n| n.set(n.get().saturating_add(bytes as u64)));
            let _ = REQUESTS.try_with(|n| n.set(n.get().saturating_add(1)));
        }
    }
    // Instrumentation belongs to the test executable, never the installed app.
    #[global_allocator]
    static ALLOCATOR: MeasuredAllocator = MeasuredAllocator;
    unsafe impl GlobalAlloc for MeasuredAllocator {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            let result = unsafe { System.alloc(layout) };
            if !result.is_null() {
                allocated(layout.size());
            }
            result
        }
        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            let result = unsafe { System.alloc_zeroed(layout) };
            if !result.is_null() {
                allocated(layout.size());
            }
            result
        }
        unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
            let result = unsafe { System.realloc(pointer, layout, size) };
            if !result.is_null() {
                allocated(size);
            }
            result
        }
        unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
            unsafe { System.dealloc(pointer, layout) }
        }
    }
    struct EndMeasurement;
    impl Drop for EndMeasurement {
        fn drop(&mut self) {
            MEASURING.with(|flag| flag.set(false));
        }
    }
    fn allocations<T>(call: impl FnOnce() -> T) -> (T, u64, u64) {
        ALLOCATED.with(|n| n.set(0));
        REQUESTS.with(|n| n.set(0));
        MEASURING.with(|flag| flag.set(true));
        let end = EndMeasurement;
        let result = call();
        drop(end);
        (result, ALLOCATED.with(Cell::get), REQUESTS.with(Cell::get))
    }

    #[cfg(windows)]
    fn thread_cpu_ns() -> Option<u64> {
        #[repr(C)]
        #[derive(Default)]
        struct FileTime {
            low: u32,
            high: u32,
        }
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetCurrentThread() -> *mut std::ffi::c_void;
            fn GetThreadTimes(
                thread: *mut std::ffi::c_void,
                created: *mut FileTime,
                exited: *mut FileTime,
                kernel: *mut FileTime,
                user: *mut FileTime,
            ) -> i32;
        }
        let (mut created, mut exited, mut kernel, mut user) = (
            FileTime::default(),
            FileTime::default(),
            FileTime::default(),
            FileTime::default(),
        );
        // The pseudo-handle is borrowed; all FILETIME buffers are valid outputs.
        let success = unsafe {
            GetThreadTimes(
                GetCurrentThread(),
                &mut created,
                &mut exited,
                &mut kernel,
                &mut user,
            )
        };
        (success != 0).then(|| {
            (((kernel.high as u64) << 32 | kernel.low as u64)
                + ((user.high as u64) << 32 | user.low as u64))
                * 100
        })
    }
    #[cfg(not(windows))]
    fn thread_cpu_ns() -> Option<u64> {
        None
    }

    fn node(
        id: &str,
        kind: &str,
        path: &str,
        metadata: Option<JsonValue>,
        generation: u64,
    ) -> Node {
        Node {
            node_id: id.into(),
            node_type: kind.into(),
            name: "Declaration".into(),
            namespace: "memory".into(),
            language: "rust".into(),
            file_path: RelPath::new(path),
            start_line: 1,
            end_line: 2,
            generation,
            metadata,
        }
    }
    fn raw_rows_hash(graph: &GraphStore) -> String {
        let tx = graph.db.begin_read().unwrap();
        let table = tx.open_table(NODES).unwrap();
        let mut digest = blake3::Hasher::new();
        for row in table.iter().unwrap() {
            let (key, value) = row.unwrap();
            digest.update(&(key.value().len() as u64).to_le_bytes());
            digest.update(key.value().as_bytes());
            digest.update(&(value.value().len() as u64).to_le_bytes());
            digest.update(value.value());
        }
        digest.finalize().to_hex().to_string()
    }

    #[derive(Serialize)]
    struct WireNode<'a> {
        node_id: &'a str,
        node_type: &'a str,
        name: &'a str,
        namespace: &'a str,
        language: &'a str,
        file_path: &'a str,
        start_line: u32,
        end_line: u32,
        generation: u64,
        metadata: Option<&'a str>,
    }
    fn wire(metadata: Option<&str>) -> Vec<u8> {
        wire_at("function", "other.rs", metadata)
    }
    fn wire_at(kind: &str, path: &str, metadata: Option<&str>) -> Vec<u8> {
        bincode::serialize(&WireNode {
            node_id: "irrelevant-symbol",
            node_type: kind,
            name: "Symbol",
            namespace: "memory",
            language: "rust",
            file_path: path,
            start_line: 1,
            end_line: 2,
            generation: 9,
            metadata,
        })
        .unwrap()
    }
    fn store_raw(graph: &GraphStore, project: &str, bytes: &[u8]) {
        let tx = graph.db.begin_write().unwrap();
        tx.open_table(NODES)
            .unwrap()
            .insert(format!("{project}\0zz-corrupt").as_str(), bytes)
            .unwrap();
        tx.commit().unwrap();
    }

    #[test]
    fn canonical_and_shadow_file_rows_preserve_order_metadata_nulls_and_physical_generations() {
        let graph = TemporaryGraphStore::new();
        graph
            .upsert_nodes(
                "p",
                &[
                    node(
                        "file:b.rs",
                        "file",
                        "b.rs",
                        Some(serde_json::json!({"file_hash":"b","mtime":7})),
                        13,
                    ),
                    node("legacy-shadow", "file", "b.rs", None, 2),
                    node("z-old-file", "file", "a.rs", Some(JsonValue::Null), 99),
                    node(
                        "a-function",
                        "function",
                        "a.rs",
                        Some(serde_json::json!({"not_a_file":true})),
                        13,
                    ),
                ],
            )
            .unwrap();
        graph
            .upsert_nodes(
                "p-extra",
                &[node("file:outside", "file", "outside.rs", None, 1)],
            )
            .unwrap();
        let before = raw_rows_hash(&graph);
        let expected = vec![
            (
                RelPath::new("b.rs"),
                Some(serde_json::json!({"file_hash":"b","mtime":7})),
                13,
            ),
            (RelPath::new("b.rs"), None, 2),
            (RelPath::new("a.rs"), Some(JsonValue::Null), 99),
        ];
        for _ in 0..2 {
            assert_eq!(
                graph.list_file_node_metadata_with_generation("p").unwrap(),
                expected
            );
            assert_eq!(
                graph.list_file_node_metadata("p").unwrap(),
                expected
                    .iter()
                    .map(|(p, m, _)| (p.clone(), m.clone()))
                    .collect::<Vec<_>>()
            );
        }
        assert_eq!(
            raw_rows_hash(&graph),
            before,
            "Metadata inspection must not rewrite any row"
        );
    }

    #[test]
    fn corrupt_symbol_rows_cannot_be_silently_skipped_by_a_file_only_scan() {
        let graph = TemporaryGraphStore::new();
        graph
            .upsert_nodes("p", &[node("file:good", "file", "good.rs", None, 1)])
            .unwrap();
        let mut truncated = wire(None);
        truncated.pop();
        let mut bad_option = wire(None);
        *bad_option.last_mut().unwrap() = 9;
        let mut cases = vec![truncated, bad_option];
        let mut bad_metadata_utf8 = wire(Some("null"));
        *bad_metadata_utf8.last_mut().unwrap() = 0xff;
        cases.push(bad_metadata_utf8);
        let encoded = wire(None);
        let mut offset = 0;
        for _ in 0..6 {
            let length =
                u64::from_le_bytes(encoded[offset..offset + 8].try_into().unwrap()) as usize;
            let mut invalid = encoded.clone();
            invalid[offset + 8] = 0xff;
            cases.push(invalid);
            offset += 8 + length;
        }
        for json in [
            "{",
            "1e9999",
            r#""\uD800""#,
            r#"{"\uD800":0}"#,
            "true false",
        ] {
            cases.push(wire(Some(json)));
        }
        for bytes in cases {
            store_raw(&graph, "p", &bytes);
            let before = raw_rows_hash(&graph);
            assert!(graph.list_file_node_metadata("p").is_err());
            assert!(graph.list_file_node_metadata_with_generation("p").is_err());
            assert_eq!(
                raw_rows_hash(&graph),
                before,
                "Corruption refusal must not repair or truncate the store"
            );
        }
    }

    #[test]
    fn old_file_path_bytes_are_returned_without_silent_path_normalization() {
        let graph = TemporaryGraphStore::new();
        let path = " leading\\legacy//../path ";
        store_raw(&graph, "p", &wire_at("file", path, Some("null")));
        let before = raw_rows_hash(&graph);
        let rows = graph.list_file_node_metadata_with_generation("p").unwrap();
        assert_eq!(rows[0].0.as_str(), path);
        assert_eq!(rows[0].1, Some(JsonValue::Null));
        assert_eq!(rows[0].2, 9);
        assert_eq!(
            graph.list_file_node_metadata("p").unwrap()[0].0.as_str(),
            path
        );
        assert_eq!(raw_rows_hash(&graph), before);
    }

    #[test]
    fn stored_node_trailing_bytes_keep_the_existing_bincode_compatibility_policy() {
        let graph = TemporaryGraphStore::new();
        for kind in ["file", "function"] {
            let mut bytes = wire_at(kind, "legacy.rs", Some("{\"hash\":7}"));
            bytes.extend_from_slice(&[0xde, 0xad, 0, 1]);
            assert!(
                bincode::deserialize::<Node>(&bytes).is_ok(),
                "Fixture must exercise an accepted legacy encoding"
            );
            store_raw(&graph, "p", &bytes);
            let before = raw_rows_hash(&graph);
            assert_eq!(
                graph.list_file_node_metadata("p").unwrap().len(),
                usize::from(kind == "file")
            );
            assert_eq!(
                graph
                    .list_file_node_metadata_with_generation("p")
                    .unwrap()
                    .len(),
                usize::from(kind == "file")
            );
            assert_eq!(raw_rows_hash(&graph), before);
        }
    }

    #[test]
    fn symbol_metadata_retains_the_full_json_values_acceptance_rules() {
        let graph = TemporaryGraphStore::new();
        graph
            .upsert_nodes("p", &[node("file:good", "file", "good.rs", None, 1)])
            .unwrap();
        let mut cases: Vec<String> = [
            "null",
            "true",
            "0",
            "-0.0",
            "1e308",
            "1e9999",
            "1844674407370955161600000",
            "[1,2,{\"escaped\":\"a\\nb\"}]",
            "{\"duplicate\":1,\"duplicate\":2}",
            r#""\uD83D\uDE00""#,
            r#""\uD800""#,
            r#"{"\uD800":1}"#,
            r#"{"$serde_json::private::RawValue":"true"}"#,
            r#"{"$serde_json::private::RawValue":"not json"}"#,
            r#"{"first":0,"$serde_json::private::RawValue":"not json"}"#,
            r#"{"nested":{"$serde_json::private::RawValue":"[1,2]"}}"#,
            r#"{"\u0024serde_json::private::RawValue":"not json"}"#,
            r#"{"$serde_json::private::RawValue":"true","extra":1}"#,
            r#"{"$serde_json::private::RawValue":"true","$serde_json::private::RawValue":"false"}"#,
            r#"{"$serde_json::private::Number":"not a number"}"#,
            r#"{"value":"$serde_json::private::RawValue"}"#,
            "true false",
            "{broken",
            "[1,]",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        for depth in [8, 127, 128, 129, 260] {
            cases.push(format!("{}0{}", "[".repeat(depth), "]".repeat(depth)));
        }
        for json in cases {
            let accepted = serde_json::from_str::<JsonValue>(&json).is_ok();
            store_raw(&graph, "p", &wire(Some(&json)));
            let before = raw_rows_hash(&graph);
            assert_eq!(
                graph.list_file_node_metadata_with_generation("p").is_ok(),
                accepted,
                "{json}"
            );
            assert_eq!(
                graph.list_file_node_metadata("p").is_ok(),
                accepted,
                "{json}"
            );
            assert_eq!(raw_rows_hash(&graph), before);
        }
    }

    type FileRows = Vec<(RelPath, Option<JsonValue>, u64)>;
    type Scan = fn(&GraphStore, &str) -> anyhow::Result<FileRows>;

    /// The previous production scan, kept only as a measured compatibility
    /// reference. Both paths inspect the same database in this executable.
    fn full_node_reference(graph: &GraphStore, project: &str) -> anyhow::Result<FileRows> {
        let prefix = format!("{project}\0");
        let tx = graph.db.begin_read()?;
        let table = tx.open_table(NODES)?;
        let mut output = Vec::new();
        for entry in table.range(prefix.as_str()..)? {
            let (key, value) = entry?;
            if !key.value().starts_with(&prefix) {
                break;
            }
            let node: Node = bincode::deserialize(value.value())?;
            if node.node_type == "file" {
                output.push((node.file_path, node.metadata, node.generation));
            }
        }
        Ok(output)
    }

    #[test]
    fn metadata_scan_resource_measurement_uses_one_bounded_owned_graph_and_writes_nothing() {
        let graph = TemporaryGraphStore::new();
        let mut nodes = Vec::new();
        for n in 0..64 {
            nodes.push(node(
                &format!("file:{n:04}.rs"),
                "file",
                &format!("{n:04}.rs"),
                Some(serde_json::json!({"file_hash":format!("hash-{n}"),"mtime":42,"size":2048})),
                n % 3 + 1,
            ));
        }
        let body = "pub fn entry() {\n    call(\"argument\");\n}\n".repeat(24);
        for n in 0..4000 {
            nodes.push(node(&format!("symbol:{n:06}"),"function",&format!("{:04}.rs",n%64),
                Some(serde_json::json!({"body":body,"calls":["Api.read","Api.write","Store.get","Store.put"],
                    "ranges":[{"start":1,"end":2},{"start":3,"end":4}],"attributes":{"public":true,"tags":["service","scope"]}})),1));
        }
        graph.upsert_nodes("p", &nodes).unwrap();
        drop(nodes);
        let before = raw_rows_hash(&graph);
        let warm = graph.list_file_node_metadata_with_generation("p").unwrap();
        assert_eq!(warm.len(), 64);
        let scans = if cfg!(debug_assertions) { 10 } else { 100 };
        let methods = [
            ("full_node_reference", full_node_reference as Scan),
            (
                "borrowed_projection",
                GraphStore::list_file_node_metadata_with_generation as Scan,
            ),
        ];
        for (_, scan) in methods {
            assert_eq!(scan(&graph, "p").unwrap(), warm);
        }
        for round in 0..4 {
            let mut ordered = methods;
            if round % 2 == 1 {
                ordered.reverse();
            }
            for (label, scan) in ordered {
                let scan = std::hint::black_box(scan);
                assert_eq!(scan(&graph, "p").unwrap(), warm);
                let (rows, bytes, requests) = allocations(|| scan(&graph, "p").unwrap());
                assert_eq!(rows, warm);
                drop(rows);
                let cpu_before = thread_cpu_ns();
                let started = std::time::Instant::now();
                for _ in 0..scans {
                    assert_eq!(std::hint::black_box(scan(&graph, "p").unwrap()), warm);
                }
                let wall_ns = started.elapsed().as_nanos();
                let cpu_ns = cpu_before
                    .zip(thread_cpu_ns())
                    .map(|(before, after)| after.saturating_sub(before));
                assert_eq!(raw_rows_hash(&graph), before);
                eprintln!(
                    "graph_file_scan_measurement:{}",
                    serde_json::json!({"round":round,"scan":label,"files":64,"symbols":4000,
                "requested_allocation_bytes_per_scan":bytes,"allocation_requests_per_scan":requests,
                "timing_scans":scans,"thread_cpu_ns":cpu_ns,"wall_ns":wall_ns,"stored_rows_unchanged":true,
                "fixture_rows_blake3":before,"profile":if cfg!(debug_assertions){"debug"}else{"release"},
                "limits":"Controlled owned fixture; requested allocation volume is not RSS, SSD writes or attribution of the reported live resource problem."})
                );
            }
        }
    }
}
