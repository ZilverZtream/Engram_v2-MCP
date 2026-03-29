//! Multi-project solution parser (.sln, .csproj, .vbproj).
//!
//! Enterprise WebForms apps typically live in solutions with 5-15 projects.
//! This module parses solution and project files to extract project structure,
//! inter-project dependencies, and framework metadata — enabling project-aware
//! wave planning, namespace resolution, and shared library detection.

use regex::Regex;
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::OnceLock;

// ─── Output types ──────────────────────────────────────────────────────────────

/// Classification of a project by its type GUID or output type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectType {
    CSharp,
    VbNet,
    WebApplication,
    WebSite,
    WindowsService,
    TestProject,
    ClassLibrary,
    WpfApplication,
    ConsoleApplication,
    Unknown,
}

impl std::fmt::Display for ProjectType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CSharp => write!(f, "csharp"),
            Self::VbNet => write!(f, "vbnet"),
            Self::WebApplication => write!(f, "web_application"),
            Self::WebSite => write!(f, "web_site"),
            Self::WindowsService => write!(f, "windows_service"),
            Self::TestProject => write!(f, "test_project"),
            Self::ClassLibrary => write!(f, "class_library"),
            Self::WpfApplication => write!(f, "wpf_application"),
            Self::ConsoleApplication => write!(f, "console_application"),
            Self::Unknown => write!(f, "unknown"),
        }
    }
}

/// A project entry parsed from a .sln file.
#[derive(Debug, Clone, Serialize)]
pub struct SolutionProject {
    /// Project GUID from the solution.
    pub project_guid: String,
    /// Display name.
    pub name: String,
    /// Relative path to the project file.
    pub relative_path: String,
    /// Project type GUID (determines language/type).
    pub type_guid: String,
    /// Classified project type.
    pub project_type: ProjectType,
}

/// A parsed project file (.csproj / .vbproj).
#[derive(Debug, Clone, Serialize)]
pub struct ProjectFileInfo {
    /// Path to the project file.
    pub project_path: String,
    /// Root namespace declared in the project.
    pub root_namespace: Option<String>,
    /// Assembly name.
    pub assembly_name: Option<String>,
    /// Target framework (e.g., "net48", "v4.7.2").
    pub target_framework: Option<String>,
    /// Output type (Library, Exe, WinExe).
    pub output_type: Option<String>,
    /// References to other projects in the solution.
    pub project_references: Vec<String>,
    /// NuGet/assembly references.
    pub package_references: Vec<PackageRef>,
    /// Source files included (only from `<Compile Include=...>` if present).
    pub source_files: Vec<String>,
}

/// A package or assembly reference.
#[derive(Debug, Clone, Serialize)]
pub struct PackageRef {
    pub name: String,
    pub version: Option<String>,
}

/// Complete solution structure analysis.
#[derive(Debug, Clone, Serialize)]
pub struct SolutionStructure {
    /// All projects in the solution.
    pub projects: Vec<SolutionProject>,
    /// Parsed project file details (keyed by project name).
    pub project_details: BTreeMap<String, ProjectFileInfo>,
    /// Project dependency graph: project_name → [dependency_names].
    pub dependency_graph: BTreeMap<String, Vec<String>>,
    /// Solution configurations (Debug, Release, etc.).
    pub configurations: Vec<String>,
    /// Projects classified as shared libraries.
    pub shared_libraries: Vec<String>,
    /// Topological ordering for migration waves.
    pub migration_order: Vec<String>,
    /// Circular dependency warnings.
    pub warnings: Vec<String>,
}

/// Classification of a shared type across projects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SharedTypeClass {
    /// Pure data class — migrate as-is.
    ModelDto,
    /// Business logic — needs repository pattern.
    Service,
    /// Static helpers — often replaceable.
    Utility,
    /// Abstract/virtual base — high blast radius.
    BaseClass,
}

// ─── Regex singletons ──────────────────────────────────────────────────────────

fn re_sln_project() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"Project\("\{([0-9A-Fa-f\-]+)\}"\)\s*=\s*"([^"]+)"\s*,\s*"([^"]+)"\s*,\s*"\{([0-9A-Fa-f\-]+)\}""#,
        )
        .expect("re_sln_project")
    })
}

fn re_sln_config() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // Matches "Debug|Any CPU = Debug|Any CPU"
        Regex::new(r"(?m)^\s+(\w+)\|(.+?)\s*=").expect("re_sln_config")
    })
}

fn re_xml_element() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // Can't use backreference (\1) in Rust regex — match any closing tag instead
        Regex::new(r"<(\w+)(?:\s[^>]*)?>([^<]*)</\w+>").expect("re_xml_elem")
    })
}

fn re_project_reference() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"<ProjectReference\s+Include="([^"]+)""#).expect("re_proj_ref"))
}

fn re_package_reference() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"<PackageReference\s+Include="([^"]+)"(?:\s+Version="([^"]+)")?"#)
            .expect("re_pkg_ref")
    })
}

fn re_assembly_reference() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"<Reference\s+Include="([^"]+)""#).expect("re_asm_ref"))
}

fn re_compile_include() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"<Compile\s+Include="([^"]+)""#).expect("re_compile"))
}

#[allow(dead_code)]
fn re_import_project() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"<Import\s+Project="([^"]+)""#).expect("re_import"))
}

// ─── Project type classification ───────────────────────────────────────────────

/// Classify project type from its type GUID in the solution file.
pub fn classify_project_type(type_guid: &str) -> ProjectType {
    let guid = type_guid.trim_matches(|c| c == '{' || c == '}');
    match guid.to_uppercase().as_str() {
        // FAE04EC0 = C#
        "FAE04EC0-301F-11D3-BF4B-00C04F79EFBC" => ProjectType::CSharp,
        // F184B08F = VB.NET
        "F184B08F-C81C-45F6-A57F-5ABD9991F28F" => ProjectType::VbNet,
        // 349C5851 = ASP.NET Web Application
        "349C5851-65DF-11DA-9384-00065B846F21" => ProjectType::WebApplication,
        // E24C65DC = ASP.NET Web Site
        "E24C65DC-7377-472B-9ABA-BC803B73C61A" => ProjectType::WebSite,
        // 3AC096D0 = Test project
        "3AC096D0-A1C2-E12C-1390-A8335801FDAB" => ProjectType::TestProject,
        // 60DC8134 = WPF
        "60DC8134-EBA5-43B8-BCC9-BB4BC16C2548" => ProjectType::WpfApplication,
        // 2150E333 = Solution folder (virtual) — skip
        "2150E333-8FDC-42A3-9474-1A3956D46DE8" => ProjectType::Unknown,
        _ => ProjectType::Unknown,
    }
}

// ─── Public API ────────────────────────────────────────────────────────────────

/// Parse a .sln file and return the list of projects.
pub fn parse_solution(sln_content: &str) -> Vec<SolutionProject> {
    let mut projects = Vec::new();

    for caps in re_sln_project().captures_iter(sln_content) {
        let type_guid = caps.get(1).map(|m| m.as_str()).unwrap_or("");
        let name = caps.get(2).map(|m| m.as_str()).unwrap_or("");
        let path = caps.get(3).map(|m| m.as_str()).unwrap_or("");
        let proj_guid = caps.get(4).map(|m| m.as_str()).unwrap_or("");

        let project_type = classify_project_type(type_guid);

        // Skip solution folders
        if project_type == ProjectType::Unknown
            && type_guid.to_uppercase() == "2150E333-8FDC-42A3-9474-1A3956D46DE8"
        {
            continue;
        }

        projects.push(SolutionProject {
            project_guid: proj_guid.to_string(),
            name: name.to_string(),
            relative_path: path.replace('\\', "/"),
            type_guid: type_guid.to_string(),
            project_type,
        });
    }

    projects
}

/// Extract solution configurations from a .sln file.
pub fn parse_solution_configs(sln_content: &str) -> Vec<String> {
    let mut configs = HashSet::new();
    for caps in re_sln_config().captures_iter(sln_content) {
        if let Some(config) = caps.get(1) {
            configs.insert(config.as_str().trim().to_string());
        }
    }
    let mut sorted: Vec<_> = configs.into_iter().collect();
    sorted.sort();
    sorted
}

/// Parse a .csproj or .vbproj file and extract project metadata.
pub fn parse_project_file(proj_content: &str, proj_path: &str) -> ProjectFileInfo {
    let mut root_namespace = None;
    let mut assembly_name = None;
    let mut target_framework = None;
    let mut output_type = None;
    let mut project_references = Vec::new();
    let mut package_references = Vec::new();
    let mut source_files = Vec::new();

    // Extract simple XML elements
    for caps in re_xml_element().captures_iter(proj_content) {
        let tag = caps.get(1).map(|m| m.as_str()).unwrap_or("");
        let value = caps.get(2).map(|m| m.as_str().trim()).unwrap_or("");
        match tag {
            "RootNamespace" => root_namespace = Some(value.to_string()),
            "AssemblyName" => assembly_name = Some(value.to_string()),
            "TargetFramework" | "TargetFrameworkVersion" => {
                target_framework = Some(value.to_string())
            }
            "OutputType" => output_type = Some(value.to_string()),
            _ => {}
        }
    }

    // Project references
    for caps in re_project_reference().captures_iter(proj_content) {
        if let Some(path) = caps.get(1) {
            project_references.push(path.as_str().replace('\\', "/"));
        }
    }

    // Package references
    for caps in re_package_reference().captures_iter(proj_content) {
        let name = caps.get(1).map(|m| m.as_str()).unwrap_or("").to_string();
        let version = caps.get(2).map(|m| m.as_str().to_string());
        if !name.is_empty() {
            package_references.push(PackageRef { name, version });
        }
    }

    // Assembly references (legacy format)
    for caps in re_assembly_reference().captures_iter(proj_content) {
        if let Some(name) = caps.get(1) {
            let name_str = name.as_str();
            // Extract just the assembly name (before comma if version info present)
            let short_name = name_str.split(',').next().unwrap_or(name_str);
            package_references.push(PackageRef {
                name: short_name.to_string(),
                version: None,
            });
        }
    }

    // Compile includes (legacy format with explicit file listing)
    for caps in re_compile_include().captures_iter(proj_content) {
        if let Some(file) = caps.get(1) {
            source_files.push(file.as_str().replace('\\', "/"));
        }
    }

    ProjectFileInfo {
        project_path: proj_path.to_string(),
        root_namespace,
        assembly_name,
        target_framework,
        output_type,
        project_references,
        package_references,
        source_files,
    }
}

/// Build a complete solution structure from parsed projects and their project files.
///
/// `project_files` maps project name → project file content.
pub fn build_solution_structure(
    sln_content: &str,
    project_files: &HashMap<String, String>,
) -> SolutionStructure {
    let projects = parse_solution(sln_content);
    let configurations = parse_solution_configs(sln_content);
    let mut project_details = BTreeMap::new();
    let mut dependency_graph: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut warnings = Vec::new();

    // Parse each project file
    for proj in &projects {
        if let Some(content) = project_files.get(&proj.name) {
            let info = parse_project_file(content, &proj.relative_path);
            project_details.insert(proj.name.clone(), info);
        }
        dependency_graph.entry(proj.name.clone()).or_default();
    }

    // Build dependency graph from ProjectReferences
    // Map relative paths to project names
    let path_to_name: HashMap<String, String> = projects
        .iter()
        .map(|p| {
            let path_lower = p.relative_path.to_lowercase();
            (path_lower, p.name.clone())
        })
        .collect();

    for proj in &projects {
        if let Some(info) = project_details.get(&proj.name) {
            for ref_path in &info.project_references {
                // Try to match the reference path to a project name
                let ref_lower = ref_path.to_lowercase();
                let dep_name = path_to_name
                    .iter()
                    .find(|(path, _)| {
                        ref_lower.ends_with(path.as_str())
                            || path.ends_with(ref_lower.as_str())
                            || extract_filename(&ref_lower) == extract_filename(path)
                    })
                    .map(|(_, name)| name.clone())
                    .unwrap_or_else(|| {
                        // Use filename as project name guess
                        extract_project_name(ref_path)
                    });

                dependency_graph
                    .entry(proj.name.clone())
                    .or_default()
                    .push(dep_name);
            }
        }
    }

    // Detect shared libraries (referenced by 2+ projects)
    let mut ref_counts: HashMap<String, usize> = HashMap::new();
    for deps in dependency_graph.values() {
        for dep in deps {
            *ref_counts.entry(dep.clone()).or_insert(0) += 1;
        }
    }
    let shared_libraries: Vec<String> = ref_counts
        .iter()
        .filter(|(_, count)| **count >= 2)
        .map(|(name, _)| name.clone())
        .collect();

    // Topological sort for migration ordering
    let (migration_order, cycle_warnings) = topological_sort(&projects, &dependency_graph);
    warnings.extend(cycle_warnings);

    SolutionStructure {
        projects,
        project_details,
        dependency_graph,
        configurations,
        shared_libraries,
        migration_order,
        warnings,
    }
}

/// Determine correct namespace for a file given its project context.
pub fn resolve_namespace(structure: &SolutionStructure, project_name: &str) -> Option<String> {
    structure
        .project_details
        .get(project_name)
        .and_then(|info| info.root_namespace.clone())
}

/// Compute cross-project blast radius multiplier for a file.
///
/// Files in shared libraries get a multiplier based on how many projects
/// reference them.
pub fn cross_project_multiplier(structure: &SolutionStructure, project_name: &str) -> f32 {
    let ref_count = structure
        .dependency_graph
        .values()
        .filter(|deps| deps.contains(&project_name.to_string()))
        .count();
    match ref_count {
        0 => 1.0,
        1 => 1.0,
        2 => 1.5,
        3..=5 => 2.0,
        _ => 3.0,
    }
}

/// Determine which project a file belongs to based on the solution structure.
pub fn file_to_project<'a>(structure: &'a SolutionStructure, file_path: &str) -> Option<&'a str> {
    let normalized = file_path.replace('\\', "/").to_lowercase();
    for proj in &structure.projects {
        let proj_dir = proj
            .relative_path
            .replace('\\', "/")
            .rsplit('/')
            .skip(1)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("/")
            .to_lowercase();
        if !proj_dir.is_empty() && normalized.contains(&proj_dir) {
            return Some(&proj.name);
        }
    }
    None
}

// ─── Internal helpers ──────────────────────────────────────────────────────────

fn extract_filename(path: &str) -> String {
    path.replace('\\', "/")
        .rsplit('/')
        .next()
        .unwrap_or(path)
        .to_lowercase()
}

fn extract_project_name(path: &str) -> String {
    let filename = path
        .replace('\\', "/")
        .rsplit('/')
        .next()
        .unwrap_or(path)
        .to_string();
    filename
        .strip_suffix(".csproj")
        .or_else(|| filename.strip_suffix(".vbproj"))
        .unwrap_or(&filename)
        .to_string()
}

fn topological_sort(
    projects: &[SolutionProject],
    deps: &BTreeMap<String, Vec<String>>,
) -> (Vec<String>, Vec<String>) {
    use std::collections::VecDeque;

    let mut in_degree: HashMap<String, usize> = HashMap::new();
    let mut adj: HashMap<String, Vec<String>> = HashMap::new();

    for proj in projects {
        in_degree.entry(proj.name.clone()).or_insert(0);
        adj.entry(proj.name.clone()).or_default();
    }

    for (proj, dependencies) in deps {
        for dep in dependencies {
            // Project depends on dep → dep should come first
            // Edge: dep → proj in the dependency graph
            adj.entry(dep.clone()).or_default().push(proj.clone());
            *in_degree.entry(proj.clone()).or_insert(0) += 1;
        }
    }

    let mut queue: VecDeque<String> = in_degree
        .iter()
        .filter(|(_, deg)| **deg == 0)
        .map(|(id, _)| id.clone())
        .collect();

    // Sort the initial queue for deterministic ordering
    let mut initial: Vec<_> = queue.drain(..).collect();
    initial.sort();
    queue.extend(initial);

    let mut order = Vec::new();
    let mut visited: HashSet<String> = HashSet::new();
    let mut warnings = Vec::new();

    while let Some(node) = queue.pop_front() {
        if !visited.insert(node.clone()) {
            continue;
        }
        order.push(node.clone());
        if let Some(neighbors) = adj.get(&node) {
            let mut sorted_neighbors: Vec<_> = neighbors.clone();
            sorted_neighbors.sort();
            for n in sorted_neighbors {
                if let Some(deg) = in_degree.get_mut(&n) {
                    *deg = deg.saturating_sub(1);
                    if *deg == 0 && !visited.contains(&n) {
                        queue.push_back(n);
                    }
                }
            }
        }
    }

    // Handle remaining nodes (circular dependencies)
    for proj in projects {
        if !visited.contains(&proj.name) {
            warnings.push(format!(
                "Circular dependency detected involving project '{}'",
                proj.name
            ));
            order.push(proj.name.clone());
        }
    }

    (order, warnings)
}

// ─── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_SLN: &str = r#"
Microsoft Visual Studio Solution File, Format Version 12.00
# Visual Studio 2019
Project("{FAE04EC0-301F-11D3-BF4B-00C04F79EFBC}") = "WebApp", "WebApp\WebApp.csproj", "{A1B2C3D4-E5F6-7890-ABCD-EF1234567890}"
EndProject
Project("{FAE04EC0-301F-11D3-BF4B-00C04F79EFBC}") = "BusinessLogic", "BusinessLogic\BusinessLogic.csproj", "{B2C3D4E5-F6A7-8901-BCDE-F12345678901}"
EndProject
Project("{FAE04EC0-301F-11D3-BF4B-00C04F79EFBC}") = "DataAccess", "DataAccess\DataAccess.csproj", "{C3D4E5F6-A7B8-9012-CDEF-012345678912}"
EndProject
Project("{2150E333-8FDC-42A3-9474-1A3956D46DE8}") = "Solution Items", "Solution Items", "{D4E5F6A7-B8C9-0123-DEF0-123456789023}"
EndProject
Global
    GlobalSection(SolutionConfigurationPlatforms) = preSolution
        Debug|Any CPU = Debug|Any CPU
        Release|Any CPU = Release|Any CPU
    EndGlobalSection
EndGlobal
"#;

    const SAMPLE_CSPROJ: &str = r#"
<Project Sdk="Microsoft.NET.Sdk.Web">
  <PropertyGroup>
    <TargetFramework>net48</TargetFramework>
    <RootNamespace>MyApp.Web</RootNamespace>
    <AssemblyName>MyApp.Web</AssemblyName>
    <OutputType>Library</OutputType>
  </PropertyGroup>
  <ItemGroup>
    <ProjectReference Include="..\BusinessLogic\BusinessLogic.csproj" />
    <ProjectReference Include="..\DataAccess\DataAccess.csproj" />
  </ItemGroup>
  <ItemGroup>
    <PackageReference Include="Newtonsoft.Json" Version="13.0.1" />
  </ItemGroup>
</Project>
"#;

    const SAMPLE_BL_CSPROJ: &str = r#"
<Project Sdk="Microsoft.NET.Sdk">
  <PropertyGroup>
    <TargetFramework>net48</TargetFramework>
    <RootNamespace>MyApp.BusinessLogic</RootNamespace>
    <AssemblyName>MyApp.BusinessLogic</AssemblyName>
    <OutputType>Library</OutputType>
  </PropertyGroup>
  <ItemGroup>
    <ProjectReference Include="..\DataAccess\DataAccess.csproj" />
  </ItemGroup>
</Project>
"#;

    const SAMPLE_DA_CSPROJ: &str = r#"
<Project Sdk="Microsoft.NET.Sdk">
  <PropertyGroup>
    <TargetFramework>net48</TargetFramework>
    <RootNamespace>MyApp.DataAccess</RootNamespace>
    <AssemblyName>MyApp.DataAccess</AssemblyName>
    <OutputType>Library</OutputType>
  </PropertyGroup>
</Project>
"#;

    #[test]
    fn parse_sln_with_three_projects() {
        let projects = parse_solution(SAMPLE_SLN);
        // Should exclude solution folder
        assert_eq!(projects.len(), 3);
        assert!(projects.iter().any(|p| p.name == "WebApp"));
        assert!(projects.iter().any(|p| p.name == "BusinessLogic"));
        assert!(projects.iter().any(|p| p.name == "DataAccess"));
    }

    #[test]
    fn parse_csproj_with_project_references() {
        let info = parse_project_file(SAMPLE_CSPROJ, "WebApp/WebApp.csproj");
        assert_eq!(info.root_namespace.as_deref(), Some("MyApp.Web"));
        assert_eq!(info.target_framework.as_deref(), Some("net48"));
        assert_eq!(info.project_references.len(), 2);
        assert!(
            info.project_references
                .iter()
                .any(|r| r.contains("BusinessLogic"))
        );
        assert!(
            info.project_references
                .iter()
                .any(|r| r.contains("DataAccess"))
        );
        assert!(
            info.package_references
                .iter()
                .any(|r| r.name == "Newtonsoft.Json")
        );
    }

    #[test]
    fn detect_project_type_from_guid() {
        assert_eq!(
            classify_project_type("FAE04EC0-301F-11D3-BF4B-00C04F79EFBC"),
            ProjectType::CSharp
        );
        assert_eq!(
            classify_project_type("F184B08F-C81C-45F6-A57F-5ABD9991F28F"),
            ProjectType::VbNet
        );
        assert_eq!(
            classify_project_type("349C5851-65DF-11DA-9384-00065B846F21"),
            ProjectType::WebApplication
        );
        assert_eq!(
            classify_project_type("E24C65DC-7377-472B-9ABA-BC803B73C61A"),
            ProjectType::WebSite
        );
        assert_eq!(
            classify_project_type("3AC096D0-A1C2-E12C-1390-A8335801FDAB"),
            ProjectType::TestProject
        );
    }

    #[test]
    fn extract_root_namespace_and_target_framework() {
        let info = parse_project_file(SAMPLE_CSPROJ, "test.csproj");
        assert_eq!(info.root_namespace.as_deref(), Some("MyApp.Web"));
        assert_eq!(info.target_framework.as_deref(), Some("net48"));
        assert_eq!(info.output_type.as_deref(), Some("Library"));
    }

    #[test]
    fn topological_sort_correct_wave_ordering() {
        let mut project_files = HashMap::new();
        project_files.insert("WebApp".to_string(), SAMPLE_CSPROJ.to_string());
        project_files.insert("BusinessLogic".to_string(), SAMPLE_BL_CSPROJ.to_string());
        project_files.insert("DataAccess".to_string(), SAMPLE_DA_CSPROJ.to_string());

        let structure = build_solution_structure(SAMPLE_SLN, &project_files);
        let order = &structure.migration_order;

        // DataAccess has no deps → first
        // BusinessLogic depends on DataAccess → second
        // WebApp depends on both → last
        let da_pos = order.iter().position(|n| n == "DataAccess");
        let bl_pos = order.iter().position(|n| n == "BusinessLogic");
        let wa_pos = order.iter().position(|n| n == "WebApp");

        assert!(da_pos.is_some());
        assert!(bl_pos.is_some());
        assert!(wa_pos.is_some());
        assert!(
            da_pos < bl_pos,
            "DataAccess should come before BusinessLogic"
        );
        assert!(bl_pos < wa_pos, "BusinessLogic should come before WebApp");
    }

    #[test]
    fn shared_library_detected() {
        let mut project_files = HashMap::new();
        project_files.insert("WebApp".to_string(), SAMPLE_CSPROJ.to_string());
        project_files.insert("BusinessLogic".to_string(), SAMPLE_BL_CSPROJ.to_string());
        project_files.insert("DataAccess".to_string(), SAMPLE_DA_CSPROJ.to_string());

        let structure = build_solution_structure(SAMPLE_SLN, &project_files);
        // DataAccess is referenced by both WebApp and BusinessLogic
        assert!(
            structure
                .shared_libraries
                .contains(&"DataAccess".to_string())
        );
    }

    #[test]
    fn test_project_excluded_from_classification() {
        let type_guid = "3AC096D0-A1C2-E12C-1390-A8335801FDAB";
        assert_eq!(classify_project_type(type_guid), ProjectType::TestProject);
    }

    #[test]
    fn scaffold_uses_correct_namespace() {
        let mut project_files = HashMap::new();
        project_files.insert("WebApp".to_string(), SAMPLE_CSPROJ.to_string());
        project_files.insert("BusinessLogic".to_string(), SAMPLE_BL_CSPROJ.to_string());
        project_files.insert("DataAccess".to_string(), SAMPLE_DA_CSPROJ.to_string());

        let structure = build_solution_structure(SAMPLE_SLN, &project_files);
        assert_eq!(
            resolve_namespace(&structure, "WebApp"),
            Some("MyApp.Web".into())
        );
        assert_eq!(
            resolve_namespace(&structure, "BusinessLogic"),
            Some("MyApp.BusinessLogic".into())
        );
    }

    #[test]
    fn scaffold_references_shared_namespace() {
        let mut project_files = HashMap::new();
        project_files.insert("WebApp".to_string(), SAMPLE_CSPROJ.to_string());
        project_files.insert("BusinessLogic".to_string(), SAMPLE_BL_CSPROJ.to_string());
        project_files.insert("DataAccess".to_string(), SAMPLE_DA_CSPROJ.to_string());

        let structure = build_solution_structure(SAMPLE_SLN, &project_files);
        let da_ns = resolve_namespace(&structure, "DataAccess");
        assert_eq!(da_ns, Some("MyApp.DataAccess".into()));
    }

    #[test]
    fn cross_project_blast_radius_multiplier() {
        let mut project_files = HashMap::new();
        project_files.insert("WebApp".to_string(), SAMPLE_CSPROJ.to_string());
        project_files.insert("BusinessLogic".to_string(), SAMPLE_BL_CSPROJ.to_string());
        project_files.insert("DataAccess".to_string(), SAMPLE_DA_CSPROJ.to_string());

        let structure = build_solution_structure(SAMPLE_SLN, &project_files);
        // DataAccess is referenced by 2 projects → 1.5x multiplier
        let mult = cross_project_multiplier(&structure, "DataAccess");
        assert!((mult - 1.5).abs() < 0.01);
        // WebApp is referenced by 0 projects → 1.0x
        let mult_wa = cross_project_multiplier(&structure, "WebApp");
        assert!((mult_wa - 1.0).abs() < 0.01);
    }

    #[test]
    fn handle_circular_project_references() {
        let sln = r#"
Project("{FAE04EC0-301F-11D3-BF4B-00C04F79EFBC}") = "A", "A\A.csproj", "{A0000000-0000-0000-0000-000000000000}"
EndProject
Project("{FAE04EC0-301F-11D3-BF4B-00C04F79EFBC}") = "B", "B\B.csproj", "{B0000000-0000-0000-0000-000000000000}"
EndProject
"#;
        let a_proj = r#"<Project><ItemGroup><ProjectReference Include="..\B\B.csproj" /></ItemGroup></Project>"#;
        let b_proj = r#"<Project><ItemGroup><ProjectReference Include="..\A\A.csproj" /></ItemGroup></Project>"#;

        let mut files = HashMap::new();
        files.insert("A".to_string(), a_proj.to_string());
        files.insert("B".to_string(), b_proj.to_string());

        let structure = build_solution_structure(sln, &files);
        // Should not crash, and should warn about cycle
        assert_eq!(structure.migration_order.len(), 2);
        assert!(!structure.warnings.is_empty());
    }

    #[test]
    fn handle_solution_folders() {
        let projects = parse_solution(SAMPLE_SLN);
        // "Solution Items" should be filtered out
        assert!(projects.iter().all(|p| p.name != "Solution Items"));
    }

    #[test]
    fn parse_legacy_vbproj_with_imports() {
        let vbproj = r#"
<Project ToolsVersion="4.0" xmlns="http://schemas.microsoft.com/developer/msbuild/2003">
  <Import Project="$(MSBuildExtensionsPath)\$(MSBuildToolsVersion)\Microsoft.Common.props" />
  <PropertyGroup>
    <RootNamespace>LegacyApp</RootNamespace>
    <AssemblyName>LegacyApp</AssemblyName>
    <TargetFrameworkVersion>v4.7.2</TargetFrameworkVersion>
    <OutputType>Library</OutputType>
  </PropertyGroup>
  <ItemGroup>
    <Reference Include="System" />
    <Reference Include="System.Data" />
    <Compile Include="Class1.vb" />
    <Compile Include="Models\User.vb" />
  </ItemGroup>
</Project>
"#;
        let info = parse_project_file(vbproj, "LegacyApp/LegacyApp.vbproj");
        assert_eq!(info.root_namespace.as_deref(), Some("LegacyApp"));
        assert_eq!(info.target_framework.as_deref(), Some("v4.7.2"));
        assert_eq!(info.source_files.len(), 2);
        assert!(info.source_files.contains(&"Class1.vb".to_string()));
        // Assembly references captured
        assert!(
            info.package_references
                .iter()
                .any(|r| r.name == "System.Data")
        );
    }

    #[test]
    fn parse_solution_configurations() {
        let configs = parse_solution_configs(SAMPLE_SLN);
        // The sample .sln has "Debug|Any CPU = ..." lines
        assert!(
            configs.contains(&"Debug".to_string()) || configs.contains(&"Release".to_string()),
            "configs: {configs:?}"
        );
    }

    // ── New tests ──────────────────────────────────────────────────────────────

    #[test]
    fn parse_empty_sln_returns_empty() {
        let projects = parse_solution("");
        assert!(projects.is_empty(), "Empty .sln should return no projects");
    }

    #[test]
    fn parse_single_csharp_project() {
        let sln = r#"
Project("{FAE04EC0-301F-11D3-BF4B-00C04F79EFBC}") = "MyLib", "MyLib\MyLib.csproj", "{11111111-1111-1111-1111-111111111111}"
EndProject
"#;
        let projects = parse_solution(sln);
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].name, "MyLib");
        assert_eq!(projects[0].project_type, ProjectType::CSharp);
    }

    #[test]
    fn parse_single_vb_project() {
        let sln = r#"
Project("{F184B08F-C81C-45F6-A57F-5ABD9991F28F}") = "VbApp", "VbApp\VbApp.vbproj", "{22222222-2222-2222-2222-222222222222}"
EndProject
"#;
        let projects = parse_solution(sln);
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].name, "VbApp");
        assert_eq!(projects[0].project_type, ProjectType::VbNet);
    }

    #[test]
    fn parse_multiple_projects() {
        let projects = parse_solution(SAMPLE_SLN);
        assert_eq!(projects.len(), 3, "Should parse exactly 3 real projects (solution folder excluded)");
    }

    #[test]
    fn project_name_extracted() {
        let projects = parse_solution(SAMPLE_SLN);
        assert!(projects.iter().any(|p| p.name == "WebApp"));
        assert!(projects.iter().any(|p| p.name == "BusinessLogic"));
        assert!(projects.iter().any(|p| p.name == "DataAccess"));
    }

    #[test]
    fn project_path_extracted() {
        let projects = parse_solution(SAMPLE_SLN);
        let web = projects.iter().find(|p| p.name == "WebApp").unwrap();
        // Backslashes are normalized to forward slashes
        assert!(web.relative_path.contains("WebApp"), "path: {}", web.relative_path);
        assert!(web.relative_path.ends_with(".csproj"), "path: {}", web.relative_path);
        assert!(!web.relative_path.contains('\\'), "backslash should be normalized");
    }

    #[test]
    fn project_guid_extracted() {
        let sln = r#"
Project("{FAE04EC0-301F-11D3-BF4B-00C04F79EFBC}") = "Alpha", "Alpha\Alpha.csproj", "{AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE}"
EndProject
"#;
        let projects = parse_solution(sln);
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].project_guid, "AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE");
        assert_eq!(projects[0].type_guid, "FAE04EC0-301F-11D3-BF4B-00C04F79EFBC");
    }

    #[test]
    fn project_type_csharp_detected() {
        let projects = parse_solution(SAMPLE_SLN);
        // All three projects in SAMPLE_SLN use the C# GUID
        assert!(projects.iter().all(|p| p.project_type == ProjectType::CSharp));
    }

    #[test]
    fn project_type_vbnet_detected() {
        let sln = r#"
Project("{F184B08F-C81C-45F6-A57F-5ABD9991F28F}") = "VbLib", "VbLib\VbLib.vbproj", "{33333333-3333-3333-3333-333333333333}"
EndProject
"#;
        let projects = parse_solution(sln);
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].project_type, ProjectType::VbNet);
    }

    #[test]
    fn solution_folder_not_treated_as_project() {
        // 2150E333 is the solution folder GUID
        let sln = r#"
Project("{FAE04EC0-301F-11D3-BF4B-00C04F79EFBC}") = "RealProject", "RealProject\RealProject.csproj", "{AAAA0000-0000-0000-0000-000000000001}"
EndProject
Project("{2150E333-8FDC-42A3-9474-1A3956D46DE8}") = "Solution Items", "Solution Items", "{BBBB0000-0000-0000-0000-000000000002}"
EndProject
"#;
        let projects = parse_solution(sln);
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].name, "RealProject");
    }

    #[test]
    fn project_build_configuration_parsed() {
        let configs = parse_solution_configs(SAMPLE_SLN);
        assert!(configs.contains(&"Debug".to_string()), "configs: {configs:?}");
        assert!(configs.contains(&"Release".to_string()), "configs: {configs:?}");
    }

    #[test]
    fn parse_sln_configurations_deduplicated() {
        let sln = r#"
Global
    GlobalSection(SolutionConfigurationPlatforms) = preSolution
        Debug|Any CPU = Debug|Any CPU
        Debug|x64 = Debug|x64
        Release|Any CPU = Release|Any CPU
        Release|x64 = Release|x64
    EndGlobalSection
EndGlobal
"#;
        let configs = parse_solution_configs(sln);
        // "Debug" appears twice but should only be in the set once
        let debug_count = configs.iter().filter(|c| c.as_str() == "Debug").count();
        assert_eq!(debug_count, 1, "Debug should appear only once: {configs:?}");
    }

    #[test]
    fn project_file_root_namespace_extracted() {
        let info = parse_project_file(SAMPLE_CSPROJ, "WebApp/WebApp.csproj");
        assert_eq!(info.root_namespace.as_deref(), Some("MyApp.Web"));
    }

    #[test]
    fn project_file_assembly_name_extracted() {
        let info = parse_project_file(SAMPLE_CSPROJ, "WebApp/WebApp.csproj");
        assert_eq!(info.assembly_name.as_deref(), Some("MyApp.Web"));
    }

    #[test]
    fn project_file_target_framework_extracted() {
        let info = parse_project_file(SAMPLE_CSPROJ, "WebApp/WebApp.csproj");
        assert_eq!(info.target_framework.as_deref(), Some("net48"));
    }

    #[test]
    fn project_file_output_type_extracted() {
        let info = parse_project_file(SAMPLE_CSPROJ, "WebApp/WebApp.csproj");
        assert_eq!(info.output_type.as_deref(), Some("Library"));
    }

    #[test]
    fn project_file_project_references_extracted() {
        let info = parse_project_file(SAMPLE_CSPROJ, "WebApp/WebApp.csproj");
        assert_eq!(info.project_references.len(), 2);
        assert!(info.project_references.iter().any(|r| r.contains("BusinessLogic")));
        assert!(info.project_references.iter().any(|r| r.contains("DataAccess")));
    }

    #[test]
    fn project_references_backslash_normalized() {
        let csproj = r#"<Project><ItemGroup><ProjectReference Include="..\Shared\Shared.csproj" /></ItemGroup></Project>"#;
        let info = parse_project_file(csproj, "App/App.csproj");
        assert_eq!(info.project_references.len(), 1);
        // Backslashes must be normalized to forward slashes
        assert!(!info.project_references[0].contains('\\'), "ref: {}", info.project_references[0]);
        assert!(info.project_references[0].contains('/'));
    }

    #[test]
    fn package_reference_name_and_version_extracted() {
        let info = parse_project_file(SAMPLE_CSPROJ, "WebApp/WebApp.csproj");
        let nj = info.package_references.iter().find(|r| r.name == "Newtonsoft.Json");
        assert!(nj.is_some(), "Newtonsoft.Json not found");
        assert_eq!(nj.unwrap().version.as_deref(), Some("13.0.1"));
    }

    #[test]
    fn assembly_reference_captured_without_version() {
        let csproj = r#"<Project><ItemGroup>
  <Reference Include="System.Web" />
  <Reference Include="System.Data" />
</ItemGroup></Project>"#;
        let info = parse_project_file(csproj, "proj.csproj");
        let names: Vec<&str> = info.package_references.iter().map(|r| r.name.as_str()).collect();
        assert!(names.contains(&"System.Web"), "refs: {names:?}");
        assert!(names.contains(&"System.Data"), "refs: {names:?}");
        // Assembly references parsed without a version
        assert!(info.package_references.iter().all(|r| r.version.is_none()));
    }

    #[test]
    fn compile_includes_extracted() {
        let csproj = r#"<Project><ItemGroup>
  <Compile Include="Form1.cs" />
  <Compile Include="Services\DataService.cs" />
  <Compile Include="Models\UserModel.cs" />
</ItemGroup></Project>"#;
        let info = parse_project_file(csproj, "App/App.csproj");
        assert_eq!(info.source_files.len(), 3);
        assert!(info.source_files.iter().any(|f| f.contains("Form1.cs")));
        assert!(info.source_files.iter().any(|f| f.contains("DataService.cs")));
    }

    #[test]
    fn compile_includes_backslash_normalized() {
        let csproj = r#"<Project><ItemGroup>
  <Compile Include="Models\User.cs" />
</ItemGroup></Project>"#;
        let info = parse_project_file(csproj, "App/App.csproj");
        assert_eq!(info.source_files.len(), 1);
        assert!(!info.source_files[0].contains('\\'), "backslash in source file: {}", info.source_files[0]);
    }

    #[test]
    fn project_with_no_references_has_empty_lists() {
        let info = parse_project_file(SAMPLE_DA_CSPROJ, "DataAccess/DataAccess.csproj");
        assert!(info.project_references.is_empty());
        assert!(info.package_references.is_empty());
        assert!(info.source_files.is_empty());
    }

    #[test]
    fn dependency_graph_populated_correctly() {
        let mut project_files = HashMap::new();
        project_files.insert("WebApp".to_string(), SAMPLE_CSPROJ.to_string());
        project_files.insert("BusinessLogic".to_string(), SAMPLE_BL_CSPROJ.to_string());
        project_files.insert("DataAccess".to_string(), SAMPLE_DA_CSPROJ.to_string());

        let structure = build_solution_structure(SAMPLE_SLN, &project_files);

        // WebApp depends on BusinessLogic and DataAccess
        let wa_deps = structure.dependency_graph.get("WebApp").unwrap();
        assert!(wa_deps.iter().any(|d| d == "BusinessLogic"), "wa_deps: {wa_deps:?}");
        assert!(wa_deps.iter().any(|d| d == "DataAccess"), "wa_deps: {wa_deps:?}");

        // BusinessLogic depends on DataAccess
        let bl_deps = structure.dependency_graph.get("BusinessLogic").unwrap();
        assert!(bl_deps.iter().any(|d| d == "DataAccess"), "bl_deps: {bl_deps:?}");

        // DataAccess has no deps
        let da_deps = structure.dependency_graph.get("DataAccess").unwrap();
        assert!(da_deps.is_empty(), "da_deps: {da_deps:?}");
    }

    #[test]
    fn cross_project_multiplier_three_refs() {
        // When 3-5 projects reference a library, multiplier should be 2.0
        let sln = r#"
Project("{FAE04EC0-301F-11D3-BF4B-00C04F79EFBC}") = "Core", "Core\Core.csproj", "{C0000000-0000-0000-0000-000000000001}"
EndProject
Project("{FAE04EC0-301F-11D3-BF4B-00C04F79EFBC}") = "AppA", "AppA\AppA.csproj", "{A0000000-0000-0000-0000-000000000002}"
EndProject
Project("{FAE04EC0-301F-11D3-BF4B-00C04F79EFBC}") = "AppB", "AppB\AppB.csproj", "{B0000000-0000-0000-0000-000000000003}"
EndProject
Project("{FAE04EC0-301F-11D3-BF4B-00C04F79EFBC}") = "AppC", "AppC\AppC.csproj", "{D0000000-0000-0000-0000-000000000004}"
EndProject
"#;
        let core_proj = r#"<Project><PropertyGroup><RootNamespace>Core</RootNamespace></PropertyGroup></Project>"#;
        let app_proj = |core_path: &str| format!(r#"<Project><ItemGroup><ProjectReference Include="{core_path}" /></ItemGroup></Project>"#);

        let mut files = HashMap::new();
        files.insert("Core".to_string(), core_proj.to_string());
        files.insert("AppA".to_string(), app_proj("..\\Core\\Core.csproj"));
        files.insert("AppB".to_string(), app_proj("..\\Core\\Core.csproj"));
        files.insert("AppC".to_string(), app_proj("..\\Core\\Core.csproj"));

        let structure = build_solution_structure(sln, &files);
        let mult = cross_project_multiplier(&structure, "Core");
        assert!(
            (mult - 2.0).abs() < 0.01,
            "3 references should give 2.0x multiplier, got {mult}"
        );
    }

    #[test]
    fn classify_web_application_type() {
        assert_eq!(
            classify_project_type("349C5851-65DF-11DA-9384-00065B846F21"),
            ProjectType::WebApplication
        );
    }

    #[test]
    fn classify_web_site_type() {
        assert_eq!(
            classify_project_type("E24C65DC-7377-472B-9ABA-BC803B73C61A"),
            ProjectType::WebSite
        );
    }

    #[test]
    fn classify_wpf_application_type() {
        assert_eq!(
            classify_project_type("60DC8134-EBA5-43B8-BCC9-BB4BC16C2548"),
            ProjectType::WpfApplication
        );
    }

    #[test]
    fn classify_unknown_guid_returns_unknown() {
        assert_eq!(
            classify_project_type("00000000-0000-0000-0000-000000000000"),
            ProjectType::Unknown
        );
    }

    #[test]
    fn classify_project_type_case_insensitive() {
        // GUIDs in .sln files may appear in different cases
        assert_eq!(
            classify_project_type("fae04ec0-301f-11d3-bf4b-00c04f79efbc"),
            ProjectType::CSharp
        );
    }

    #[test]
    fn no_circular_dependency_warning_for_linear_chain() {
        let mut project_files = HashMap::new();
        project_files.insert("WebApp".to_string(), SAMPLE_CSPROJ.to_string());
        project_files.insert("BusinessLogic".to_string(), SAMPLE_BL_CSPROJ.to_string());
        project_files.insert("DataAccess".to_string(), SAMPLE_DA_CSPROJ.to_string());

        let structure = build_solution_structure(SAMPLE_SLN, &project_files);
        assert!(
            structure.warnings.is_empty(),
            "Linear chain should produce no warnings: {:?}",
            structure.warnings
        );
    }

    #[test]
    fn project_file_info_path_set_correctly() {
        let info = parse_project_file(SAMPLE_CSPROJ, "src/WebApp/WebApp.csproj");
        assert_eq!(info.project_path, "src/WebApp/WebApp.csproj");
    }

    #[test]
    fn empty_project_file_returns_empty_info() {
        let info = parse_project_file("", "empty.csproj");
        assert!(info.root_namespace.is_none());
        assert!(info.assembly_name.is_none());
        assert!(info.target_framework.is_none());
        assert!(info.output_type.is_none());
        assert!(info.project_references.is_empty());
        assert!(info.package_references.is_empty());
        assert!(info.source_files.is_empty());
    }
}
