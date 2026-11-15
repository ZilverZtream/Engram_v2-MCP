//! Revision-bound shared guidance reaches ordinary planning without live substitution.
use anyhow::{Result, ensure};
use engram_server::services::project_policy;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

const DIRECT: &str =
    "Read [accepted save rules](docs/save.md#exceptions).\nPreserve explicit exceptions.\n";
const SHARED: &str = "---\napplyTo: maps/**\n---\nCheck layer ownership before saving.\nException: read-only previews do not save.\n";

fn write(root: &Path, path: &str, content: &str) -> Result<()> {
    let path = root.join(path);
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(path, content)?;
    Ok(())
}

struct Fixture {
    // No open Git/store owners are retained in this fixture.
    temporary: tempfile::TempDir,
    root: PathBuf,
    revision: git2::Oid,
}
impl Fixture {
    fn new(direct: &str, shared: &str) -> Result<Self> {
        let parent = std::env::var_os("ENGRAM_TEST_FIXTURE_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let temporary = tempfile::Builder::new()
            .prefix("")
            .rand_bytes(2)
            .tempdir_in(parent)?;
        let repo_root = temporary.path().join("r");
        let root = repo_root.join("Site");
        write(&root, "AGENTS.md", direct)?;
        write(&root, "docs/save.md", shared)?;
        write(
            &root,
            "maps/Save.ts",
            "export function saveLayer() { return 'saved'; }\n",
        )?;
        let revision = {
            let repo = git2::Repository::init(&repo_root)?;
            let mut index = repo.index()?;
            index.add_all(["*"], git2::IndexAddOption::DEFAULT, None)?;
            index.write()?;
            let tree = repo.find_tree(index.write_tree()?)?;
            let signature = git2::Signature::now("Fixture", "fixture@example.invalid")?;
            repo.commit(
                Some("HEAD"),
                &signature,
                &signature,
                "Accepted save guidance",
                &tree,
                &[],
            )?
        };
        Ok(Self {
            temporary,
            root,
            revision,
        })
    }
    fn resolve(&self) -> Result<project_policy::PolicyContext> {
        Ok(project_policy::resolve_at_revision(
            &self.root,
            self.revision,
            &["maps/Save.ts".into()],
            &[],
        )?
        .with_source_recovery("owned-historical-project"))
    }
}

#[test]
fn historical_shared_guidance_survives_checkout_deletion_and_retains_exact_scope_and_exceptions()
-> Result<()> {
    let fixture = Fixture::new(DIRECT, SHARED)?;
    std::fs::remove_file(fixture.root.join("docs/save.md"))?;
    write(&fixture.root, "AGENTS.md", "CURRENT_ONLY_GUIDANCE\n")?;
    let context = fixture.resolve()?;
    ensure!(context.sources.len() == 1 && context.sources[0].content == DIRECT);
    ensure!(context.linked_sources.len() == 1);
    let linked = &context.linked_sources[0];
    ensure!(linked.path == "docs/save.md" && linked.content == SHARED && !linked.truncated);
    ensure!(linked.blake3 == blake3::hash(SHARED.as_bytes()).to_hex().as_str());
    ensure!(context.source_revision.as_deref() == Some(fixture.revision.to_string().as_str()));
    ensure!(context.references[0].target_blake3.as_deref() == Some(linked.blake3.as_str()));
    ensure!(context.references[0].read_action.is_none());
    ensure!(context.references[0].fragment.as_deref() == Some("exceptions"));
    ensure!(context.render_markdown().contains(SHARED));
    ensure!(!serde_json::to_string(&context)?.contains("CURRENT_ONLY_GUIDANCE"));
    ensure!(context.instruction.contains("`Site`"));
    drop(context);
    fixture.temporary.close()?;
    Ok(())
}

#[test]
fn historical_scope_missing_and_budget_omissions_use_only_the_requested_git_tree() -> Result<()> {
    let direct =
        "Read [scope](docs/save.md), [missing](docs/missing.md), [large](docs/large.md).\n";
    let fixture = Fixture::new(direct, "---\napplyTo: Billing/**\n---\nBILLING_ONLY\n")?;
    // Commit an oversized linked document without changing the original scope.
    let large = "historical complete source\n".repeat(6000);
    write(&fixture.root, "docs/large.md", &large)?;
    let revision = {
        let repo = git2::Repository::open(fixture.temporary.path().join("r"))?;
        let mut index = repo.index()?;
        index.add_path(Path::new("Site/docs/large.md"))?;
        index.write()?;
        let tree = repo.find_tree(index.write_tree()?)?;
        let parent = repo.find_commit(fixture.revision)?;
        let sig = git2::Signature::now("Fixture", "fixture@example.invalid")?;
        repo.commit(
            Some("HEAD"),
            &sig,
            &sig,
            "Large linked source",
            &tree,
            &[&parent],
        )?
    };
    // All three are readable and apparently applicable today; none may replace Git evidence.
    for path in ["docs/save.md", "docs/missing.md", "docs/large.md"] {
        write(&fixture.root, path, "CURRENT_ONLY_REPLACEMENT\n")?;
    }
    let context = project_policy::resolve_at_revision(
        &fixture.root,
        revision,
        &["maps/Save.ts".into()],
        &[],
    )?;
    ensure!(context.linked_sources.is_empty());
    for (path, status) in [
        ("docs/save.md", "linked_document_outside_declared_scope"),
        ("docs/missing.md", "linked_document_unavailable"),
        ("docs/large.md", "linked_document_unavailable"),
    ] {
        ensure!(
            context
                .references
                .iter()
                .any(|r| r.path.as_deref() == Some(path) && r.status == status),
            "missing {path}: {context:?}"
        );
    }
    ensure!(context.reference_coverage == "incomplete");
    ensure!(!serde_json::to_string(&context)?.contains("CURRENT_ONLY_REPLACEMENT"));
    ensure!(
        context
            .notes
            .iter()
            .any(|note| note.contains("remaining read limit"))
    );
    drop(context);
    fixture.temporary.close()?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revision_pinned_change_set_returns_committed_shared_guidance_in_json_and_markdown()
-> Result<()> {
    use engram_core::Config;
    use engram_server::{AppState, Engram};
    let fixture = Fixture::new(DIRECT, SHARED)?;
    let (state, _) = AppState::new(Config {
        data_dir: fixture.temporary.path().join("data"),
        allowed_roots: vec![fixture.root.clone()],
        embedding_backend: "fts_only".into(),
        llm_backend: "none".into(),
        ..Default::default()
    })?;
    let engram = Engram::new(state);
    let indexed = engram.handle_index_project(serde_json::from_value(json!({
        "directory":fixture.root,"project_name":"Historical linked policy", "project_type":"general", "wait":true
    }))?).await.map_err(|e| anyhow::anyhow!("index fixture: {e:?}"))?;
    let text = &indexed.content[0].as_text().unwrap().text;
    let pid = text
        .split("project_id: ")
        .nth(1)
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .trim();
    std::fs::remove_file(fixture.root.join("docs/save.md"))?;
    write(&fixture.root, "AGENTS.md", "CURRENT_ONLY_GUIDANCE\n")?;
    for output_json in [true, false] {
        let result = engram.handle_get_change_set(serde_json::from_value(json!({
            "project_id":pid,"story":"Update saveLayer ownership checks", "concepts":["saveLayer"],
            "as_of_rev":fixture.revision.to_string(),"output_json":output_json,"max_tokens":32768
        }))?).await.map_err(|e| anyhow::anyhow!("change set: {e:?}"))?;
        let text = &result.content[0].as_text().unwrap().text;
        ensure!(!text.contains("CURRENT_ONLY_GUIDANCE"));
        if output_json {
            let value: Value = serde_json::from_str(text)?;
            let policy = &value["project_policy_sources"];
            ensure!(policy["source_revision"] == fixture.revision.to_string());
            ensure!(policy["linked_sources"][0]["content"] == SHARED);
            ensure!(
                policy["linked_sources"][0]["blake3"]
                    == blake3::hash(SHARED.as_bytes()).to_hex().to_string()
            );
            ensure!(policy["sources"][0]["content"] == DIRECT);
            ensure!(policy["references"][0].get("read_action").is_none());
        } else {
            ensure!(text.contains(SHARED) && text.contains(&fixture.revision.to_string()));
        }
    }
    drop(engram);
    fixture.temporary.close()?;
    Ok(())
}
