use quick_xml::{Reader, events::Event};
use regex::Regex;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;
use std::sync::LazyLock;

fn family(path: &str) -> Option<String> {
    static CULTURE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)\.[a-z]{2,3}(?:-[a-z0-9]{2,8})*$").unwrap());
    let lower = path.to_ascii_lowercase();
    let stem = lower.strip_suffix(".resx")?;
    Some(CULTURE.replace(stem, "").into_owned())
}

fn resource_keys(text: &str) -> anyhow::Result<BTreeSet<String>> {
    let mut reader = Reader::from_str(text.trim_start_matches('\u{feff}'));
    let mut keys = BTreeSet::new();
    let mut depth = 0usize;
    let mut root_seen = false;
    loop {
        let event = reader.read_event()?;
        match &event {
            Event::Empty(e) | Event::Start(e) => {
                if depth == 0 {
                    anyhow::ensure!(
                        e.name().as_ref() == b"root" && !root_seen,
                        "invalid resource root"
                    );
                    root_seen = true;
                } else if depth == 1 && e.name().as_ref() == b"data" {
                    let name = e
                        .try_get_attribute("name")?
                        .ok_or_else(|| anyhow::anyhow!("missing resource name"))?;
                    keys.insert(
                        name.decode_and_unescape_value(reader.decoder())?
                            .into_owned(),
                    );
                }
                if matches!(event, Event::Start(_)) {
                    depth += 1;
                }
            }
            Event::End(_) => {
                depth = depth
                    .checked_sub(1)
                    .ok_or_else(|| anyhow::anyhow!("unexpected closing tag"))?
            }
            Event::Eof => break,
            _ => {}
        }
    }
    anyhow::ensure!(root_seen && depth == 0, "incomplete resource XML");
    Ok(keys)
}

/// Compare only keys changed by this checkout relative to HEAD. A translated
/// value can change independently. A key delta can be incomplete even when
/// every locale file already participates in the patch.
pub(super) fn assess(
    root: &Path,
    edits: &[(String, String)],
    current: &[String],
    edited: &HashSet<String>,
    result: &mut BTreeSet<(String, String, String)>,
) {
    let families: BTreeSet<_> = edits.iter().filter_map(|(_, path)| family(path)).collect();
    if families.is_empty() {
        return;
    }
    let repo = git2::Repository::discover(root).ok();
    let head = repo
        .as_ref()
        .and_then(|r| r.head().ok()?.peel_to_tree().ok());
    let prefix = repo
        .as_ref()
        .and_then(|r| root.strip_prefix(r.workdir()?.canonicalize().ok()?).ok());
    let mut members = BTreeMap::<String, Vec<&str>>::new();
    for path in current {
        if let Some(f) = family(path)
            && families.contains(&f)
        {
            members.entry(f).or_default().push(path);
        }
    }
    for paths in members.values_mut() {
        paths.sort_unstable();
        let mut contents = HashMap::new();
        let mut added = BTreeMap::new();
        let mut removed = BTreeMap::new();
        let mut any_changed = false;
        let Some(&anchor) = paths
            .iter()
            .find(|p| edited.contains(&super::edit_path_key(p)))
        else {
            continue;
        };
        for &path in paths.iter() {
            let text = engram_core::safe_read_to_string(root, path);
            let Some((text, keys)) = text
                .ok()
                .and_then(|text| resource_keys(&text).ok().map(|keys| (text, keys)))
            else {
                result.insert((
                    anchor.into(),
                    path.into(),
                    "locale key coverage unavailable: unreadable or invalid resource XML".into(),
                ));
                continue;
            };
            if edited.contains(&super::edit_path_key(path))
                && let (Some(tree), Some(repo), Some(prefix)) = (&head, &repo, prefix)
            {
                let previous = match tree.get_path(&prefix.join(path)) {
                    Ok(entry) => repo.find_blob(entry.id()).ok().and_then(|blob| {
                        let previous = std::str::from_utf8(blob.content()).ok()?;
                        any_changed |= previous != text;
                        resource_keys(previous).ok()
                    }),
                    Err(e) if e.code() == git2::ErrorCode::NotFound => {
                        any_changed = true;
                        Some(BTreeSet::new())
                    }
                    Err(_) => None,
                };
                if let Some(before) = previous {
                    for key in keys.difference(&before) {
                        added.entry(key.clone()).or_insert(path);
                    }
                    for key in before.difference(&keys) {
                        removed.entry(key.clone()).or_insert(path);
                    }
                } else {
                    result.insert((
                        anchor.into(),
                        path.into(),
                        "previous locale key set unavailable at HEAD".into(),
                    ));
                }
            }
            contents.insert(path, keys);
        }
        for (&path, keys) in &contents {
            for (key, source) in &added {
                if !keys.contains(key) {
                    result.insert((
                        source.to_string(),
                        path.into(),
                        format!("missing added locale key `{key}` (working checkout vs HEAD)"),
                    ));
                }
            }
            for (key, source) in &removed {
                if keys.contains(key) {
                    result.insert((
                        source.to_string(),
                        path.into(),
                        format!(
                            "still contains removed locale key `{key}` (working checkout vs HEAD)"
                        ),
                    ));
                }
            }
            if !any_changed && !edited.contains(&super::edit_path_key(path)) {
                result.insert((
                    anchor.into(),
                    path.into(),
                    "sibling locale resource; no working key delta established".into(),
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn changed_translation_is_independent_but_new_keys_must_reach_edited_siblings() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let repo = git2::Repository::init(&root).unwrap();
        let files: Vec<String> = ["labels.resx", "labels.fr.resx", "labels.de.resx"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        let original = "<root><data name='title'><value>old</value></data></root>";
        let mut index = repo.index().unwrap();
        for file in &files {
            fs::write(root.join(file), original).unwrap();
            index.add_path(Path::new(file)).unwrap();
        }
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let author = git2::Signature::now("test", "test@example.invalid").unwrap();
        repo.commit(Some("HEAD"), &author, &author, "initial", &tree, &[])
            .unwrap();
        let edits = vec![
            (files[0].clone(), files[0].clone()),
            (files[1].clone(), files[1].clone()),
        ];
        let edited = edits
            .iter()
            .map(|(_, path)| super::super::edit_path_key(path))
            .collect();
        fs::write(root.join(&files[1]), original.replace("old", "traduit")).unwrap();
        let mut findings = BTreeSet::new();
        assess(&root, &edits, &files, &edited, &mut findings);
        assert!(findings.is_empty());
        fs::write(
            root.join(&files[0]),
            original.replace("</root>", "<data name='new_key'/></root>"),
        )
        .unwrap();
        assess(&root, &edits, &files, &edited, &mut findings);
        assert_eq!(findings.len(), 2);
        assert!(
            findings
                .iter()
                .any(|(_, path, reason)| path == &files[1] && reason.contains("new_key"))
        );
    }
}
