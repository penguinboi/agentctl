use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{Result, StorageError, error::io_error};

const DEFAULT_MAX_DECOMPRESSED_SIZE: u64 = 256 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BlobRef {
    pub digest: String,
    pub size: u64,
    pub compression: String,
    pub media_type: String,
    pub redacted: bool,
}

#[derive(Clone, Debug)]
pub struct BlobStore {
    root: PathBuf,
    max_decompressed_size: u64,
}

impl BlobStore {
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        reject_symlink(&root)?;
        create_private_dir(&root)?;
        Ok(Self {
            root,
            max_decompressed_size: DEFAULT_MAX_DECOMPRESSED_SIZE,
        })
    }

    #[cfg(test)]
    #[must_use]
    fn with_max_decompressed_size(mut self, bytes: u64) -> Self {
        self.max_decompressed_size = bytes;
        self
    }

    pub fn put(
        &self,
        data: &[u8],
        media_type: impl Into<String>,
        redacted: bool,
    ) -> Result<BlobRef> {
        let digest = format!("sha256:{}", hex::encode(Sha256::digest(data)));
        let path = self.path_for_digest(&digest)?;
        if !path.exists() {
            let parent = path
                .parent()
                .ok_or_else(|| StorageError::InvalidDigest("blob path has no parent".to_owned()))?;
            create_private_dir(parent)?;
            let compressed =
                zstd::stream::encode_all(data, 3).map_err(|error| io_error(path.clone(), error))?;
            let temporary = parent.join(format!(
                ".{}.{}.tmp",
                path.file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("blob"),
                uuid::Uuid::new_v4()
            ));
            let write_result = (|| -> Result<()> {
                let mut file = private_create_new(&temporary)?;
                file.write_all(&compressed)
                    .map_err(|error| io_error(&temporary, error))?;
                file.sync_all()
                    .map_err(|error| io_error(&temporary, error))?;
                match fs::rename(&temporary, &path) {
                    Ok(()) => Ok(()),
                    Err(_error) if path.exists() => {
                        let _ = fs::remove_file(&temporary);
                        Ok(())
                    }
                    Err(error) => Err(io_error(&path, error)),
                }
            })();
            if write_result.is_err() {
                let _ = fs::remove_file(&temporary);
            }
            write_result?;
        }

        Ok(BlobRef {
            digest,
            size: data.len() as u64,
            compression: "zstd".to_owned(),
            media_type: media_type.into(),
            redacted,
        })
    }

    pub fn get(&self, blob: &BlobRef) -> Result<Vec<u8>> {
        if blob.compression != "zstd" {
            return Err(StorageError::InvalidData(format!(
                "unsupported compression {}",
                blob.compression
            )));
        }
        if blob.size > self.max_decompressed_size {
            return Err(StorageError::BlobTooLarge(self.max_decompressed_size));
        }
        let path = self.path_for_digest(&blob.digest)?;
        let file = File::open(&path).map_err(|error| io_error(&path, error))?;
        let decoder =
            zstd::stream::read::Decoder::new(file).map_err(|error| io_error(&path, error))?;
        let mut data = Vec::with_capacity(blob.size.min(1024 * 1024) as usize);
        decoder
            .take(self.max_decompressed_size.saturating_add(1))
            .read_to_end(&mut data)
            .map_err(|error| io_error(&path, error))?;
        if data.len() as u64 > self.max_decompressed_size {
            return Err(StorageError::BlobTooLarge(self.max_decompressed_size));
        }
        let actual = format!("sha256:{}", hex::encode(Sha256::digest(&data)));
        if actual != blob.digest {
            return Err(StorageError::DigestMismatch {
                expected: blob.digest.clone(),
                actual,
            });
        }
        if data.len() as u64 != blob.size {
            return Err(StorageError::InvalidData(format!(
                "blob size is {}, metadata says {}",
                data.len(),
                blob.size
            )));
        }
        Ok(data)
    }

    pub fn verify(&self, blob: &BlobRef) -> Result<()> {
        self.get(blob).map(|_| ())
    }

    pub fn contains(&self, digest: &str) -> Result<bool> {
        Ok(self.path_for_digest(digest)?.is_file())
    }

    pub fn delete(&self, digest: &str) -> Result<bool> {
        let path = self.path_for_digest(digest)?;
        match fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(io_error(path, error)),
        }
    }

    fn path_for_digest(&self, digest: &str) -> Result<PathBuf> {
        let hash = digest
            .strip_prefix("sha256:")
            .ok_or_else(|| StorageError::InvalidDigest(digest.to_owned()))?;
        if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(StorageError::InvalidDigest(digest.to_owned()));
        }
        let normalized = hash.to_ascii_lowercase();
        Ok(self
            .root
            .join(&normalized[..2])
            .join(format!("{normalized}.zst")))
    }
}

fn create_private_dir(path: &Path) -> Result<()> {
    reject_symlink(path)?;
    fs::create_dir_all(path).map_err(|error| io_error(path, error))?;
    set_mode(path, 0o700)
}

fn reject_symlink(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(StorageError::UnsafeSymlink(path.to_path_buf()))
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io_error(path, error)),
    }
}

fn private_create_new(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path).map_err(|error| io_error(path, error))
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .map_err(|error| io_error(path, error))
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blobs_are_content_addressed_and_verified() {
        let directory = tempfile::tempdir().unwrap();
        let store = BlobStore::open(directory.path()).unwrap();
        let first = store.put(b"hello", "text/plain", true).unwrap();
        let second = store.put(b"hello", "text/plain", true).unwrap();
        assert_eq!(first.digest, second.digest);
        assert_eq!(store.get(&first).unwrap(), b"hello");
        store.verify(&first).unwrap();
    }

    #[test]
    fn rejects_path_traversal_disguised_as_digest() {
        let directory = tempfile::tempdir().unwrap();
        let store = BlobStore::open(directory.path()).unwrap();
        assert!(store.contains("sha256:../../secret").is_err());
    }

    #[test]
    fn enforces_decompressed_limit() {
        let directory = tempfile::tempdir().unwrap();
        let store = BlobStore::open(directory.path()).unwrap();
        let blob = store.put(b"hello", "text/plain", false).unwrap();
        let limited = BlobStore::open(directory.path())
            .unwrap()
            .with_max_decompressed_size(4);
        assert!(matches!(
            limited.get(&blob),
            Err(StorageError::BlobTooLarge(4))
        ));
    }
}
