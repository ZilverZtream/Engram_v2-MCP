Public Class LoginAudit
    ' Records login attempts for support without validating credentials.
    Public Sub RecordAttempt(username As String, succeeded As Boolean)
        Audit.Write("login", username, succeeded)
    End Sub
End Class
