;;; VB.NET tree-sitter query for Engram symbol + call-graph extraction.
;;;
;;; Grammar: arborium-vb 1.3.0 (tree-sitter ABI 14)
;;;
;;; Capture tags:
;;;   @ns         — namespace name (Pass 1 FQN)
;;;   @class      — class / module / structure / interface / enum name (Pass 1 + symbol)
;;;   @func       — method_declaration node (symbol, wraps @name)
;;;   @name       — identifier within the current @class / @func match
;;;   @call.name  — callee identifier for call-graph edges
;;;   @import     — imported namespace name string
;;;
;;; NOTE: SQL extraction is handled by regex post-processing (regex_extract_sql)
;;; rather than tree-sitter patterns, to avoid grammar-version-specific issues.

;;; Pass-1 namespace / FQN captures

(namespace_block (namespace_name) @ns)

;;; Type symbols

(class_block
  (identifier) @name @class)

(module_block
  (identifier) @name @class)

(structure_block
  (identifier) @name @class)

(interface_block
  (identifier) @name @class)

(enum_block
  (identifier) @name @class)

;;; Method / Sub / Function symbols

(method_declaration
  (identifier) @name @func)

(constructor_declaration) @func

;;; Property blocks

(property_declaration
  (identifier) @name @property)

;;; Event declarations

(event_declaration
  (identifier) @name @event)

;;; Field declarations (especially WithEvents for control resolution)

(field_declaration
  (variable_declarator
    (identifier) @name @field))

;;; Call graph: invocation targets
;;;
;;; tree-sitter matches at any depth, so these patterns capture invocations
;;; whether they appear as bare statements (call_statement -> expression -> invocation)
;;; or nested in expressions. Uses child syntax per the grammar highlights.scm.

;;; Direct call:  Foo(args)  or  Call Foo(args)
(invocation
  (identifier) @call.name)

;;; Member call:  obj.Bar(args)  -- capture the full access
(invocation
  (member_access) @call.name)

;;; Imports

(imports_statement
  namespace: (namespace_name) @import)
