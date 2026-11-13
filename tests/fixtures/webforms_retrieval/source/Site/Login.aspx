<%@ Page Language="VB" CodeFile="Login.aspx.vb" Inherits="Login" %>
<asp:TextBox ID="username" runat="server" />
<asp:TextBox ID="password" TextMode="Password" runat="server" />
<asp:Button ID="submit" runat="server" OnClick="SignIn" Text="Sign in" />
