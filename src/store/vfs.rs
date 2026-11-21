//! A read-only view of a branch as a Windows drive.
//!
//! `gesta mount <branch> <drive>` presents the branch's tree as a filesystem, so a tool that only
//! knows how to open files can read a repository that has no checkout. The point is not to replace a
//! working tree: it is to let an editor, a compiler or a diff tool read a commit without every file
//! being written to disk first.
//!
//! Everything here is read-only on purpose. A writable view has to answer what a write means when the
//! branch is not checked out, which is a merge policy, and a filesystem that silently discards a write
//! is worse than one that refuses it.
//!
//! The tree comes from `branch_tree`, which the store maintains per branch, and each file's bytes come
//! from `objects` by hash. Neither needs a working tree.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::RwLock;

use winfsp::filesystem::{
    DirBuffer, DirInfo, FileInfo, FileSecurity, FileSystemContext, OpenFileInfo, WideNameInfo,
};
use winfsp::host::{CoarseGuard, FileSystemHost, VolumeParams};
use winfsp::{FspError, U16CStr};

/// `FILE_ATTRIBUTE_DIRECTORY`. The crate types the field as a plain `u32` and does not re-export the
/// Windows constants, so they are named here by value with the name they have in the SDK.
const ATTR_DIRECTORY: u32 = 0x10;
/// `FILE_ATTRIBUTE_NORMAL`.
const ATTR_NORMAL: u32 = 0x80;

/// `STATUS_OBJECT_NAME_NOT_FOUND`, which is what a reader means by "no such file".
const NOT_FOUND: i32 = 0xC0000034u32 as i32;
/// `STATUS_INVALID_DEVICE_REQUEST`, for an operation a read-only view does not offer.
const NOT_SUPPORTED: i32 = 0xC0000010u32 as i32;

/// One open handle.
///
/// A directory carries its listing buffer, which is the shape the driver expects: the buffer is
/// acquired on the first `read_directory` and reused while the handle is open, so a reader that walks
/// a directory in several calls does not rebuild the entries every time.
pub struct OpenFile {
    path: String,
    /// The whole file, read when it was opened. A source file is small and a reader asks for several
    /// buffers, so one read per open beats one query per four kilobytes.
    body: Vec<u8>,
    /// The directory's entries. Only set for a directory.
    listing: Option<DirBuffer>,
    is_dir: bool,
}

pub struct TreeView {
    /// Repository path to blob hash.
    files: RwLock<HashMap<String, String>>,
    /// Every directory implied by those paths, with its children.
    dirs: RwLock<HashMap<String, Vec<String>>>,
    /// Where the store lives.
    root: String,
}

impl TreeView {
    fn normalise(name: &U16CStr) -> String {
        let raw = name.to_string().unwrap_or_default();
        let trimmed = raw.trim_matches('\\').replace('\\', "/");
        if trimmed.is_empty() {
            ".".to_string()
        } else {
            trimmed
        }
    }

    fn children(&self, dir: &str) -> Vec<String> {
        self.dirs
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(dir)
            .cloned()
            .unwrap_or_default()
    }

    fn is_dir(&self, path: &str) -> bool {
        self.dirs
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(path)
    }

    /// The size recorded for a path, without reading its body.
    fn size_of(&self, path: &str) -> u64 {
        let hash = self
            .files
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(path)
            .cloned();
        let Some(hash) = hash else { return 0 };
        let Ok(conn) = crate::store::open_db(std::path::Path::new(&self.root)) else {
            return 0;
        };
        conn.query_row(
            "SELECT size FROM objects WHERE hash = ?1",
            rusqlite::params![hash],
            |r| r.get::<_, i64>(0),
        )
        .map(|n| n as u64)
        .unwrap_or(0)
    }

    fn body_of(&self, path: &str) -> Option<Vec<u8>> {
        let hash = self
            .files
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(path)
            .cloned()?;
        let conn = crate::store::open_db(std::path::Path::new(&self.root)).ok()?;
        crate::store::get_object_bytes(&conn, &hash).ok()
    }
}

impl FileSystemContext for TreeView {
    type FileContext = OpenFile;

    fn get_security_by_name(
        &self,
        file_name: &U16CStr,
        _security_descriptor: Option<&mut [c_void]>,
        _resolve_reparse_points: impl FnOnce(&U16CStr) -> Option<FileSecurity>,
    ) -> winfsp::Result<FileSecurity> {
        let path = Self::normalise(file_name);
        let is_dir = path == "." || self.is_dir(&path);
        if is_dir
            || self
                .files
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .contains_key(&path)
        {
            // The attributes are how the driver decides whether a path is a directory before it opens
            // it. Returning zero here made every path look like an unattributed entry, and the driver
            // stopped after this call: no open, no read_directory, and a mount that listed nothing.
            return Ok(FileSecurity {
                attributes: if is_dir { ATTR_DIRECTORY } else { ATTR_NORMAL },
                reparse: false,
                sz_security_descriptor: 0,
            });
        }
        Err(FspError::NTSTATUS(NOT_FOUND))
    }

    fn open(
        &self,
        file_name: &U16CStr,
        _create_options: u32,
        _granted_access: winfsp_sys::FILE_ACCESS_RIGHTS,
        file_info: &mut OpenFileInfo,
    ) -> winfsp::Result<Self::FileContext> {
        let path = Self::normalise(file_name);
        let is_dir = path == "." || self.is_dir(&path);
        // The driver reads the entry's attributes and size from `file_info` right after `open`, and a
        // caller that leaves it empty gets a handle the driver treats as neither file nor directory.
        let size = if is_dir {
            0u64
        } else {
            self.files
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .get(&path)
                .and_then(|_| self.body_of(&path).map(|b| b.len() as u64))
                .ok_or(FspError::NTSTATUS(NOT_FOUND))?
        };
        let info: &mut FileInfo = file_info.as_mut();
        info.file_attributes = if is_dir { ATTR_DIRECTORY } else { ATTR_NORMAL };
        info.file_size = size;
        info.allocation_size = size;

        // The driver uses the normalised name to decide whether the handle it got back belongs to the
        // path it asked for. Leaving it unset made `open` succeed and the driver stop there, retrying
        // the open instead of reading the directory.
        let wide: Vec<u16> = path.encode_utf16().collect();
        file_info.set_normalized_name(&wide, None);
        if is_dir {
            return Ok(OpenFile {
                path,
                body: Vec::new(),
                listing: Some(DirBuffer::new()),
                is_dir: true,
            });
        }
        let body = self.body_of(&path).ok_or(FspError::NTSTATUS(NOT_FOUND))?;
        Ok(OpenFile {
            path,
            body,
            listing: None,
            is_dir: false,
        })
    }

    fn close(&self, _context: Self::FileContext) {}

    /// Flush is a no-op on a read-only view, and it must answer `Ok`.
    ///
    /// The trait's default returns `STATUS_INVALID_DEVICE_REQUEST`, which is what "Incorrect
    /// function" is. The driver flushes after it opens a handle, so every listing and every read
    /// came back as that error while the callbacks underneath were fine.
    fn flush(
        &self,
        _context: Option<&Self::FileContext>,
        _file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        Ok(())
    }

    fn get_file_info(
        &self,
        context: &Self::FileContext,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        file_info.file_attributes = if context.is_dir {
            ATTR_DIRECTORY
        } else {
            ATTR_NORMAL
        };
        file_info.file_size = if context.is_dir {
            0
        } else {
            context.body.len() as u64
        };
        file_info.allocation_size = file_info.file_size;
        Ok(())
    }

    fn read_directory(
        &self,
        context: &Self::FileContext,
        _pattern: Option<&U16CStr>,
        marker: winfsp::filesystem::DirMarker,
        buffer: &mut [u8],
    ) -> winfsp::Result<u32> {
        let Some(listing) = context.listing.as_ref() else {
            return Err(FspError::NTSTATUS(NOT_SUPPORTED));
        };
        // Reset only on the first call. Resetting every time made the driver read the same first
        // entries forever and report failure, because it asks for the listing in as many calls as the
        // buffer needs and expects each call to continue where the last one stopped.
        let first = marker.is_none();
        let lock = listing.acquire(first, None)?;
        if first {
            let parent = if context.path == "." {
                String::new()
            } else {
                context.path.clone()
            };
            for name in self.children(&parent) {
                let full = if parent.is_empty() {
                    name.clone()
                } else {
                    format!("{}/{}", parent, name)
                };
                let is_dir = self.is_dir(&full);
                let mut entry: DirInfo<255> = DirInfo::new();
                entry.reset();
                entry.set_name(name.as_str())?;
                let info = entry.file_info_mut();
                info.file_attributes = if is_dir { ATTR_DIRECTORY } else { ATTR_NORMAL };
                // The timestamps have to be plausible. Zero is 1601, which the directory reader
                // rejects with "Incorrect function" while every callback underneath returned
                // correct data.
                let now =
                    crate::support::clock::unix_now() as u64 * 10_000_000 + 116_444_736_000_000_000;
                info.creation_time = now;
                info.last_access_time = now;
                info.last_write_time = now;
                info.change_time = now;
                // Sizes come from the cache, not from reading each body: a listing of a thousand
                // files that read every object would open the whole tree to answer one query. The
                // number is what the store recorded when the file was written.
                let size = if is_dir { 0 } else { self.size_of(&full) };
                info.file_size = size;
                info.allocation_size = size;
                lock.write(&mut entry)?;
            }
        }
        drop(lock);
        Ok(listing.read(marker, buffer))
    }

    fn read(
        &self,
        context: &Self::FileContext,
        buffer: &mut [u8],
        offset: u64,
    ) -> winfsp::Result<u32> {
        if context.is_dir {
            return Err(FspError::NTSTATUS(NOT_SUPPORTED));
        }
        let start = offset as usize;
        if start >= context.body.len() {
            // Reading at or past the end is zero bytes, not an error: a reader that tests for EOF by
            // reading one byte past the end does this on every file it opens.
            return Ok(0);
        }
        let end = (start + buffer.len()).min(context.body.len());
        let n = end - start;
        buffer[..n].copy_from_slice(&context.body[start..end]);
        Ok(n as u32)
    }
}

/// Mount a branch on a drive letter or directory. Blocks until the process ends.
pub fn mount(root: &std::path::Path, branch: &str, mount_point: &str) -> crate::ZResult<()> {
    let conn = crate::store::open_db(root)?;
    let mut files: HashMap<String, String> = HashMap::new();
    {
        let mut stmt = conn.prepare("SELECT path, hash FROM branch_tree WHERE branch = ?1")?;
        let rows = stmt.query_map(rusqlite::params![branch], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (path, hash) = row?;
            files.insert(path, hash);
        }
    }
    if files.is_empty() {
        return Err(Box::new(crate::support::error::Error::new(
            crate::support::error::Code::NotFound,
            format!("branch {} has no files to show", branch),
        )));
    }

    let mut dirs: HashMap<String, Vec<String>> = HashMap::new();
    dirs.entry(String::new()).or_default();
    for path in files.keys() {
        let parts: Vec<&str> = path.split('/').collect();
        let mut prefix = String::new();
        for (i, part) in parts.iter().enumerate() {
            let full = if prefix.is_empty() {
                part.to_string()
            } else {
                format!("{}/{}", prefix, part)
            };
            let parent = prefix.clone();
            let entry = dirs.entry(parent).or_default();
            if !entry.iter().any(|e| e == part) {
                entry.push(part.to_string());
            }
            if i + 1 < parts.len() {
                dirs.entry(full.clone()).or_default();
            }
            prefix = full;
        }
    }
    for children in dirs.values_mut() {
        children.sort();
    }

    let view = TreeView {
        files: RwLock::new(files),
        dirs: RwLock::new(dirs),
        root: root.to_string_lossy().to_string(),
    };

    // The values WinFSP's own memfs sample uses. A sector size of 512 with one sector per allocation
    // unit gives a 512 byte cluster, and the driver refused every request with a 512 byte cluster even
    // though the mount itself succeeded.
    let mut params = VolumeParams::new();
    params
        .sector_size(512)
        .sectors_per_allocation_unit(8)
        .max_component_length(255)
        .file_info_timeout(1000);
    params.filesystem_name("gesta");
    params.case_sensitive_search(true);
    params.case_preserved_names(true);
    params.unicode_on_disk(true);
    params.read_only_volume(true);

    // The guard is named because `start` exists on two impls and the call site cannot infer which.
    // `CoarseGuard` serialises every callback behind one lock: with a read-only view over a shared
    // map there is nothing to gain from concurrent callbacks, and the coarse guard only needs `Send`.
    let mut host: FileSystemHost<TreeView, CoarseGuard> = FileSystemHost::new(params, view)
        .map_err(|e| {
            Box::new(crate::support::error::Error::new(
                crate::support::error::Code::Usage,
                format!("{}", e),
            ))
        })?;
    host.mount(mount_point).map_err(|e| {
        Box::new(crate::support::error::Error::new(
            crate::support::error::Code::Usage,
            format!("could not mount at {}: {}", mount_point, e),
        ))
    })?;
    host.start_with_threads(0).map_err(|e| {
        Box::new(crate::support::error::Error::new(
            crate::support::error::Code::Usage,
            format!("{}", e),
        ))
    })?;

    println!("mounted {} at {}", branch, mount_point);
    println!("press Ctrl-C to unmount");
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}
