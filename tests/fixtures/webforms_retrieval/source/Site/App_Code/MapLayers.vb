Public Class MapLayers
    ' Visibility belongs to the owning project; a shared layer is also visible.
    Public Function IsVisible(layer As MapLayer, projectId As Integer) As Boolean
        Return layer.OwnerProjectId = projectId OrElse layer.IsShared
    End Function
End Class
