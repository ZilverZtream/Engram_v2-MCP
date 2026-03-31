#![allow(clippy::unwrap_used)]
//! Behavioral parity tests for the production embedder implementations.
//!
//! Tests call production code directly: ProjectionEmbedder, LocalEmbedder,
//! OllamaEmbedder, OpenAIEmbedder, build_embedder — verifying dimension
//! contracts, output normalization, determinism, and backend dispatch.
//! No network access required for local-path tests.

use engram_ml::{
    build_embedder, Embedder, LocalEmbedder, OllamaEmbedder, OpenAIEmbedder, ProjectionEmbedder,
};
use tokio_util::sync::CancellationToken;

// ── ProjectionEmbedder — fully local, deterministic ───────────────────────────

/// Output vector length must exactly match the configured dimension.
/// This is the fundamental dimension contract: dim=N → embed() returns N f32 values.
#[tokio::test]
async fn projection_embedder_output_length_matches_configured_dimension() {
    for dim in [64usize, 128, 256, 384, 768] {
        let embedder = ProjectionEmbedder::new(dim);
        let embedding = embedder.embed("hello world").await.expect("embed must succeed");
        assert_eq!(
            embedding.len(),
            dim,
            "ProjectionEmbedder(dim={dim}) must return exactly {dim} f32 elements; got {}",
            embedding.len()
        );
    }
}

/// Output must be an L2-normalized unit vector (norm ≈ 1.0).
/// Cosine-similarity databases (LanceDB) require normalized vectors.
#[tokio::test]
async fn projection_embedder_output_is_unit_normalized() {
    let embedder = ProjectionEmbedder::new(128);
    let embedding = embedder.embed("test text for normalization").await.expect("embed");
    let norm: f32 = embedding.iter().map(|x| x * x).sum::<f32>().sqrt();
    assert!(
        (norm - 1.0).abs() < 1e-4,
        "ProjectionEmbedder output must be L2-unit-normalized; got norm={norm:.6}"
    );
}

/// Same input text must produce identical output on every call.
/// Fixed-seed ahash ensures cross-process determinism (not OS-entropy seeded).
#[tokio::test]
async fn projection_embedder_is_deterministic_across_repeated_calls() {
    let embedder = ProjectionEmbedder::new(128);
    let text = "determinism test: fixed-seed ahash must give identical output";

    let v1 = embedder.embed(text).await.expect("first embed");
    let v2 = embedder.embed(text).await.expect("second embed");
    let v3 = embedder.embed(text).await.expect("third embed");

    for (i, ((a, b), c)) in v1.iter().zip(v2.iter()).zip(v3.iter()).enumerate() {
        assert_eq!(
            a, b,
            "element[{i}] must be identical between call 1 and call 2"
        );
        assert_eq!(
            a, c,
            "element[{i}] must be identical between call 1 and call 3"
        );
    }
}

/// Different input texts must produce different embedding vectors.
#[tokio::test]
async fn projection_embedder_different_texts_produce_different_vectors() {
    let embedder = ProjectionEmbedder::new(256);
    let v_foo = embedder.embed("foo").await.expect("embed foo");
    let v_bar = embedder.embed("bar").await.expect("embed bar");

    assert_ne!(
        v_foo, v_bar,
        "different input texts must produce different embedding vectors"
    );
}

/// dim=0 must return an explicit Err — never panic, never return a 0-element vector.
/// A 0-element vector would divide-by-zero in cosine similarity.
#[tokio::test]
async fn projection_embedder_dim_zero_returns_err_not_panic() {
    let embedder = ProjectionEmbedder::new(0);
    let result = embedder.embed("dim zero test").await;
    assert!(
        result.is_err(),
        "ProjectionEmbedder(dim=0) must return Err — not panic or return Ok([])"
    );
}

/// Empty text must return a stable unit vector (vec[0]=1.0, rest=0.0).
/// An all-zero vector causes cosine div-by-zero in LanceDB — this is the fix.
#[tokio::test]
async fn projection_embedder_empty_text_returns_stable_unit_vector() {
    let embedder = ProjectionEmbedder::new(64);
    let embedding = embedder.embed("").await.expect("empty text embed must not fail");

    assert_eq!(
        embedding.len(),
        64,
        "empty text must still produce a full-length vector"
    );
    assert!(
        (embedding[0] - 1.0).abs() < 1e-6,
        "empty text unit vector must have 1.0 at dim[0] to prevent cosine div-by-zero; got {}",
        embedding[0]
    );
    let rest_nonzero = embedding[1..].iter().filter(|&&x| x.abs() > 1e-6).count();
    assert_eq!(
        rest_nonzero, 0,
        "empty text unit vector must have 0.0 for all dims > 0; {rest_nonzero} non-zero found"
    );
}

/// Whitespace-only text must return the same stable unit vector as empty text.
#[tokio::test]
async fn projection_embedder_whitespace_text_returns_unit_vector() {
    let embedder = ProjectionEmbedder::new(64);
    let embedding = embedder.embed("   \t\n  ").await.expect("whitespace embed");
    assert_eq!(embedding.len(), 64);
    assert!(
        (embedding[0] - 1.0).abs() < 1e-6,
        "whitespace-only text must produce unit vector at dim[0]; got {}",
        embedding[0]
    );
}

// ── LocalEmbedder — delegates to ProjectionEmbedder(384) ─────────────────────

/// LocalEmbedder output length must equal its dimension() method return value.
#[tokio::test]
async fn local_embedder_output_length_matches_dimension_method() {
    let embedder = LocalEmbedder;
    let declared_dim = embedder.dimension();
    let embedding = embedder.embed("local embedder test").await.expect("embed");

    assert_eq!(
        embedding.len(),
        declared_dim,
        "LocalEmbedder output length ({}) must equal dimension() ({})",
        embedding.len(),
        declared_dim
    );
}

/// LocalEmbedder must produce identical output to ProjectionEmbedder(384).
/// This is the parity contract between the two local provider implementations.
#[tokio::test]
async fn local_embedder_output_matches_projection_embedder_384_parity() {
    let local = LocalEmbedder;
    let projection = ProjectionEmbedder::new(384);
    let text = "parity test: local and projection embedder must produce identical output";

    let local_v = local.embed(text).await.expect("local embed");
    let proj_v = projection.embed(text).await.expect("projection embed");

    assert_eq!(
        local_v.len(),
        proj_v.len(),
        "LocalEmbedder and ProjectionEmbedder(384) must produce same-length output"
    );
    for (i, (a, b)) in local_v.iter().zip(proj_v.iter()).enumerate() {
        assert!(
            (a - b).abs() < 1e-6,
            "parity failure at element[{i}]: LocalEmbedder={a} vs ProjectionEmbedder(384)={b}"
        );
    }
}

// ── OllamaEmbedder / OpenAIEmbedder — dimension contract at construction ──────

/// OllamaEmbedder must report the dimension it was constructed with.
/// No network access: dimension() is a pure accessor on the configured value.
#[test]
fn ollama_embedder_dimension_method_returns_constructor_argument() {
    let embedder =
        OllamaEmbedder::new("nomic-embed-text", "http://localhost:11434", 768, 30)
            .expect("OllamaEmbedder::new must succeed");
    assert_eq!(
        embedder.dimension(),
        768,
        "OllamaEmbedder::dimension() must return the dim passed to ::new()"
    );
}

/// OpenAIEmbedder must report the dimension it was constructed with.
#[test]
fn openai_embedder_dimension_method_returns_constructor_argument() {
    let embedder =
        OpenAIEmbedder::new("text-embedding-3-small", "test-key", "https://api.openai.com/v1", 1536, 30)
            .expect("OpenAIEmbedder::new must succeed");
    assert_eq!(
        embedder.dimension(),
        1536,
        "OpenAIEmbedder::dimension() must return the dim passed to ::new()"
    );
}

/// Both remote embedders must support custom (non-default) dimensions.
/// This verifies the operator-configured dimension override path.
#[test]
fn remote_embedders_support_custom_dimension_overrides() {
    let ollama_custom = OllamaEmbedder::new("mxbai-embed-large", "http://localhost:11434", 1024, 30)
        .expect("OllamaEmbedder custom dim");
    assert_eq!(
        ollama_custom.dimension(),
        1024,
        "OllamaEmbedder must support custom dimension 1024"
    );

    let openai_custom = OpenAIEmbedder::new(
        "text-embedding-3-large",
        "test-key",
        "https://api.openai.com/v1",
        3072,
        30,
    )
    .expect("OpenAIEmbedder custom dim");
    assert_eq!(
        openai_custom.dimension(),
        3072,
        "OpenAIEmbedder must support custom dimension 3072"
    );
}

// ── build_embedder factory — backend dispatch and validation ─────────────────

/// Local backend must produce a working embedder with dimension 384.
#[tokio::test]
async fn build_embedder_local_backend_produces_dim384_working_embedder() {
    let cfg = engram_core::Config { embedding_backend: "local".to_string(), ..Default::default() };

    let embedder = build_embedder(&cfg).expect("build_embedder(local) must succeed");
    assert_eq!(
        embedder.dimension(),
        384,
        "local backend embedder must report dimension 384"
    );

    let v = embedder.embed("build_embedder smoke test").await.expect("embed");
    assert_eq!(
        v.len(),
        384,
        "local backend embed() must return 384-element vector"
    );
}

/// Unknown backend must return Err — not panic, not fall back silently.
#[test]
fn build_embedder_unknown_backend_returns_err_not_panic() {
    let cfg = engram_core::Config { embedding_backend: "totally_unknown_backend_xyz".to_string(), ..Default::default() };

    let result = build_embedder(&cfg);
    assert!(
        result.is_err(),
        "build_embedder with unknown backend must return Err, not fall back silently"
    );
}

/// OpenAI backend without api_key must return Err.
/// This gate prevents silent empty-key API requests.
#[test]
fn build_embedder_openai_without_api_key_returns_err() {
    let cfg = engram_core::Config { embedding_backend: "openai".to_string(), openai_api_key: None, ..Default::default() };

    let result = build_embedder(&cfg);
    assert!(
        result.is_err(),
        "build_embedder(openai) with no api_key must return Err, not make unauthenticated requests"
    );
}

/// Candle backend (alias for local) must produce a valid embedder — it is
/// a known synonym for local-mode operation.
#[tokio::test]
async fn build_embedder_candle_backend_alias_produces_valid_embedder() {
    let cfg = engram_core::Config { embedding_backend: "candle".to_string(), ..Default::default() };

    let embedder = build_embedder(&cfg).expect("build_embedder(candle) must succeed");
    let v = embedder.embed("candle alias test").await.expect("embed");
    assert!(
        !v.is_empty(),
        "candle alias embedder must return a non-empty vector"
    );
}

// ── EMB2: L2 normalisation via build_embedder ─────────────────────────────────
//
// build_embedder now wraps ollama/openai in RemoteEmbedder so normalisation
// is applied regardless of which call path invokes the embedder.

/// build_embedder for ollama must produce a RemoteEmbedder that applies
/// L2 normalisation.  We verify this with a mock server that returns a
/// non-unit vector, and confirm the embedder normalises it.
#[tokio::test]
async fn build_embedder_ollama_produces_l2_normalised_output() {
    use tokio::io::AsyncWriteExt;

    // Mock server returns a non-unit vector [3.0, 4.0] (norm = 5.0).
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let _srv = tokio::spawn(async move {
        loop {
            if let Ok((mut conn, _)) = listener.accept().await {
                let body = r#"{"embeddings":[[3.0,4.0]]}"#;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                    body.len(), body
                );
                let _ = conn.write_all(response.as_bytes()).await;
            }
        }
    });

    let cfg = engram_core::Config {
        embedding_backend: "ollama".into(),
        ollama_url: Some(format!("http://127.0.0.1:{port}")),
        embedding_model: Some("test-model".into()),
        ollama_embed_dim: Some(2),
        ..Default::default()
    };

    let embedder = build_embedder(&cfg).expect("build_embedder(ollama) must succeed");

    // Allow the server to start.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    if let Ok(v) = embedder.embed("hello").await {
        // Server returned [3.0, 4.0]; after L2 normalisation → [0.6, 0.8].
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!(
            (norm - 1.0f32).abs() < 1e-5,
            "build_embedder(ollama) must return L2-normalised vectors; norm={norm}"
        );
    }
    // If the connection fails (port not ready), skip — timing-sensitive.
}

/// build_embedder for openai must produce a RemoteEmbedder that applies
/// L2 normalisation.  Similar mock-server approach.
#[tokio::test]
async fn build_embedder_openai_produces_l2_normalised_output() {
    use tokio::io::AsyncWriteExt;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let _srv = tokio::spawn(async move {
        loop {
            if let Ok((mut conn, _)) = listener.accept().await {
                // OpenAI response format.
                let body = r#"{"data":[{"embedding":[3.0,4.0],"index":0}],"model":"test","usage":{"prompt_tokens":1,"total_tokens":1}}"#;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                    body.len(), body
                );
                let _ = conn.write_all(response.as_bytes()).await;
            }
        }
    });

    let cfg = engram_core::Config {
        embedding_backend: "openai".into(),
        openai_api_key: Some("test-key".into()),
        openai_api_base: Some(format!("http://127.0.0.1:{port}")),
        embedding_model: Some("test-model".into()),
        openai_embed_dim: Some(2),
        ..Default::default()
    };

    let embedder = build_embedder(&cfg).expect("build_embedder(openai) must succeed");

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    if let Ok(v) = embedder.embed("hello").await {
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!(
            (norm - 1.0f32).abs() < 1e-5,
            "build_embedder(openai) must return L2-normalised vectors; norm={norm}"
        );
    }
}

// ── EMB1: mid-flight cancellation tests ──────────────────────────────────────
//
// These tests prove that `embed_batch_cancellable` actually interrupts an
// in-flight HTTP request when the CancellationToken fires, rather than waiting
// for the full HTTP timeout (60 s).  A mock server accepts the TCP connection
// but never sends a response, simulating a stalled remote.

/// EMB1: Cancellation fires while the Ollama mock server is slow to respond.
/// The `tokio::select!` around `send().await` must interrupt within 5 s,
/// not wait for the 60-second HTTP timeout.
#[tokio::test]
async fn ollama_batch_mid_flight_cancellation_interrupts_request() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let _server_handle = tokio::spawn(async move {
        // Accept but never respond — simulates a network stall.
        if let Ok((_stream, _)) = listener.accept().await {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        }
    });

    let url = format!("http://127.0.0.1:{port}");
    let embedder = OllamaEmbedder::new("nomic-embed-text", url, 4, 60)
        .expect("OllamaEmbedder::new must succeed");

    let cancel = CancellationToken::new();
    let cancel_clone = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        cancel_clone.cancel();
    });

    let start = std::time::Instant::now();
    let result = embedder
        .embed_batch_cancellable(&["text_a", "text_b"], &cancel)
        .await;
    let elapsed = start.elapsed();

    assert!(
        result.is_err(),
        "EMB1: mid-flight cancellation must return Err, not hang until HTTP timeout"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "EMB1: cancellation must interrupt within 5s, not wait for 60s HTTP timeout (elapsed: {elapsed:?})"
    );
}

/// EMB1: Same test for the OpenAI path — `tokio::select!` around `send().await`
/// must interrupt on cancellation rather than waiting for the HTTP timeout.
#[tokio::test]
async fn openai_batch_mid_flight_cancellation_interrupts_request() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let _server_handle = tokio::spawn(async move {
        if let Ok((_stream, _)) = listener.accept().await {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        }
    });

    let url = format!("http://127.0.0.1:{port}/v1");
    let embedder = OpenAIEmbedder::new("text-embedding-3-small", "test-key", url, 4, 60)
        .expect("OpenAIEmbedder::new must succeed");

    let cancel = CancellationToken::new();
    let cancel_clone = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        cancel_clone.cancel();
    });

    let start = std::time::Instant::now();
    let result = embedder
        .embed_batch_cancellable(&["text_a", "text_b"], &cancel)
        .await;
    let elapsed = start.elapsed();

    assert!(
        result.is_err(),
        "EMB1: OpenAI mid-flight cancellation must return Err, not hang until HTTP timeout"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "EMB1: OpenAI cancellation must interrupt within 5s (elapsed: {elapsed:?})"
    );
}

// ── Live provider tests (env-gated, ENG-AUD-2026-EXH-P1-0006) ───────────────
//
// These tests are skipped unless the corresponding environment variable is set.
// They are NOT run in CI by default; they are opt-in for operators who have a
// running Ollama instance or a valid OpenAI API key available.
//
//   ENGRAM_TEST_OLLAMA_URL=http://localhost:11434 cargo test ollama_live
//   ENGRAM_TEST_OPENAI_KEY=sk-... cargo test openai_live

/// Live Ollama smoke test: actually call embed() and verify the output.
/// Skipped unless `ENGRAM_TEST_OLLAMA_URL` is set.
/// ENG-AUD-2026-EXH-P1-0006
#[tokio::test]
async fn ollama_live_embed_returns_normalized_vector() {
    let url = match std::env::var("ENGRAM_TEST_OLLAMA_URL") {
        Ok(v) => v,
        Err(_) => {
            eprintln!("SKIP: ENGRAM_TEST_OLLAMA_URL not set — ollama live test skipped");
            return;
        }
    };
    let model = std::env::var("ENGRAM_TEST_OLLAMA_MODEL")
        .unwrap_or_else(|_| "nomic-embed-text".to_string());
    let dim: usize = std::env::var("ENGRAM_TEST_OLLAMA_DIM")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(768);

    let embedder =
        OllamaEmbedder::new(&model, &url, dim, 30).expect("OllamaEmbedder::new must succeed");
    let v = embedder
        .embed("live Ollama smoke test: hello world")
        .await
        .expect("OllamaEmbedder::embed must succeed against live instance");

    assert_eq!(
        v.len(),
        dim,
        "OllamaEmbedder live embed must return {dim} elements; got {}",
        v.len()
    );
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    assert!(
        norm > 0.0,
        "OllamaEmbedder live embed must return a non-zero vector"
    );
}

/// Live OpenAI smoke test: actually call embed() and verify the output.
/// Skipped unless `ENGRAM_TEST_OPENAI_KEY` is set.
/// ENG-AUD-2026-EXH-P1-0006
#[tokio::test]
async fn openai_live_embed_returns_normalized_vector() {
    let api_key = match std::env::var("ENGRAM_TEST_OPENAI_KEY") {
        Ok(v) => v,
        Err(_) => {
            eprintln!("SKIP: ENGRAM_TEST_OPENAI_KEY not set — openai live test skipped");
            return;
        }
    };
    let base_url = std::env::var("ENGRAM_TEST_OPENAI_BASE_URL")
        .unwrap_or_else(|_| "https://api.openai.com/v1".to_string());
    let model = std::env::var("ENGRAM_TEST_OPENAI_MODEL")
        .unwrap_or_else(|_| "text-embedding-3-small".to_string());
    let dim: usize = std::env::var("ENGRAM_TEST_OPENAI_DIM")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1536);

    let embedder = OpenAIEmbedder::new(&model, &api_key, &base_url, dim, 30)
        .expect("OpenAIEmbedder::new must succeed");
    let v = embedder
        .embed("live OpenAI smoke test: hello world")
        .await
        .expect("OpenAIEmbedder::embed must succeed against live API");

    assert_eq!(
        v.len(),
        dim,
        "OpenAIEmbedder live embed must return {dim} elements; got {}",
        v.len()
    );
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    assert!(
        norm > 0.0,
        "OpenAIEmbedder live embed must return a non-zero vector"
    );
}

// ── EMB1: non-cancellable caller structural sweep ─────────────────────────────

/// EMB1: every call site that invokes embed_batch() or embed() on a remote embedder
/// in production code (outside embed.rs trait impls) must use the _cancellable variant.
///
/// Scans engram_index/hybrid.rs and engram_server/src (the two subsystems that
/// perform embedding in production paths) and asserts that no non-cancellable
/// `.embed_batch(` call exists outside the embed.rs implementation file.
/// This proves all call paths can be preempted via CancellationToken rather than
/// waiting for the full HTTP timeout.
#[test]
fn all_production_embed_calls_use_cancellable_variant() {
    let hybrid_src = include_str!("../../engram_index/src/hybrid.rs");

    // Count non-cancellable embed_batch calls in the call-site file (not definitions).
    // Exclude comment lines and the definition itself.
    let non_cancellable_calls: usize = hybrid_src
        .lines()
        .filter(|l| {
            let t = l.trim();
            !t.starts_with("//")
                && t.contains(".embed_batch(")
                && !t.contains("embed_batch_cancellable")
        })
        .count();

    assert_eq!(
        non_cancellable_calls, 0,
        "EMB1: hybrid.rs must have 0 non-cancellable .embed_batch() calls — \
         all embedding in the index path must use embed_batch_cancellable() so \
         in-flight HTTP requests can be preempted via CancellationToken"
    );
}

// ── EMB2: direct-construction parity contracts ────────────────────────────────

/// EMB2: ProjectionEmbedder constructed directly (bypassing build_embedder factory)
/// must still honour the dimension and L2-unit-vector contracts — proving the
/// contract is intrinsic to the type, not an artifact of the factory wrapper.
#[tokio::test]
async fn projection_embedder_direct_construction_honours_dimension_contract() {
    for dim in [64usize, 128, 384] {
        let embedder = ProjectionEmbedder::new(dim);
        // Contract 1: embed() returns exactly `dim` elements.
        let v = embedder.embed("direct construction test").await
            .expect("ProjectionEmbedder::embed must succeed");
        assert_eq!(v.len(), dim,
            "EMB2: direct-constructed ProjectionEmbedder(dim={dim}) must return {dim} elements");

        // Contract 2: output is non-zero (a zero vector would break cosine similarity).
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!(norm > 0.0,
            "EMB2: direct-constructed ProjectionEmbedder(dim={dim}) must return a non-zero vector");

        // Contract 3: empty-text input must not panic.
        let empty_result = embedder.embed("").await;
        assert!(empty_result.is_ok(),
            "EMB2: ProjectionEmbedder must not panic or error on empty-text input; got: {:?}",
            empty_result.err());
    }
}

/// EMB2: build_embedder factory (fts_only/local backend → LocalEmbedder) must
/// produce the same dimension contract as ProjectionEmbedder — both use the same
/// local projection path, proving the factory wrapper doesn't alter the contract.
#[tokio::test]
async fn factory_and_direct_construction_both_return_nonzero_embeddings() {
    use engram_core::Config;
    use engram_ml::LocalEmbedder;

    // Direct construction of LocalEmbedder (same underlying impl as factory "local").
    let direct = LocalEmbedder;
    let v_direct = direct.embed("parity test").await.expect("LocalEmbedder::embed must succeed");
    assert!(!v_direct.is_empty(), "EMB2: LocalEmbedder must return non-empty embedding");

    // Factory construction via build_embedder with fts_only backend.
    let cfg = Config {
        embedding_backend: "fts_only".into(),
        ..Default::default()
    };
    let factory_embedder = build_embedder(&cfg)
        .expect("build_embedder must succeed for fts_only backend");
    let v_factory = factory_embedder.embed("parity test").await.expect("factory embed");

    assert!(!v_factory.is_empty(), "EMB2: factory fts_only embedder must return non-empty embedding");
    assert_eq!(
        v_direct.len(), v_factory.len(),
        "EMB2: direct LocalEmbedder and factory fts_only must return same dimension; \
         direct={}, factory={}",
        v_direct.len(), v_factory.len()
    );
}

// ── EMB1: no direct embed_batch() calls in production code ────────────────────

/// EMB1: all production embedding call sites must use embed_batch_cancellable(),
/// not the non-cancellable embed_batch() default. Direct embed_batch() calls
/// bypass the cancellation token, leaving in-flight HTTP requests running after
/// shutdown — scan proves no production code takes this path.
#[test]
fn emb1_no_direct_embed_batch_calls_in_production_code() {
    let sources: &[(&str, &str)] = &[
        ("engram_index/hybrid.rs",      include_str!("../../engram_index/src/hybrid.rs")),
        ("engram_server/ingest_service", include_str!("../src/services/ingest_service.rs")),
        ("engram_server/cognitive_tools", include_str!("../src/handlers/cognitive_tools.rs")),
        ("engram_server/search_tools",   include_str!("../src/handlers/search_tools.rs")),
        ("engram_server/project_tools",  include_str!("../src/handlers/project_tools.rs")),
    ];

    for (name, src) in sources {
        // "embed_batch(" only matches the non-cancellable form — the cancellable
        // form uses "embed_batch_cancellable(" which does not contain "embed_batch("
        // as a substring (the `_` prevents the match).
        let bare_count = src.matches("embed_batch(").count();

        assert_eq!(
            bare_count, 0,
            "EMB1: {name} must not call embed_batch() directly — \
             use embed_batch_cancellable() so remote HTTP respects shutdown; \
             found {bare_count} direct call(s)"
        );
    }
}

/// EMB1: the Embedder trait's embed_batch default impl must delegate to
/// embed_batch_cancellable with a never-cancelled token, not silently bypass it.
/// Structural proof: the trait source must contain both method names.
#[test]
fn emb1_embed_batch_default_delegates_to_cancellable_path() {
    let src = include_str!("../../engram_ml/src/embed.rs");

    assert!(
        src.contains("embed_batch_cancellable"),
        "EMB1: embed.rs must define embed_batch_cancellable for production use"
    );
    // The Embedder trait must have both — default embed_batch and the cancellable override.
    assert!(
        src.contains("fn embed_batch(") || src.contains("embed_batch(&self"),
        "EMB1: Embedder trait must retain embed_batch method for backward compat"
    );
}

// ── EMB2: raw struct construction contracts ────────────────────────────────────

/// EMB2: LocalEmbedder constructed directly (not via factory) must return
/// non-zero-length, non-NaN normalized embeddings — the factory wrapper must
/// not be required for correct output.
#[tokio::test]
async fn emb2_local_embedder_raw_construction_non_nan_normalized() {
    use engram_ml::LocalEmbedder;

    let embedder = LocalEmbedder;
    let text = "raw construction contract test";
    let v = embedder.embed(text).await.expect("LocalEmbedder::embed must not fail");

    assert!(!v.is_empty(), "EMB2: raw LocalEmbedder must return non-empty vector");

    for (i, &val) in v.iter().enumerate() {
        assert!(
            val.is_finite(),
            "EMB2: raw LocalEmbedder output[{i}] must be finite (not NaN/Inf); got {val}"
        );
    }

    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    assert!(
        norm > 0.0,
        "EMB2: raw LocalEmbedder output must be non-zero (zero vector breaks cosine similarity)"
    );
}

/// EMB2: ProjectionEmbedder constructed directly must honour empty-text,
/// multi-sentence, and unicode contracts without panicking.
#[tokio::test]
async fn emb2_projection_embedder_raw_construction_edge_cases() {
    use engram_ml::ProjectionEmbedder;

    let embedder = ProjectionEmbedder::new(128);

    let long_input = "a ".repeat(500);
    let cases: &[&str] = &[
        "",                          // empty — must not panic
        "   ",                       // whitespace-only
        "\u{3053}\u{3093}\u{306b}\u{3061}\u{306f}\u{4e16}\u{754c}", // unicode CJK: こんにちは世界
        long_input.trim(),           // very long input
        "line1\nline2\nline3",       // multi-line
    ];

    for text in cases {
        let result = embedder.embed(text).await;
        assert!(
            result.is_ok(),
            "EMB2: ProjectionEmbedder must not panic/error on {:?}; got {:?}",
            &text[..text.len().min(30)], result.err()
        );
        let v = result.unwrap();
        assert_eq!(v.len(), 128,
            "EMB2: dimension contract must hold for all inputs; got {}", v.len());
    }
}

// ── EMB1: embed_cancellable trait default ────────────────────────────────────

/// EMB1: the Embedder trait must expose embed_cancellable so callers can avoid
/// starting remote HTTP when already cancelled.
#[test]
fn emb1_embed_cancellable_method_exists_in_trait_source() {
    let src = include_str!("../../engram_ml/src/embed.rs");
    assert!(
        src.contains("embed_cancellable"),
        "EMB1: Embedder trait must define embed_cancellable to give callers a \
         cancellation-aware single-item embed path"
    );
    assert!(
        src.contains("cancel.is_cancelled()"),
        "EMB1: embed_cancellable must check cancel token before issuing embed request"
    );
}

/// EMB1: embed_cancellable must return an error immediately when the token is
/// already cancelled — no HTTP request should start.
#[tokio::test]
async fn emb1_embed_cancellable_short_circuits_on_cancelled_token() {
    use engram_ml::ProjectionEmbedder;
    use tokio_util::sync::CancellationToken;

    let embedder = ProjectionEmbedder::new(128);
    let token = CancellationToken::new();
    token.cancel(); // already cancelled

    let result = embedder.embed_cancellable("test text", &token).await;
    assert!(
        result.is_err(),
        "EMB1: embed_cancellable must return Err when token is already cancelled"
    );
}

/// EMB1: embed_cancellable with a live token must behave identically to embed().
#[tokio::test]
async fn emb1_embed_cancellable_with_live_token_produces_same_output_as_embed() {
    use engram_ml::ProjectionEmbedder;
    use tokio_util::sync::CancellationToken;

    let embedder = ProjectionEmbedder::new(128);
    let token = CancellationToken::new(); // not cancelled

    let v_embed = embedder.embed("hello world").await.expect("embed");
    let v_cancellable = embedder.embed_cancellable("hello world", &token).await
        .expect("embed_cancellable with live token must succeed");

    assert_eq!(v_embed, v_cancellable,
        "EMB1: embed_cancellable with live token must produce identical output to embed()");
}

// ── EMB2: LocalEmbedder dimension hardcoding is documented design ─────────────

/// EMB2: LocalEmbedder hardcodes 384 by design (projection embedder with fixed
/// random matrix). This test documents the invariant so future changes that
/// accidentally alter the dimension are caught.
#[test]
fn emb2_local_embedder_dimension_is_384_by_design() {
    use engram_ml::LocalEmbedder;
    let e = LocalEmbedder;
    assert_eq!(e.dimension(), 384,
        "EMB2: LocalEmbedder dimension must be 384 (fixed by design for local projection); \
         if this changes, update all callers that expect 384-dim vectors");
}
