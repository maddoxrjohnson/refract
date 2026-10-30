//! File-system pieces the migration needs that `std` lacks: walking a tree in a fixed
//! order, SHA-256 of a file, setting a modification time without following symlinks,
//! free space, and a rename that never replaces.

use std::ffi::CString;
use std::fs::{self, File, Metadata};
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// What [`walk`] hands its visitor.
pub enum Visit<'a> {
    /// A file, folder, symlink or special file, before what is inside a folder.
    Entry {
        rel: &'a Path,
        abs: &'a Path,
        meta: &'a Metadata,
    },
    /// A folder, after everything inside it.
    DirDone {
        rel: &'a Path,
        abs: &'a Path,
        meta: &'a Metadata,
    },
    /// A path whose metadata or listing could not be read (`""` for the root itself).
    /// What lies under it is unknown; callers must never take it for gone.
    Error { rel: &'a Path, error: io::Error },
}

/// Walks the tree under `root` (not `root` itself), parents before children and names
/// in byte order, without following symlinks. `skip` leaves a path, and whatever is
/// under it, out. The walk stops early when `visit` returns false; the result is then
/// false.
pub fn walk(
    root: &Path,
    skip: &dyn Fn(&Path) -> bool,
    visit: &mut dyn FnMut(Visit<'_>) -> bool,
) -> bool {
    walk_dir(root, Path::new(""), skip, visit)
}

fn walk_dir(
    abs_dir: &Path,
    rel_dir: &Path,
    skip: &dyn Fn(&Path) -> bool,
    visit: &mut dyn FnMut(Visit<'_>) -> bool,
) -> bool {
    let names = match fs::read_dir(abs_dir) {
        Ok(entries) => {
            let mut names = Vec::new();
            for entry in entries {
                match entry {
                    Ok(entry) => names.push(entry.file_name()),
                    // The listing broke off: what follows is unknown, not gone.
                    Err(error) => {
                        if !visit(Visit::Error {
                            rel: rel_dir,
                            error,
                        }) {
                            return false;
                        }
                        break;
                    }
                }
            }
            names.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
            names
        }
        Err(error) => {
            return visit(Visit::Error {
                rel: rel_dir,
                error,
            })
        }
    };
    for name in names {
        let rel = rel_dir.join(&name);
        if skip(&rel) {
            continue;
        }
        let abs = abs_dir.join(&name);
        let meta = match fs::symlink_metadata(&abs) {
            Ok(meta) => meta,
            Err(error) => {
                if !visit(Visit::Error { rel: &rel, error }) {
                    return false;
                }
                continue;
            }
        };
        if !visit(Visit::Entry {
            rel: &rel,
            abs: &abs,
            meta: &meta,
        }) {
            return false;
        }
        if meta.is_dir() {
            if !walk_dir(&abs, &rel, skip, visit) {
                return false;
            }
            if !visit(Visit::DirDone {
                rel: &rel,
                abs: &abs,
                meta: &meta,
            }) {
                return false;
            }
        }
    }
    true
}

/// Modification time in nanoseconds since the Unix epoch.
pub fn mtime_ns(meta: &Metadata) -> i64 {
    meta.mtime()
        .saturating_mul(1_000_000_000)
        .saturating_add(meta.mtime_nsec())
}

/// Lowercase hex SHA-256 of a file's content.
pub fn sha256_file(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0; 1 << 20];
    loop {
        match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => hasher.update(&buf[..n]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(hex(&hasher.finalize()))
}

/// Lowercase hex SHA-256 of bytes.
pub fn sha256_bytes(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn c_path(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes()).map_err(|_| io::Error::other("path holds NUL"))
}

/// Sets the modification time of `path` itself (a symlink is not followed), leaving the
/// access time.
pub fn set_mtime(path: &Path, mtime_ns: i64) -> io::Result<()> {
    let path = c_path(path)?;
    let times = [
        libc::timespec {
            tv_sec: 0,
            tv_nsec: libc::UTIME_OMIT,
        },
        libc::timespec {
            tv_sec: mtime_ns.div_euclid(1_000_000_000) as libc::time_t,
            tv_nsec: mtime_ns.rem_euclid(1_000_000_000) as _,
        },
    ];
    // SAFETY: `path` is a NUL-terminated string and `times` holds two timespecs, as
    // utimensat requires; both outlive the call.
    let rc = unsafe {
        libc::utimensat(
            libc::AT_FDCWD,
            path.as_ptr(),
            times.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Free bytes for an unprivileged user on the file system holding `path`, or its
/// nearest existing ancestor.
pub fn free_bytes(path: &Path) -> io::Result<u64> {
    let existing = path
        .ancestors()
        .find(|p| p.exists())
        .unwrap_or(Path::new("/"));
    let c = c_path(existing)?;
    // SAFETY: statvfs fills the zeroed struct it is given; `c` is NUL-terminated.
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(c.as_ptr(), &mut stat) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((stat.f_bavail as u64).saturating_mul(stat.f_frsize as u64))
}

/// True when `path` is on a 9p file system, which is how WSL 2 mounts Windows drives.
pub fn is_9p(path: &Path) -> bool {
    const V9FS_MAGIC: i64 = 0x0102_1997;
    let Ok(c) = c_path(path) else {
        return false;
    };
    // SAFETY: statfs fills the zeroed struct it is given; `c` is NUL-terminated.
    let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statfs(c.as_ptr(), &mut stat) };
    rc == 0 && stat.f_type as i64 == V9FS_MAGIC
}

/// Renames `from` to `to`, failing with `AlreadyExists` if `to` exists, atomically
/// (`renameat2` with `RENAME_NOREPLACE`). Both must be on one file system.
pub fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    let (a, b) = (c_path(from)?, c_path(to)?);
    // SAFETY: both strings are NUL-terminated and outlive the call; renameat2 takes two
    // directory fds, two paths and flags.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            a.as_ptr(),
            libc::AT_FDCWD,
            b.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Removes a file, symlink or whole folder; a missing path is fine.
pub fn remove_any(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Writes `bytes` to `path` through a temporary file in the same folder renamed over
/// it, so a reader never sees half of it. The file is flushed to disk before the
/// rename and the folder after it, so once this returns the new contents survive a
/// crash or power loss (a state file saying "deleting" must not come back older).
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;
    let dir = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(dir)?;
    let tmp = PathBuf::from(format!(
        "{}.tmp-{}",
        path.display(),
        uuid::Uuid::new_v4().simple()
    ));
    let written = fs::File::create(&tmp).and_then(|mut file| {
        file.write_all(bytes)?;
        file.sync_all()
    });
    written
        .and_then(|()| fs::rename(&tmp, path))
        .inspect_err(|_| {
            let _ = fs::remove_file(&tmp);
        })?;
    fs::File::open(dir)?.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn walks_in_order_and_skips() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        for dir in ["b", "a/x", "node_modules/left-pad"] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        fs::write(root.join("a/x/f"), "1").unwrap();
        fs::write(root.join("c"), "2").unwrap();
        std::os::unix::fs::symlink("a", root.join("link")).unwrap();
        let mut seen = Vec::new();
        walk(root, &|rel| rel == Path::new("node_modules"), &mut |v| {
            match v {
                Visit::Entry { rel, .. } => seen.push(format!("+{}", rel.display())),
                Visit::DirDone { rel, .. } => seen.push(format!("-{}", rel.display())),
                Visit::Error { .. } => seen.push("!".into()),
            }
            true
        });
        assert_eq!(
            seen,
            ["+a", "+a/x", "+a/x/f", "-a/x", "-a", "+b", "-b", "+c", "+link"]
        );
    }

    #[test]
    fn mtimes_hashes_and_renames() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("f");
        fs::write(&file, "abc").unwrap();
        set_mtime(&file, 1_600_000_000_123_456_789).unwrap();
        assert_eq!(
            mtime_ns(&fs::metadata(&file).unwrap()),
            1_600_000_000_123_456_789
        );
        assert_eq!(
            sha256_file(&file).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(sha256_bytes(b"abc"), sha256_file(&file).unwrap());
        let other = tmp.path().join("g");
        fs::write(&other, "x").unwrap();
        let err = rename_noreplace(&file, &other).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        rename_noreplace(&file, &tmp.path().join("h")).unwrap();
        assert!(free_bytes(&tmp.path().join("missing/deeper")).unwrap() > 0);
    }
}
