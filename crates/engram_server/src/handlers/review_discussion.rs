//! Hash-bound recovery of complete imported discussion captures, not authority.
use crate::{state::AppState,services::review_verdict_service::{CapturedReviewThread,ReviewThread}};
use engram_core::registry::{ReviewDiscussionRecord,ReviewDiscussionWrite};
use serde::{Deserialize,Serialize};
use serde_json::{json,Value};
use std::{collections::BTreeSet,io::Read,path::Path};

pub(crate) const THREAD_BYTES: usize=256*1024;
pub(crate) const BATCH_BYTES: usize=4*1024*1024;
pub(crate) const INPUT_ROWS: usize=1024;
pub(crate) const IMPORT_ALLOWANCE: u64=64*1024*1024;
const PAGE_BYTES: usize=24*1024;
const MARKER: &str="\nEngram imported discussion v1: ";

#[derive(Debug,Clone,Default,Deserialize,schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DiscussionSelector {
    /// Both thread_id and capture_blake3 select one immutable capture. With
    /// neither supplied, list the current bounded capture catalog for review_id.
    #[serde(default)]
    pub thread_id: Option<u64>,
    #[serde(default)]
    pub capture_blake3: Option<String>,
    /// UTF-8 byte offset into the immutable captured JSON (not its raw_utf8 field).
    #[serde(default)]
    pub offset: usize,
    /// Hash of the assembled recovery JSON, required on nonzero continuation
    /// offsets so a changed serializer/response cannot splice incompatible pages.
    #[serde(default)]
    pub expected_recovery_blake3: Option<String>,
    /// Opaque catalog cursor from next_call. Cannot accompany a capture selector.
    #[serde(default)]
    pub after: Option<String>,
}

#[derive(Debug,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct Capture {
    format: u32,
    review_id: String,
    thread_id: u64,
    representation: String,
    provenance: Value,
    projection_inputs_blake3: String,
    raw_utf8_blake3: String,
    raw_utf8_bytes: usize,
}
#[derive(Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct Reference {
    review_id: String,
    thread_id: u64,
    capture_blake3: String,
    summary_blake3: String,
}

fn validate_shape(value: &Value) -> anyhow::Result<()> {
    let comments=value.get("comments").and_then(Value::as_array)
        .ok_or_else(||anyhow::anyhow!("review input must have a comments array"))?;
    anyhow::ensure!(comments.len()<=1024,"review thread exceeds 1024 comments");
    for comment in comments {
        if let Some(likes)=comment.get("likes").and_then(Value::as_array) {
            anyhow::ensure!(likes.len()<=256,"review comment exceeds 256 likes");
        }
    }
    Ok(())
}

pub(crate) fn read_jsonl(path: &Path) -> anyhow::Result<Vec<CapturedReviewThread>> {
    let mut file=std::fs::File::open(path)?;
    let before=file.metadata()?;
    anyhow::ensure!(before.is_file() && before.len()<=BATCH_BYTES as u64,
        "review JSONL must be a regular file at most 4 MiB; no import published");
    let mut bytes=Vec::new();
    (&mut file).take(BATCH_BYTES as u64+1).read_to_end(&mut bytes)?;
    let after=file.metadata()?;
    anyhow::ensure!(bytes.len()<=BATCH_BYTES && before.len()==after.len()
        && before.modified().ok()==after.modified().ok(),"review JSONL grew or changed during bounded read; no import published");
    let text=std::str::from_utf8(&bytes)?;
    let file_hash=blake3::hash(&bytes).to_hex().to_string();
    let mut result=Vec::new();
    for (number,line) in text.split_inclusive('\n').enumerate() {
        anyhow::ensure!(number<INPUT_ROWS,"review JSONL exceeds 1024 input rows; no import published");
        if line.trim().is_empty() { continue; }
        anyhow::ensure!(line.len()<=THREAD_BYTES,"review JSONL thread exceeds 256 KiB; no import published");
        let value: Value=serde_json::from_str(line)
            .map_err(|error|anyhow::anyhow!("review JSONL line {}: {error}",number+1))?;
        validate_shape(&value)?;
        let thread: ReviewThread=serde_json::from_value(value)?;
        result.push(CapturedReviewThread {thread,raw_utf8:line.to_owned(),
            representation:"exact_original_jsonl_line_utf8".into(),
            provenance:json!({"source_kind":"json_file","source_path":path.to_string_lossy(),
                "source_file_blake3":file_hash,"source_line":number+1,
                "scope":"Exact supplied line bytes, including its original line ending and unknown fields. Other source-file rows are not part of this capture; source file is not an authenticated atomic snapshot."})});
    }
    Ok(result)
}

pub(crate) fn prepare(
    imported: Vec<CapturedReviewThread>,decision_makers: &[String],author_trust: Option<&std::collections::BTreeMap<String,String>>,
) -> anyhow::Result<(Vec<ReviewThread>,Vec<ReviewDiscussionRecord>)> {
    anyhow::ensure!(imported.len()<=INPUT_ROWS,"review import exceeds 1024 threads");
    let projection=json!({"classifier":"review_verdict_rules_v1","decision_makers":decision_makers,"author_trust":author_trust});
    let projection_hash=blake3::hash(&serde_json::to_vec(&projection)?).to_hex().to_string();
    let mut threads=Vec::new();
    let mut records=Vec::new();
    let mut identities=BTreeSet::new();
    let mut total=0usize;
    for imported in imported {
        anyhow::ensure!(imported.raw_utf8.len()<=THREAD_BYTES,"review thread exceeds 256 KiB");
        let review_id=format!("PR-{}",imported.thread.pr_id);
        anyhow::ensure!(identities.insert((imported.thread.pr_id,imported.thread.thread_id)),
            "multiple inputs claim the same review/thread; split or reconcile the import before publication");
        let raw_payload=imported.raw_utf8;
        let raw_payload_blake3=blake3::hash(raw_payload.as_bytes()).to_hex().to_string();
        let capture=Capture {format:2,review_id:review_id.clone(),thread_id:imported.thread.thread_id,
            representation:imported.representation,provenance:imported.provenance,
            projection_inputs_blake3:projection_hash.clone(),raw_utf8_blake3:raw_payload_blake3.clone(),
            raw_utf8_bytes:raw_payload.len()};
        let payload=serde_json::to_string(&capture)?;
        total=total.checked_add(payload.len()).and_then(|n|n.checked_add(raw_payload.len()))
            .ok_or_else(||anyhow::anyhow!("capture batch overflow"))?;
        anyhow::ensure!(total<=BATCH_BYTES,"review metadata/raw-body batch exceeds 4 MiB; no import published");
        records.push(ReviewDiscussionRecord {review_id,thread_id:imported.thread.thread_id,
            capture_blake3:blake3::hash(payload.as_bytes()).to_hex().to_string(),payload,raw_payload_blake3,raw_payload});
        threads.push(imported.thread);
    }
    Ok((threads,records))
}

pub(crate) fn append_reference(mut summary: String,record: &ReviewDiscussionRecord) -> anyhow::Result<String> {
    let reference=Reference {review_id:record.review_id.clone(),thread_id:record.thread_id,
        capture_blake3:record.capture_blake3.clone(),summary_blake3:blake3::hash(summary.as_bytes()).to_hex().to_string()};
    summary.push_str(MARKER);
    summary.push_str(&serde_json::to_string(&reference)?);
    summary.push('\n');
    Ok(summary)
}

fn capture_call(project: &str,review: &str,thread:u64,hash:&str,offset:usize) -> Value {
    json!({"tool":"get_review_decisions","arguments":{"project_id":project,"review_id":review,
        "discussion":{"thread_id":thread,"capture_blake3":hash,"offset":offset}}})
}
pub(crate) fn summary_reference(project: &str,summary:&str) -> Value {
    let Some((original,encoded))=summary.rsplit_once(MARKER) else {
        return json!({"status":"legacy_uncaptured","coverage":"Complete originally supplied discussion was not retained for this legacy summary. Reimport the original source to capture it; no missing text is reconstructed."});
    };
    let parsed=serde_json::from_str::<Reference>(encoded.trim_end());
    let reference=match parsed {
        Ok(reference) if reference.summary_blake3==blake3::hash(original.as_bytes()).to_hex().as_str()=>reference,
        _=>return json!({"status":"invalid_capture_reference","coverage":"Stored capture reference or summary binding is malformed; recover no guessed evidence."}),
    };
    json!({"status":"capture_reference","capture_blake3":reference.capture_blake3,
        "coverage":"Complete captured input is available only after exact hash recovery; this reference does not authenticate source, authority, accepted scope or an as-of decision. Provider capture is canonical supplied JSON, not original HTTP wire bytes or unreturned provider history.",
        "recovery":capture_call(project,&reference.review_id,reference.thread_id,&reference.capture_blake3,0)})
}

pub(crate) fn recover(state:&AppState,project:&str,review:&str,selector:DiscussionSelector)->anyhow::Result<String> {
    match (selector.thread_id,selector.capture_blake3) {
        (Some(thread),Some(hash))=>{
            anyhow::ensure!(selector.after.is_none(),"capture selection cannot use a catalog cursor");
            state.registry.inspect_review_discussion(project,review,thread,&hash,|metadata,body| {
                let metadata=std::str::from_utf8(metadata)?;
                let body=std::str::from_utf8(body)?;
                let capture:Capture=serde_json::from_str(metadata)?;
                anyhow::ensure!(capture.format==2 && capture.review_id==review && capture.thread_id==thread
                    && capture.raw_utf8_bytes==body.len() && body.len()<=THREAD_BYTES
                    && blake3::hash(body.as_bytes()).to_hex().as_str()==capture.raw_utf8_blake3,
                    "capture metadata/body identity,format,length or hash differs; preserved");
                // The selector hash binds exact small metadata; that metadata
                // binds the reused raw body. A separate document hash binds the
                // deterministic recovery envelope and every continuation page.
                let payload=serde_json::to_string(&json!({"capture_metadata_json":metadata,"raw_utf8":body}))?;
                let recovery_hash=blake3::hash(payload.as_bytes()).to_hex().to_string();
                anyhow::ensure!(selector.expected_recovery_blake3.as_deref().is_none_or(|expected|expected==recovery_hash)
                    && (selector.offset==0 || selector.expected_recovery_blake3.is_some()),
                    "recovery hash differs or is missing from a continuation; restart this exact capture at offset zero");
                let offset=selector.offset;
                anyhow::ensure!(offset<=payload.len() && payload.is_char_boundary(offset),
                    "discussion offset must be a UTF-8 boundary within this exact immutable recovery document");
                let mut end=offset.saturating_add(8*1024).min(payload.len());
                while !payload.is_char_boundary(end){end-=1;}
                loop {
                    let complete=end==payload.len();
                    let mut next=capture_call(project,review,thread,&hash,end);
                    next["arguments"]["discussion"]["expected_recovery_blake3"]=json!(recovery_hash);
                    let page=json!({"format":"engram_imported_review_capture_bundle_v2","capture_blake3":hash,
                        "recovery_blake3":recovery_hash,"raw_utf8_blake3":capture.raw_utf8_blake3,
                        "review_id":review,"thread_id":thread,"total_utf8_bytes":payload.len(),"start_byte":offset,"end_byte":end,
                        "complete":complete,"capture_json":&payload[offset..end],
                        "recovery_scope":"Concatenate capture_json and verify recovery_blake3. Decode its capture_metadata_json string and verify that exact string against capture_blake3; decode metadata and verify raw_utf8 against its raw_utf8_blake3 and raw_utf8_bytes. Source is supplied input only, not authenticated authority, accepted scope or an as-of decision.",
                        "next_call":if complete{Value::Null}else{next}});
                    let encoded=serde_json::to_string(&page)?;
                    if encoded.len()<=PAGE_BYTES{return Ok(encoded);}
                    anyhow::ensure!(end>offset,"capture recovery metadata exceeds response budget");
                    end=offset+(end-offset)/2;while !payload.is_char_boundary(end){end-=1;}
                }
            })
        }
        (None,None)=>{
            anyhow::ensure!(selector.offset==0 && selector.expected_recovery_blake3.is_none(),"catalog selection cannot use byte offsets/recovery hashes");
            let page=state.registry.list_review_discussions(project,review,selector.after.as_deref())?;
            let refs=page.records.iter().map(|record|{
                let mut call=capture_call(project,review,record.thread_id,&record.capture_blake3,0);
                call["raw_utf8_blake3"]=json!(record.raw_utf8_blake3);call
            }).collect::<Vec<_>>();
            let result=json!({"review_id":review,"status":if refs.is_empty(){"no_captured_discussions"}else{"captured_discussion_references"},
                "captures":refs,"next_call":page.next.map(|after|json!({"tool":"get_review_decisions",
                    "arguments":{"project_id":project,"review_id":review,"discussion":{"after":after}}})),
                "coverage":"Current bounded catalog of small immutable source/provenance captures and reused raw body hashes, including captures whose later index publication failed. Not an atomic multi-page snapshot; concurrent inserts can precede an earlier cursor. Empty catalog is not proof that no original discussions exist. Legacy uncaptured summaries cannot be reconstructed. No automatic evidence deletion or quota increase occurs."});
            let encoded=serde_json::to_string(&result)?;anyhow::ensure!(encoded.len()<=PAGE_BYTES,"capture catalog metadata exceeds response budget");Ok(encoded)
        }
        _=>anyhow::bail!("discussion capture requires both thread_id and capture_blake3; supply neither for catalog recovery"),
    }
}

pub(crate) fn retained_note(project:&str,reviews:&[String],write:&ReviewDiscussionWrite) -> String {
    let examples=reviews.iter().take(3).map(|review|json!({"tool":"get_review_decisions",
        "arguments":{"project_id":project,"review_id":review,"discussion":{}}})).collect::<Vec<_>>();
    format!("Full discussion captures: stored={}, unchanged={}, stored_payloads={}, reused_payloads={}, project_retained_objects={}, project_retained_captures={}, project_retained_payloads={}, project_retained_bytes={}; immutable evidence is retained even if later summary indexing fails. Recovery catalog examples: {}; additional review catalogs={}. No authors, URLs, acceptance or historical applicability were authenticated.",
        write.stored,write.unchanged,write.stored_payloads,write.reused_payloads,write.retained_records,
        write.retained_captures,write.retained_payloads,write.retained_bytes,json!(examples),reviews.len().saturating_sub(3))
}
