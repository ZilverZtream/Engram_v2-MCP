Imports System
Imports System.Globalization

Public NotInheritable Class EngramInstrumentation
    Private Sub New()
    End Sub

    ' Call explicitly at an observed event; supply your application's log sink.
    Public Shared Sub LogEngramEvent(sink As Action(Of String), sourcePath As String,
                                    eventName As String, controlId As String, sqlNodeId As String)
        If sink Is Nothing Then Throw New ArgumentNullException("sink")
        sourcePath = Field(sourcePath, "sourcePath", False)
        eventName = Field(eventName, "eventName", False)
        controlId = Field(controlId, "controlId", True)
        sqlNodeId = Field(sqlNodeId, "sqlNodeId", False)
        Const prefix As String = "sql:inline:"
        If Not sqlNodeId.StartsWith(prefix, StringComparison.Ordinal) OrElse sqlNodeId.Length = prefix.Length Then
            Throw New ArgumentException("Supply an existing sql:inline:<id> node; stored procedures are not supported by this log importer.", "sqlNodeId")
        End If
        sink(String.Join("|", New String() {"ENGRAM_LOG", DateTime.UtcNow.ToString("o", CultureInfo.InvariantCulture),
            sourcePath, eventName, controlId, sqlNodeId.Substring(prefix.Length)}))
    End Sub

    Private Shared Function Field(value As String, name As String, allowEmpty As Boolean) As String
        value = If(value, String.Empty)
        If (Not allowEmpty AndAlso String.IsNullOrWhiteSpace(value)) OrElse value.IndexOfAny(New Char() {"|"c, ChrW(13), ChrW(10)}) >= 0 Then
            Throw New ArgumentException("A log field must not contain a record delimiter or newline; required fields must not be empty.", name)
        End If
        Return value
    End Function
End Class
