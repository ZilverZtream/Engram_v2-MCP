// Git service: git_update_stream, index_git_history, search_history, temporal couplings, reverts.
// The core git_update_stream logic is kept on the Engram impl (in handlers/git_tools.rs)
// because it needs &self access to ensure_project_runtime and get_active_generation.
// This module exists as a namespace for future extraction.
