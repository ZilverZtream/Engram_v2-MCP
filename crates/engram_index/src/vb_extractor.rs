use crate::parsing::{ExtractedEdge, ExtractedSymbol};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Mutex, OnceLock};

struct Sidecar {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

static SIDECAR: OnceLock<Mutex<Option<Sidecar>>> = OnceLock::new();

fn sidecar_binary_path() -> PathBuf {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("engram_server"));
    let dir = exe.parent().unwrap_or_else(|| Path::new("."));
    let name = match std::env::consts::OS {
        "windows" => "vb_roslyn_sidecar-win-x64.exe",
        "macos" => "vb_roslyn_sidecar-osx-x64",
        _ => "vb_roslyn_sidecar-linux-x64",
    };
    dir.join(name)
}

fn get_or_spawn_sidecar() -> &'static Mutex<Option<Sidecar>> {
    SIDECAR.get_or_init(|| Mutex::new(None))
}

fn ensure_sidecar(guard: &mut Option<Sidecar>) -> std::io::Result<&mut Sidecar> {
    if guard.is_none() {
        let sidecar_bin = sidecar_binary_path();
        let mut child = Command::new(sidecar_bin)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        let stdin = child.stdin.take().expect("sidecar stdin should be piped");
        let stdout = BufReader::new(child.stdout.take().expect("sidecar stdout should be piped"));
        *guard = Some(Sidecar {
            child,
            stdin,
            stdout,
        });
    }
    Ok(guard.as_mut().expect("sidecar initialized"))
}

#[derive(Serialize)]
struct SidecarRequest<'a> {
    cmd: &'a str,
    path: String,
    source: &'a str,
}

#[derive(Debug, Deserialize)]
struct SidecarResponse {
    #[serde(default)]
    symbols: Vec<SidecarSymbol>,
    #[serde(default)]
    edges: Vec<SidecarEdge>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SidecarSymbol {
    name: String,
    kind: String,
    start_line: u32,
    end_line: u32,
    #[serde(default)]
    metadata: HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct SidecarEdge {
    source_name: String,
    source_kind: String,
    source_start_line: u32,
    source_language: String,
    target_name: String,
    target_kind: Option<String>,
    target_start_line: Option<u32>,
    kind: String,
    #[serde(default)]
    metadata: HashMap<String, String>,
}

fn str_to_static(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

fn parse_via_sidecar(
    sidecar: &mut Sidecar,
    path: &Path,
    source: &str,
) -> anyhow::Result<(Vec<ExtractedSymbol>, Vec<ExtractedEdge>)> {
    let req = SidecarRequest {
        cmd: "parse",
        path: path.display().to_string(),
        source,
    };
    writeln!(sidecar.stdin, "{}", serde_json::to_string(&req)?)?;
    sidecar.stdin.flush()?;

    let mut line = String::new();
    sidecar.stdout.read_line(&mut line)?;
    let response: SidecarResponse = serde_json::from_str(&line)?;
    if let Some(error) = response.error {
        anyhow::bail!(error);
    }

    let symbols = response
        .symbols
        .into_iter()
        .map(|s| ExtractedSymbol {
            name: s.name,
            kind: str_to_static(s.kind),
            start_line: s.start_line,
            end_line: s.end_line,
            metadata: if s.metadata.is_empty() {
                None
            } else {
                Some(s.metadata)
            },
        })
        .collect::<Vec<_>>();

    let edges = response
        .edges
        .into_iter()
        .map(|e| ExtractedEdge {
            source_name: e.source_name,
            source_kind: str_to_static(e.source_kind),
            source_start_line: e.source_start_line,
            source_language: str_to_static(e.source_language),
            target_name: e.target_name,
            target_kind: e.target_kind.map(str_to_static),
            target_start_line: e.target_start_line,
            kind: str_to_static(e.kind),
            metadata: if e.metadata.is_empty() {
                None
            } else {
                Some(e.metadata)
            },
        })
        .collect::<Vec<_>>();

    Ok((symbols, edges))
}

pub fn extract_vb(path: &Path, source: &str) -> (Vec<ExtractedSymbol>, Vec<ExtractedEdge>) {
    if source.trim().is_empty() {
        return (Vec::new(), Vec::new());
    }

    let mutex = get_or_spawn_sidecar();
    let mut guard = match mutex.lock() {
        Ok(g) => g,
        Err(poisoned) => {
            tracing::error!("VB sidecar mutex poisoned: {poisoned}");
            return fallback_extract_vb(path, source);
        }
    };

    let sidecar = match ensure_sidecar(&mut guard) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("failed to spawn sidecar: {e}; using fallback VB extraction");
            return fallback_extract_vb(path, source);
        }
    };

    match parse_via_sidecar(sidecar, path, source) {
        Ok(result) => result,
        Err(err) => {
            tracing::warn!("VB sidecar parse failed for {}: {err}", path.display());
            let _ = sidecar.child.kill();
            *guard = None;
            fallback_extract_vb(path, source)
        }
    }
}

fn fallback_extract_vb(path: &Path, source: &str) -> (Vec<ExtractedSymbol>, Vec<ExtractedEdge>) {
    // Lightweight fallback if sidecar isn't available.
    let mut symbols = Vec::new();
    let mut edges = Vec::new();
    let mut ns = String::new();
    let mut ty = String::new();

    let mut add_symbol =
        |name: String, kind: &'static str, line: u32, metadata: Option<HashMap<String, String>>| {
            symbols.push(ExtractedSymbol {
                name,
                kind,
                start_line: line,
                end_line: line,
                metadata,
            });
        };

    for (idx, raw_line) in source.lines().enumerate() {
        let line_no = idx as u32 + 1;
        let line = raw_line.trim();
        let lower = line.to_ascii_lowercase();

        if lower.starts_with("namespace ") {
            ns = line
                .split_whitespace()
                .nth(1)
                .unwrap_or_default()
                .to_string();
        } else if lower.contains(" class ")
            || lower.starts_with("class ")
            || lower.contains(" partial class ")
        {
            let name = line
                .split_whitespace()
                .skip_while(|t| t.to_ascii_lowercase() != "class")
                .nth(1)
                .unwrap_or_default()
                .to_string();
            ty = name.clone();
            add_symbol(name.clone(), "class", line_no, None);
            if !ns.is_empty() {
                edges.push(ExtractedEdge {
                    source_name: ns.clone(),
                    source_kind: "namespace",
                    source_start_line: 1,
                    source_language: "vb",
                    target_name: name,
                    target_kind: Some("class"),
                    target_start_line: Some(line_no),
                    kind: "contains",
                    metadata: None,
                });
            }
        } else if lower.contains(" sub ")
            || lower.starts_with("sub ")
            || lower.contains(" function ")
            || lower.starts_with("function ")
        {
            let tokens: Vec<_> = line.split_whitespace().collect();
            let kw_idx = tokens
                .iter()
                .position(|t| matches!(t.to_ascii_lowercase().as_str(), "sub" | "function"));
            if let Some(i) = kw_idx {
                let mut method = tokens.get(i + 1).copied().unwrap_or_default().to_string();
                if let Some(p) = method.find('(') {
                    method.truncate(p);
                }
                let mut meta = HashMap::new();
                if lower.contains("async ") {
                    meta.insert("async".to_string(), "true".to_string());
                }
                if let Some((stage, seq)) = webforms_lifecycle_info(&method) {
                    meta.insert("lifecycle_stage".to_string(), stage.to_string());
                    meta.insert("lifecycle_sequence".to_string(), seq.to_string());
                }
                let fqn = if ns.is_empty() {
                    if ty.is_empty() {
                        method.clone()
                    } else {
                        format!("{ty}.{method}")
                    }
                } else if ty.is_empty() {
                    format!("{ns}.{method}")
                } else {
                    format!("{ns}.{ty}.{method}")
                };
                add_symbol(
                    fqn.clone(),
                    "function",
                    line_no,
                    if meta.is_empty() {
                        None
                    } else {
                        Some(meta.clone())
                    },
                );
                if !ty.is_empty() {
                    edges.push(ExtractedEdge {
                        source_name: ty.clone(),
                        source_kind: "class",
                        source_start_line: line_no,
                        source_language: "vb",
                        target_name: fqn.clone(),
                        target_kind: Some("function"),
                        target_start_line: Some(line_no),
                        kind: "contains",
                        metadata: None,
                    });
                }
                if let Some(handles_pos) = lower.find(" handles ") {
                    let handles = &line[handles_pos + 9..];
                    for part in handles.split(',') {
                        let part = part.trim();
                        if let Some((control, _evt)) = part.split_once('.') {
                            let mut m = HashMap::new();
                            m.insert("fqn".to_string(), fqn.clone());
                            edges.push(ExtractedEdge {
                                source_name: control.trim().to_string(),
                                source_kind: "control",
                                source_start_line: line_no,
                                source_language: "vb",
                                target_name: method.clone(),
                                target_kind: Some("function"),
                                target_start_line: Some(line_no),
                                kind: "event_wiring",
                                metadata: Some(m),
                            });
                        }
                    }
                }
            }
        } else if lower.starts_with("imports ") {
            let target = line[8..].trim().to_string();
            edges.push(ExtractedEdge {
                source_name: path.display().to_string(),
                source_kind: "file",
                source_start_line: line_no,
                source_language: "vb",
                target_name: target,
                target_kind: Some("namespace"),
                target_start_line: None,
                kind: "imports",
                metadata: None,
            });
        }
    }

    (symbols, edges)
}

fn webforms_lifecycle_info(name: &str) -> Option<(&'static str, &'static str)> {
    match name.to_ascii_lowercase().as_str() {
        "page_preinit" => Some(("PreInit", "1")),
        "page_init" | "oninit" => Some(("Init", "2")),
        "page_preload" => Some(("PreLoad", "4")),
        "page_load" | "onload" => Some(("Load", "5")),
        "page_prerender" | "onprerender" => Some(("PreRender", "7")),
        "page_unload" | "onunload" => Some(("Unload", "11")),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::extract_vb;
    use std::path::Path;

    #[test]
    fn indexes_partial_class_without_modifier() {
        let code = "Partial Class Foo\n  Public Sub Bar()\n  End Sub\nEnd Class";
        let (symbols, _) = extract_vb(Path::new("foo.vb"), code);
        assert!(symbols.iter().any(|s| s.name.contains("Foo")));
        assert!(symbols.iter().any(|s| s.name.contains("Bar")));
    }

    #[test]
    fn indexes_async_function() {
        let code =
            "Class Foo\n  Async Function Bar() As Task(Of String)\n  End Function\nEnd Class";
        let (symbols, _) = extract_vb(Path::new("foo.vb"), code);
        let bar = symbols.iter().find(|s| s.name.contains("Bar")).unwrap();
        let md = bar.metadata.as_ref().unwrap();
        assert_eq!(md.get("async").map(String::as_str), Some("true"));
    }

    #[test]
    fn parses_interpolation_and_null_conditional() {
        let code = "Class Foo\n Sub Bar()\n   Dim x = app?.Name\n   Dim s = $\"Hello {name}\"\n End Sub\nEnd Class";
        let (symbols, _) = extract_vb(Path::new("foo.vb"), code);
        assert!(symbols.iter().any(|s| s.name.contains("Bar")));
    }
}
