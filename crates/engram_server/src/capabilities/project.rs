//! Ecosystem evidence from admitted paths. No build execution or dependency downloads.
use crate::models::ProjectType;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Default, Serialize)]
pub struct ProjectCapabilities {
    pub ecosystems: BTreeSet<String>,
    pub source_languages: BTreeSet<String>,
    pub evidence: BTreeMap<String, Vec<String>>,
    pub evidence_omitted: usize,
    pub inspected_files: usize,
    pub webforms: bool,
    pub classic_asp: bool,
    pub dotnet: bool,
    pub visual_basic: bool,
    pub javascript: bool,
}

impl ProjectCapabilities {
    pub fn from_paths<'a>(paths: impl IntoIterator<Item = &'a str>) -> Self {
        let mut result = Self::default();
        let mut retained_bytes = 0;
        let mut retained_count = 0;
        for path in paths {
            result.inspected_files += 1;
            let name = path
                .rsplit(['/', '\\'])
                .next()
                .unwrap_or(path)
                .to_ascii_lowercase();
            let ext = name.rsplit_once('.').map_or("", |(_, ext)| ext);
            let ecosystem = match name.as_str() {
                "cargo.toml" => Some("rust"),
                "package.json" => Some("javascript_typescript"),
                "pyproject.toml" | "setup.py" | "setup.cfg" | "requirements.txt" => Some("python"),
                "go.mod" | "go.work" => Some("go"),
                "pom.xml" | "build.gradle" | "build.gradle.kts" => Some("java"),
                _ => match ext {
                    "sln" | "slnx" | "csproj" | "vbproj" => Some("dotnet"),
                    _ => None,
                },
            };
            let language = match ext {
                "rs" => Some(("rust", "rust")),
                "js" | "ts" | "tsx" | "jsx" | "mjs" | "cjs" => {
                    Some(("javascript_typescript", "javascript_typescript"))
                }
                "py" | "pyi" => Some(("python", "python")),
                "cs" => Some(("dotnet", "csharp")),
                "vb" => Some(("dotnet", "visual_basic")),
                "go" => Some(("go", "go")),
                "java" => Some(("java", "java")),
                "c" | "h" => Some(("c_cpp", "c")),
                "cpp" | "cc" | "cxx" | "hpp" | "c++" | "hh" | "hxx" | "h++" | "ixx" | "cppm" => {
                    Some(("c_cpp", "cpp"))
                }
                "ml" | "mlinc" => Some(("minilang", "minilang")),
                _ => None,
            };
            let mut markers = Vec::new();
            if let Some(ecosystem) = ecosystem {
                markers.push(ecosystem);
            }
            if let Some((ecosystem, language)) = language {
                markers.push(ecosystem);
                result.source_languages.insert(language.to_owned());
            }
            if matches!(ext, "aspx" | "ascx" | "master") {
                result.webforms = true;
                markers.extend(["dotnet", "webforms"]);
            }
            if ext == "asp" {
                result.classic_asp = true;
                markers.push("classic_asp");
            }
            if ext == "vbproj" || ext == "vb" {
                result.visual_basic = true;
            }
            markers.sort_unstable();
            markers.dedup();
            for marker in markers {
                if marker != "webforms" {
                    result.ecosystems.insert(marker.to_owned());
                }
                if retained_count < 32
                    && path.len() <= 2048
                    && retained_bytes + path.len() <= 16 * 1024
                {
                    result
                        .evidence
                        .entry(marker.to_owned())
                        .or_default()
                        .push(path.to_owned());
                    retained_count += 1;
                    retained_bytes += path.len();
                } else {
                    result.evidence_omitted += 1;
                }
            }
        }
        result.dotnet = result.ecosystems.contains("dotnet");
        result.javascript = result.ecosystems.contains("javascript_typescript");
        result
    }

    /// Select once at registration; mixed projects keep the broad source set.
    /// Existing/explicit profiles are never silently changed by refresh.
    pub fn indexing_preset(&self) -> ProjectType {
        if self.webforms
            && self
                .ecosystems
                .iter()
                .all(|e| matches!(e.as_str(), "dotnet" | "javascript_typescript"))
        {
            return match (self.visual_basic, self.source_languages.contains("csharp")) {
                (true, false) => ProjectType::DotnetWebformsVb,
                (false, _) => ProjectType::DotnetWebformsCs,
                _ => ProjectType::General,
            };
        }
        if self.ecosystems.len() != 1 {
            return ProjectType::General;
        }
        match self.ecosystems.first().map(String::as_str) {
            Some("rust") => ProjectType::Rust,
            Some("python") => ProjectType::Python,
            Some("javascript_typescript") => ProjectType::JavaScript,
            Some("dotnet") if !self.visual_basic => ProjectType::CSharp,
            Some("c_cpp") if self.source_languages.contains("cpp") => ProjectType::Cpp,
            Some("c_cpp") => ProjectType::C,
            Some("minilang") => ProjectType::MiniLang,
            _ => ProjectType::General,
        }
    }

    pub fn include_explicit_profile(&mut self, profile: &str) {
        match ProjectType::from_registry_str(profile) {
            Some(ProjectType::DotnetWebformsCs) => {
                self.webforms = true;
                self.dotnet = true;
            }
            Some(ProjectType::DotnetWebformsVb) => {
                self.webforms = true;
                self.dotnet = true;
                self.visual_basic = true;
            }
            Some(ProjectType::CSharp) => self.dotnet = true,
            Some(ProjectType::JavaScript) => self.javascript = true,
            _ => {}
        }
    }

    pub fn union(&mut self, other: &Self) {
        self.webforms |= other.webforms;
        self.classic_asp |= other.classic_asp;
        self.dotnet |= other.dotnet;
        self.visual_basic |= other.visual_basic;
        self.javascript |= other.javascript;
    }

    pub fn allows_legacy_tool(&self, name: &str) -> bool {
        match name {
            "get_vb_translation_traps" => self.visual_basic,
            "analyze_sync_hazards" => self.dotnet,
            "get_jquery_inventory" => self.javascript || self.webforms || self.classic_asp,
            _ => self.webforms || self.classic_asp,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_detection_distinguishes_webforms_and_preserves_mixed_sources() {
        let ordinary = ProjectCapabilities::from_paths(["app/App.csproj", "app/Program.cs"]);
        assert_eq!(ordinary.indexing_preset(), ProjectType::CSharp);
        assert!(!ordinary.webforms);
        let web = ProjectCapabilities::from_paths([
            "Site.vbproj",
            "Default.aspx",
            "Default.aspx.vb",
            "client.js",
        ]);
        assert_eq!(web.indexing_preset(), ProjectType::DotnetWebformsVb);
        assert!(web.webforms && web.visual_basic);
        let mixed = ProjectCapabilities::from_paths([
            "Cargo.toml",
            "src/lib.rs",
            "web/package.json",
            "web/app.ts",
            "jobs/pyproject.toml",
            "jobs/task.py",
        ]);
        assert_eq!(mixed.indexing_preset(), ProjectType::General);
        assert_eq!(mixed.ecosystems.len(), 3);
        let exts = crate::utils::files::exts_for_project_type_enum(mixed.indexing_preset());
        for ext in ["rs", "ts", "py", "toml", "json"] {
            assert!(exts.contains(&ext));
        }
    }

    #[test]
    fn project_detection_uses_manifests_and_bounds_evidence_without_losing_capabilities() {
        for (path, preset) in [
            ("Cargo.toml", ProjectType::Rust),
            ("package.json", ProjectType::JavaScript),
            ("pyproject.toml", ProjectType::Python),
        ] {
            assert_eq!(
                ProjectCapabilities::from_paths([path]).indexing_preset(),
                preset
            );
        }
        let mut paths = (0..100).map(|n| format!("src/{n}.rs")).collect::<Vec<_>>();
        paths.push("site/Default.aspx".to_owned());
        let detected = ProjectCapabilities::from_paths(paths.iter().map(String::as_str));
        assert!(detected.webforms && detected.evidence_omitted > 0);
        assert_eq!(detected.inspected_files, 101);
        assert_eq!(detected.indexing_preset(), ProjectType::General);
        assert_eq!(
            ProjectCapabilities::from_paths(["README.md"]).indexing_preset(),
            ProjectType::General
        );
    }

    #[test]
    fn project_detection_specialized_visibility_tracks_available_capabilities() {
        let mut union = ProjectCapabilities::from_paths(["package.json", "app.ts"]);
        assert!(!union.allows_legacy_tool("generate_migration_plan"));
        assert!(!union.allows_legacy_tool("get_vb_translation_traps"));
        union.union(&ProjectCapabilities::from_paths([
            "legacy/Default.aspx",
            "legacy/Default.aspx.vb",
        ]));
        assert!(union.allows_legacy_tool("generate_migration_plan"));
        assert!(union.allows_legacy_tool("get_vb_translation_traps"));
        let mut empty = ProjectCapabilities::default();
        assert!(!empty.allows_legacy_tool("get_page_context"));
        empty.include_explicit_profile("webforms_cs");
        assert!(empty.allows_legacy_tool("get_page_context"));
    }

    #[tokio::test]
    async fn project_detection_respects_ignored_dependencies_and_keeps_owned_source() {
        let temp = tempfile::tempdir().unwrap();
        {
            let root = temp.path().join("source");
            std::fs::create_dir(&root).unwrap();
            drop(git2::Repository::init(&root).unwrap());
            std::fs::write(root.join(".gitignore"), "vendor/\n").unwrap();
            std::fs::write(root.join("pyproject.toml"), "[project]\nname='owned'\n").unwrap();
            std::fs::write(root.join("app.py"), "def owned(): return 17\n").unwrap();
            std::fs::create_dir(root.join("vendor")).unwrap();
            std::fs::write(root.join("vendor/Default.aspx"), "<%@ Page %>").unwrap();
            let (state, _) = crate::state::AppState::new(engram_core::Config {
                data_dir: temp.path().join("data"),
                allowed_roots: vec![temp.path().to_owned()],
                embedding_backend: "fts_only".into(),
                ..Default::default()
            })
            .unwrap();
            let preset = crate::services::project_service::detect_indexing_preset(&state, &root)
                .await
                .unwrap();
            assert_eq!(preset, ProjectType::Python);
            std::fs::write(root.join("schema.sql"), "CREATE TABLE owned(id INTEGER);\n").unwrap();
            assert_eq!(
                crate::services::project_service::detect_indexing_preset(&state, &root)
                    .await
                    .unwrap(),
                ProjectType::General
            );
            std::fs::write(
                root.join("Cargo.toml"),
                "[package]\nname='owned-rust'\nversion='0.1.0'\n",
            )
            .unwrap();
            std::fs::write(root.join("lib.rs"), "pub fn owned() -> u8 { 17 }\n").unwrap();
            assert_eq!(
                crate::services::project_service::detect_indexing_preset(&state, &root)
                    .await
                    .unwrap(),
                ProjectType::General
            );
        }
        temp.close().unwrap();
    }
}
