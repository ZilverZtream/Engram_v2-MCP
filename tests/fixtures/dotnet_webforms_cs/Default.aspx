<%@ Page Language="C#" AutoEventWireup="true" CodeBehind="Default.aspx.cs" Inherits="LegacyApp.DefaultPage" %>
<!DOCTYPE html>
<html>
<body>
    <form id="form1" runat="server">
        <asp:Button ID="btnSubmit" runat="server" OnClick="btnSubmit_Click" Text="Submit" />
        <asp:GridView ID="gvData" runat="server" OnRowCommand="gvData_RowCommand" />
    </form>
</body>
</html>
