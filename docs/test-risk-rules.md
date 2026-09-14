# Hot-reloaded test risk rules

`derive_test_matrix` can add organization and repository-specific test axes without rebuilding or restarting Engram.

Engram reads these files on every call:

1. `<data_dir>/rules/test-risk-rules.yaml` for organization defaults.
2. `<project>/.engram/test-risk-rules.yaml` for repository policy.

Rules merge by `id`; a repository rule with the same ID replaces the organization rule. Engram does not create either file. Missing packs are valid. Invalid packs are reported as incomplete evidence in the tool response rather than silently ignored.

Maintained framework packs live under `rule-packs/`. Install or merge the relevant rules into the organization file above. For example, `rule-packs/dotnet-web.yaml` covers ASP.NET session-lock, authentication cleanup and principal-nullability hazards. These packs are data: editing the installed YAML takes effect on the next tool call without rebuilding or restarting Engram.

```yaml
version: 1
rules:
  - id: vb.no-single-line-if
    title: Repository conditional style
    guidance: Expand the conditional and exercise both branches.
    extensions: [vb]
    any_terms: [" Then Return ", " Then Throw "]
    none_terms: ["' generated"]

  - id: webforms.deferred-binding
    title: Repository control binding lifecycle
    guidance: Prove the property is assigned before Init/Load consumers and after refresh.
    severity: warning
    extensions: [aspx, ascx, master]
    path_any: [modules/, controls/]
    all_terms: ["<%#"]
    any_terms: [DataBind, TablePrefix]
```

All predicates are case-insensitive literal substrings. `extensions`, `path_any`, `all_terms`, `any_terms`, and `none_terms` are optional, but every rule needs at least one predicate. `all_terms` must all occur, at least one `any_terms` value must occur, no `none_terms` value may occur, and at least one `path_any` value must occur. An empty predicate list places no restriction on that dimension. `severity` is optional and defaults to `warning`; allowed values are `critical`, `warning`, `info`, and `style`.

The built-in matrix also derives terminal-boundary checks that should not need a repository rule:

- HTML attributes, DOM properties and popover/tooltip/`innerHTML` flows are treated as multi-stage sinks. Encoding an intermediate attribute does not prove safety after browser decoding or plugin reinterpretation.
- Spreadsheet writers require upstream-length reconciliation and exact 32,767/32,768-character Excel cases.
- `VARCHAR(MAX)`, `NVARCHAR(MAX)` and equivalent unbounded persistence require an explicit retention, duplication, audience and volume decision plus downstream-limit checks.
- When `change_intent` contains approved literal mappings or schema constraints, the matrix emits an invariant-fidelity axis. Pass the complete approved contract and decision table rather than only the story title.

For VB changes, the built-in matrix can also detect an incomplete migration from string literals to a canonical member. It activates only when a requested file's current HEAD-to-worktree diff adds a member argument whose name contains a canonical marker such as `prefix`, `token`, or `constant`, or when the exact member appears in `change_intent`. Engram then uses the Roslyn sidecar to find string literals passed to the same callee and argument identity across the bounded project scan, including multiline and named arguments. Resolved parameter ordinals connect named and positional forms when Roslyn has enough semantic information; otherwise the output states that matching is lexical.

The migration sweep is review evidence rather than an instruction to edit every match. It prioritizes requested files, excludes generated and dependency directories, caps candidate members, source bytes, files, invocations, arguments, results, and response size, and reports every cap or parser failure as `INCOMPLETE`. Diff collection is limited to literal requested paths and combines staged and unstaged changes into final worktree coordinates. Older or unavailable sidecars fail closed without a regex fallback.

The loader accepts version 1, at most 128 rules per pack, at most 32 values per predicate list, and files no larger than 256 KiB. It rejects unknown YAML fields, duplicate IDs inside a pack, control characters, and repository-pack paths that resolve outside the repository. The format deliberately has no regular expressions, scripts, commands, or templating.

Each emitted matrix axis names its stable rule ID and whether it came from the organization or repository pack. The same rule is checked against added diff content by `pre_commit_review`, so repository policy participates before implementation and again before completion. Rules propose tests or review work; they do not certify behavior or execute commands.
