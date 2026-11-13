Public Class Login
    Public Function ValidateCredentials(username As String, password As String) As Boolean
        If String.IsNullOrWhiteSpace(username) OrElse String.IsNullOrEmpty(password) Then Return False
        Dim account = Accounts.FindByUsername(username)
        Return account IsNot Nothing AndAlso PasswordHasher.Verify(password, account.PasswordHash)
    End Function
    Protected Sub SignIn(sender As Object, e As EventArgs)
        If ValidateCredentials(username.Text, password.Text) Then FormsAuthentication.SetAuthCookie(username.Text, False)
    End Sub
End Class
