//! Complete stage outcomes for the composite refresh. No freshness certification.
use rmcp::{
    ErrorData,
    model::{CallToolResult, Content},
};
use serde_json::{Value, json};

#[derive(Default)]
pub struct Report {
    stages: Vec<Value>,
    errors: usize,
}

impl Report {
    pub fn record(&mut self, stage: &str, result: Result<CallToolResult, ErrorData>) {
        self.stages.push(match result {
            Ok(result) => {
                let failed = result.is_error.unwrap_or(false);
                self.errors += usize::from(failed);
                json!({"stage":stage, "status":if failed {"error"} else {"returned"},
                    "tool_result":result})
            }
            Err(error) => {
                self.errors += 1;
                json!({"stage":stage, "status":"error", "error":error})
            }
        });
    }

    pub fn skip(&mut self, stage: &str, reason: &str) {
        self.stages
            .push(json!({"stage":stage, "status":"skipped", "reason":reason}));
    }

    pub fn finish(self) -> CallToolResult {
        let mut result = CallToolResult::success(vec![Content::text(json!({
            "schema_version":1,
            "stage_errors":self.errors,
            "status":if self.errors > 0 {"stage_errors_reported"} else {"no_stage_errors_reported"},
            "stages":self.stages,
            "notes":[
                "Each attempted stage retains its complete original MCP result or protocol error. Returned is a handler outcome, not proof of corpus completeness or working-tree freshness; inspect stage warnings and get_index_freshness.",
                "Independent stages continue after errors. Earlier successful writes are not rolled back. Recovery of this saved report never repeats refresh operations.",
                "Code-review corpora refresh only with pat_token and all ado_* coordinates. Imported dispositions do not establish accepted exceptions or automatically suppress findings.",
                "Quality gates refresh through ingest_quality_gates on reviewed sources; KB wikis regenerate through describe_setting/describe_table on demand."
            ]
        }).to_string())]);
        result.is_error = Some(self.errors > 0);
        result
    }
}
