//! Compose existing read contracts without truncating evidence inside a result.
use crate::{
    models::{BatchReadOperation, BatchReadRequest},
    handlers::planning_tools::change_set_delivery as snapshots,
    tools::Engram,
};
use rmcp::{
    ErrorData as McpError,
    model::{CallToolResult, Content},
};
use serde::Serialize;
use serde_json::{Value, json};
use std::path::Path;
use std::collections::BTreeSet;

const OMITTED: &str = "omitted_shared_response_budget";

struct Row {
    request_id: String,
    status: &'static str,
    /// Serialized underlying result/error size, not the whole response size.
    response_bytes: u64,
    response: Option<Value>,
    recovery: Value,
    included: bool,
}

#[derive(Serialize)]
struct WireRow<'a> {
    request_id: &'a str,
    status: &'static str,
    response_bytes: u64,
    response: Option<&'a Value>,
    recovery: &'a Value,
}

#[derive(Serialize)]
struct Reply<'a> {
    complete: bool,
    omitted_count: usize,
    max_tokens: usize,
    budget_accounting: &'static str,
    consistency: &'static str,
    results: Vec<WireRow<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_delivery: Option<&'a Value>,
}

struct BudgetedBatch {
    rows: Vec<Row>,
    response_delivery: Option<Value>,
    max_tokens: usize,
}

fn serialization(error: serde_json::Error) -> McpError {
    McpError::internal_error(error.to_string(), None)
}

fn token_budget(request: &BatchReadRequest) -> Result<usize, McpError> {
    let tokens = request.max_tokens.unwrap_or(8192);
    if !(1..=65536).contains(&tokens) {
        return Err(McpError::invalid_params("max_tokens must be in 1..=65536", None));
    }
    Ok(tokens)
}

fn delivery(id: &str, original_bytes: u64, tokens: usize) -> Value {
    json!({
        "status":"DEFERRED_BATCH_RESULTS", "complete":false,
        "response_id":id, "original_bytes":original_bytes, "original_format":"json",
        "response_budget_bytes":tokens * 4,
        "retention":"At most 24 hours or 32 newer batch captures in this server data directory; unavailable captures require fresh reads and reconciliation.",
        "instruction":"Recover all original executed results, errors and qualifications by concatenating content in byte-offset order and verifying BLAKE3 response_id. Recovery performs no underlying reads and cannot certify current freshness or an atomic multi-read source snapshot. Per-row recovery calls refresh that read against then-current state.",
        "next_call":snapshots::batch_read_continuation(id,0,tokens * 4)
    })
}

impl BudgetedBatch {
    fn new(request: &BatchReadRequest) -> Result<Self, McpError> {
        if request.requests.is_empty() || request.requests.len() > 16 {
            return Err(McpError::invalid_params(
                "requests must contain 1..=16 reads",
                None,
            ));
        }
        let max_tokens = token_budget(request)?;
        let mut ids = BTreeSet::new();
        let mut rows = Vec::new();
        for item in &request.requests {
            if item.request_id.trim().is_empty()
                || item.request_id.len() > 128
                || item.request_id.chars().any(char::is_control)
                || !ids.insert(&item.request_id)
            {
                return Err(McpError::invalid_params(
                    "request_id must be nonempty, unique, at most 128 bytes, without control characters",
                    None,
                ));
            }
            let recovery = serde_json::to_value(&item.request).map_err(serialization)?;
            // Reserve the longest status and numeric width before any read.
            // Every returned row always retains its complete recovery request.
            rows.push(Row {
                request_id: item.request_id.clone(),
                status: OMITTED,
                response_bytes: u64::MAX,
                response: None,
                recovery,
                included: false,
            });
        }
        let mut batch = Self {
            rows, max_tokens,
            // Reserve immutable recovery controls before executing any reads.
            response_delivery: Some(delivery(&"0".repeat(64), u64::MAX, max_tokens)),
        };
        let size = batch.encode()?.len();
        if size > max_tokens * 4 {
            let mut minimum = size.div_ceil(4);
            loop {
                batch.max_tokens = minimum;
                batch.response_delivery = Some(delivery(&"0".repeat(64), u64::MAX, minimum));
                let required = batch.encode()?.len().div_ceil(4);
                if required <= minimum {
                    break;
                }
                minimum = required;
            }
            return Err(McpError::invalid_params(
                format!(
                    "Batch budget cannot fit all request identities and recovery calls; minimum {minimum} estimated tokens. Reduce the batch or increase max_tokens (maximum 65536). No reads executed."
                ),
                None,
            ));
        }
        batch.response_delivery = None;
        Ok(batch)
    }

    fn encode(&self) -> Result<String, McpError> {
        self.encode_rows(false, self.response_delivery.as_ref())
    }

    fn encode_rows(&self, original: bool, response_delivery: Option<&Value>) -> Result<String, McpError> {
        let results: Vec<_> = self.rows.iter().map(|row| {
            let include = original || row.included;
            WireRow {
                request_id: &row.request_id,
                status: if include { row.status } else { OMITTED },
                response_bytes: row.response_bytes,
                response: row.response.as_ref().filter(|_| include),
                recovery: &row.recovery,
            }
        }).collect();
        let omitted_count = results.iter().filter(|row| row.status == OMITTED).count();
        serde_json::to_string(&Reply { complete: omitted_count == 0, omitted_count,
            max_tokens: self.max_tokens, budget_accounting: "ceil(complete JSON UTF-8 bytes / 4)",
            consistency: "Sequential ordinary reads; no atomic batch snapshot. Each result retains its existing freshness and citation contract. Per-row recovery reruns that read against then-current state; response_delivery recovers original executed results without rerunning reads.",
            results, response_delivery }).map_err(serialization)
    }

    fn insert(
        &mut self,
        index: usize,
        status: &'static str,
        response: Value,
    ) -> Result<(), McpError> {
        let row = &mut self.rows[index];
        row.response_bytes =
            serde_json::to_vec(&response).map_err(serialization)?.len() as u64;
        row.status = status;
        row.response = Some(response);
        row.included = true;
        if self.encode()?.len() > self.max_tokens * 4 {
            self.rows[index].included = false;
        }
        Ok(())
    }
    fn finish(mut self, data: &Path) -> Result<String, McpError> {
        if self.rows.iter().all(|row| row.included) {
            return self.encode();
        }
        let original = self.encode_rows(true, None)?;
        let id = blake3::hash(original.as_bytes()).to_hex().to_string();
        self.response_delivery = Some(delivery(&id, original.len() as u64, self.max_tokens));
        // Recovery metadata was reserved before reads. Keep whole fitting
        // results; return every omitted result through the exact saved batch.
        for index in (0..self.rows.len()).rev() {
            if self.encode()?.len() <= self.max_tokens * 4 { break; }
            self.rows[index].included = false;
        }
        let output = self.encode()?;
        if output.len() > self.max_tokens * 4 {
            return Err(McpError::internal_error("Reserved batch recovery controls exceeded response budget", None));
        }
        snapshots::save_batch_read(data, &original)?;
        Ok(output)
    }
}

impl Engram {
    pub async fn handle_batch_read(
        &self,
        request: BatchReadRequest,
    ) -> Result<CallToolResult, McpError> {
        let tokens = token_budget(&request)?;
        if let Some(id) = request.response_id.as_deref() {
            if !request.requests.is_empty() {
                return Err(McpError::invalid_params("response_id recovery cannot include new reads", None));
            }
            let body = snapshots::recover_batch_read(&self.state.cfg.data_dir, id, request.response_offset, tokens * 4)?;
            return Ok(CallToolResult::success(vec![Content::text(body)]));
        }
        if request.response_offset != 0 {
            return Err(McpError::invalid_params("response_offset requires response_id", None));
        }
        let mut batch = BudgetedBatch::new(&request)?;
        for (index, item) in request.requests.into_iter().enumerate() {
            let result = match item.request {
                BatchReadOperation::MethodInfo(arguments) => {
                    if arguments.response_id.is_some() || arguments.response_offset != 0 {
                        self.handle_get_method_info(arguments).await
                    } else {
                        // The batch owns its complete-reply budget. Keep complete
                        // metadata/guidance here rather than nesting a deferral.
                        self.method_info_evidence(arguments).await
                    }
                }
                BatchReadOperation::ResolveId(arguments) => self.handle_resolve_id(arguments).await,
                BatchReadOperation::Chunk(arguments) => self.handle_get_chunk(arguments).await,
                BatchReadOperation::MemorySection(arguments) => {
                    // Keep the ordinary section identity, provenance, byte/line
                    // citation guards and per-read document delivery contract.
                    self.handle_read_memory_bank(arguments).await
                }
                BatchReadOperation::Schema(arguments) => {
                    // Preserve the normal source/citation, policy, caller-budget
                    // and saved-result recovery contract for each SQL read.
                    self.handle_query_schema(arguments).await
                }
                BatchReadOperation::MethodBody(arguments) => {
                    // Keep the direct source contract, including hash guards,
                    // authored guidance and immutable report continuations.
                    self.handle_get_full_method_body(arguments).await
                }
                BatchReadOperation::PageContext(arguments) => {
                    // Preserve page options, scoped policy and hash-bound
                    // context continuation before applying the shared budget.
                    self.handle_get_page_context(arguments).await
                }
                BatchReadOperation::RequestTrace(arguments) => {
                    self.handle_trace_user_request(arguments).await
                }
                BatchReadOperation::ReviewDecisions(arguments) => {
                    self.handle_get_review_decisions(arguments).await
                }
            };
            let (status, response) = match result {
                Ok(result) => (
                    if result.is_error.unwrap_or(false) {
                        "tool_error"
                    } else {
                        "ok"
                    },
                    serde_json::to_value(result),
                ),
                Err(error) => ("request_error", serde_json::to_value(error)),
            };
            batch.insert(index, status, response.map_err(serialization)?)?;
        }
        Ok(CallToolResult::success(vec![Content::text(
            batch.finish(&self.state.cfg.data_dir)?,
        )]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn request(max_tokens: usize) -> BatchReadRequest {
        serde_json::from_value(json!({"max_tokens":max_tokens,"requests":[
            {"request_id":"source","request":{"tool":"get_chunk","arguments":{"project_id":"p","doc_id":"doc","inject_rules":true}}},
            {"request_id":"identity","request":{"tool":"resolve_id","arguments":{"project_id":"p","id":"file:page.aspx"}}}
        ]})).unwrap()
    }

    #[test]
    fn complete_results_keep_error_data_citations_and_guidance_intact() {
        let mut batch = BudgetedBatch::new(&request(8192)).unwrap();
        let evidence = json!({"content":[{"type":"text","text":"日本語 source\nExcept for approved scope: never remove this qualification."}],"isError":false});
        let failure =
            json!({"code":-32602,"message":"stale citation","data":{"expected":"original-hash"}});
        batch.insert(0, "ok", evidence.clone()).unwrap();
        batch.insert(1, "request_error", failure.clone()).unwrap();
        let reply: Value = serde_json::from_str(&batch.encode().unwrap()).unwrap();
        assert_eq!(reply["results"][0]["response"], evidence);
        assert_eq!(reply["results"][1]["response"], failure);
        assert_eq!(reply["results"][0]["request_id"], "source");
        assert_eq!(reply["results"][1]["request_id"], "identity");
        assert_eq!(reply["complete"], true);
    }

    #[test]
    fn shared_budget_omits_whole_evidence_and_preserves_exact_recovery_and_later_results() {
        let req = request(1024);
        let mut batch = BudgetedBatch::new(&req).unwrap();
        batch
            .insert(
                0,
                "ok",
                json!({"content":[{"type":"text","text":"日本語\n\"scope\"".repeat(2000)}]}),
            )
            .unwrap();
        batch
            .insert(
                1,
                "ok",
                json!({"content":[{"type":"text","text":"resolved identity"}]}),
            )
            .unwrap();
        let output = batch.encode().unwrap();
        assert!(output.len() <= 4096);
        let reply: Value = serde_json::from_str(&output).unwrap();
        assert_eq!(reply["complete"], false);
        assert_eq!(reply["omitted_count"], 1);
        assert_eq!(reply["results"][0]["status"], OMITTED);
        assert!(reply["results"][0]["response"].is_null());
        assert_eq!(
            reply["results"][0]["recovery"],
            serde_json::to_value(&req.requests[0].request).unwrap()
        );
        assert_eq!(reply["results"][1]["status"], "ok");
    }

    #[test]
    fn invalid_batches_and_mutating_tool_names_are_rejected() {
        assert!(BudgetedBatch::new(&request(1)).is_err());
        let mut duplicate = request(8192);
        duplicate.requests[1].request_id = duplicate.requests[0].request_id.clone();
        assert!(BudgetedBatch::new(&duplicate).is_err());
        let mut too_many = request(8192);
        too_many.requests = (0..17)
            .map(|i| {
                let mut item = duplicate.requests[0].clone();
                item.request_id = i.to_string();
                item
            })
            .collect();
        assert!(BudgetedBatch::new(&too_many).is_err());
        assert!(serde_json::from_value::<BatchReadRequest>(json!({"requests":[{"request_id":"write","request":{"tool":"delete_project","arguments":{"project_id":"p"}}}]})).is_err());
    }
}
