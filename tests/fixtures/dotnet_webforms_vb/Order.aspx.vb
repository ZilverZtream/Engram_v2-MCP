Imports System.Data.SqlClient

Namespace LegacyApp
    Partial Public Class OrderPage
        Inherits System.Web.UI.Page

        Protected Sub Page_Load(ByVal sender As Object, ByVal e As System.EventArgs) Handles Me.Load
        End Sub

        Protected Sub lbCancel_Click(ByVal sender As Object, ByVal e As EventArgs)
            ' Handled by OnClick in markup
            Dim db As New DataLayer()
            db.CancelOrder(123)
        End Sub

        Protected Sub btnSave_Click(ByVal sender As Object, ByVal e As EventArgs) Handles btnSave.Click
            Dim db As New DataLayer()
            db.SaveOrder("new order")
        End Sub
    End Class

    Public Class DataLayer
        Public Sub SaveOrder(ByVal data As String)
            Dim cmd As New SqlCommand("proc_SaveOrder", Nothing)
            cmd.CommandType = System.Data.CommandType.StoredProcedure
            cmd.ExecuteNonQuery()
        End Sub

        Public Sub CancelOrder(ByVal id As Integer)
            Dim cmd As New SqlCommand()
            cmd.CommandText = "UPDATE Orders SET Status = 'Cancelled' WHERE ID = 1"
            cmd.ExecuteNonQuery()
        End Sub
    End Class
End Namespace
