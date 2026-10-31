# Engram working instructions

- Tests establish wanted Engram behavior. Do not add tests that merely repeat an implementation.
- Push only to `origin/master`. Do not add AI attribution or coauthor trailers, or create tags.
- Preserve existing uncommitted work and commits that exist only locally.
- Reuse the existing checkout and build target. Create an isolated worktree only when needed to protect work or separate branches; record its exact path and removal condition.
- Keep temporary work in one recorded location. Do not scatter copies across drive roots, AppData, or unrelated folders, or expand scratch volumes to accommodate leftovers.
- Every test fixture owns its temporary resources and cleans them up on completion and unwinding. Close database handles before removing their directories. Do not use `mem::forget`, `TempDir::keep`, or `into_path` to avoid managing fixture lifetimes.
- If a runner intentionally retains evidence or an interrupted run leaves artifacts, record their location, purpose, and cleanup condition. A completed task includes accounting for its temporary resources.
- Existing folders are not disposable merely because their names or ages look temporary. Check unique work, ignored files, Git history, references, and active use before proposing removal. Remove registered worktrees through Git.
- The current inventory of preexisting project copies and temporary folders is read-only. Further deletion requires approval of exact paths. This does not prevent teardown of new fixtures owned by the current test run.
