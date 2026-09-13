//! Helpers shared by the tests of more than one module.
//!
//! Compiled only under `cfg(test)`, and deliberately small: this crate has no
//! runtime dependency it does not need, and a test helper is not a reason to
//! take one on.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use crate::NAMESPACE_MARKER_FILENAME;

/// A directory that removes itself.
pub(crate) struct TempDir(PathBuf);

impl TempDir {
    pub(crate) fn new(name: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let unique = format!(
            "cmgr-artifact-server-test-{}-{}-{}",
            name,
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let path = std::env::temp_dir().join(unique);
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("failed to create temp dir");
        // Canonicalized as main.rs canonicalizes the artifact directory, so
        // the parent comparisons under test see the same shape they do in the
        // binary.
        TempDir(fs::canonicalize(&path).expect("failed to canonicalize temp dir"))
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Makes a directory under `root` and marks it as one of cork's
/// per-destination artifact directories, as cork does.
pub(crate) fn namespace_dir(root: &Path, name: &str) -> PathBuf {
    let dir = root.join(name);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join(NAMESPACE_MARKER_FILENAME), b"").unwrap();
    dir
}

/// Creates an empty file, and any directory leading to it.
pub(crate) fn touch(path: &Path) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, b"").unwrap();
}

/// Writes a gzipped tarball holding the given regular files.
pub(crate) fn write_tarball(path: &Path, files: &[(&str, &[u8])]) {
    let entries: Vec<_> = files
        .iter()
        .map(|(name, data)| (tar::EntryType::Regular, *name, *data))
        .collect();
    write_tarball_of(path, &entries);
}

/// Writes a gzipped tarball whose entries are of the given types, for the
/// cases where what matters is that an entry is *not* an ordinary file.
pub(crate) fn write_tarball_of(path: &Path, entries: &[(tar::EntryType, &str, &[u8])]) {
    let file = fs::File::create(path).unwrap();
    let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::default());
    let mut builder = tar::Builder::new(encoder);
    for (entry_type, name, data) in entries {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(*entry_type);
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        // Written into the header's name field directly, because
        // `Header::set_path` refuses a name holding `..` -- and a name holding
        // `..` is one of the things the server has to be shown refusing. A
        // hostile or merely broken archive is not written by this crate's
        // writer, so its rules are not the ones that matter.
        let name = name.as_bytes();
        assert!(name.len() < 100, "test tarball entry name is too long");
        header.as_gnu_mut().unwrap().name[..name.len()].copy_from_slice(name);
        header.set_cksum();
        builder.append(&header, *data).unwrap();
    }
    builder.into_inner().unwrap().finish().unwrap();
}

/// The names in a directory, sorted, for comparison.
pub(crate) fn dir_entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| {
            e.unwrap()
                .path()
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    names.sort();
    names
}
