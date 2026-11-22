//! Writing files so that one is either whole or not there: the bytes go to a temporary file in the same directory,
//! which is then moved into place. A full disk or a crash half-way leaves the temporary file's name, never a
//! truncated file under the real one.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// A name no other write of this process or another is using, beside the file it will become: a rename only stays
/// on one filesystem, and so atomic, within a directory.
fn temp_beside(dir: &Path) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    dir.join(format!(
        ".cloudmail-{}-{}.part",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

/// Writes `bytes` to a new file at `tmp` and has them on disk before returning; `mode` is for a file only the user
/// may read, and without it the file gets what the umask gives any new file.
fn write_whole(tmp: &Path, bytes: &[u8], mode: Option<u32>) -> io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    if let Some(mode) = mode {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(mode);
    }
    let mut file = options.open(tmp)?;
    file.write_all(bytes)?;
    file.sync_all()
}

/// `path` for the first try, then `name (1).ext`, `name (2).ext`, …: the names `util::unused_path` gives.
fn numbered(path: &Path, i: u32) -> PathBuf {
    if i == 0 {
        return path.to_path_buf();
    }
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext = path
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    path.with_file_name(format!("{stem} ({i}){ext}"))
}

/// Saves `bytes` as a new file in `dir` called `name`, or `name (1).ext`, … when that is taken, and returns where.
/// Nothing already there is ever replaced: the finished temporary file is hard-linked to the name, which fails if
/// the name exists, where a rename would silently replace a file that appeared since the name was chosen.
pub fn save_new(dir: &Path, name: &str, bytes: &[u8]) -> io::Result<PathBuf> {
    let tmp = temp_beside(dir);
    let placed = write_whole(&tmp, bytes, None).and_then(|()| place_new(&tmp, &dir.join(name)));
    // The link is the file now; on failure the temporary file is all there is, and it goes too.
    let _ = std::fs::remove_file(&tmp);
    placed
}

fn place_new(tmp: &Path, wanted: &Path) -> io::Result<PathBuf> {
    // More names than anyone has copies of one attachment, and an end to the loop if every try fails the same way.
    for i in 0..10_000 {
        let path = numbered(wanted, i);
        match std::fs::hard_link(tmp, &path) {
            Ok(()) => return Ok(path),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            // A filesystem without hard links (FAT on a USB stick): rename to a name that is free now, which is
            // still atomic and only loses the guarantee against a file appearing in this instant.
            Err(_) => {
                let path = crate::util::unused_path(wanted.to_path_buf());
                std::fs::rename(tmp, &path)?;
                return Ok(path);
            }
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!("no unused name for {}", wanted.display()),
    ))
}

/// Writes `bytes` to `path`, replacing what is there in one step, in a file only the user can read: for the app's
/// own state, which may hold mail.
pub fn replace_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "the file has no directory"))?;
    let tmp = temp_beside(dir);
    let written = write_whole(&tmp, bytes, Some(0o600)).and_then(|()| std::fs::rename(&tmp, path));
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

/// A directory of this test's own under the system's temporary one, removed when dropped.
#[cfg(test)]
pub struct TestDir(pub PathBuf);

#[cfg(test)]
impl TestDir {
    pub fn new(name: &str) -> Self {
        let dir = temp_beside(&std::env::temp_dir()).with_extension(name);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(&self.0)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }
}

#[cfg(test)]
impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_download_never_replaces_a_file_and_leaves_no_temporary_one() {
        let dir = TestDir::new("downloads");
        std::fs::write(dir.0.join("map.gpx"), b"mine").unwrap();
        let first = save_new(&dir.0, "map.gpx", b"theirs").unwrap();
        let second = save_new(&dir.0, "map.gpx", b"again").unwrap();
        assert_eq!(first, dir.0.join("map (1).gpx"));
        assert_eq!(second, dir.0.join("map (2).gpx"));
        assert_eq!(std::fs::read(dir.0.join("map.gpx")).unwrap(), b"mine");
        assert_eq!(std::fs::read(first).unwrap(), b"theirs");
        assert_eq!(dir.names(), ["map (1).gpx", "map (2).gpx", "map.gpx"]);
    }

    #[test]
    fn a_name_that_only_looks_free_is_not_replaced() {
        // A dangling link does not "exist", so choosing a name by looking would take it; the link refuses.
        let dir = TestDir::new("dangling");
        std::os::unix::fs::symlink(dir.0.join("nowhere"), dir.0.join("a.txt")).unwrap();
        let path = save_new(&dir.0, "a.txt", b"x").unwrap();
        assert_eq!(path, dir.0.join("a (1).txt"));
        assert!(dir.0.join("a.txt").symlink_metadata().unwrap().file_type().is_symlink());
    }

    #[test]
    fn a_download_that_cannot_be_written_leaves_nothing() {
        let dir = TestDir::new("missing");
        assert!(save_new(&dir.0.join("not-a-directory"), "a.txt", b"x").is_err());
        assert!(dir.names().is_empty());
    }

    #[test]
    fn state_is_replaced_whole_and_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TestDir::new("state");
        let path = dir.0.join("draft.json");
        replace_private(&path, b"one").unwrap();
        replace_private(&path, b"two").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"two");
        assert_eq!(path.metadata().unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(dir.names(), ["draft.json"]);
    }
}
