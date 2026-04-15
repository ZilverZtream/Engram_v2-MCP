<%@ Page Language="C#" AutoEventWireup="true" CodeBehind="Search.aspx.cs" Inherits="LegacyApp.FrontendBridgePage" %>
<!DOCTYPE html>
<html>
<head>
    <script src="Scripts/uiTriggers.js"></script>
    <script src="Scripts/pageMethods.ts"></script>
</head>
<body>
    <form id="form1" runat="server">
        <asp:TextBox ID="txtQuery" runat="server" />
        <asp:Button ID="btnSearch" runat="server" OnClick="btnSearch_Click" Text="Search" />
        <asp:Label ID="lblResult" runat="server" />
    </form>
</body>
</html>
