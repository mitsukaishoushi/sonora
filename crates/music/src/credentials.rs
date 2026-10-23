use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context as _, Result};

/// The file every provider keeps its sign-in under, inside its own cache folder.
pub(crate) const FILE: &str = "credentials.json";

/// Tells apart the temporary files of two writes running at once in one process.
static WRITES: AtomicU64 = AtomicU64::new(0);

/// Sonora's cache root, `$XDG_CACHE_HOME/sonora`, falling back to the temp dir. Credentials are
/// never written under the fallback, since anyone on the machine can read and write there.
pub(crate) fn root() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("sonora")
}

/// A provider's own cache folder, named by its slug.
pub(crate) fn dir(slug: &str) -> PathBuf {
    root().join(slug)
}

/// Writes a credential file readable by the owner alone, creating its folder first. The bytes
/// go to a temporary file beside it that is synced and then renamed over the old one, so a
/// crash leaves either the old sign-in or the new one and never half of either. It fails when
/// the system names no cache folder, rather than leaving credentials in the shared temp dir.
pub(crate) fn write(path: &Path, contents: &[u8]) -> Result<()> {
    let private = dirs::cache_dir().context("cannot find a private folder for credentials")?;
    if !path.starts_with(&private) {
        anyhow::bail!(
            "cannot store credentials outside {}: {}",
            private.display(),
            path.display()
        );
    }
    let parent = path
        .parent()
        .with_context(|| format!("cannot find the folder of {}", path.display()))?;
    create_private(parent)?;
    let name = path
        .file_name()
        .with_context(|| format!("cannot name a temporary file for {}", path.display()))?
        .to_string_lossy();
    let temporary = parent.join(format!(
        ".{name}.{}.{}.tmp",
        std::process::id(),
        WRITES.fetch_add(1, Ordering::Relaxed)
    ));
    let written = write_synced(&temporary, contents).and_then(|()| {
        std::fs::rename(&temporary, path)
            .with_context(|| format!("cannot move credentials into {}", path.display()))
    });
    if written.is_err() {
        remove(&temporary);
        return written;
    }
    sync_folder(parent);
    Ok(())
}

/// Restricts an existing credential file to its owner. A missing file is left alone.
pub(crate) fn secure(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if let Err(error) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            && error.kind() != std::io::ErrorKind::NotFound
        {
            log::warn!("credentials: cannot restrict {}: {error}", path.display());
        }
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// Deletes a credential file, treating one that is already gone as success.
pub(crate) fn remove(path: &Path) {
    if let Err(error) = std::fs::remove_file(path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        log::warn!("credentials: cannot remove {}: {error}", path.display());
    }
}

/// Creates a credential folder and the folders above it that are missing, open to the owner
/// alone on unix. A folder that already exists is restricted to its owner too.
fn create_private(folder: &Path) -> Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
        builder.mode(0o700);
        builder
            .create(folder)
            .with_context(|| format!("cannot create {}", folder.display()))?;
        std::fs::set_permissions(folder, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("cannot restrict {}", folder.display()))?;
    }
    #[cfg(not(unix))]
    builder
        .create(folder)
        .with_context(|| format!("cannot create {}", folder.display()))?;
    Ok(())
}

/// Writes a new file that only its owner can read on unix and syncs it to disk.
fn write_synced(path: &Path, contents: &[u8]) -> Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("cannot create {}", path.display()))?;
    file.write_all(contents)
        .with_context(|| format!("cannot write {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("cannot sync {}", path.display()))
}

/// Syncs a folder so a rename inside it survives a crash. Only unix can open a folder for this,
/// and a failure costs at most the latest write, so it is logged rather than returned.
fn sync_folder(folder: &Path) {
    #[cfg(unix)]
    if let Err(error) = std::fs::File::open(folder).and_then(|opened| opened.sync_all()) {
        log::debug!("credentials: cannot sync {}: {error}", folder.display());
    }
    #[cfg(not(unix))]
    let _ = folder;
}
