<%@ Page Language="VB" AutoEventWireup="false" CodeBehind="Order.aspx.vb" Inherits="LegacyApp.OrderPage" %>
<!DOCTYPE html>
<html>
<body>
    <form id="form1" runat="server">
        <asp:LinkButton ID="lbCancel" runat="server" OnClick="lbCancel_Click">Cancel</asp:LinkButton>
        <asp:Button ID="btnSave" runat="server" Text="Save" />
    </form>
</body>
</html>
