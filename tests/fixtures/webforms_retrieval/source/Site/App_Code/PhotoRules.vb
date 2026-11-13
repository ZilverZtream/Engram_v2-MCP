Public Class PhotoRules
    ' Completion requires the configured minimum photos for each required group.
    Public Function CanComplete(groups As IEnumerable(Of PhotoGroup)) As Boolean
        For Each group In groups
            If group.UploadedPhotos.Count < group.RequiredImageCount Then Return False
        Next
        Return True
    End Function
End Class
