//! Offline access to the production policy context used by planning/review.
//! No index, database, model, server or project copy is opened.
use std::{io::Read, path::Path};

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    target_paths: Vec<String>,
    #[serde(default)]
    historical: bool,
}

fn main() -> anyhow::Result<()> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    anyhow::ensure!(
        args.len() == 2,
        "usage: instruction_context <root> <request.json>"
    );
    let mut bytes = Vec::new();
    std::fs::File::open(&args[1])?
        .take(64 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    anyhow::ensure!(bytes.len() <= 64 * 1024, "Context request exceeds 64 KiB");
    let request: Request = serde_json::from_slice(&bytes)?;
    anyhow::ensure!(
        !request.target_paths.is_empty()
            && request.target_paths.len() <= 16
            && request.target_paths.iter().all(|path| path.len() <= 512),
        "Use 1..16 bounded target paths"
    );
    let context = if request.historical {
        engram_server::services::project_policy::PolicyContext::historical_unavailable()
    } else {
        engram_server::services::project_policy::resolve(
            Path::new(&args[0]),
            &request.target_paths,
            &request.target_paths,
        )
    };
    serde_json::to_writer_pretty(std::io::stdout().lock(), &context)?;
    Ok(())
}
