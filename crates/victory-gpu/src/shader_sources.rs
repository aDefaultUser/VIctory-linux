//! The title's shader containers kept on disk as the title passed them
//! (`VICTORY_SHADER_SOURCES=1`), for `victory warm`: a translator change
//! keys every translation in the shader cache anew, and the title's shaders
//! are mostly not in its files as plain containers, so a warm-up before a
//! later launch translates these again. Kept apart from `shader_cache.rs`,
//! whose own source is part of every translation's key.

use std::fs::File;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

static DIR: OnceLock<PathBuf> = OnceLock::new();

/// Keeps every container a run translates in `dir` from now on.
pub fn keep_sources(dir: PathBuf) {
    let _ = DIR.set(dir);
}

/// A kept container's file name: the hash a run's log names it by, then
/// its stage (as `VICTORY_SHADER_SAVE` names its `.cdxil` files).
pub fn source_name(hash: &str, stage: &str) -> String {
    format!("{hash}.{stage}.cdxil")
}

/// Keeps `container` unless it is kept already.
pub(crate) fn keep_source(hash: &str, stage: &str, container: &[u8]) {
    if let Some(dir) = DIR.get() {
        write_source(dir, &source_name(hash, stage), container);
    }
}

/// A temporary file name of this writer's own for `name`: two threads (or
/// two runs) translating one container at once each write their own, and
/// neither renames or removes the other's while it is being written.
fn temporary(dir: &Path, name: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    dir.join(format!("{name}.{}-{n}.tmp", std::process::id()))
}

/// Writes a source file whole or not at all: a reader never sees a part.
fn write_source(dir: &Path, name: &str, container: &[u8]) {
    let temporaries = std::iter::repeat_with(|| temporary(dir, name));
    write_source_through(dir, name, container, temporaries);
}

/// `write_source`, trying `temporaries` in turn. The process id alone does
/// not make a name this writer's own (two Wine servers or sandboxes sharing
/// one cache can hand out the same id), so a name that is taken already is
/// passed over, and only a file this writer made is ever removed.
fn write_source_through(
    dir: &Path,
    name: &str,
    container: &[u8],
    temporaries: impl IntoIterator<Item = PathBuf>,
) {
    let path = dir.join(name);
    if path.exists() {
        return;
    }
    let (tmp, mut file) =
        match std::fs::create_dir_all(dir).and_then(|()| create_temporary(temporaries)) {
            Ok(created) => created,
            Err(e) => {
                tracing::debug!(error = %e, path = %path.display(), "shader source not kept");
                return;
            }
        };
    let written = file.write_all(container);
    // Closed before the rename, which Windows refuses for an open file.
    drop(file);
    if let Err(e) = written.and_then(|()| std::fs::rename(&tmp, &path)) {
        let _ = std::fs::remove_file(&tmp);
        tracing::debug!(error = %e, path = %path.display(), "shader source not kept");
    }
}

/// Makes the first of `temporaries` that does not exist yet. `create_new`
/// neither truncates another writer's file nor follows a link left there.
fn create_temporary(
    temporaries: impl IntoIterator<Item = PathBuf>,
) -> std::io::Result<(PathBuf, File)> {
    const ATTEMPTS: usize = 16;
    for tmp in temporaries.into_iter().take(ATTEMPTS) {
        match File::options().write(true).create_new(true).open(&tmp) {
            Ok(file) => return Ok((tmp, file)),
            Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(ErrorKind::AlreadyExists.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sources_are_kept_once_and_whole() {
        let dir = tempfile::tempdir().unwrap();
        let sources = dir.path().join("shader-sources");
        let name = source_name("0123456789abcdef", "compute");
        assert_eq!(name, "0123456789abcdef.compute.cdxil");
        write_source(&sources, &name, b"first");
        write_source(&sources, &name, b"second");
        assert_eq!(
            std::fs::read(sources.join(&name)).unwrap(),
            b"first",
            "never replaced"
        );
        let files: Vec<_> = std::fs::read_dir(&sources)
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(
            files,
            [std::ffi::OsString::from(&name)],
            "no temporary file left"
        );

        // A folder that cannot be made costs the run nothing.
        let blocked = dir.path().join("file");
        std::fs::write(&blocked, b"").unwrap();
        write_source(&blocked.join("sub"), &name, b"x");
        assert!(!blocked.join("sub").exists());
    }

    /// Another writer of the same container, part way through a temporary
    /// file of the name this writer tries first (another process with the
    /// same id over a shared cache), keeps it: this writer neither writes
    /// over it, renames it into place, nor removes it.
    #[test]
    fn a_taken_temporary_name_is_passed_over() {
        let dir = tempfile::tempdir().unwrap();
        let name = source_name("fedcba9876543210", "pixel");
        assert_ne!(temporary(dir.path(), &name), temporary(dir.path(), &name));
        let theirs = dir.path().join(format!("{name}.7-0.tmp"));
        let mine = dir.path().join(format!("{name}.7-1.tmp"));
        std::fs::write(&theirs, b"par").unwrap();

        // Every name taken: nothing kept, and their file stays.
        write_source_through(dir.path(), &name, b"whole", [theirs.clone()]);
        assert!(!dir.path().join(&name).exists());
        assert_eq!(std::fs::read(&theirs).unwrap(), b"par");

        write_source_through(dir.path(), &name, b"whole", [theirs.clone(), mine.clone()]);
        assert_eq!(std::fs::read(dir.path().join(&name)).unwrap(), b"whole");
        assert_eq!(std::fs::read(&theirs).unwrap(), b"par");
        assert!(!mine.exists(), "renamed into place");

        // A link left at a taken name is not written through.
        #[cfg(unix)]
        {
            let name = source_name("0011223344556677", "vertex");
            let target = dir.path().join("target");
            std::fs::write(&target, b"theirs").unwrap();
            let link = dir.path().join(format!("{name}.7-0.tmp"));
            std::os::unix::fs::symlink(&target, &link).unwrap();
            write_source_through(dir.path(), &name, b"whole", [link]);
            assert_eq!(std::fs::read(&target).unwrap(), b"theirs");
        }
    }
}
