#![allow(clippy::unwrap_used)]
//! `vector_search` must report a real similarity and a usable doc_id.
//!
//! The LanceDB query never set a distance type, so it used the default —
//! SQUARED L2. The handler then reported `1.0 - _distance` as "similarity".
//! For the unit-normalised vectors this codebase produces, squared L2 is
//! `2 - 2·cos`, which makes the reported number `2·cos - 1`: it crosses zero
//! at cos 0.5 and goes negative below that. Since text embeddings put
//! genuinely related content around cos 0.4-0.8, the tool routinely printed
//! zero and negative "similarity" for its own best hits.
//!
//! Ranking was never wrong (the transform is monotonic, and hybrid search
//! fuses by RANK via RRF), which is why this survived — but the numbers the
//! tool prints are the whole product for a tool whose job is scoring.
//!
//! Second defect: the result lines carried no doc_id, while the truncation
//! notice told the caller to "call get_chunk(doc_id)".

use engram_core::Config;
use engram_server::AppState;
use rmcp::handler::server::tool::Parameters;
use tempfile::tempdir;

async fn setup() -> (tempfile::TempDir, engram_server::Engram, String) {
    let tmp = tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/invoice.rs"),
        "pub fn calculate_invoice_total(lines: &[Line]) -> Money {\n    \
         lines.iter().map(|l| l.amount).sum()\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/shipping.rs"),
        "pub fn estimate_shipping_cost(weight: f64) -> Money {\n    \
         Money::from(weight * 2.5)\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/auth.rs"),
        "pub fn verify_session_token(token: &str) -> bool {\n    \
         token.starts_with(\"sess_\")\n}\n",
    )
    .unwrap();

    let cfg = Config {
        allowed_roots: vec![root.clone()],
        data_dir: root.join("engram_data"),
        max_project_files: Some(50),
        max_project_bytes: Some(1024 * 1024),
        // The projection embedder is deterministic, unit-normalised and needs
        // no network — exactly what a score-scale test wants.
        embedding_backend: "local".into(),
        max_concurrent_jobs: 2,
        ..Default::default()
    };
    std::fs::create_dir_all(&cfg.data_dir).unwrap();
    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = engram_server::Engram::new(state.clone());
    engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: root.to_string_lossy().to_string(),
            project_name: "VecScoreTest".into(),
            project_type: engram_server::models::ProjectType::General,
            wait: true,
            dedupe_by_directory: false,
        }))
        .await
        .unwrap();
    let pid = state.registry.list_projects().unwrap()[0]
        .project_id
        .clone();
    (tmp, engram, pid)
}

async fn search(engram: &engram_server::Engram, pid: &str, query: &str) -> String {
    let res = engram
        .vector_search(Parameters(engram_server::VectorSearchRequest {
            project_id: pid.to_string(),
            query: query.to_string(),
            namespace: "memory".into(),
            top_k: 10,
            use_mmr: false,
            include_path_prefixes: None,
            exclude_path_prefixes: None,
            language_filters: None,
            include_content: false,
            max_content_chars: 1200,
        }))
        .await
        .expect("vector_search must succeed");
    res.content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect::<Vec<_>>()
        .join("\n")
}

fn similarities(out: &str) -> Vec<f32> {
    out.lines()
        .filter_map(|l| l.split("similarity=").nth(1))
        .filter_map(|rest| rest.split_whitespace().next())
        .filter_map(|v| v.parse::<f32>().ok())
        .collect()
}

/// Every reported similarity must be a cosine value.
///
/// The discriminator between the two scales is the BOTTOM of the range: a
/// near-orthogonal document has cosine ~0, but `1 - squaredL2` puts it at
/// ~-1.0. Both scales agree at cosine 1.0, so the top of the range proves
/// nothing on its own.
#[tokio::test]
async fn reported_similarity_is_a_cosine_not_a_squared_distance() {
    let (_tmp, engram, pid) = setup().await;
    let out = search(&engram, &pid, "calculate the total amount of an invoice").await;

    let scores = similarities(&out);
    assert!(
        !scores.is_empty(),
        "expected vector hits, got output:\n{out}"
    );
    for s in &scores {
        assert!(
            (-1.0..=1.0).contains(s),
            "similarity {s} is outside cosine range — output:\n{out}"
        );
        assert!(
            *s > -0.5,
            "similarity {s} is far negative: unrelated content sits at cosine ~0, so \
             a value near -1 means squared-L2 is being reported as a similarity — \
             output:\n{out}"
        );
    }
    assert!(
        scores[0] > 0.1,
        "the best hit for a query that matches an indexed chunk must be clearly \
         positive; got {} in:\n{out}",
        scores[0]
    );
}

/// Querying with the exact indexed text must score at (or very near) 1.0.
/// Under squared L2 the same self-match reports 1 - 0 = 1.0 too, so this pins
/// the top of the scale while the negative-value assertion pins the bottom.
#[tokio::test]
async fn exact_text_match_scores_near_one() {
    let (_tmp, engram, pid) = setup().await;
    let out = search(
        &engram,
        &pid,
        "pub fn verify_session_token(token: &str) -> bool",
    )
    .await;
    let top = *similarities(&out)
        .first()
        .unwrap_or_else(|| panic!("no hits in:\n{out}"));
    assert!(
        top > 0.5,
        "an exact-text query should match its own chunk strongly; got {top} in:\n{out}"
    );
}

/// Results must carry the doc_id the output itself tells callers to use.
#[tokio::test]
async fn results_carry_a_doc_id_for_get_chunk() {
    let (_tmp, engram, pid) = setup().await;
    let out = search(&engram, &pid, "estimate shipping cost by weight").await;

    assert!(
        out.contains("doc_id="),
        "vector_search results must carry doc_id — the output tells callers to \
         call get_chunk(doc_id). Output:\n{out}"
    );
    let doc_id = out
        .split("doc_id=")
        .nth(1)
        .and_then(|r| r.split_whitespace().next())
        .expect("doc_id value")
        .to_string();
    assert!(!doc_id.is_empty(), "doc_id must not be blank in:\n{out}");

    // And it must actually resolve.
    engram
        .get_chunk(Parameters(engram_server::GetChunkRequest {
            project_id: pid.clone(),
            doc_id,
            namespace: "memory".into(),
            inject_rules: false,
            logical_slice: None,
        }))
        .await
        .expect("the doc_id vector_search printed must resolve via get_chunk");
}
