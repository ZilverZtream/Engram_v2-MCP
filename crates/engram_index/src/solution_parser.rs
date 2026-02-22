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
    match type_guid.to_uppercase().as_str() {
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
}
