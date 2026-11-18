//! Authored guidance remains available without becoming implementation evidence.
use anyhow::{Result, ensure};
use engram_core::Config;
use engram_server::{Engram, state::AppState};
use serde_json::{Value, json};
use std::path::PathBuf;

const DOCUMENTS: &[(&str, &str)] = &[
    (
        "AGENTS.md",
        "When ProjectBeaconToggle is disabled, previews must not create audit rows. Exception: approved administrative replay may persist an audit row.\n",
    ),
    (
        ".github/instructions/preview.instructions.md",
        "---\napplyTo: site/**\n---\nRequire ProjectBeaconToggle for preview audit writes; preserve the read-only exception.\n",
    ),
    (
        "README.md",
        "Review status: closed. ProjectBeaconToggle is a requested feature; its implementation has not been established by this document.\n",
    ),
];
struct Fixture {
    app: Engram,
    pid: String,
    owned: tempfile::TempDir,
    root: PathBuf,
}
impl Fixture {
    async fn new(implemented: bool) -> Result<Self> {
        let parent = std::env::var_os("ENGRAM_TEST_FIXTURE_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let owned = tempfile::Builder::new()
            .prefix("authority-")
            .tempdir_in(parent)?;
        let root = owned.path().join("project");
        std::fs::create_dir_all(root.join("site"))?;
        for (name, body) in DOCUMENTS {
            let path = root.join(name);
            std::fs::create_dir_all(path.parent().unwrap())?;
            std::fs::write(path, body)?;
        }
        let source = if implemented {
            "export function ProjectBeaconToggle() {\n // AGENTS.md and README.md describe intent; this is an actual declaration.\n return true;\n}\n"
        } else {
            "export const unrelated = 1;\n"
        };
        std::fs::write(root.join("site/feature.ts"), source)?;
        let (state, _) = AppState::new(Config {
            data_dir: owned.path().join("data"),
            allowed_roots: vec![root.clone()],
            embedding_backend: "fts_only".into(),
            llm_backend: "none".into(),
            ..Default::default()
        })?;
        let app = Engram::new(state);
        let indexed=app.handle_index_project(serde_json::from_value(json!({"directory":root,"project_name":"Owned documentary authority","project_type":"general","wait":true}))?).await?;
        let pid = indexed.content[0]
            .as_text()
            .unwrap()
            .text
            .split("project_id: ")
            .nth(1)
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .trim()
            .to_owned();
        Ok(Self {
            app,
            pid,
            owned,
            root,
        })
    }
    async fn ask(&self, format: &str) -> Result<String> {
        let reply=self.app.handle_ask_codebase(serde_json::from_value(json!({"project_id":self.pid,"question":"Where is ProjectBeaconToggle implemented?","output_format":format,"max_tokens":65536,"deadline_ms":1000,"depth":"deep"}))?).await?;
        ensure!(reply.is_error != Some(true));
        Ok(reply.content[0].as_text().unwrap().text.clone())
    }
    fn close(self) -> Result<()> {
        let preserved = (|| -> Result<()> {
            for (path, body) in DOCUMENTS {
                ensure!(std::fs::read_to_string(self.root.join(path))? == *body);
            }
            Ok(())
        })();
        let Self { app, owned, .. } = self;
        drop(app);
        let cleaned = owned.close();
        preserved?;
        cleaned?;
        Ok(())
    }
}
fn documentary(report: &Value) -> Vec<&Value> {
    report["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| {
            row["path"]
                .as_str()
                .is_some_and(|path| path.ends_with(".md"))
        })
        .collect()
}
fn verify_documents(report: &Value) -> Result<()> {
    let docs = documentary(report);
    ensure!(docs.len() == DOCUMENTS.len(), "{report}");
    for item in docs {
        ensure!(
            item["kind"] == "doc_section" && item["authority"] == "current_docs",
            "{item}"
        );
        ensure!(
            item["warnings"]
                .to_string()
                .contains("not executable implementation")
        );
        ensure!(
            item["warnings"]
                .to_string()
                .contains("does not establish approval")
        );
        ensure!(item["generation"] == report["snapshot"]["project_generation"]);
        ensure!(
            item["document_namespace"] == "memory"
                && item["document_id"]
                    .as_str()
                    .is_some_and(|id| !id.is_empty())
        );
    }
    Ok(())
}

#[tokio::test]
async fn guidance_and_readme_without_implementation_keep_documentary_authority_and_qualifications()
-> Result<()> {
    let fixture = Fixture::new(false).await?;
    let result = async {
        let report: Value = serde_json::from_str(&fixture.ask("json").await?)?;
        verify_documents(&report)?;
        ensure!(
            !report["evidence"]
                .as_array()
                .unwrap()
                .iter()
                .any(|row| row["kind"] == "source_code")
        );
        let guide = documentary(&report)
            .into_iter()
            .find(|row| row["path"] == "AGENTS.md")
            .unwrap();
        ensure!(
            guide["content"]
                .as_str()
                .unwrap()
                .contains("Exception: approved administrative replay")
        );
        let scoped = documentary(&report)
            .into_iter()
            .find(|row| row["path"] == ".github/instructions/preview.instructions.md")
            .unwrap();
        ensure!(
            scoped["content"]
                .as_str()
                .unwrap()
                .contains("applyTo: site/**")
        );
        Ok(())
    }
    .await;
    fixture.close()?;
    result
}

#[tokio::test]
async fn actual_typescript_implementation_keeps_code_authority_and_precedes_documentary_context()
-> Result<()> {
    let fixture = Fixture::new(true).await?;
    let result = async {
        let report: Value = serde_json::from_str(&fixture.ask("json").await?)?;
        verify_documents(&report)?;
        let rows = report["evidence"].as_array().unwrap();
        let code = rows
            .iter()
            .position(|row| row["kind"] == "source_code" && row["path"] == "site/feature.ts")
            .unwrap();
        ensure!(rows[code]["authority"] == "current_code");
        ensure!(
            rows[code]["content"]
                .as_str()
                .unwrap()
                .contains("return true")
        );
        ensure!(
            !rows[code]["warnings"]
                .to_string()
                .contains("Markdown documentation or guidance")
        );
        ensure!(
            rows.iter()
                .enumerate()
                .filter(|(_, row)| row["kind"] == "doc_section"
                    && row["path"].as_str().is_some_and(|p| p.ends_with(".md")))
                .all(|(index, _)| code < index)
        );
        ensure!(
            report["project_policy_sources"]
                .to_string()
                .contains("read-only exception")
        );
        Ok(())
    }
    .await;
    fixture.close()?;
    result
}

#[tokio::test]
async fn markdown_discloses_documentary_limits_and_original_instruction_text_remains_recoverable()
-> Result<()> {
    let fixture = Fixture::new(false).await?;
    let result=async {
        let report: Value=serde_json::from_str(&fixture.ask("json").await?)?;verify_documents(&report)?;
        let item=documentary(&report).into_iter().find(|row|row["path"]==".github/instructions/preview.instructions.md").unwrap();
        let reply=fixture.app.handle_get_chunk(serde_json::from_value(json!({"project_id":fixture.pid,"namespace":"memory","doc_id":item["document_id"],"inject_rules":false}))?).await?;
        ensure!(reply.is_error!=Some(true));let body=&reply.content[0].as_text().unwrap().text;
        ensure!(body.contains(DOCUMENTS[1].1.trim()));
        let markdown=fixture.ask("markdown").await?;
        ensure!(markdown.contains("not executable implementation") && markdown.contains("does not establish approval"));
        ensure!(markdown.contains("ProjectBeaconToggle"));
        Ok(())
    }.await;
    fixture.close()?;
    result
}
