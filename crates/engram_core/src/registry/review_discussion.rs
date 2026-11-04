//! Immutable discussion metadata and reused raw bodies; one quota transaction.
use super::*;
const PREFIX:&str="review_discussion_v1\0";
const BODIES:&str="review_discussion_payload_v1\0";
const QUOTA:&str="review_discussion_quota_v1";
const MAGIC:&[u8;8]=b"EGQD\0\0\0\x02";
const LINK:&[u8;8]=b"EGDC\0\0\0\x02";
const METADATA_BYTES:usize=64*1024;
const BODY_BYTES:usize=256*1024;
pub const REVIEW_DISCUSSION_RECORD_BYTES:usize=1024*1024;
pub const REVIEW_DISCUSSION_BATCH_BYTES:usize=4*1024*1024;
pub const REVIEW_DISCUSSION_BATCH_ROWS:usize=1024;
pub const REVIEW_DISCUSSION_PROJECT_ROWS:usize=4096;
pub const REVIEW_DISCUSSION_PROJECT_BYTES:usize=128*1024*1024;
pub struct ReviewDiscussionRecord {
    pub review_id:String,pub thread_id:u64,pub capture_blake3:String,
    pub payload:String,pub raw_payload_blake3:String,pub raw_payload:String,
}
#[derive(Debug,Default,Serialize)]
pub struct ReviewDiscussionWrite {
    pub stored:usize,pub unchanged:usize,pub stored_payloads:usize,pub reused_payloads:usize,
    pub retained_records:usize,pub retained_bytes:usize,pub retained_captures:usize,pub retained_payloads:usize,
}
#[derive(Debug,Serialize)]
pub struct ReviewDiscussionReference {pub thread_id:u64,pub capture_blake3:String,pub raw_utf8_blake3:String,pub cursor:String}
pub struct ReviewDiscussionPage {pub records:Vec<ReviewDiscussionReference>,pub next:Option<String>}
fn hash_valid(hash:&str)->bool {hash.len()==64 && hash.bytes().all(|b|b.is_ascii_digit() || (b'a'..=b'f').contains(&b))}
fn scope(project:&str,review:&str)->anyhow::Result<String> {
    vk("project_id",project)?;vk("review_id",review)?;
    anyhow::ensure!(project.len()<=128 && review.len()<=256,"review capture identity too long");
    Ok(format!("{project}\0{PREFIX}{review}\0"))
}
fn body_key(project:&str,hash:&str)->anyhow::Result<String> {
    anyhow::ensure!(hash_valid(hash),"invalid review payload hash");Ok(format!("{project}\0{BODIES}{hash}"))
}
fn suffix(thread:u64,hash:&str)->anyhow::Result<String> {
    anyhow::ensure!(hash_valid(hash),"capture hash must be 64 lowercase hexadecimal characters");Ok(format!("{thread:020}:{hash}"))
}
fn parse_suffix(value:&str)->anyhow::Result<(u64,&str)> {
    anyhow::ensure!(value.len()==85,"invalid capture catalog cursor length");
    let (number,hash)=value.split_once(':').ok_or_else(||anyhow::anyhow!("invalid capture identity"))?;
    let thread=number.parse::<u64>()?;
    anyhow::ensure!(suffix(thread,hash)?==value,"noncanonical capture identity");Ok((thread,hash))
}
fn quota(bytes:&[u8])->anyhow::Result<(usize,usize)> {
    anyhow::ensure!(bytes.len()==24 && &bytes[..8]==MAGIC,"invalid or unsupported capture quota; preserved");
    let rows=usize::try_from(u64::from_le_bytes(bytes[8..16].try_into().unwrap()))?;
    let bytes=usize::try_from(u64::from_le_bytes(bytes[16..24].try_into().unwrap()))?;
    anyhow::ensure!(rows<=REVIEW_DISCUSSION_PROJECT_ROWS && bytes<=REVIEW_DISCUSSION_PROJECT_BYTES,"inconsistent capture quota; preserved");
    Ok((rows,bytes))
}
fn unpack(bytes:&[u8])->anyhow::Result<(&str,&[u8])> {
    anyhow::ensure!(bytes.len()>=72 && bytes.len()<=72+METADATA_BYTES && &bytes[..8]==LINK,
        "unsupported capture metadata format; preserved");
    let hash=std::str::from_utf8(&bytes[8..72])?;
    anyhow::ensure!(hash_valid(hash),"invalid capture payload reference; preserved");Ok((hash,&bytes[72..]))
}
impl Registry {
    /// All new bodies, metadata and accounting publish atomically. Repeated raw
    /// content is reused even when its source/provenance capture changes. The
    /// 4096-object/128MiB quota includes BOTH tiers, keys and link headers.
    pub fn store_review_discussions(&self,project:&str,records:&[ReviewDiscussionRecord])->anyhow::Result<ReviewDiscussionWrite> {
        vk("project_id",project)?;
        anyhow::ensure!(project.len()<=128 && records.len()<=REVIEW_DISCUSSION_BATCH_ROWS,"review capture batch count exceeded");
        let mut bytes=0usize;let mut keys=std::collections::BTreeSet::new();
        let mut bodies=std::collections::BTreeMap::new();
        for record in records {
            anyhow::ensure!(record.payload.len()<=METADATA_BYTES && record.raw_payload.len()<=BODY_BYTES
                && blake3::hash(record.payload.as_bytes()).to_hex().as_str()==record.capture_blake3
                && blake3::hash(record.raw_payload.as_bytes()).to_hex().as_str()==record.raw_payload_blake3,
                "capture metadata/body exceeds budget or differs from its content hash");
            bytes=bytes.checked_add(record.payload.len()).and_then(|n|n.checked_add(record.raw_payload.len()))
                .ok_or_else(||anyhow::anyhow!("capture batch overflow"))?;
            anyhow::ensure!(bytes<=REVIEW_DISCUSSION_BATCH_BYTES,"review capture batch byte budget exceeded");
            let key=format!("{}{}",scope(project,&record.review_id)?,suffix(record.thread_id,&record.capture_blake3)?);
            anyhow::ensure!(keys.insert(key),"duplicate capture metadata in batch");
            if let Some(prior)=bodies.insert(record.raw_payload_blake3.as_str(),record.raw_payload.as_bytes()) {
                anyhow::ensure!(prior==record.raw_payload.as_bytes(),"same body hash names different bytes; preserved");
            }
        }
        let write=self.db.begin_write()?;let result;
        {
            let mut table=write.open_table(META)?;
            let quota_key=format!("{project}\0{QUOTA}");
            let stated=table.get(quota_key.as_str())?.map(|raw|quota(raw.value())).transpose()?;
            let mut observed=(0usize,0usize);let mut capture_count=0usize;let mut body_count=0usize;
            for is_body in [false,true] {
                let prefix=format!("{project}\0{}",if is_body{BODIES}else{PREFIX});
                for row in table.range(prefix.as_str()..)? {
                    let (key,value)=row?;let key=key.value();if !key.starts_with(&prefix){break;}
                    anyhow::ensure!(observed.0<REVIEW_DISCUSSION_PROJECT_ROWS && key.len()<=512,"capture inventory count/key budget exceeded; preserved");
                    let value=value.value();
                    if is_body {
                        anyhow::ensure!(hash_valid(&key[prefix.len()..]) && value.len()<=BODY_BYTES,"unsupported payload inventory; preserved");
                        body_count+=1;
                    } else {
                        let (review,tail)=key[prefix.len()..].split_once('\0').ok_or_else(||anyhow::anyhow!("invalid capture inventory key"))?;
                        vk("review_id",review)?;anyhow::ensure!(review.len()<=256,"invalid capture review identity");
                        let (_,hash)=parse_suffix(tail)?;let (raw_hash,metadata)=unpack(value)?;
                        anyhow::ensure!(blake3::hash(metadata).to_hex().as_str()==hash,"capture metadata hash differs; preserved");
                        let body=table.get(body_key(project,raw_hash)?.as_str())?
                            .ok_or_else(||anyhow::anyhow!("capture references a missing raw payload; preserved"))?;
                        anyhow::ensure!(body.value().len()<=BODY_BYTES,"capture raw payload exceeds budget; preserved");
                        capture_count+=1;
                    }
                    observed.0+=1;
                    observed.1=observed.1.checked_add(key.len()).and_then(|n|n.checked_add(value.len()))
                        .ok_or_else(||anyhow::anyhow!("capture inventory overflow"))?;
                    anyhow::ensure!(observed.1<=REVIEW_DISCUSSION_PROJECT_BYTES,"capture inventory byte budget exceeded; preserved");
                }
            }
            anyhow::ensure!(stated.is_some() || observed.0==0,"retained captures/bodies lack quota authority; preserved");
            anyhow::ensure!(stated.unwrap_or((0,0))==observed,"capture quota differs from retained inventory; preserved");
            let mut counts=observed;let mut pending_bodies=Vec::new();let mut pending_captures=Vec::new();
            for (hash,payload) in &bodies {
                let key=body_key(project,hash)?;
                if let Some(prior)=table.get(key.as_str())? {
                    anyhow::ensure!(prior.value()==*payload,"immutable raw payload differs; preserved");continue;
                }
                counts.0=counts.0.checked_add(1).ok_or_else(||anyhow::anyhow!("object count overflow"))?;
                counts.1=counts.1.checked_add(key.len()).and_then(|n|n.checked_add(payload.len())).ok_or_else(||anyhow::anyhow!("object bytes overflow"))?;
                pending_bodies.push((key,*payload));
            }
            for record in records {
                let key=format!("{}{}",scope(project,&record.review_id)?,suffix(record.thread_id,&record.capture_blake3)?);
                if let Some(prior)=table.get(key.as_str())? {
                    let (raw_hash,metadata)=unpack(prior.value())?;
                    anyhow::ensure!(raw_hash==record.raw_payload_blake3 && metadata==record.payload.as_bytes(),"immutable capture metadata differs; preserved");continue;
                }
                counts.0=counts.0.checked_add(1).ok_or_else(||anyhow::anyhow!("object count overflow"))?;
                counts.1=counts.1.checked_add(key.len()).and_then(|n|n.checked_add(72)).and_then(|n|n.checked_add(record.payload.len()))
                    .ok_or_else(||anyhow::anyhow!("object bytes overflow"))?;
                pending_captures.push((key,record));
            }
            anyhow::ensure!(counts.0<=REVIEW_DISCUSSION_PROJECT_ROWS && counts.1<=REVIEW_DISCUSSION_PROJECT_BYTES,
                "retained discussion quota exceeded (4096 total capture/body objects,128 MiB); existing evidence preserved; no summaries published by this import");
            result=ReviewDiscussionWrite {stored:pending_captures.len(),unchanged:records.len()-pending_captures.len(),
                stored_payloads:pending_bodies.len(),reused_payloads:bodies.len()-pending_bodies.len(),
                retained_records:counts.0,retained_bytes:counts.1,retained_captures:capture_count+pending_captures.len(),retained_payloads:body_count+pending_bodies.len()};
            if pending_bodies.is_empty() && pending_captures.is_empty(){return Ok(result);}
            // Every object/link/quota check above precedes the first write.
            for (key,payload) in pending_bodies {table.insert(key.as_str(),payload)?;}
            for (key,record) in pending_captures {
                let value=[LINK.as_slice(),record.raw_payload_blake3.as_bytes(),record.payload.as_bytes()].concat();
                table.insert(key.as_str(),value.as_slice())?;
            }
            let state=[MAGIC.as_slice(),&(counts.0 as u64).to_le_bytes(),&(counts.1 as u64).to_le_bytes()].concat();
            table.insert(quota_key.as_str(),state.as_slice())?;
        }
        write.commit()?;Ok(result)
    }
    /// Metadata and raw body are borrowed under one actual read transaction.
    /// Both content hashes are checked before the callback; no handle escapes.
    pub fn inspect_review_discussion<T>(&self,project:&str,review:&str,thread:u64,hash:&str,
        inspect:impl FnOnce(&[u8],&[u8])->anyhow::Result<T>)->anyhow::Result<T> {
        let key=format!("{}{}",scope(project,review)?,suffix(thread,hash)?);
        let read=self.db.begin_read()?;let table=read.open_table(META)?;
        let value=table.get(key.as_str())?.ok_or_else(||anyhow::anyhow!("exact captured discussion metadata unavailable"))?;
        let (raw_hash,metadata)=unpack(value.value())?;
        anyhow::ensure!(blake3::hash(metadata).to_hex().as_str()==hash,"capture metadata hash differs; preserved");
        let body=table.get(body_key(project,raw_hash)?.as_str())?.ok_or_else(||anyhow::anyhow!("exact captured raw payload unavailable"))?;
        let body=body.value();
        anyhow::ensure!(body.len()<=BODY_BYTES && blake3::hash(body).to_hex().as_str()==raw_hash,"raw payload failed bounded hash check; preserved");
        inspect(metadata,body)
    }
    pub fn list_review_discussions(&self,project:&str,review:&str,after:Option<&str>)->anyhow::Result<ReviewDiscussionPage> {
        let prefix=scope(project,review)?;if let Some(after)=after{parse_suffix(after)?;}
        let start=format!("{prefix}{}",after.unwrap_or_default());let read=self.db.begin_read()?;let table=read.open_table(META)?;
        let mut records=Vec::new();let mut more=false;
        for entry in table.range(start.as_str()..)? {
            let (key,value)=entry?;let Some(local)=key.value().strip_prefix(&prefix)else{break;};
            if after.is_some_and(|after|local<=after){continue;}
            anyhow::ensure!(local.len()==85,"unsupported capture catalog key; preserved");
            if records.len()==16{more=true;break;}
            let (thread_id,hash)=parse_suffix(local)?;let (raw_hash,metadata)=unpack(value.value())?;
            anyhow::ensure!(blake3::hash(metadata).to_hex().as_str()==hash,"catalog capture metadata hash differs; preserved");
            records.push(ReviewDiscussionReference {thread_id,capture_blake3:hash.into(),raw_utf8_blake3:raw_hash.into(),cursor:local.into()});
        }
        let next=more.then(||records.last().unwrap().cursor.clone());Ok(ReviewDiscussionPage {records,next})
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn record(n:usize)->ReviewDiscussionRecord {
        let raw_payload=format!("Complete original raw evidence {n}");let raw_payload_blake3=blake3::hash(raw_payload.as_bytes()).to_hex().to_string();
        let payload=format!("{{\"original_payload_hash\":\"{raw_payload_blake3}\",\"source\":{n}}}");
        ReviewDiscussionRecord {review_id:"PR-1".into(),thread_id:n as u64,capture_blake3:blake3::hash(payload.as_bytes()).to_hex().to_string(),payload,raw_payload_blake3,raw_payload}
    }
    #[test]
    fn immutable_discussion_quota_refuses_atomically_and_old_evidence_remains_recoverable()->anyhow::Result<()> {
        let temporary=tempfile::tempdir()?;
        let result=(||->anyhow::Result<()> {
            let registry=Registry::open(&temporary.path().join("registry.redb"))?;
            for start in (0..REVIEW_DISCUSSION_PROJECT_ROWS/2).step_by(REVIEW_DISCUSSION_BATCH_ROWS) {
                let batch=(start..start+REVIEW_DISCUSSION_BATCH_ROWS).map(record).collect::<Vec<_>>();
                let stored=registry.store_review_discussions("p",&batch)?;
                assert_eq!((stored.stored,stored.stored_payloads),(1024,1024));
            }
            let original=record(0);let repeated=registry.store_review_discussions("p",&[record(0)])?;
            assert_eq!((repeated.stored,repeated.stored_payloads,repeated.unchanged,repeated.retained_records),(0,0,1,4096));
            assert!(registry.store_review_discussions("p",&[record(0),record(2048)]).is_err());
            assert_eq!(registry.inspect_review_discussion("p","PR-1",0,&original.capture_blake3,
                |metadata,body|Ok((std::str::from_utf8(metadata)?.to_owned(),std::str::from_utf8(body)?.to_owned())))?,(original.payload,original.raw_payload));
            let refused=record(2048);
            assert!(registry.inspect_review_discussion("p","PR-1",2048,&refused.capture_blake3,|_,_|Ok(())).is_err());
            {
                let read=registry.db.begin_read()?;let table=read.open_table(META)?;
                assert!(table.get(body_key("p",&refused.raw_payload_blake3)?.as_str())?.is_none(),"quota refusal must not strand an unreferenced body object");
            }
            let page=registry.list_review_discussions("p","PR-1",None)?;assert_eq!(page.records.len(),16);assert!(page.next.is_some());
            assert!(registry.inspect_review_discussion("other","PR-1",0,&original.capture_blake3,|_,_|Ok(())).is_err());Ok(())
        })();let cleanup=temporary.close();result?;cleanup?;Ok(())
    }
}
