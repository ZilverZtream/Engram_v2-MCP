using System;
using System.Data;
using System.Data.SqlClient;
using System.Web.Services;

namespace LegacyApp
{
    public partial class FrontendBridgePage : System.Web.UI.Page
    {
        protected void Page_Load(object sender, EventArgs e)
        {
        }

        protected void btnSearch_Click(object sender, EventArgs e)
        {
            var dal = new CustomerDal();
            lblResult.Text = dal.LookupCustomer(txtQuery.Text);
        }

        [WebMethod]
        public static string GetCustomer(string id)
        {
            var dal = new CustomerDal();
            return dal.LookupCustomer(id);
        }
    }

    public class CustomerDal
    {
        public string LookupCustomer(string customerId)
        {
            var cmd = new SqlCommand("SELECT Name FROM Customers WHERE CustomerId = @customerId");
            cmd.Parameters.AddWithValue("@customerId", customerId);
            var adapter = new SqlDataAdapter(cmd);
            var table = new DataTable();
            adapter.Fill(table);
            return table.Rows.Count > 0 ? table.Rows[0]["Name"].ToString() : "";
        }
    }
}
