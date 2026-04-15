using System;
using System.Data.SqlClient;
using Dapper;
using System.Web.UI;

namespace LegacyApp {
    public partial class OrdersPage : Page {
        public event EventHandler SaveCompleted;
        public delegate void SaveDelegate(int id);
        public string Title { get; set; }

        public OrdersPage() {
            this.Load += this.Page_Load;
        }

        protected override void OnInit(EventArgs e) {
            base.OnInit(e);
            btnSave.Click += btnSave_Click;
        }

        protected void Page_Load(object sender, EventArgs e) {
            void LocalAudit() { }
            LocalAudit();
        }

        protected void btnSave_Click(object sender, EventArgs e) {
            var cmd = new SqlCommand("SELECT Id FROM Orders");
            cmd.CommandText = "EXEC proc_SaveOrder";
            connection.Query("SELECT Name FROM Customers");
        }
    }
}
