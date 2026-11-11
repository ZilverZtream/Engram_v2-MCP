//! Source body reads stay within the bytes admitted before blocking work.
//! Decoding and parser output allocations have separate accounting contracts.
use std::fs::{File, Metadata};
use std::io::{self, Read};
use std::path::Path;

pub(super) fn read_source(path: &Path, admitted_len: u64) -> anyhow::Result<(Option<Vec<u8>>, Metadata)> {
    anyhow::ensure!(admitted_len <= crate::ingest::MAX_FILE_SIZE,
        "Source admission exceeds the ingestion limit for {}; retry the update", path.display());
    let mut file = File::open(path).map_err(|error|
        anyhow::anyhow!("Cannot open source content {}: {error}; retry the update", path.display()))?;
    let metadata = file.metadata().map_err(|error|
        anyhow::anyhow!("Cannot inspect admitted source {}: {error}; retry the update", path.display()))?;
    anyhow::ensure!(metadata.is_file(), "Admitted source is not a regular file: {}; retry the update", path.display());
    anyhow::ensure!(metadata.len() == admitted_len,
        "Source length changed after admission for {} (admitted {admitted_len}, observed {}); retry the update",
        path.display(), metadata.len());
    let bytes = read_admitted(&mut file, admitted_len as usize, path)?;
    Ok((bytes, metadata))
}

fn read_exact_source(reader: &mut impl Read, bytes: &mut [u8], path: &Path) -> anyhow::Result<()> {
    reader.read_exact(bytes).map_err(|error| {
        if error.kind() == io::ErrorKind::UnexpectedEof {
            anyhow::anyhow!("Source truncated after admission while reading {}; retry the update", path.display())
        } else {
            anyhow::anyhow!("Cannot read source content {}: {error}; retry the update", path.display())
        }
    })
}

fn read_admitted(reader: &mut impl Read, admitted_len: usize, path: &Path) -> anyhow::Result<Option<Vec<u8>>> {
    let mut prefix = [0u8; 8192];
    let prefix_len = admitted_len.min(prefix.len());
    read_exact_source(reader, &mut prefix[..prefix_len], path)?;
    // Preserve prefix-only classification without allocating a binary body.
    if prefix[..prefix_len].contains(&0) {
        return Ok(None);
    }
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(admitted_len).map_err(|error|
        anyhow::anyhow!("Cannot allocate admitted source bytes for {}: {error}; retry the update", path.display()))?;
    bytes.resize(admitted_len, 0);
    bytes[..prefix_len].copy_from_slice(&prefix[..prefix_len]);
    read_exact_source(reader, &mut bytes[prefix_len..], path)?;
    // Read one stack byte, never extend the admitted body allocation.
    let mut extra = [0u8; 1];
    loop {
        match reader.read(&mut extra) {
            Ok(0) => return Ok(Some(bytes)),
            Ok(_) => anyhow::bail!("Source grew after admission while reading {}; retry the update", path.display()),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => anyhow::bail!("Cannot verify admitted source EOF for {}: {error}; retry the update", path.display()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actual_growth_and_truncation_after_admission_are_refused() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("source.rs");
        std::fs::write(&path, b"pub fn original() {}\n").unwrap();
        let admitted = std::fs::metadata(&path).unwrap().len();
        std::fs::write(&path, b"pub fn original() {}\npub fn added() {}\n").unwrap();
        let error = read_source(&path, admitted).unwrap_err().to_string();
        assert!(error.contains("changed after admission") && error.contains("retry"));
        std::fs::write(&path, b"short").unwrap();
        assert!(read_source(&path, admitted).unwrap_err().to_string().contains("changed after admission"));
        root.close().unwrap();
    }

    #[test]
    fn exact_empty_binary_and_size_limit_controls() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("source.rs");
        let exact = b"pub fn source() {}\n";
        std::fs::write(&path, exact).unwrap();
        let (bytes, metadata) = read_source(&path, exact.len() as u64).unwrap();
        assert_eq!(bytes.unwrap(), exact);
        assert_eq!(metadata.len(), exact.len() as u64);
        std::fs::write(&path, b"").unwrap();
        assert_eq!(read_source(&path, 0).unwrap().0, Some(Vec::new()));
        std::fs::write(&path, b"binary\0payload").unwrap();
        assert!(read_source(&path, 14).unwrap().0.is_none());
        let mut late_null = vec![b'x'; 8192];
        late_null.extend_from_slice(b"\0tail");
        std::fs::write(&path, &late_null).unwrap();
        assert_eq!(read_source(&path, late_null.len() as u64).unwrap().0, Some(late_null));
        assert!(read_source(&path, crate::ingest::MAX_FILE_SIZE + 1).unwrap_err().to_string().contains("ingestion limit"));
        root.close().unwrap();
    }

    #[test]
    fn growth_during_read_consumes_only_admitted_body_and_one_probe_byte() {
        // Unlike metadata-only controls, this reader continues producing bytes
        // after the admitted length. The read must fail without draining it.
        struct Growing { consumed: usize }
        impl Read for Growing {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                buffer.fill(b'x');
                self.consumed += buffer.len();
                Ok(buffer.len())
            }
        }
        let admitted = 8192 + 23;
        let mut reader = Growing { consumed: 0 };
        assert!(read_admitted(&mut reader, admitted, Path::new("growing.rs"))
            .unwrap_err().to_string().contains("grew after admission"));
        assert_eq!(reader.consumed, admitted + 1);
    }

    #[test]
    fn truncation_during_prefix_or_body_is_refused() {
        for (available, admitted) in [(3, 4), (8192 + 2, 8192 + 3)] {
            let mut reader = io::Cursor::new(vec![b'x'; available]);
            assert!(read_admitted(&mut reader, admitted, Path::new("truncated.rs"))
                .unwrap_err().to_string().contains("truncated after admission"));
        }
    }
}
