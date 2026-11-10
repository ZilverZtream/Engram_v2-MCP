use super::*;
use anyhow::{Result, ensure};

#[test]
fn project_compiler_context_must_be_present_and_match_before_metadata_is_reused() -> Result<()> {
    let owned = tempfile::tempdir()?;
    let mut context = VbParserContext::from_authority(None);
    context.compiler_context = Some(VbCompilerContext {
        root: std::fs::canonicalize(owned.path())?.to_string_lossy().into_owned(),
        fingerprint: "a".repeat(64),
        mode: "source_only".into(),
        reference_bytes: 0,
        reason: Some("No supported project compiler configuration".into()),
    });
    let metadata = serde_json::to_value(VbParserProvenance::RustFallback {
        context: context.clone(), reason: VbFallbackReason::Unavailable,
    })?;
    ensure!(context.matches_metadata(&metadata));
    let mut old_metadata = metadata.clone();
    old_metadata["context"].as_object_mut().unwrap().remove("compiler_context");
    ensure!(!context.matches_metadata(&old_metadata),
        "Pre-context source metadata cannot inherit a newly configured compilation");
    let mut changed_imports = context.clone();
    changed_imports.compiler_context.as_mut().unwrap().fingerprint = "b".repeat(64);
    ensure!(!changed_imports.matches_metadata(&metadata),
        "Unchanged VB bytes do not authorize reuse after compiler inputs change");
    let mut http = context.clone();
    let compiler = http.compiler_context.as_mut().unwrap();
    compiler.mode = "framework_plus_web_http".into();
    compiler.reference_bytes = 1024;
    compiler.reason = None;
    let http_metadata = serde_json::to_value(VbParserProvenance::RustFallback {
        context: http.clone(), reason: VbFallbackReason::Unavailable,
    })?;
    ensure!(http.matches_metadata(&http_metadata),
        "The explicit partial HTTP profile must retain its admitted compiler authority");
    ensure!(!http.matches_metadata(&metadata) && !context.matches_metadata(&http_metadata),
        "A new metadata profile must not reuse old source-only receipts, or vice versa");
    let mut changed_http = http.clone();
    changed_http.compiler_context.as_mut().unwrap().fingerprint = "c".repeat(64);
    ensure!(!changed_http.matches_metadata(&http_metadata),
        "Byte-identical VB source cannot reuse witnesses after an HTTP reference changes");
    let standalone = VbParserContext::from_authority(None);
    ensure!(!standalone.matches_metadata(&metadata),
        "Rootless extraction cannot inherit project compiler authority");
    owned.close()?;
    Ok(())
}

#[test]
fn unavailable_vb_parser_records_actual_fallback_for_zero_symbol_source() -> Result<()> {
    let owned = tempfile::tempdir()?;
    let path = owned.path().join("empty.vb");
    std::fs::write(&path, " \n")?;
    let mut client = None;
    let (symbols, edges, provenance) = extract_vb_with_client(&mut client,
        &owned.path().join("unavailable-owned-companion"), &path, " \n");
    ensure!(symbols.is_empty() && edges.is_empty());
    ensure!(provenance == VbParserProvenance::fallback() && provenance.is_verified());
    ensure!(client.is_none());
    drop(client);
    owned.close()?;
    Ok(())
}

/// Runs only with an explicitly selected built companion. No installed runtime
/// asset is copied, overwritten or removed; each test owns and joins its child.
#[test]
#[ignore = "requires ENGRAM_VB_SIDECAR_TEST_PATH pointing to the built identity-capable real companion"]
fn real_vb_child_receipt_changes_fallback_compatibility_and_survives_restart_without_identity_churn() -> Result<()> {
    let binary = PathBuf::from(std::env::var_os("ENGRAM_VB_SIDECAR_TEST_PATH")
        .ok_or_else(|| anyhow::anyhow!("Set the explicitly built companion path"))?);
    let owned = tempfile::tempdir()?;
    let path = owned.path().join("empty.vb");
    std::fs::write(&path, " \n")?;
    let mut client = None;
    let result = (|| -> Result<()> {
        let (_, _, unavailable) = extract_vb_with_client(&mut client,
            &owned.path().join("unavailable-owned-companion"), &path, " \n");
        let (symbols, edges, actual) = extract_vb_with_client(&mut client, &binary, &path, " \n");
        ensure!(symbols.is_empty() && edges.is_empty(), "Zero-symbol output must still carry the actual parser");
        ensure!(matches!(actual, VbParserProvenance::Roslyn { .. }) && actual.is_verified());
        ensure!(actual != unavailable, "Newly available Roslyn must not inherit fallback provenance");
        let first_pid = client.as_ref().ok_or_else(|| anyhow::anyhow!("Child missing"))?.transport.pid();
        let (_, _, repeated) = extract_vb_with_client(&mut client, &binary, &path, " \n");
        ensure!(repeated == actual && client.as_ref().unwrap().transport.pid() == first_pid,
            "Repeated extraction through the same child must remain compatible");
        client.as_mut().unwrap().stop()?;
        let (_, _, restarted) = extract_vb_with_client(&mut client, &binary, &path, " \n");
        ensure!(restarted == actual && client.as_ref().unwrap().transport.pid() != first_pid,
            "A restarted child must reacquire its own loaded-module receipt without generation churn for the same parser");
        Ok(())
    })();
    drop(client);
    let cleanup = owned.close();
    match (result, cleanup) {
        (Err(error), Err(cleanup)) => Err(error.context(format!("Fixture cleanup failed: {cleanup}"))),
        (Err(error), _) => Err(error),
        (Ok(()), cleanup) => cleanup.map_err(Into::into),
    }
}

#[test]
#[ignore = "requires ENGRAM_VB_SIDECAR_LEGACY_TEST_PATH pointing to an existing pre-identity real companion"]
fn real_legacy_vb_child_preserves_parse_protocol_but_cannot_claim_verified_ingestion() -> Result<()> {
    let binary = PathBuf::from(std::env::var_os("ENGRAM_VB_SIDECAR_LEGACY_TEST_PATH")
        .ok_or_else(|| anyhow::anyhow!("Set an explicit legacy companion path"))?);
    let owned = tempfile::tempdir()?;
    let path = owned.path().join("legacy.vb");
    let source = "Public Class Legacy\nPublic Sub Run()\nEnd Sub\nEnd Class\n";
    std::fs::write(&path, source)?;
    let mut client = None;
    let result = (|| -> Result<()> {
        let (symbols, _, provenance) = extract_vb_with_client(&mut client, &binary, &path, source);
        ensure!(symbols.iter().any(|symbol| symbol.kind == "function"), "Legacy parse protocol should still work");
        ensure!(provenance == VbParserProvenance::UnverifiedRoslyn && !provenance.is_verified(),
            "An old child cannot silently acquire a guessed parser identity");
        Ok(())
    })();
    drop(client);
    let cleanup = owned.close();
    match (result, cleanup) {
        (Err(error), Err(cleanup)) => Err(error.context(format!("Fixture cleanup failed: {cleanup}"))),
        (Err(error), _) => Err(error),
        (Ok(()), cleanup) => cleanup.map_err(Into::into),
    }
}

#[test]
#[ignore = "requires ENGRAM_VB_SIDECAR_TEST_PATH pointing to the built real companion"]
fn real_vb_child_cancelled_pipe_request_joins_owner_before_replacement() -> Result<()> {
    let binary = PathBuf::from(std::env::var_os("ENGRAM_VB_SIDECAR_TEST_PATH")
        .ok_or_else(|| anyhow::anyhow!("Set the explicitly built companion path"))?);
    let owned = tempfile::tempdir()?;
    let mut client = Some(transport::Client::spawn(&binary)?);
    let result = (|| -> Result<()> {
        let payload = serde_json::json!({"cmd":"parse","path":owned.path().join("queued.vb"),
            "source":"' owned bounded queued parser work\n".repeat(80_000)}).to_string();
        let start = std::time::Instant::now();
        ensure!(client.as_mut().unwrap().request(payload, 4096, Duration::ZERO).is_err(),
            "A large real pipe request must respect its zero deadline");
        client.as_mut().unwrap().stop()?;
        ensure!(start.elapsed() < Duration::from_secs(10));
        drop(client.take());
        let mut replacement = transport::Client::spawn(&binary)?;
        let receipt = replacement.request("{\"cmd\":\"identity\"}".into(), 4096, Duration::from_secs(5))?;
        ensure!(serde_json::from_str::<SidecarResponse>(&receipt)?.parser_identity.is_some_and(|identity| identity.is_supported()));
        replacement.stop()?;
        Ok(())
    })();
    drop(client);
    let cleanup = owned.close();
    match (result, cleanup) {
        (Err(error), Err(cleanup)) => Err(error.context(format!("Fixture cleanup failed: {cleanup}"))),
        (Err(error), _) => Err(error),
        (Ok(()), cleanup) => cleanup.map_err(Into::into),
    }
}

#[test]
#[ignore = "requires ENGRAM_VB_SIDECAR_TEST_PATH pointing to the built v5 real companion"]
fn real_vb_project_parse_fences_prepared_bytes_while_standalone_uses_current_source() -> Result<()> {
    let binary = PathBuf::from(std::env::var_os("ENGRAM_VB_SIDECAR_TEST_PATH")
        .ok_or_else(|| anyhow::anyhow!("Set the explicitly built companion path"))?);
    let owned = tempfile::tempdir()?;
    let mut client = None;
    let result = (|| -> Result<()> {
        let root_a = owned.path().join("project A with spaces");
        let root_b = owned.path().join("project B with spaces");
        std::fs::create_dir(&root_a)?;
        std::fs::create_dir(&root_b)?;
        let old = "Public Class BeforeWarm\nEnd Class\n";
        let changed = "Public Class RequestedAfterWarm\nEnd Class\n";
        let other = "Public Class SeparateProject\nEnd Class\n";
        std::fs::write(root_a.join("Same.vb"), old)?;
        std::fs::write(root_b.join("Same.vb"), other)?;
        let root_a = std::fs::canonicalize(root_a)?;
        let root_b = std::fs::canonicalize(root_b)?;
        let path_a = std::fs::canonicalize(root_a.join("Same.vb"))?;
        let path_b = std::fs::canonicalize(root_b.join("Same.vb"))?;
        ensure_sidecar_at(&mut client, &binary)?;
        ensure!(refresh_project_locked(&mut client, &root_a, &[engram_core::RelPath::new("Same.vb")],
            &[path_a.to_string_lossy().into_owned()]));
        let context = VbParserContext::from_authority(client.as_ref().unwrap().identity.clone());
        ensure!(matches!(parse_via_sidecar(client.as_mut().unwrap(), &path_a, changed, &context, Some(&root_a)),
            Err(SidecarParseError::Identity)), "Changed request bytes must not mix with unrefreshed peer trees");
        let parsed = parse_via_sidecar(client.as_mut().unwrap(), &path_a, changed, &context, None)
            .map_err(|error| anyhow::anyhow!("Changed request failed: {error:?}"))?;
        ensure!(parsed.0.iter().any(|s| s.name == "RequestedAfterWarm")
            && !parsed.0.iter().any(|s| s.name == "BeforeWarm"),
            "A cached disk tree must not substitute its old source for the actual request");
        ensure!(std::fs::read_to_string(&path_a)? == old, "This proof must change only the request, not the warmup bytes");
        ensure!(matches!(parse_via_sidecar(client.as_mut().unwrap(), &path_b, other, &context, Some(&root_b)),
            Err(SidecarParseError::Identity)), "A different root must be refused without contaminating the warm compilation");
        let standalone = extract_vb_with_client(&mut client, &binary, &path_b, other);
        ensure!(standalone.2.is_verified() && standalone.0.iter().any(|s| s.name == "SeparateProject"));
        let after = parse_via_sidecar(client.as_mut().unwrap(), &path_a, old, &context, Some(&root_a))
            .map_err(|error| anyhow::anyhow!("Standalone query contaminated project A: {error:?}"))?;
        ensure!(after.0.iter().any(|s| s.name == "BeforeWarm"));
        // A deliberately high raw-source allowance must not permit escaped
        // JSON to exceed the transport budget or send a partial request. The
        // next valid file must still parse in the same healthy child.
        let mut high_source_limit = context.clone();
        let expanded = "\u{0001}".repeat(6 * 1024 * 1024);
        high_source_limit.max_source_bytes = expanded.len() + 1;
        let pid = client.as_ref().unwrap().transport.pid();
        ensure!(matches!(parse_via_sidecar(client.as_mut().unwrap(), &path_a, &expanded,
            &high_source_limit, Some(&root_a)), Err(SidecarParseError::SourceLimit)),
            "Escaped request overflow must be refused before parser IPC");
        drop(expanded);
        ensure!(client.as_ref().unwrap().transport.pid() == pid
            && client.as_ref().unwrap().transport.is_running());
        let next = parse_via_sidecar(client.as_mut().unwrap(), &path_a, old, &context, Some(&root_a))
            .map_err(|error| anyhow::anyhow!("Bounded refusal damaged the next real parse: {error:?}"))?;
        ensure!(next.0.iter().any(|symbol| symbol.name == "BeforeWarm"));
        // Old rootless JSON requests remain legal; they need not understand
        // either the new root echo or the parser receipt.
        let legacy = serde_json::json!({"cmd":"parse","path":path_a,"source":old}).to_string();
        let line = client.as_mut().unwrap().transport.request(legacy, 64 * 1024 * 1024, Duration::from_secs(10))?;
        ensure!(serde_json::from_str::<SidecarResponse>(&line)?.symbols.iter().any(|s| s.name == "BeforeWarm"));
        Ok(())
    })();
    drop(client);
    let cleanup = owned.close();
    match (result, cleanup) {
        (Err(error), Err(cleanup)) => Err(error.context(format!("Fixture cleanup failed: {cleanup}"))),
        (Err(error), _) => Err(error),
        (Ok(()), cleanup) => cleanup.map_err(Into::into),
    }
}

#[test]
#[ignore = "requires ENGRAM_VB_SIDECAR_TEST_PATH pointing to the built v5 real companion"]
fn real_vb_peer_refresh_prepares_changed_peers_before_b_first_and_cold_b_only() -> Result<()> {
    let binary = PathBuf::from(std::env::var_os("ENGRAM_VB_SIDECAR_TEST_PATH")
        .ok_or_else(|| anyhow::anyhow!("Set the explicitly built companion path"))?);
    let owned = tempfile::tempdir()?;
    let mut client = None;
    let result = (|| -> Result<()> {
        let root = owned.path().join("admitted project with spaces");
        std::fs::create_dir(&root)?;
        let root = std::fs::canonicalize(root)?;
        let a = root.join("A.vb");
        let b = root.join("B.vb");
        let source_a = |namespace: &str| format!("Namespace {namespace}\nPublic Class Peer\nPublic Shared Sub Ping()\nEnd Sub\nPublic Shared Property Flag As Integer\nEnd Class\nEnd Namespace\n");
        let source_b = |namespace: &str| format!("Imports {namespace}\nPublic Class Consumer\nPublic Function Run() As Integer\nPeer.Ping()\nExcluded.Ping()\nReturn Peer.Flag\nEnd Function\nEnd Class\n");
        let old_a = format!("\u{feff}{}", source_a("BeforeBindings"));
        let mut old_b_bytes = source_b("BeforeBindings").into_bytes();
        old_b_bytes.extend_from_slice(b"' owned lossy comment: \xff\n");
        let old_b = String::from_utf8_lossy(&old_b_bytes).into_owned();
        let new_a = format!("\u{feff}{}", source_a("AfterBindings"));
        let new_b = source_b("AfterBindings");
        std::fs::write(&a, &old_a)?;
        std::fs::write(&b, &old_b_bytes)?;
        // Present on disk but excluded by the caller's admission. A C# walk
        // would incorrectly resolve this invocation on cold initialization.
        std::fs::write(root.join("Ignored.vb"), "Public Class Excluded\nPublic Shared Sub Ping()\nEnd Sub\nEnd Class\n")?;
        let inventory = vec![engram_core::RelPath::new("A.vb"), engram_core::RelPath::new("B.vb")];
        let changed = vec![a.to_string_lossy().into_owned(), b.to_string_lossy().into_owned()];
        ensure_sidecar_at(&mut client, &binary)?;
        // An older client still warms peers then sends a rootless parse. Its
        // omitted new fields retain cross-file binding, without claiming the
        // new trusted prepared-source contract.
        ensure!(begin_project_locked(&mut client, &root, &changed));
        let legacy = serde_json::json!({"cmd":"parse", "path":b, "source":old_b}).to_string();
        let legacy = client.as_mut().unwrap().transport.request(legacy, 64 * 1024 * 1024, Duration::from_secs(10))?;
        let legacy: SidecarResponse = serde_json::from_str(&legacy)?;
        ensure!(legacy.error.is_none() && legacy.edges.iter().any(|edge| edge.target_name == "BeforeBindings.Peer.Ping"
            && !edge.metadata.contains_key("unresolved")), "Legacy warmed rootless B must still bind A");
        ensure!(refresh_project_locked(&mut client, &root, &inventory, &changed));
        let context = VbParserContext::from_authority(client.as_ref().unwrap().identity.clone());
        let check = |client: &mut Option<Sidecar>, source: &str, namespace: &str| -> Result<()> {
            let parsed = parse_via_sidecar(client.as_mut().unwrap(), &b, source, &context, Some(&root))
                .map_err(|error| anyhow::anyhow!("Actual peer parse failed: {error:?}"))?;
            ensure!(parsed.2.is_verified());
            ensure!(parsed.1.iter().any(|edge| edge.kind == "calls" && edge.target_name == format!("{namespace}.Peer.Ping")
                && edge.metadata.as_ref().is_none_or(|metadata| metadata.get("unresolved").is_none())),
                "B must resolve the current A before A has an extraction request: {:?}", parsed.1);
            ensure!(parsed.1.iter().any(|edge| edge.target_name == format!("{namespace}.Peer.Flag")
                && edge.metadata.as_ref().is_some_and(|metadata| metadata.get("via").is_some_and(|via| via == "property_get"))),
                "B must see the refreshed source property cache");
            ensure!(parsed.1.iter().any(|edge| edge.target_name == "Excluded.Ping"
                && edge.metadata.as_ref().is_some_and(|metadata| metadata.get("unresolved").is_some_and(|value| value == "true"))),
                "Unadmitted disk sources must not enter the compilation");
            Ok(())
        };
        check(&mut client, &old_b, "BeforeBindings")?;
        std::fs::write(&a, &new_a)?;
        std::fs::write(&b, &new_b)?;
        ensure!(refresh_project_locked(&mut client, &root, &inventory, &changed));
        check(&mut client, &new_b, "AfterBindings")?;
        parse_via_sidecar(client.as_mut().unwrap(), &a, &new_a, &context, Some(&root))
            .map_err(|error| anyhow::anyhow!("A parse failed: {error:?}"))?;
        // Individual extraction limits are separate from aggregate warmed
        // peers. Keep A's declarations available to B despite A's own honest
        // per-request SourceLimit fallback.
        let large_a = format!("{new_a}' {}\n", "x".repeat(6 * 1024 * 1024));
        ensure!(context.max_source_bytes < large_a.len());
        std::fs::write(&a, &large_a)?;
        ensure!(refresh_project_locked(&mut client, &root, &inventory, &changed[..1]));
        let actual_a = extract_vb_in_context(&mut client, &a, &large_a, context.clone(), Some(&root));
        ensure!(matches!(actual_a.2, VbParserProvenance::RustFallback { reason: VbFallbackReason::SourceLimit, .. }));
        check(&mut client, &new_b, "AfterBindings")?;
        let b_only = format!("{new_b}' later B-only edit\n");
        std::fs::write(&b, &b_only)?;
        ensure!(refresh_project_locked(&mut client, &root, &inventory, &changed[1..]));
        check(&mut client, &b_only, "AfterBindings")?;
        // A new actual child receives admitted A+B even though this operation
        // produces only B. Same-code restart must not erase unchanged peers.
        client.as_mut().unwrap().stop()?;
        ensure_sidecar_at(&mut client, &binary)?;
        ensure!(refresh_project_locked(&mut client, &root, &inventory, &changed[1..]));
        check(&mut client, &b_only, "AfterBindings")?;
        std::fs::remove_file(&a)?;
        ensure!(matches!(invalidate_locked(&mut client, &changed[..1]), Invalidate::Done));
        let deleted = parse_via_sidecar(client.as_mut().unwrap(), &b, &b_only, &context, Some(&root))
            .map_err(|error| anyhow::anyhow!("Deleted-peer parse failed: {error:?}"))?;
        ensure!(!deleted.1.iter().any(|edge| edge.target_name.starts_with("AfterBindings.Peer.")),
            "A deletion must remove both semantic and property-cache peers");
        ensure!(refresh_project_locked(&mut client, &root, &inventory[1..], &changed[1..]));
        // The real metadata plan cannot authorize bytes that appear later.
        let plan = serde_json::json!({"cmd":"plan_refresh", "project_root":root,
            "paths":[b], "compilation_policy":context.compilation});
        let planned = client.as_mut().unwrap().transport.request(transport::encode_request(&plan)?, 4096, Duration::from_secs(10))?;
        let planned: SidecarResponse = serde_json::from_str(&planned)?;
        let token = planned.compilation_plan.ok_or_else(|| anyhow::anyhow!("Real growth plan missing"))?.id;
        std::fs::write(&b, format!("{b_only}' growth after admitted metadata\n"))?;
        let refused = client.as_mut().unwrap().transport.request(
            format!("{{\"cmd\":\"refresh_project\",\"plan_id\":{token}}}"), 4096, Duration::from_secs(10))?;
        let refused: SidecarResponse = serde_json::from_str(&refused)?;
        ensure!(refused.compilation_error && refused.error.as_deref().is_some_and(|error| error.contains("changed after planning")),
            "A real file growth must fail before consuming unreserved source bytes");
        ensure!(matches!(parse_via_sidecar(client.as_mut().unwrap(), &b, &b_only, &context, Some(&root)),
            Err(SidecarParseError::Identity)), "Failed growth preparation must not retain trusted cache authority");
        std::fs::write(&b, &b_only)?;
        ensure!(refresh_project_locked(&mut client, &root, &[], &[]));
        ensure!(matches!(parse_via_sidecar(client.as_mut().unwrap(), &b, &b_only, &context, Some(&root)),
            Err(SidecarParseError::Identity)), "Explicit empty inventory must not trigger a disk walk");
        Ok(())
    })();
    drop(client);
    let cleanup = owned.close();
    match (result, cleanup) {
        (Err(error), Err(cleanup)) => Err(error.context(format!("Fixture cleanup failed: {cleanup}"))),
        (Err(error), _) => Err(error),
        (Ok(()), cleanup) => cleanup.map_err(Into::into),
    }
}

#[test]
#[ignore = "requires v5 ENGRAM_VB_SIDECAR_PATH and serialized idle global parser ownership"]
fn real_vb_restarted_child_refuses_old_batch_and_fresh_batch_reprepares_its_root() -> Result<()> {
    shutdown_idle_parser()?;
    struct ParserOwner;
    impl Drop for ParserOwner {
        fn drop(&mut self) {
            if let Err(error) = shutdown_idle_parser() { eprintln!("Owned VB restart fixture cleanup failed: {error:#}"); }
        }
    }
    let owned = tempfile::tempdir()?;
    let owner = ParserOwner;
    let result = (|| -> Result<()> {
        let root = owned.path().join("restart root with spaces");
        std::fs::create_dir(&root)?;
        let path = root.join("source.vb");
        let source = "Public Class RepreparedAfterRestart\nEnd Class\n";
        std::fs::write(&path, source)?;
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
        let tested = runtime.block_on(async {
            let inventory = Arc::new(vec![path.clone()]);
            let lease = prepare_ingestion_batch(&root, std::slice::from_ref(&path), Some(inventory.clone()), None, None, None).await?
                .ok_or_else(|| anyhow::anyhow!("Missing real compilation lease"))?;
            ensure!(lease.context.roslyn.is_some(), "Actual v5 companion required");
            let context = lease.context.clone();
            // This fixture owns the quiescent global child. Actual termination
            // and replacement exercise a same-code fresh process, not a forged
            // receipt or changed executable path.
            {
                let mut guard = get_or_spawn_sidecar().lock().map_err(|error| anyhow::anyhow!("{error}"))?;
                guard.as_mut().unwrap().stop()?;
                *guard = None;
            }
            ensure!(current_parser_context_for_project(&root)? == context,
                "Same loaded code and project compiler inputs must remain semantically compatible");
            ensure!(extract_vb_in_batch(&lease, &path, source).is_err(),
                "Semantic code equality cannot authorize a new child with no warmed compilation");
            drop(lease);
            let fresh = prepare_ingestion_batch(&root, std::slice::from_ref(&path), Some(inventory), None, None, None).await?.unwrap();
            let actual = extract_vb_in_batch(&fresh, &path, source)?;
            ensure!(actual.2.is_verified() && actual.0.iter().any(|s| s.name == "RepreparedAfterRestart"));
            Ok::<(), anyhow::Error>(())
        });
        drop(runtime);
        tested
    })();
    let stopped = shutdown_idle_parser();
    drop(owner);
    let cleanup = owned.close();
    result?;
    stopped?;
    cleanup?;
    Ok(())
}

/// A configuration-only update must rebuild the warmed semantic compilation,
/// even when the complete admitted VB inventory and every source byte agree.
#[cfg(windows)]
#[test]
#[ignore = "requires v5 ENGRAM_VB_SIDECAR_TEST_PATH and installed .NET Framework 4.8 references"]
fn real_vb_project_import_change_rebuilds_unchanged_sources_and_invalidates_old_metadata() -> Result<()> {
    let binary = PathBuf::from(std::env::var_os("ENGRAM_VB_SIDECAR_TEST_PATH")
        .ok_or_else(|| anyhow::anyhow!("Set the explicitly built companion path"))?);
    let owned = tempfile::tempdir()?;
    let mut client = None;
    let result = (|| -> Result<()> {
        let root = std::fs::canonicalize(owned.path())?;
        let config = root.join("web.config");
        let configuration = |namespace: &str| format!(
            "<configuration><system.web><compilation targetFramework=\"4.8\" defaultLanguage=\"vb\">
             <assemblies><add assembly=\"System.Data.Linq, Version=4.0.0.0, Culture=neutral, PublicKeyToken=b77a5c561934e089\"/></assemblies></compilation>
             <pages><namespaces><clear/><add namespace=\"{namespace}\"/></namespaces></pages>
             </system.web><system.codedom><compilers><compiler language=\"vb;vbs;visualbasic;vbscript\" extension=\".vb\"
             type=\"Microsoft.CodeDom.Providers.DotNetCompilerPlatform.VBCodeProvider, Microsoft.CodeDom.Providers.DotNetCompilerPlatform, Version=4.1.0.0, Culture=neutral, PublicKeyToken=31bf3856ad364e35\"
             compilerOptions=\"/optioninfer+\"><providerOption name=\"UseAspNetSettings\" value=\"true\"/>
             </compiler></compilers></system.codedom></configuration>");
        std::fs::write(&config, configuration("BeforeImports"))?;
        let declarations = "Namespace BeforeImports\nPublic Class Row\nPublic Property Code As Integer\nEnd Class\nEnd Namespace\nNamespace AfterImports\nPublic Class Row\nPublic Property Code As Integer\nEnd Class\nEnd Namespace\n";
        let source = "Public Class Consumer\nPublic Function Read(value As Row) As Integer\nReturn value.Code\nEnd Function\nEnd Class\n";
        let peer = root.join("Rows.vb");
        let path = root.join("Consumer.vb");
        std::fs::write(&peer, declarations)?;
        std::fs::write(&path, source)?;
        let inventory = [engram_core::RelPath::new("Rows.vb"), engram_core::RelPath::new("Consumer.vb")];
        let changed = [peer.to_string_lossy().into_owned(), path.to_string_lossy().into_owned()];
        ensure_sidecar_at(&mut client, &binary)?;
        let before = project_parser_context_locked(&mut client, &root)?;
        let compiler = before.compiler_context.as_ref().ok_or_else(|| anyhow::anyhow!("Missing compiler context"))?;
        ensure!(compiler.mode == "framework_only" && compiler.reference_bytes > 0,
            "This fixture requires real installed .NET Framework metadata, not source-only fallback: {compiler:?}");
        ensure!(project_parser_context_locked(&mut client, &root)? == before,
            "Repeated discovery of unchanged inputs must be stable");
        let plan_request = serde_json::json!({"cmd":"plan_refresh", "project_root":root,
            "files":changed, "paths":changed, "compilation_policy":before.compilation,
            "compiler_context":before.compiler_context});
        let planned = client.as_mut().unwrap().transport.request(transport::encode_request(&plan_request)?,
            64 * 1024, Duration::from_secs(60))?;
        let planned: SidecarResponse = serde_json::from_str(&planned)?;
        ensure!(planned.error.is_none() && !planned.compilation_error);
        let plan = planned.compilation_plan.ok_or_else(|| anyhow::anyhow!("Missing real framework compilation plan"))?;
        let (retained, scratch) = plan.charges(&before.compilation)?;
        ensure!(plan.reference_bytes == compiler.reference_bytes && plan.compiler_context == before.compiler_context
            && retained > before.compilation.charge(plan.total_source_bytes, plan.tree_count)?
            && scratch >= compiler.reference_bytes,
            "Real framework references must be admitted in both retained and transient compilation memory");
        let budget = Arc::new(engram_core::memory::MemoryBudget::new(0));
        refresh_project_admitted(&mut client, &root, &inventory, &changed, &before, budget.clone())?;
        let first = parse_via_sidecar(client.as_mut().unwrap(), &path, source, &before, Some(&root))
            .map_err(|error| anyhow::anyhow!("Initial configured parse failed: {error:?}"))?;
        let property_target = |edges: &[ExtractedEdge], target: &str| edges.iter().any(|edge|
            edge.kind == "calls" && edge.source_name == "Consumer.Read" && edge.target_name == target
                && edge.metadata.as_ref().is_some_and(|metadata| metadata.get("via").is_some_and(|via| via == "property_get")));
        ensure!(property_target(&first.1, "BeforeImports.Row.Code")
            && !property_target(&first.1, "AfterImports.Row.Code"), "Configured namespace must choose the actual source property");
        let metadata = serde_json::to_value(&first.2)?;
        ensure!(before.matches_metadata(&metadata));
        // Remove the old global import and add its namesake in another source
        // namespace. Neither VB file is edited or reported as changed.
        std::fs::write(&config, configuration("AfterImports"))?;
        let after = project_parser_context_locked(&mut client, &root)?;
        ensure!(after != before && !after.matches_metadata(&metadata),
            "A project configuration change must invalidate old source provenance");
        // The old immutable snapshot can finish extraction, but its publication
        // fence must revalidate it. A new context cannot borrow that warm cache.
        ensure!(matches!(parse_via_sidecar(client.as_mut().unwrap(), &path, source, &after, Some(&root)),
            Err(SidecarParseError::Identity)), "New configuration must not authorize an old warm compilation");
        refresh_project_admitted(&mut client, &root, &inventory, &[], &after, budget)?;
        let reparsed = parse_via_sidecar(client.as_mut().unwrap(), &path, source, &after, Some(&root))
            .map_err(|error| anyhow::anyhow!("Configuration-only refresh failed: {error:?}"))?;
        ensure!(property_target(&reparsed.1, "AfterImports.Row.Code")
            && !property_target(&reparsed.1, "BeforeImports.Row.Code"),
            "Empty changed paths must still rebuild peer binding after compiler inputs change");
        ensure!(std::fs::read_to_string(&peer)? == declarations && std::fs::read_to_string(&path)? == source);
        ensure!(client.as_ref().unwrap().cached_compiler_context == after.compiler_context);
        Ok(())
    })();
    let stopped = client.as_mut().map(Sidecar::stop).transpose();
    drop(client);
    let cleanup = owned.close();
    result?;
    stopped?;
    cleanup?;
    Ok(())
}
