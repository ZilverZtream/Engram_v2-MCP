//! External audit 2026-08-29 P0-3: the reference story names `iFalt.dbml`
//! (the LINQ-to-SQL model that changes with every table change) and it never
//! rendered — live or on the fixture — because `.dbml` is not an indexed
//! extension for .NET projects, so no retrieval arm can ever return it. The
//! Entity Framework twin (`.edmx`) has the same role.

use engram_server::utils::files::exts_for_project_type;

#[test]
fn dotnet_projects_index_the_orm_model_files() {
    for pt in ["dotnet_webforms_vb", "dotnet_webforms_cs"] {
        let exts = exts_for_project_type(pt);
        assert!(
            exts.contains(&"dbml"),
            "{pt}: .dbml (LINQ-to-SQL model) must be indexed: {exts:?}"
        );
        assert!(
            exts.contains(&"edmx"),
            "{pt}: .edmx (EF model) must be indexed: {exts:?}"
        );
    }
}

#[test]
fn the_model_files_are_read_as_xml() {
    assert_eq!(
        engram_core::guess_language(std::path::Path::new("Site/App_Code/iFalt.dbml")),
        "xml"
    );
    assert_eq!(
        engram_core::guess_language(std::path::Path::new("Models/Ocius.edmx")),
        "xml"
    );
}
