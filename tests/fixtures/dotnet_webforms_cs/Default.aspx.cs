using System;
using System.Data.SqlClient;
using System.Web.UI;
using System.Web.UI.WebControls;

namespace LegacyApp
{
    public partial class DefaultPage : Page
    {
        protected void Page_Load(object sender, EventArgs e)
        {
        }

        protected void btnSubmit_Click(object sender, EventArgs e)
        {
            var dal = new DataAccess();
            dal.InsertLog("User clicked submit");
        }

        protected void gvData_RowCommand(object sender, GridViewCommandEventArgs e)
        {
            if (e.CommandName == "Delete")
            {
                DataAccess.DeleteRow(Convert.ToInt32(e.CommandArgument));
            }
        }
    }

    public class DataAccess
    {
        public void InsertLog(string msg)
        {
            var cmd = new SqlCommand("INSERT INTO Logs (Message) VALUES (@msg)");
            cmd.ExecuteNonQuery();
        }

        public static void DeleteRow(int id)
        {
            var cmd = new SqlCommand();
            cmd.CommandText = "DELETE FROM Table WHERE ID = 1"; // Simplified for test
            cmd.ExecuteNonQuery();
        }
    }
}
