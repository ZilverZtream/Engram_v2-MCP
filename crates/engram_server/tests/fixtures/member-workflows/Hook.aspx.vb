Imports System.Data
Imports System.Data.SqlClient
Partial Class Hook
 Inherits System.Web.UI.Page
 Public Sub Page_Load(sender As Object, e As System.EventArgs) Handles Me.Load
  For Each id As Integer In New Integer() {1,2}
   Using cmd As New SqlCommand("SELECT id FROM dbo.Wanted WHERE id=@wanted")
    cmd.Parameters.Add("@wanted",SqlDbType.Int).Value=id
    cmd.ExecuteScalar()
   End Using
   ReadHelper(id)
  Next
 End Sub
 Public Sub Unrelated()
  For Each id As Integer In New Integer() {1,2}
   Using cmd As New SqlCommand("UPDATE dbo.Noise SET id=@noise")
    cmd.Parameters.Add("@noise",SqlDbType.Int).Value=id
    cmd.ExecuteNonQuery()
   End Using
  Next
 End Sub
 Private Sub ReadHelper(id As Integer)
  Using cmd As New SqlCommand("SELECT id FROM dbo.Helper WHERE id=@helper")
   cmd.Parameters.Add("@helper",SqlDbType.Int).Value=id
   cmd.ExecuteScalar()
  End Using
 End Sub
End Class
