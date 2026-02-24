#![allow(clippy::unwrap_used)]
use engram_server::*;
use schemars::schema_for;

#[test]
fn test_tool_schemas_golden() {
    // 1. search_memory
    let schema = serde_json::to_string(&schema_for!(SearchMemoryRequest)).unwrap();
    assert!(
        schema.contains("query"),
        "SearchMemoryRequest missing 'query'"
    );
    assert!(
        schema.contains("project_id"),
        "SearchMemoryRequest missing 'project_id'"
    );
    assert!(
        schema.contains("namespace"),
        "SearchMemoryRequest missing 'namespace'"
    );

    // 2. index_project
    let schema = serde_json::to_string(&schema_for!(IndexProjectRequest)).unwrap();
    assert!(
        schema.contains("directory"),
        "IndexProjectRequest missing 'directory'"
    );
    assert!(
        schema.contains("project_name"),
        "IndexProjectRequest missing 'project_name'"
    );

    // 3. update_memory_bank
    let schema = serde_json::to_string(&schema_for!(UpdateMemoryBankRequest)).unwrap();
    assert!(
        schema.contains("section"),
        "UpdateMemoryBankRequest missing 'section' (parity)"
    );
    assert!(
        schema.contains("content"),
        "UpdateMemoryBankRequest missing 'content'"
    );

    // 4. add_repo_rule
    let schema = serde_json::to_string(&schema_for!(AddRepoRuleRequest)).unwrap();
    assert!(
        schema.contains("priority"),
        "AddRepoRuleRequest missing 'priority' (parity)"
    );
}
