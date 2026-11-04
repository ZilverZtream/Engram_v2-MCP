//! Read-only offline access to the same evidence/proposal checks as
//! pre_push_audit(instruction_maintenance=...). Opens no index or server.
//! Usage: instruction_maintenance <project-root> <project-id> <request.json>
use std::{io::Read, path::Path};

fn main() -> anyhow::Result<()> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    anyhow::ensure!(
        args.len() == 3,
        "usage: instruction_maintenance <project-root> <project-id> <request.json>"
    );
    let mut bytes = Vec::new();
    std::fs::File::open(&args[2])?
        .take(1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() <= 1024 * 1024,
        "Maintenance request exceeds 1 MiB"
    );
    let request = serde_json::from_slice(&bytes)?;
    let report = engram_server::services::instruction_maintenance::review(
        Path::new(&args[0]),
        &args[1],
        &request,
    )?;
    serde_json::to_writer_pretty(std::io::stdout().lock(), &report)?;
    Ok(())
}
