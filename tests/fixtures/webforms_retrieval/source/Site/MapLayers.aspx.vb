Public Class LayerPage
    Inherits System.Web.UI.Page
    ' Bind the layers grid after selecting project-visible records.
    Protected Sub LoadLayers(projectId As Integer)
        layers.DataSource = Repository.VisibleLayers(projectId)
        layers.DataBind()
    End Sub
End Class
