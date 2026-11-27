//! Write a committed file back to disk without following symlinks.
//!
//! `std::fs::write` follows symlinks: if the destination is a symlink to
//! `/etc/passwd`, the write lands outside the repository. This module opens
//! the destination with `O_NOFOLLOW` on Unix so a symlink target is refused
//! instead of overwritten, and refuses symlinked parents on all platforms.

use std::fs;
use std::io::Write;
use std::path::Path;

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

use crate::support::error::{Code, Error};

/// Write `content` to `root.join(rel)`, refusing to follow any symlink.
///
/// If the destination exists and is a symlink, the write is refused. If any
/// parent directory in the chain is a symlink that would let the write escape
/// `root`, the write is refused.
pub fn write_safe(root: &Path, rel: &str, content: &[u8]) -> crate::ZResult<usize> {
    let target = root.join(rel);

    if let Some(parent) = target.parent() {
        // Every component of the chain, not only the direct parent (#273).
        //
        // `O_NOFOLLOW` applies to the last component of the path it is given and nothing else, so a
        // write to `a/c/d.txt` still went through a symlink at `a`: the kernel resolved `a` -> `/etc`
        // while walking to `root/a/c`, and the `O_NOFOLLOW` on that final open was satisfied by an
        // ordinary `/etc/c`. The doc above promised "any parent directory in the chain"; only the
        // direct parent was ever opened.
        //
        // One pass over the components, from the repository root down, refusing at the first symlink.
        // A component that does not exist yet is not a link, so the walk stops there and
        // `create_dir_all` makes the rest. The check runs before the creation, because a symlink that
        // is already in place must be refused rather than have directories built through it.
        let mut so_far = root.to_path_buf();
        for part in rel.split('/') {
            if part.is_empty() || part == "." {
                continue;
            }
            so_far.push(part);
            // The final component is the file itself, which the open below handles; only the
            // directories leading to it are this loop's business.
            if so_far == target {
                break;
            }
            match fs::symlink_metadata(&so_far) {
                Ok(m) if m.file_type().is_symlink() => {
                    return Err(Box::new(Error::new(
                        Code::Usage,
                        format!(
                            "refusing to write {}: {} is a symlink, and a write through it would \
                             leave the repository",
                            target.display(),
                            so_far.display()
                        ),
                    )));
                }
                Ok(_) => {}
                // Missing is not a failure: the directory does not exist yet, and something that does
                // not exist cannot be a symlink. `create_dir_all` below makes it.
                Err(_) => break,
            }
        }
        fs::create_dir_all(parent)?;
        #[cfg(unix)]
        {
            // The direct parent is opened with `O_NOFOLLOW | O_DIRECTORY` as well, which closes the
            // other order: a symlink swapped in between the walk above and this open makes the open
            // fail instead of redirecting the write. Each check alone leaves a window.
            let mut opts = fs::OpenOptions::new();
            opts.read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY);
            opts.open(parent).map_err(|e| {
                Box::new(Error::new(
                    Code::Usage,
                    format!("cannot open parent directory {}: {}", parent.display(), e),
                )) as Box<dyn std::error::Error>
            })?;
        }
    }

    let mut opts = fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);

    #[cfg(unix)]
    {
        // Reject hardlinks to files outside the repository: writing through any
        // link to a shared inode overwrites the other link's content too.
        if let Ok(meta) = fs::symlink_metadata(&target) {
            if meta.file_type().is_symlink() {
                return Err(Box::new(Error::new(
                    Code::Usage,
                    format!("{} is a symbolic link", target.display()),
                )));
            }
            if meta.nlink() > 1 {
                return Err(Box::new(Error::new(
                    Code::Usage,
                    format!("{} has multiple hard links", target.display()),
                )));
            }
        }
        opts.custom_flags(libc::O_NOFOLLOW);
    }

    let mut file = opts.open(&target).map_err(|e| {
        Box::new(Error::new(Code::Other, e.to_string())) as Box<dyn std::error::Error>
    })?;
    file.write_all(content)?;
    Ok(content.len())
}

// Both tests below are `#[cfg(unix)]`, and an empty module with a wildcard import is an unused import
// on Windows - an error under `-D warnings`, which the Windows build job runs. The module is gated the
// same way its tests are, so the file compiles on a platform where neither can run.
#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    #[cfg(unix)]
    fn a_mid_chain_symlink_is_refused_not_walked_through() {
        // #273: the doc promised "if any parent directory in the chain is a symlink ... the write is
        // refused", and the code opened only the DIRECT parent with O_NOFOLLOW. That flag applies to
        // the last component of the path it is given, so a link at `a` in `a/c/d.txt` was walked
        // normally: the kernel resolved `a` -> outside while getting to `root/a/c`.
        let root = std::env::temp_dir().join(format!("gesta-safe-mid-{}", std::process::id()));
        let outside = std::env::temp_dir().join(format!("gesta-safe-out-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();

        // `a` is a symlink pointing out of the repository.
        std::os::unix::fs::symlink(&outside, root.join("a")).unwrap();

        let err = write_safe(&root, "a/c/d.txt", b"escaped").expect_err(
            "a write whose parent chain leaves the repository through a symlink must be refused",
        );
        let msg = err.to_string();
        assert!(
            msg.contains("symlink"),
            "the refusal must name the symlink: {msg}"
        );
        assert!(
            !outside.join("c/d.txt").exists(),
            "nothing may be created outside the repository"
        );
        assert!(
            !outside.join("c").exists(),
            "no directory may be created outside the repository either"
        );

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    #[cfg(unix)]
    fn an_ordinary_nested_path_still_writes() {
        // The control: the walk must not refuse a path that is simply nested.
        let root = std::env::temp_dir().join(format!("gesta-safe-ok-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        let n = write_safe(&root, "a/b/c.txt", b"fine").expect("a nested path must be writable");
        assert_eq!(n, 4);
        assert_eq!(std::fs::read(root.join("a/b/c.txt")).unwrap(), b"fine");

        let _ = std::fs::remove_dir_all(&root);
    }
}
