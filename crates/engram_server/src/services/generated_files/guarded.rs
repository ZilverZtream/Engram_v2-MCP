//! Mandatory Windows file sharing and handle-bound mutations. No path CAS is
//! claimed on other platforms or on remote/reparse/non-NTFS directories.
use std::path::{Path, PathBuf};
#[cfg(windows)]
use std::{fs::File, sync::Arc};

#[cfg(windows)]
mod windows {
    use super::*;
    use std::{io::{Read, Write}, os::windows::{fs::OpenOptionsExt, io::AsRawHandle}, path::{Component, Prefix}};
    use windows_sys::Win32::{Foundation::{GENERIC_READ, GENERIC_WRITE}, Storage::FileSystem::*};

    fn path_label(path: &Path) -> String { path.to_string_lossy().chars().take(240).collect() }

    fn information(file: &File) -> anyhow::Result<BY_HANDLE_FILE_INFORMATION> {
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        // SAFETY: a live File handle and a correctly sized writable structure.
        anyhow::ensure!(unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } != 0,
            "Cannot inspect protected artifact: {}", std::io::Error::last_os_error());
        anyhow::ensure!(info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT == 0,
            "Publication guard refuses reparse points");
        Ok(info)
    }

    fn file_identity(file: &File) -> anyhow::Result<String> {
        let info = information(file)?;
        Ok(format!("{:08x}:{:08x}{:08x}", info.dwVolumeSerialNumber, info.nFileIndexHigh, info.nFileIndexLow))
    }

    fn directory(path: &Path) -> anyhow::Result<File> {
        // Metadata-only access does not participate in Windows share-access
        // enforcement. GENERIC_READ includes directory listing access, so
        // withholding FILE_SHARE_DELETE actually pins this directory's name.
        let file = std::fs::OpenOptions::new().read(true).access_mode(GENERIC_READ)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT).open(path)
            .map_err(|error| anyhow::Error::new(error).context(format!("Cannot pin publication directory {}", path_label(path))))?;
        anyhow::ensure!(information(&file)?.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0,
            "Publication ancestor is not a directory");
        Ok(file)
    }

    fn local_ntfs(file: &File) -> anyhow::Result<()> {
        let mut name = [0u16; 32];
        // SAFETY: live directory handle and a bounded writable UTF-16 buffer;
        // optional outputs are null as permitted by this API.
        anyhow::ensure!(unsafe { GetVolumeInformationByHandleW(file.as_raw_handle(), std::ptr::null_mut(), 0,
            std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), name.as_mut_ptr(), name.len() as u32) } != 0,
            "Publication volume cannot be verified: {}", std::io::Error::last_os_error());
        let end = name.iter().position(|value| *value == 0).unwrap_or(name.len());
        anyhow::ensure!(String::from_utf16(&name[..end])? == "NTFS",
            "Guarded publication requires a local NTFS volume; complete proposal preserved");
        Ok(())
    }

    #[derive(Clone)]
    pub(crate) struct Root {
        pub path: PathBuf,
        _ancestors: Arc<Vec<File>>,
    }

    impl Root {
        /// Acquire the complete root authority before any receipt or artifact
        /// observation. Directory handles deny rename/delete for this lifetime.
        pub fn open(root: &Path) -> anyhow::Result<Self> {
            let canonical = std::fs::canonicalize(root)?;
            let mut path = PathBuf::new();
            let mut ancestors = Vec::new();
            for component in canonical.components() {
                match component {
                    Component::Prefix(prefix) => {
                        anyhow::ensure!(matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_)),
                            "Guarded publication refuses remote/unsupported path roots; complete proposal preserved");
                        path.push(component.as_os_str());
                        continue;
                    }
                    Component::RootDir => path.push(component.as_os_str()),
                    Component::Normal(part) => path.push(part),
                    _ => anyhow::bail!("Unsupported publication ancestor"),
                }
                anyhow::ensure!(ancestors.len() < 64, "Publication ancestor guard budget exceeded");
                if ancestors.is_empty() {
                    use std::os::windows::ffi::OsStrExt;
                    let drive: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
                    // SAFETY: NUL-terminated volume-root path. DRIVE_FIXED=3;
                    // mapped/network/removable volumes are deliberately refused.
                    anyhow::ensure!(unsafe { GetDriveTypeW(drive.as_ptr()) } == 3,
                        "Guarded publication requires a fixed local volume; complete proposal preserved");
                }
                let file = directory(&path)?;
                if ancestors.is_empty() { local_ntfs(&file)?; }
                ancestors.push(file);
            }
            anyhow::ensure!(!ancestors.is_empty(), "Publication path has no volume anchor");
            Ok(Self { path: canonical, _ancestors: Arc::new(ancestors) })
        }

        pub fn identity(&self) -> anyhow::Result<String> {
            file_identity(self._ancestors.last().ok_or_else(|| anyhow::anyhow!("Publication root authority unavailable"))?)
        }
    }

    #[derive(Clone)]
    pub(crate) struct Parent {
        pub path: PathBuf,
        _root: Root,
        _ancestors: Arc<Vec<File>>,
    }

    impl Parent {
        pub fn open(root: &Root, relative: &str, create: bool) -> anyhow::Result<Self> {
            let destination = engram_core::safe_join(&root.path, relative)?;
            let parent = destination.parent().ok_or_else(|| anyhow::anyhow!("Artifact has no parent"))?;
            let suffix = parent.strip_prefix(&root.path)?;
            let mut path = root.path.clone();
            let mut ancestors = Vec::new();
            for component in suffix.components() {
                let Component::Normal(part) = component else { anyhow::bail!("Unsupported publication ancestor"); };
                path.push(part);
                anyhow::ensure!(ancestors.len() < 64, "Publication ancestor guard budget exceeded");
                match directory(&path) {
                    Ok(file) => ancestors.push(file),
                    Err(error) if create
                        && error.downcast_ref::<std::io::Error>().is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) => {
                        // The preceding directory handles deny deletion/rename.
                        // create_dir does not overwrite a competing entry.
                        match std::fs::create_dir(&path) {
                            Ok(()) => {},
                            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {},
                            Err(error) => return Err(error.into()),
                        }
                        ancestors.push(directory(&path)?);
                    }
                    Err(error) => return Err(error),
                }
            }
            Ok(Self { path: parent.to_owned(), _root: root.clone(), _ancestors: Arc::new(ancestors) })
        }

        pub fn identity(&self) -> anyhow::Result<String> {
            match self._ancestors.last() { Some(file) => file_identity(file), None => self._root.identity() }
        }

        pub fn existing(&self, relative: &str) -> anyhow::Result<Option<HeldFile>> {
            let name = Path::new(relative).file_name().ok_or_else(|| anyhow::anyhow!("Artifact has no filename"))?;
            let path = self.path.join(name);
            let file = match std::fs::OpenOptions::new().read(true).write(true).access_mode(GENERIC_READ | GENERIC_WRITE | DELETE)
                .share_mode(FILE_SHARE_READ).custom_flags(FILE_FLAG_OPEN_REPARSE_POINT).open(&path) {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(anyhow::Error::new(error).context(format!("Cannot guard existing artifact {}", path_label(&path)))),
            };
            let info = information(&file)?;
            anyhow::ensure!(info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0, "Artifact is not a regular file");
            anyhow::ensure!(file.metadata()?.len() <= super::super::MAX_FILE_BYTES, "Artifact exceeds guarded verification budget");
            let mut bytes = Vec::new();
            (&file).take(super::super::MAX_FILE_BYTES + 1).read_to_end(&mut bytes)?;
            anyhow::ensure!(bytes.len() as u64 <= super::super::MAX_FILE_BYTES, "Artifact exceeds guarded verification budget");
            file.sync_all()?;
            let identity = file_identity(&file)?;
            Ok(Some(HeldFile { file, path, parent: self.clone(), identity,
                hash: blake3::hash(&bytes).to_hex().to_string(), cleanup: false }))
        }

        pub fn stage(&self, text: &str) -> anyhow::Result<HeldFile> {
            anyhow::ensure!(text.len() as u64 <= super::super::MAX_FILE_BYTES, "Artifact exceeds staging budget");
            let path = self.path.join(format!(".engram-{}.stage", uuid::Uuid::new_v4().simple()));
            let file = std::fs::OpenOptions::new().read(true).write(true).access_mode(GENERIC_READ | GENERIC_WRITE | DELETE)
                .share_mode(FILE_SHARE_READ).create_new(true).custom_flags(FILE_FLAG_OPEN_REPARSE_POINT).open(&path)
                .map_err(|error| anyhow::Error::new(error).context(format!("Cannot create owned publication stage in {}", path_label(&self.path))))?;
            // Ownership begins immediately after create_new, before any
            // fallible identity/read/write/clone operation. Keep the original
            // handle: cleanup never reopens a potentially replaced pathname.
            let mut staged = HeldFile { file, path, parent: self.clone(), identity: String::new(),
                hash: String::new(), cleanup: true };
            let prepared = (|| -> anyhow::Result<()> {
                staged.identity = file_identity(&staged.file)?;
                staged.hash = blake3::hash(text.as_bytes()).to_hex().to_string();
                staged.file.write_all(text.as_bytes())?;
                staged.file.sync_all()?;
                Ok(())
            })();
            if let Err(error) = prepared {
                if let Err(cleanup) = staged.mark_deleted() {
                    staged.retain();
                    return Err(error.context(format!("Owned stage retained at {} after cleanup failed ({cleanup}); inspect this exact file before removal", staged.path.display())));
                }
                staged.retain();
                return Err(error);
            }
            Ok(staged)
        }
    }

    pub(crate) struct HeldFile {
        file: File,
        pub path: PathBuf,
        parent: Parent,
        pub identity: String,
        pub hash: String,
        cleanup: bool,
    }

    impl HeldFile {
        pub fn retain(&mut self) { self.cleanup = false; }

        pub fn rename(&mut self, name: &std::ffi::OsStr) -> anyhow::Result<()> {
            use std::os::windows::ffi::OsStrExt;
            anyhow::ensure!(Path::new(name).components().count() == 1
                && matches!(Path::new(name).components().next(), Some(Component::Normal(_))), "Invalid guarded rename target");
            let destination = self.parent.path.join(name);
            let wide: Vec<u16> = destination.as_os_str().encode_wide().collect();
            anyhow::ensure!(wide.len() <= 32767, "Publication rename path exceeds Windows limit");
            let offset = std::mem::offset_of!(FILE_RENAME_INFO, FileName);
            // Win32 FILE_RENAME_INFO.FileName is NUL-terminated even though
            // FileNameLength counts only its non-NUL bytes. Do not depend on
            // incidental zero padding from the allocation's u64 alignment.
            let size = offset + (wide.len() + 1) * 2;
            // u64 storage supplies FILE_RENAME_INFO's alignment on Windows.
            let mut storage = vec![0u64; size.div_ceil(8)];
            let info = storage.as_mut_ptr().cast::<FILE_RENAME_INFO>();
            // SAFETY: aligned storage covers the header and complete UTF-16
            // payload; the live source handle owns DELETE access. ReplaceIfExists
            // remains zero, so a human destination can never be overwritten.
            unsafe {
                (*info).RootDirectory = std::ptr::null_mut();
                (*info).FileNameLength = (wide.len() * 2) as u32;
                std::ptr::copy_nonoverlapping(wide.as_ptr(), (*info).FileName.as_mut_ptr(), wide.len());
                if SetFileInformationByHandle(self.file.as_raw_handle(), FileRenameInfo,
                    info.cast(), size as u32) == 0 {
                    return Err(anyhow::Error::new(std::io::Error::last_os_error()).context(format!(
                        "Guarded no-replace rename to {} failed ({} UTF-16 units, {} buffer bytes)",
                        path_label(&destination), wide.len(), size)));
                }
            }
            self.path = destination;
            Ok(())
        }

        fn mark_deleted(&self) -> anyhow::Result<()> {
            let info = FILE_DISPOSITION_INFO { DeleteFile: true };
            // SAFETY: live DELETE-capable handle and exact native structure.
            anyhow::ensure!(unsafe { SetFileInformationByHandle(self.file.as_raw_handle(), FileDispositionInfo,
                (&info as *const FILE_DISPOSITION_INFO).cast(), std::mem::size_of_val(&info) as u32) } != 0,
                "Cannot retire guarded artifact: {}", std::io::Error::last_os_error());
            Ok(())
        }

        pub fn delete(mut self) -> anyhow::Result<()> {
            self.mark_deleted()?;
            self.cleanup = false;
            Ok(())
        }
    }

    impl Drop for HeldFile {
        fn drop(&mut self) {
            if self.cleanup {
                if let Err(error) = self.mark_deleted() {
                    tracing::warn!(path = %self.path.display(), error = %error,
                        "Owned generated-artifact stage cleanup failed; retain for inspection before removal");
                }
            }
        }
    }
}

#[cfg(windows)]
pub(super) use windows::{HeldFile, Parent, Root};

#[cfg(not(windows))]
pub(super) struct Root { pub path: PathBuf }
#[cfg(not(windows))]
impl Root {
    pub fn identity(&self) -> anyhow::Result<String> { anyhow::bail!("Mandatory guarded publication is unavailable on this platform") }
    pub fn open(_root: &Path) -> anyhow::Result<Self> {
        anyhow::bail!("Mandatory guarded publication is unavailable on this platform; complete proposal preserved")
    }
}

#[cfg(not(windows))]
pub(super) struct Parent { pub path: PathBuf }
#[cfg(not(windows))]
pub(super) struct HeldFile {
    pub path: PathBuf,
    pub identity: String,
    pub hash: String,
}
#[cfg(not(windows))]
impl Parent {
    pub fn open(_root: &Root, _relative: &str, _create: bool) -> anyhow::Result<Self> {
        anyhow::bail!("Mandatory guarded publication is unavailable on this platform; complete proposal preserved")
    }
    pub fn identity(&self) -> anyhow::Result<String> { unreachable!() }
    pub fn existing(&self, _relative: &str) -> anyhow::Result<Option<HeldFile>> { unreachable!() }
    pub fn stage(&self, _text: &str) -> anyhow::Result<HeldFile> { unreachable!() }
}
#[cfg(not(windows))]
impl HeldFile {
    pub fn retain(&mut self) {}
    pub fn rename(&mut self, _name: &std::ffi::OsStr) -> anyhow::Result<()> { unreachable!() }
    pub fn delete(self) -> anyhow::Result<()> { unreachable!() }
}
