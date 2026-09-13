# External generator receipts

`validate_generated_code` can bind an external IDE, compiler, or code generator
run to the exact files it produced. Engram does not launch the toolchain. It
validates a project-local JSON receipt and re-hashes its source and target files
against their current on-disk bytes.

Supply all of these fields together:

- `code_file` and its raw-byte `code_file_blake3`;
- the same exact project-relative path as `target_file`;
- project-relative `generator_receipt_file` and its raw-byte
  `generator_receipt_sha256`.

The receipt accepts snake_case names and the PascalCase aliases shown here:

```json
{
  "SourceFile": "C:\\project\\Model.schema",
  "CustomTool": "ExampleGenerator",
  "ProjectName": "Example",
  "SolutionFile": "C:\\project\\Example.sln",
  "VisualStudioVersion": "host-version",
  "Invoked": true,
  "Files": [
    {
      "Path": "C:\\project\\Generated.cs",
      "ExistedBefore": true,
      "ExistsAfter": true,
      "LengthBefore": 120,
      "LengthAfter": 142,
      "Sha256Before": "0000000000000000000000000000000000000000000000000000000000000000",
      "Sha256After": "1111111111111111111111111111111111111111111111111111111111111111",
      "Changed": true
    }
  ]
}
```

Absolute member paths are accepted only when they resolve inside the registered
project root; this permits receipts emitted by IDEs. The receipt file itself must
be project-relative. A valid receipt must identify the tool, say it was invoked,
cover its source and the requested target, contain internally consistent
before/after data, and match every current file it claims exists. Stale hashes,
path escapes, an uninvoked tool, and a missing target produce a failing check or
an input error. A successful receipt establishes generator provenance and byte
identity; it does not establish product behavior, database deployment, runtime
success, or semantic equivalence to another file.
