# Hot-reloaded test risk rules

`derive_test_matrix` can add organization and repository-specific test axes without rebuilding or restarting Engram.

Engram reads these files on every call:

1. `<data_dir>/rules/test-risk-rules.yaml` for organization defaults.
2. `<project>/.engram/test-risk-rules.yaml` for repository policy.

Rules merge by `id`; a repository rule with the same ID replaces the organization rule. Engram does not create either file. Missing packs are valid. Invalid packs are reported as incomplete evidence in the tool response rather than silently ignored.

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

The loader accepts version 1, at most 128 rules per pack, at most 32 values per predicate list, and files no larger than 256 KiB. It rejects unknown YAML fields, duplicate IDs inside a pack, control characters, and repository-pack paths that resolve outside the repository. The format deliberately has no regular expressions, scripts, commands, or templating.

Each emitted matrix axis names its stable rule ID and whether it came from the organization or repository pack. The same rule is checked against added diff content by `pre_commit_review`, so repository policy participates before implementation and again before completion. Rules propose tests or review work; they do not certify behavior or execute commands.
