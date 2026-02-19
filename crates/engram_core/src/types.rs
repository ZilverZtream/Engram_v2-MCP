use thiserror::Error;

pub type Result<T> = std::result::Result<T, EngramError>;

#[derive(Debug, Error)]
pub enum EngramError {
    #[error("config error: {0}")]
    Config(String),

    #[error("path not allowed: {0}")]
    PathNotAllowed(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("serialization error: {0}")]
    Serde(String),

    #[error("internal error: {0}")]
    Internal(String),
}

impl From<serde_yaml::Error> for EngramError {
    fn from(e: serde_yaml::Error) -> Self {
        EngramError::Serde(e.to_string())
    }
}

pub fn guess_language(path: &std::path::Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase()
        .as_str()
    {
        "rs" => "rust",
        "py" => "python",
        "js" => "javascript",
        "ts" => "typescript",
        "tsx" => "tsx",
        "jsx" => "jsx",
        "go" => "go",
        "java" => "java",
        "cs" => "csharp",
        "vb" => "vbnet",
        "cpp" | "cc" | "cxx" | "hpp" | "h" => "cpp",
        "c" => "c",
        "md" => "markdown",
        "json" => "json",
        "yaml" | "yml" => "yaml",
        // .NET WebForms / ASP.NET
        "aspx" | "ascx" | "master" => "aspx",
        "config" => "xml",
        "xml" => "xml",
        "sln" => "text",
        "csproj" | "vbproj" => "xml",
        "sql" => "sql",
        "rdlc" => "xml",
        _ => "text",
    }
}
