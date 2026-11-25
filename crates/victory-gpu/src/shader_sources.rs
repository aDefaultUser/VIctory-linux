//! The title's shader containers kept on disk as the title passed them
//! (`VICTORY_SHADER_SOURCES=1`), for `victory warm`: a translator change
//! keys every translation in the shader cache anew, and the title's shaders
//! are mostly not in its files as plain containers, so a warm-up before a
//! later launch translates these again. Kept apart from `shader_cache.rs`,
//! whose own source is part of every translation's key.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

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

/// Writes a source file whole or not at all: a reader never sees a part.
fn write_source(dir: &Path, name: &str, container: &[u8]) {
    let path = dir.join(name);
    if path.exists() {
        return;
    }
    let tmp = dir.join(format!("{name}.tmp"));
    let written = std::fs::create_dir_all(dir)
        .and_then(|()| std::fs::write(&tmp, container))
        .and_then(|()| std::fs::rename(&tmp, &path));
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        tracing::debug!(error = %e, path = %path.display(), "shader source not kept");
    }
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
}
