use engram_index::vb_extractor::extract_vb_fallback_for_eval;
use std::path::Path;

#[test]
fn prose_and_non_declarations_do_not_create_functions() {
    let source = r#"Namespace Sample
Public Class Calculator
    ''' <summary>This helper function computes a total.</summary>
    ' The next function documents the implementation.
    REM This function describes the algorithm.
    Public Function Calculate() As String
        Dim message = "This function explains the result"
        Dim value = 1 ' function narrates an assignment
        Return message
    End Function ' finished
    Public Sub Reset()
    End Sub ' finished
End Class
End Namespace"#;
    let (symbols, _) = extract_vb_fallback_for_eval(Path::new("calculator.vb"), source);
    let methods: Vec<_> = symbols
        .iter()
        .filter(|s| s.kind == "function")
        .map(|s| (s.name.as_str(), s.start_line, s.end_line))
        .collect();
    assert_eq!(
        methods,
        vec![
            ("Sample.Calculator.Calculate", 6, 10),
            ("Sample.Calculator.Reset", 11, 12)
        ]
    );
}

#[test]
fn declaration_modifiers_and_multiline_signatures_keep_real_methods() {
    let source = r#"Public MustInherit Class Worker
    Protected Friend Overridable Function Read(
        value As Integer
    ) As Integer
        Return value
    End Function
    Public Shared Async Function Run() As Task
    End Function
    Private Iterator Function Items() As IEnumerable(Of Integer)
    End Function
    Public MustOverride Sub Execute()
    Sub Reset()
    End Sub
End Class"#;
    let (symbols, _) = extract_vb_fallback_for_eval(Path::new("worker.vb"), source);
    let methods: Vec<_> = symbols
        .iter()
        .filter(|s| s.kind == "function")
        .map(|s| s.name.as_str())
        .collect();
    assert_eq!(
        methods,
        vec![
            "Worker.Read",
            "Worker.Run",
            "Worker.Items",
            "Worker.Execute",
            "Worker.Reset"
        ]
    );
}

#[test]
fn attributed_generic_and_external_declarations_keep_real_methods() {
    let source = r#"Public Module Helpers
    <Obsolete> Public Function Identity(Of T)(value As T) As T
        Return value
    End Function
    Public Declare Unicode Function NativeCall Lib "sample" () As Integer
    Public Delegate Sub Callback(value As Integer)
    Private Sub [Select]()
    End Sub
End Module"#;
    let (symbols, _) = extract_vb_fallback_for_eval(Path::new("helpers.vb"), source);
    let methods: Vec<_> = symbols
        .iter()
        .filter(|s| s.kind == "function")
        .map(|s| s.name.as_str())
        .collect();
    assert_eq!(
        methods,
        vec![
            "Helpers.Identity",
            "Helpers.NativeCall",
            "Helpers.Callback",
            "Helpers.[Select]"
        ]
    );
}
#[test]
fn inline_attribute_greater_than_in_string_keeps_method() {
    let source = r#"Public Class Reader
    <DisplayName("a > b")> Public Function Read() As Integer
        Return 1
    End Function
End Class"#;
    let (symbols, _) = extract_vb_fallback_for_eval(Path::new("reader.vb"), source);
    let names: Vec<_> = symbols
        .iter()
        .filter(|s| s.kind == "function")
        .map(|s| s.name.as_str())
        .collect();
    assert_eq!(names, ["Reader.Read"]);
}

#[test]
fn inline_attribute_function_token_does_not_rename_method() {
    let source = r#"Public Class Reader
    <Description("a Function Wrong")> Public Function Right() As Integer
        Return 1
    End Function
End Class"#;
    let (symbols, _) = extract_vb_fallback_for_eval(Path::new("reader.vb"), source);
    let names: Vec<_> = symbols
        .iter()
        .filter(|s| s.kind == "function")
        .map(|s| s.name.as_str())
        .collect();
    assert_eq!(names, ["Reader.Right"]);
}

#[test]
fn multiple_attributes_with_escaped_quotes_keep_method_name() {
    let source = r#"Public Class Reader
    <Description("a ""quoted > Function Wrong"" value")> <Obsolete> Public Function Right() As Integer
        Return 1
    End Function
End Class"#;
    let (symbols, _) = extract_vb_fallback_for_eval(Path::new("reader.vb"), source);
    let names: Vec<_> = symbols
        .iter()
        .filter(|s| s.kind == "function")
        .map(|s| s.name.as_str())
        .collect();
    assert_eq!(names, ["Reader.Right"]);
}
