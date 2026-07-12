use std::{
    fs, io,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug)]
pub(crate) struct AgentctlPaths {
    pub home: PathBuf,
    pub database: PathBuf,
    pub blobs: PathBuf,
    pub protocols: PathBuf,
    pub plugins: PathBuf,
    pub locks: PathBuf,
    pub logs: PathBuf,
    pub config_file: PathBuf,
}

impl AgentctlPaths {
    pub(crate) fn resolve(override_home: Option<PathBuf>) -> io::Result<Self> {
        let home = if let Some(home) = override_home {
            home
        } else if let Some(home) = std::env::var_os("AGENTCTL_HOME") {
            PathBuf::from(home)
        } else {
            directories::UserDirs::new()
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "home directory not found"))?
                .home_dir()
                .join(".agentctl")
        };
        let paths = Self {
            database: home.join("agentctl.db"),
            blobs: home.join("blobs"),
            protocols: home.join("protocols"),
            plugins: home.join("plugins"),
            locks: home.join("locks"),
            logs: home.join("logs"),
            config_file: home.join("config.toml"),
            home,
        };
        paths.ensure()?;
        Ok(paths)
    }

    pub(crate) fn ensure(&self) -> io::Result<()> {
        ensure_private_directory(&self.home, true)?;
        for path in [
            &self.blobs,
            &self.protocols,
            &self.plugins,
            &self.locks,
            &self.logs,
        ] {
            ensure_private_directory(path, false)?;
        }

        validate_private_file_if_present(&self.database)?;
        validate_private_file_if_present(&self.config_file)?;
        for suffix in ["-wal", "-shm"] {
            let mut sidecar = self.database.as_os_str().to_os_string();
            sidecar.push(suffix);
            validate_private_file_if_present(Path::new(&sidecar))?;
        }
        Ok(())
    }

    /// Root for installed-version Codex protocol schemas owned by this agentctl home.
    pub(crate) fn codex_protocol_root(&self) -> PathBuf {
        self.protocols.join("codex")
    }
}

fn ensure_private_directory(path: &Path, recursive: bool) -> io::Result<()> {
    match checked_metadata(path)? {
        Some(metadata) => require_directory(path, &metadata)?,
        None => create_private_directory(path, recursive)?,
    }

    // Inspect again after creation so a concurrent replacement cannot turn the final managed
    // path into a symlink before permissions are applied.
    let metadata = checked_metadata(path)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("managed directory disappeared: {}", path.display()),
        )
    })?;
    require_directory(path, &metadata)?;
    set_private_directory(path)
}

fn validate_private_file_if_present(path: &Path) -> io::Result<()> {
    let Some(metadata) = checked_metadata(path)? else {
        return Ok(());
    };
    if !metadata.is_file() {
        return Err(unsafe_managed_path(path, "a regular file"));
    }
    set_private_file(path)
}

fn checked_metadata(path: &Path) -> io::Result<Option<fs::Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(unsafe_managed_path(path, "not a symlink"))
        }
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn require_directory(path: &Path, metadata: &fs::Metadata) -> io::Result<()> {
    if metadata.is_dir() {
        Ok(())
    } else {
        Err(unsafe_managed_path(path, "a directory"))
    }
}

fn unsafe_managed_path(path: &Path, expected: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!(
            "unsafe managed path {}: expected {expected}",
            path.display()
        ),
    )
}

#[cfg(unix)]
fn create_private_directory(path: &Path, recursive: bool) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;

    let mut builder = fs::DirBuilder::new();
    builder.recursive(recursive).mode(0o700);
    match builder.create(path) {
        Ok(()) => Ok(()),
        // A racing creator is safe only after the no-follow metadata check in the caller.
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(not(unix))]
fn create_private_directory(path: &Path, recursive: bool) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(recursive);
    match builder.create(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
fn set_private_directory(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let directory = fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_CLOEXEC | nix::libc::O_DIRECTORY | nix::libc::O_NOFOLLOW)
        .open(path)?;
    if !directory.metadata()?.is_dir() {
        return Err(unsafe_managed_path(path, "a directory"));
    }
    directory.set_permissions(fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn set_private_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_private_file(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_CLOEXEC | nix::libc::O_NOFOLLOW)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(unsafe_managed_path(path, "a regular file"));
    }
    file.set_permissions(fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_private_file(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_protocol_cache_is_scoped_to_the_resolved_home() {
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path().join("isolated-agentctl-home");
        let paths = AgentctlPaths::resolve(Some(home.clone())).unwrap();

        assert_eq!(
            paths.codex_protocol_root(),
            home.join("protocols").join("codex")
        );
    }

    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;

        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_home_without_changing_target_permissions() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let temporary = tempfile::tempdir().unwrap();
        let target = temporary.path().join("target");
        fs::create_dir(&target).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o751)).unwrap();
        let home = temporary.path().join("home-link");
        symlink(&target, &home).unwrap();

        let error = AgentctlPaths::resolve(Some(home)).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("not a symlink"));
        assert_eq!(mode(&target), 0o751);
        assert!(fs::read_dir(&target).unwrap().next().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_subdirectory_without_changing_target_permissions() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path().join("home");
        let target = temporary.path().join("target");
        fs::create_dir(&home).unwrap();
        fs::create_dir(&target).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o750)).unwrap();
        symlink(&target, home.join("blobs")).unwrap();

        let error = AgentctlPaths::resolve(Some(home)).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("not a symlink"));
        assert_eq!(mode(&target), 0o750);
        assert!(fs::read_dir(&target).unwrap().next().is_none());
    }

    #[test]
    fn rejects_wrong_managed_path_types() {
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path().join("home");
        fs::create_dir(&home).unwrap();
        fs::write(home.join("blobs"), b"not a directory").unwrap();

        let error = AgentctlPaths::resolve(Some(home)).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("expected a directory"));
    }
}
