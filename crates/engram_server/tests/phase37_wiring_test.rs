//! Phase 37: Wiring — Integration tests for newly exposed tools.
//!
//! Tests: analyze_database_intelligence, get_sp_details, list_triggers,
//!        analyze_sync_hazards, get_jquery_inventory.

use engram_core::Config;
use engram_server::AppState;
use rmcp::handler::server::tool::Parameters;
use tempfile::tempdir;

// ── Helper ──────────────────────────────────────────────────────────────────

async fn setup_project(
    sql: &str,
    vb: &str,
    js: &str,
    aspx: &str,
) -> (engram_server::Engram, String, tempfile::TempDir) {
    let tmp = tempdir().unwrap();
    let root = tmp.path();

    if !sql.is_empty() {
        std::fs::write(root.join("database.sql"), sql).unwrap();
    }
    if !vb.is_empty() {
        std::fs::write(root.join("default.aspx.vb"), vb).unwrap();
    }
    if !js.is_empty() {
        std::fs::write(root.join("site.js"), js).unwrap();
    }
    if !aspx.is_empty() {
        std::fs::write(root.join("default.aspx"), aspx).unwrap();
    }

    let cfg = Config {
        allowed_roots: vec![root.to_path_buf()],
        data_dir: root.join("engram_data"),
        max_project_files: Some(200),
        max_project_bytes: Some(10 * 1024 * 1024),
        embedding_backend: "fts_only".into(),
        embedding_model: None,
        ollama_url: None,
        openai_api_key: None,
        max_concurrent_jobs: 2,
        ..Default::default()
    };
    std::fs::create_dir_all(&cfg.data_dir).unwrap();

    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = engram_server::Engram::new(state.clone());

    engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: root.to_string_lossy().to_string(),
            project_name: "Phase37Test".into(),
            project_type: "webforms".into(),
            wait: true,
            dedupe_by_directory: false,
        }))
        .await
        .unwrap();

    let projects = state.registry.list_projects().unwrap();
    let pid = projects[0].project_id.clone();

    (engram, pid, tmp)
}

// ── Fixture SQL ─────────────────────────────────────────────────────────────

const FIXTURE_SQL: &str = r#"
CREATE TABLE Customers (
    CustomerID INT NOT NULL PRIMARY KEY,
    FirstName NVARCHAR(100) NOT NULL,
    LastName NVARCHAR(100) NOT NULL,
    Email NVARCHAR(255) NULL,
    CreatedAt DATETIME NOT NULL DEFAULT GETDATE()
);

CREATE TABLE Orders (
    OrderID INT NOT NULL IDENTITY(1,1) PRIMARY KEY,
    CustomerID INT NOT NULL FOREIGN KEY REFERENCES Customers(CustomerID),
    OrderDate DATETIME NOT NULL DEFAULT GETDATE(),
    TotalAmount DECIMAL(18,2) NOT NULL,
    Status VARCHAR(20) NOT NULL DEFAULT 'Pending'
);

CREATE TABLE AuditLog (
    LogID INT NOT NULL IDENTITY(1,1) PRIMARY KEY,
    TableName VARCHAR(100) NOT NULL,
    Action VARCHAR(20) NOT NULL,
    RecordID INT NOT NULL,
    ChangedAt DATETIME NOT NULL DEFAULT GETDATE()
);

CREATE PROCEDURE sp_GetCustomerOrders
    @CustomerID INT,
    @Status VARCHAR(20) = NULL
AS
BEGIN
    SELECT o.OrderID, o.OrderDate, o.TotalAmount, o.Status
    FROM Orders o
    WHERE o.CustomerID = @CustomerID
    AND (@Status IS NULL OR o.Status = @Status)
END

CREATE PROCEDURE sp_CreateOrder
    @CustomerID INT,
    @TotalAmount DECIMAL(18,2)
AS
BEGIN
    BEGIN TRANSACTION
    INSERT INTO Orders (CustomerID, TotalAmount) VALUES (@CustomerID, @TotalAmount)
    DECLARE @OrderID INT = SCOPE_IDENTITY()
    EXEC sp_LogAction 'Orders', 'INSERT', @OrderID
    COMMIT TRANSACTION
END

CREATE PROCEDURE sp_LogAction
    @TableName VARCHAR(100),
    @Action VARCHAR(20),
    @RecordID INT
AS
BEGIN
    INSERT INTO AuditLog (TableName, Action, RecordID) VALUES (@TableName, @Action, @RecordID)
END

CREATE TRIGGER TR_Orders_Audit
ON Orders
AFTER INSERT, UPDATE
AS
BEGIN
    INSERT INTO AuditLog (TableName, Action, RecordID)
    SELECT 'Orders', 'TRIGGER', i.OrderID FROM inserted i
END

CREATE TRIGGER TR_Customers_Audit
ON Customers
AFTER DELETE
AS
BEGIN
    INSERT INTO AuditLog (TableName, Action, RecordID)
    SELECT 'Customers', 'DELETE', d.CustomerID FROM deleted d
END

CREATE VIEW vw_PendingOrders AS
SELECT o.OrderID, c.FirstName, c.LastName, o.TotalAmount
FROM Orders o
INNER JOIN Customers c ON o.CustomerID = c.CustomerID
WHERE o.Status = 'Pending'
"#;

const FIXTURE_VB: &str = r#"
Imports System.Data.SqlClient

Public Class OrderPage
    Inherits System.Web.UI.Page

    Protected Sub Page_Load(sender As Object, e As EventArgs) Handles Me.Load
        If Not IsPostBack Then
            Dim orders = GetOrders()
            GridView1.DataSource = orders
            GridView1.DataBind()
        End If
    End Sub

    Private Function GetOrders() As DataTable
        Dim conn As New SqlConnection("Server=.;Database=MyDB;Integrated Security=True")
        Dim cmd As New SqlCommand("sp_GetCustomerOrders", conn)
        cmd.CommandType = CommandType.StoredProcedure
        cmd.Parameters.AddWithValue("@CustomerID", Session("UserID"))

        Dim dt As New DataTable()
        conn.Open()
        dt.Load(cmd.ExecuteReader())
        conn.Close()
        Return dt
    End Function

    Protected Sub btnSave_Click(sender As Object, e As EventArgs)
        On Error Resume Next
        Dim task = SomeAsyncMethod()
        Dim result = task.Result
        Thread.Sleep(1000)

        Dim conn As New SqlConnection("Server=.;Database=MyDB;Integrated Security=True")
        Dim cmd As New SqlCommand("sp_CreateOrder", conn)
        cmd.CommandType = CommandType.StoredProcedure
        cmd.Parameters.AddWithValue("@CustomerID", CInt(Session("UserID")))
        cmd.Parameters.AddWithValue("@TotalAmount", CDec(lblTotal.Text))
        conn.Open()
        cmd.ExecuteNonQuery()
        conn.Close()
    End Sub

    Private Async Function SomeAsyncMethod() As Task(Of String)
        Dim client As New System.Net.WebClient()
        Return Await client.DownloadStringTaskAsync("http://api.example.com")
    End Function
End Class
"#;

const FIXTURE_JS: &str = r##"
// jQuery UI datepicker
$(document).ready(function() {
    $("#txtDate").datepicker({
        dateFormat: "yy-mm-dd"
    });

    // AJAX call
    $.ajax({
        url: "OrderService.asmx/GetOrders",
        type: "POST",
        data: JSON.stringify({ customerId: 123 }),
        success: function(response) {
            $("#gvOrders").DataTable();
        }
    });

    // Deprecated patterns
    $(".old-handler").live("click", function() {
        alert("clicked");
    });

    $("input").bind("change", function() {
        console.log("changed");
    });

    // jQuery validate
    $("#frmCheckout").validate({
        rules: { email: { required: true, email: true } }
    });

    // Select2
    $(".combo").select2();

    // Custom plugin
    $.fn.myCustomPlugin = function(options) {
        return this.each(function() {
            $(this).addClass("custom");
        });
    };
});
"##;

const FIXTURE_ASPX: &str = r#"
<%@ Page Language="VB" CodeBehind="default.aspx.vb" Inherits="OrderPage" %>
<html>
<head>
    <script src="jquery-1.9.0.min.js"></script>
    <script src="site.js"></script>
</head>
<body>
    <form runat="server">
        <asp:GridView ID="GridView1" runat="server" />
        <asp:Label ID="lblTotal" runat="server" />
        <asp:Button ID="btnSave" runat="server" OnClick="btnSave_Click" Text="Save" />
        <input type="text" id="txtDate" />
    </form>
</body>
</html>
"#;

// ═══════════════════════════════════════════════════════════════════════════════
// 37-W1: analyze_database_intelligence
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn w1_analyze_database_intelligence_markdown() {
    let (engram, pid, _tmp) = setup_project(FIXTURE_SQL, FIXTURE_VB, "", "").await;

    let result = engram
        .analyze_database_intelligence(Parameters(
            engram_server::AnalyzeDatabaseIntelligenceRequest {
                project_id: pid.clone(),
                sql_file_path: None,
                sp_limit: 50,
                output_json: false,
            },
        ))
        .await
        .unwrap();

    let text = &result.content[0].as_text().unwrap().text;

    assert!(text.contains("Stored Procedure"), "should have SP section");
    assert!(text.contains("sp_GetCustomerOrders"), "should list SPs");
    assert!(text.contains("sp_CreateOrder"), "should list create SP");
    assert!(text.contains("TR_Orders_Audit"), "should detect triggers");
    assert!(text.contains("Customers"), "should have schema tables");
    assert!(text.contains("Orders"), "should have orders table");
    assert!(text.contains("AuditLog"), "should detect AuditLog table");
    assert!(
        text.contains("vw_PendingOrders") || text.contains("View"),
        "should detect views or reference view section"
    );
}

#[tokio::test]
async fn w1_analyze_database_intelligence_json() {
    let (engram, pid, _tmp) = setup_project(FIXTURE_SQL, "", "", "").await;

    let result = engram
        .analyze_database_intelligence(Parameters(
            engram_server::AnalyzeDatabaseIntelligenceRequest {
                project_id: pid.clone(),
                sql_file_path: None,
                sp_limit: 50,
                output_json: true,
            },
        ))
        .await
        .unwrap();

    let text = &result.content[0].as_text().unwrap().text;

    let parsed: serde_json::Value = serde_json::from_str(&text).expect("should be valid JSON");
    assert!(parsed["sp_logic"].is_array());
    assert!(parsed["triggers"].is_array());
    assert!(parsed["schema"]["tables"].is_array());
}

#[tokio::test]
async fn w1_analyze_database_intelligence_specific_file() {
    let (engram, pid, _tmp) = setup_project(FIXTURE_SQL, "", "", "").await;

    let result = engram
        .analyze_database_intelligence(Parameters(
            engram_server::AnalyzeDatabaseIntelligenceRequest {
                project_id: pid.clone(),
                sql_file_path: Some("database.sql".into()),
                sp_limit: 50,
                output_json: false,
            },
        ))
        .await
        .unwrap();

    let text = &result.content[0].as_text().unwrap().text;

    assert!(text.contains("sp_GetCustomerOrders"));
}

#[tokio::test]
async fn w1_no_sql_files_returns_message() {
    let (engram, pid, _tmp) = setup_project("", FIXTURE_VB, "", "").await;

    let result = engram
        .analyze_database_intelligence(Parameters(
            engram_server::AnalyzeDatabaseIntelligenceRequest {
                project_id: pid.clone(),
                sql_file_path: None,
                sp_limit: 50,
                output_json: false,
            },
        ))
        .await
        .unwrap();

    let text = &result.content[0].as_text().unwrap().text;

    assert!(text.contains("No .sql files found"));
}

// ═══════════════════════════════════════════════════════════════════════════════
// 37-W2: get_sp_details
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn w2_get_sp_details_basic() {
    let (engram, pid, _tmp) = setup_project(FIXTURE_SQL, FIXTURE_VB, "", "").await;

    let result = engram
        .get_sp_details(Parameters(engram_server::GetSpDetailsRequest {
            project_id: pid.clone(),
            sp_name: "sp_GetCustomerOrders".into(),
            force_refresh: false,
        }))
        .await
        .unwrap();

    let text = &result.content[0].as_text().unwrap().text;

    assert!(
        text.contains("sp_GetCustomerOrders"),
        "should contain SP name"
    );
    assert!(text.contains("@CustomerID"), "should list parameters");
    assert!(text.contains("Orders"), "should detect tables");
    assert!(
        text.contains("## Tables Read"),
        "should have Tables Read section"
    );
    assert!(
        text.contains("## Tables Written"),
        "should have Tables Written section"
    );
    assert!(
        text.contains("Complexity"),
        "should show complexity estimate"
    );
    assert!(text.contains("Source file"), "should show source file");
}

#[tokio::test]
async fn w2_get_sp_details_with_call_chain() {
    let (engram, pid, _tmp) = setup_project(FIXTURE_SQL, "", "", "").await;

    let result = engram
        .get_sp_details(Parameters(engram_server::GetSpDetailsRequest {
            project_id: pid.clone(),
            sp_name: "sp_CreateOrder".into(),
            force_refresh: false,
        }))
        .await
        .unwrap();

    let text = &result.content[0].as_text().unwrap().text;

    assert!(text.contains("sp_CreateOrder"), "should contain SP name");
    assert!(
        text.contains("sp_LogAction"),
        "should detect calls to other SPs"
    );
    assert!(
        text.contains("transaction") || text.contains("Transaction"),
        "should detect transaction side effect"
    );
    // sp_CreateOrder INSERTs into Orders, so Orders should be in Tables Written
    assert!(
        text.contains("## Tables Written"),
        "should have Tables Written section"
    );
    assert!(
        text.contains("## Calls Other Stored Procedures"),
        "should have calls-other section"
    );
}

#[tokio::test]
async fn w2_get_sp_details_not_found() {
    let (engram, pid, _tmp) = setup_project(FIXTURE_SQL, "", "", "").await;

    let result = engram
        .get_sp_details(Parameters(engram_server::GetSpDetailsRequest {
            project_id: pid.clone(),
            sp_name: "sp_NonExistent".into(),
            force_refresh: false,
        }))
        .await;

    // Should return an error or message about not found
    assert!(
        result.is_err() || {
            let r = result.unwrap();
            let text = &r.content[0].as_text().unwrap().text;
            text.contains("not found")
        }
    );
}

#[tokio::test]
async fn w2_get_sp_details_trigger_detection() {
    let (engram, pid, _tmp) = setup_project(FIXTURE_SQL, "", "", "").await;

    let result = engram
        .get_sp_details(Parameters(engram_server::GetSpDetailsRequest {
            project_id: pid.clone(),
            sp_name: "sp_CreateOrder".into(),
            force_refresh: false,
        }))
        .await
        .unwrap();

    let text = &result.content[0].as_text().unwrap().text;

    // sp_CreateOrder writes to Orders which has TR_Orders_Audit trigger
    assert!(
        text.contains("TR_Orders_Audit"),
        "should show TR_Orders_Audit trigger on written table Orders"
    );
    assert!(
        text.contains("Triggers That May Fire"),
        "should have trigger section header"
    );
}

#[tokio::test]
async fn w2_get_sp_details_called_by_sps() {
    let (engram, pid, _tmp) = setup_project(FIXTURE_SQL, "", "", "").await;

    // sp_LogAction is EXEC'd by sp_CreateOrder — should show reverse caller
    let result = engram
        .get_sp_details(Parameters(engram_server::GetSpDetailsRequest {
            project_id: pid.clone(),
            sp_name: "sp_LogAction".into(),
            force_refresh: false,
        }))
        .await
        .unwrap();

    let text = &result.content[0].as_text().unwrap().text;

    assert!(text.contains("sp_LogAction"), "should contain SP name");
    assert!(
        text.contains("sp_CreateOrder"),
        "should show sp_CreateOrder as a reverse caller"
    );
    assert!(
        text.contains("Called By Other Stored Procedures"),
        "should have reverse-caller section header"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// 37-W3: list_triggers
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn w3_list_triggers_all() {
    let (engram, pid, _tmp) = setup_project(FIXTURE_SQL, "", "", "").await;

    let result = engram
        .list_triggers(Parameters(engram_server::ListTriggersRequest {
            project_id: pid.clone(),
            table_name: None,
            output_json: false,
        }))
        .await
        .unwrap();

    let text = &result.content[0].as_text().unwrap().text;

    assert!(
        text.contains("TR_Orders_Audit"),
        "should find orders trigger"
    );
    assert!(
        text.contains("TR_Customers_Audit"),
        "should find customers trigger"
    );
    assert!(text.contains("AFTER"), "should show trigger type");
}

#[tokio::test]
async fn w3_list_triggers_filtered_by_table() {
    let (engram, pid, _tmp) = setup_project(FIXTURE_SQL, "", "", "").await;

    let result = engram
        .list_triggers(Parameters(engram_server::ListTriggersRequest {
            project_id: pid.clone(),
            table_name: Some("Orders".into()),
            output_json: false,
        }))
        .await
        .unwrap();

    let text = &result.content[0].as_text().unwrap().text;

    assert!(
        text.contains("TR_Orders_Audit"),
        "should find trigger on Orders"
    );
    assert!(
        !text.contains("TR_Customers_Audit"),
        "should NOT include trigger on Customers when filtering by Orders"
    );
    assert!(
        text.contains("INSERT"),
        "should show trigger event types (at least INSERT)"
    );
    assert!(text.contains("AFTER"), "should show trigger type (AFTER)");
}

#[tokio::test]
async fn w3_list_triggers_json() {
    let (engram, pid, _tmp) = setup_project(FIXTURE_SQL, "", "", "").await;

    let result = engram
        .list_triggers(Parameters(engram_server::ListTriggersRequest {
            project_id: pid.clone(),
            table_name: None,
            output_json: true,
        }))
        .await
        .unwrap();

    let text = &result.content[0].as_text().unwrap().text;

    let parsed: serde_json::Value = serde_json::from_str(&text).expect("should be valid JSON");
    assert!(parsed.is_array());
    assert!(parsed.as_array().unwrap().len() >= 2);
}

#[tokio::test]
async fn w3_list_triggers_no_match() {
    let (engram, pid, _tmp) = setup_project(FIXTURE_SQL, "", "", "").await;

    let result = engram
        .list_triggers(Parameters(engram_server::ListTriggersRequest {
            project_id: pid.clone(),
            table_name: Some("NonExistentTable".into()),
            output_json: false,
        }))
        .await
        .unwrap();

    let text = &result.content[0].as_text().unwrap().text;

    assert!(text.contains("No triggers found"));
}

// ═══════════════════════════════════════════════════════════════════════════════
// 37-W4: analyze_sync_hazards
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn w4_analyze_sync_hazards_detects_result() {
    let (engram, pid, _tmp) = setup_project("", FIXTURE_VB, "", "").await;

    let result = engram
        .analyze_sync_hazards(Parameters(engram_server::AnalyzeSyncHazardsRequest {
            project_id: pid.clone(),
            file_path: None,
            min_severity: "medium".into(),
            output_json: false,
        }))
        .await
        .unwrap();

    let text = &result.content[0].as_text().unwrap().text;

    assert!(
        text.contains("task_result") || text.contains(".Result"),
        "should detect .Result hazard"
    );
    assert!(
        text.contains("thread_sleep") || text.contains("Thread.Sleep"),
        "should detect Thread.Sleep hazard"
    );
}

#[tokio::test]
async fn w4_analyze_sync_hazards_severity_filter() {
    let (engram, pid, _tmp) = setup_project("", FIXTURE_VB, "", "").await;

    let result = engram
        .analyze_sync_hazards(Parameters(engram_server::AnalyzeSyncHazardsRequest {
            project_id: pid.clone(),
            file_path: None,
            min_severity: "critical".into(),
            output_json: false,
        }))
        .await
        .unwrap();

    let text = &result.content[0].as_text().unwrap().text;

    // With critical filter, only Critical-level items should appear.
    // The counts header should show the filter level.
    assert!(
        text.contains("Min severity filter**: critical"),
        "should show critical severity filter in header"
    );
    // After the fix, totals now reflect only qualifying hazards.
    // Medium-only items should NOT contribute to reported counts.
}

#[tokio::test]
async fn w4_analyze_sync_hazards_specific_file() {
    let (engram, pid, _tmp) = setup_project("", FIXTURE_VB, "", "").await;

    let result = engram
        .analyze_sync_hazards(Parameters(engram_server::AnalyzeSyncHazardsRequest {
            project_id: pid.clone(),
            file_path: Some("default.aspx.vb".into()),
            min_severity: "medium".into(),
            output_json: false,
        }))
        .await
        .unwrap();

    let text = &result.content[0].as_text().unwrap().text;

    assert!(
        text.contains("default.aspx.vb") || text.contains("readiness"),
        "should analyze the specific file"
    );
}

#[tokio::test]
async fn w4_analyze_sync_hazards_json() {
    let (engram, pid, _tmp) = setup_project("", FIXTURE_VB, "", "").await;

    let result = engram
        .analyze_sync_hazards(Parameters(engram_server::AnalyzeSyncHazardsRequest {
            project_id: pid.clone(),
            file_path: None,
            min_severity: "medium".into(),
            output_json: true,
        }))
        .await
        .unwrap();

    let text = &result.content[0].as_text().unwrap().text;

    let parsed: serde_json::Value = serde_json::from_str(&text).expect("should be valid JSON");
    assert!(parsed["files_scanned"].is_number());
    assert!(parsed["reports"].is_array());
}

#[tokio::test]
async fn w4_analyze_sync_hazards_invalid_severity() {
    let (engram, pid, _tmp) = setup_project("", FIXTURE_VB, "", "").await;

    let result = engram
        .analyze_sync_hazards(Parameters(engram_server::AnalyzeSyncHazardsRequest {
            project_id: pid.clone(),
            file_path: None,
            min_severity: "invalid".into(),
            output_json: false,
        }))
        .await;

    assert!(result.is_err(), "should reject invalid severity value");
}

#[tokio::test]
async fn w4_sync_hazards_clean_csharp() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();

    // Clean async code — no hazards expected
    let cs = r#"
using System.Threading.Tasks;
public class CleanAsync {
    public async Task<string> GetDataAsync() {
        var client = new HttpClient();
        var result = await client.GetStringAsync("http://api.example.com");
        return result;
    }
}
"#;
    std::fs::write(root.join("Clean.cs"), cs).unwrap();

    let cfg = Config {
        allowed_roots: vec![root.to_path_buf()],
        data_dir: root.join("engram_data"),
        max_project_files: Some(100),
        max_project_bytes: Some(1024 * 1024),
        embedding_backend: "fts_only".into(),
        ..Default::default()
    };
    std::fs::create_dir_all(&cfg.data_dir).unwrap();
    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = engram_server::Engram::new(state.clone());

    engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: root.to_string_lossy().to_string(),
            project_name: "CleanTest".into(),
            project_type: "csharp".into(),
            wait: true,
            dedupe_by_directory: false,
        }))
        .await
        .unwrap();

    let projects = state.registry.list_projects().unwrap();
    let pid = &projects[0].project_id;

    let result = engram
        .analyze_sync_hazards(Parameters(engram_server::AnalyzeSyncHazardsRequest {
            project_id: pid.clone(),
            file_path: None,
            min_severity: "medium".into(),
            output_json: false,
        }))
        .await
        .unwrap();

    let text = &result.content[0].as_text().unwrap().text;

    // Clean code should have no hazards (or at most "No sync hazards found")
    let has_hazards = text.contains("task_result") || text.contains("thread_sleep");
    assert!(!has_hazards, "clean async code should have no hazards");
}

// ═══════════════════════════════════════════════════════════════════════════════
// 37-W5: get_jquery_inventory
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn w5_jquery_inventory_detects_plugins() {
    let (engram, pid, _tmp) = setup_project("", "", FIXTURE_JS, FIXTURE_ASPX).await;

    let result = engram
        .get_jquery_inventory(Parameters(engram_server::GetJQueryInventoryRequest {
            project_id: pid.clone(),
            file_filter: None,
            output_json: false,
        }))
        .await
        .unwrap();

    let text = &result.content[0].as_text().unwrap().text;

    assert!(text.contains("jQuery Inventory"), "should have header");
    assert!(
        text.contains("datepicker") || text.contains("Datepicker"),
        "should detect jQuery UI datepicker"
    );
}

#[tokio::test]
async fn w5_jquery_inventory_detects_deprecated() {
    let (engram, pid, _tmp) = setup_project("", "", FIXTURE_JS, FIXTURE_ASPX).await;

    let result = engram
        .get_jquery_inventory(Parameters(engram_server::GetJQueryInventoryRequest {
            project_id: pid.clone(),
            file_filter: None,
            output_json: false,
        }))
        .await
        .unwrap();

    let text = &result.content[0].as_text().unwrap().text;

    assert!(
        text.contains(".live") || text.contains("Deprecated"),
        "should detect deprecated .live() pattern"
    );
}

#[tokio::test]
async fn w5_jquery_inventory_detects_third_party() {
    let (engram, pid, _tmp) = setup_project("", "", FIXTURE_JS, FIXTURE_ASPX).await;

    let result = engram
        .get_jquery_inventory(Parameters(engram_server::GetJQueryInventoryRequest {
            project_id: pid.clone(),
            file_filter: None,
            output_json: false,
        }))
        .await
        .unwrap();

    let text = &result.content[0].as_text().unwrap().text;

    assert!(
        text.contains("DataTable") || text.contains("DataTables"),
        "should detect DataTables"
    );
    assert!(
        text.contains("Validate") || text.contains("validate"),
        "should detect jQuery Validate"
    );
    assert!(
        text.contains("Select2") || text.contains("select2"),
        "should detect Select2"
    );
}

#[tokio::test]
async fn w5_jquery_inventory_detects_custom() {
    let (engram, pid, _tmp) = setup_project("", "", FIXTURE_JS, FIXTURE_ASPX).await;

    let result = engram
        .get_jquery_inventory(Parameters(engram_server::GetJQueryInventoryRequest {
            project_id: pid.clone(),
            file_filter: None,
            output_json: false,
        }))
        .await
        .unwrap();

    let text = &result.content[0].as_text().unwrap().text;

    assert!(
        text.contains("myCustomPlugin") || text.contains("Custom Plugin"),
        "should detect custom plugin"
    );
}

#[tokio::test]
async fn w5_jquery_inventory_detects_version() {
    let (engram, pid, _tmp) = setup_project("", "", FIXTURE_JS, FIXTURE_ASPX).await;

    let result = engram
        .get_jquery_inventory(Parameters(engram_server::GetJQueryInventoryRequest {
            project_id: pid.clone(),
            file_filter: None,
            output_json: false,
        }))
        .await
        .unwrap();

    let text = &result.content[0].as_text().unwrap().text;

    // Version detection from script tag in ASPX: jquery-1.9.0.min.js
    assert!(
        text.contains("1.9.0") || text.contains("VULNERABLE"),
        "should detect jQuery 1.9.0 and flag as vulnerable"
    );
}

#[tokio::test]
async fn w5_jquery_inventory_json() {
    let (engram, pid, _tmp) = setup_project("", "", FIXTURE_JS, FIXTURE_ASPX).await;

    let result = engram
        .get_jquery_inventory(Parameters(engram_server::GetJQueryInventoryRequest {
            project_id: pid.clone(),
            file_filter: None,
            output_json: true,
        }))
        .await
        .unwrap();

    let text = &result.content[0].as_text().unwrap().text;

    let parsed: serde_json::Value = serde_json::from_str(&text).expect("should be valid JSON");
    assert!(parsed["files_analyzed"].is_number());
    assert!(parsed["total_usages"].is_number());
}

#[tokio::test]
async fn w5_jquery_inventory_file_filter() {
    let (engram, pid, _tmp) = setup_project("", "", FIXTURE_JS, FIXTURE_ASPX).await;

    let result = engram
        .get_jquery_inventory(Parameters(engram_server::GetJQueryInventoryRequest {
            project_id: pid.clone(),
            file_filter: Some("*.js".into()),
            output_json: false,
        }))
        .await
        .unwrap();

    let text = &result.content[0].as_text().unwrap().text;

    assert!(text.contains("jQuery Inventory"), "should produce output");
}

#[tokio::test]
async fn w5_jquery_inventory_no_files() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();

    // Only a .vb file, no JS or ASPX
    std::fs::write(root.join("test.vb"), "Module Foo\nEnd Module").unwrap();

    let cfg = Config {
        allowed_roots: vec![root.to_path_buf()],
        data_dir: root.join("engram_data"),
        max_project_files: Some(100),
        max_project_bytes: Some(1024 * 1024),
        embedding_backend: "fts_only".into(),
        ..Default::default()
    };
    std::fs::create_dir_all(&cfg.data_dir).unwrap();
    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = engram_server::Engram::new(state.clone());

    engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: root.to_string_lossy().to_string(),
            project_name: "NoJQ".into(),
            project_type: "vbnet".into(),
            wait: true,
            dedupe_by_directory: false,
        }))
        .await
        .unwrap();

    let projects = state.registry.list_projects().unwrap();
    let pid = &projects[0].project_id;

    let result = engram
        .get_jquery_inventory(Parameters(engram_server::GetJQueryInventoryRequest {
            project_id: pid.clone(),
            file_filter: None,
            output_json: false,
        }))
        .await
        .unwrap();

    let text = &result.content[0].as_text().unwrap().text;

    assert!(
        text.contains("No JS or markup files") || text.contains("No jQuery usage"),
        "should report no files found"
    );
}
