//! Persistent ML-DSA-87 miner key (`node::keystore`).
//!
//! The key file holds only the 32-byte FIPS 204 seed behind a versioned magic:
//! `PLAU/MLDSA87/v1\n || seed`. The full key pair is re-derived from the seed on load.

use std::io::Write;
use std::path::{Path, PathBuf};

use chain::OwnerKeypair;
use chain::keys::SEED_LEN;

use crate::NodeError;

/// Default key file name inside `--data-dir`.
pub const KEY_FILE_NAME: &str = "miner_key.bin";

/// Lock file a running node holds in its data directory.
pub const LOCK_FILE_NAME: &str = "LOCK";

/// Versioned header of the key file.
const KEY_FILE_MAGIC: &[u8; 16] = b"PLAU/MLDSA87/v1\n";

/// Total key file length: magic plus seed.
const KEY_FILE_LEN: usize = KEY_FILE_MAGIC.len() + SEED_LEN;

/// The key file to use: `key_file` if given, else `miner_key.bin` in `data_dir`.
pub fn key_path(data_dir: &Path, key_file: Option<&Path>) -> PathBuf {
    key_file.map_or_else(|| data_dir.join(KEY_FILE_NAME), Path::to_path_buf)
}

/// Loads the miner key from `path`, or generates one and saves it there if the file
/// does not exist. Returns the key pair and whether it was created.
///
/// An existing key file is never overwritten: a file that is not a valid key file,
/// or a symlink whose target is missing, is an error. If another process creates the
/// file at the same moment, its key is loaded instead.
pub fn load_or_create(path: &Path) -> Result<(OwnerKeypair, bool), NodeError> {
    match std::fs::read(path) {
        Ok(bytes) => Ok((parse_key_file(path, &bytes)?, false)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if std::fs::symlink_metadata(path).is_ok() {
                return Err(format!(
                    "miner key {} is a symlink to a missing file; refusing to replace it",
                    path.display()
                )
                .into());
            }
            let keypair = OwnerKeypair::generate()?;
            match publish_key_file(path, keypair.seed()) {
                Ok(()) => Ok((keypair, true)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    Ok((parse_key_file(path, &std::fs::read(path)?)?, false))
                }
                Err(e) => Err(format!("cannot write miner key {}: {e}", path.display()).into()),
            }
        }
        Err(e) => Err(format!("cannot read miner key {}: {e}", path.display()).into()),
    }
}

fn parse_key_file(path: &Path, bytes: &[u8]) -> Result<OwnerKeypair, NodeError> {
    if bytes.len() != KEY_FILE_LEN || !bytes.starts_with(KEY_FILE_MAGIC) {
        return Err(format!(
            "{} is not a Plauthium ML-DSA-87 key file ({} bytes); move it away to generate a new key",
            path.display(),
            bytes.len()
        )
        .into());
    }
    let seed: [u8; SEED_LEN] = bytes[KEY_FILE_MAGIC.len()..]
        .try_into()
        .map_err(|_| "key file seed has the wrong length")?;
    Ok(OwnerKeypair::from_seed(&seed))
}

/// Writes the key file without ever replacing an existing one. The seed goes to a
/// uniquely named temporary file (created exclusively, owner-only on Unix), which is
/// then hard-linked to `path`. Linking fails with `AlreadyExists` if `path` exists, so
/// a crash never leaves a truncated key and a concurrent start never overwrites one.
fn publish_key_file(path: &Path, seed: &[u8; SEED_LEN]) -> std::io::Result<()> {
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
    if let Some(parent) = parent {
        std::fs::create_dir_all(parent)?;
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let mut tmp_name = path.as_os_str().to_owned();
    tmp_name.push(format!(".{}.{nanos}.tmp", std::process::id()));
    let tmp_path = Path::new(&tmp_name);

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| {
        let mut file = options.open(tmp_path)?;
        file.write_all(KEY_FILE_MAGIC)?;
        file.write_all(seed)?;
        file.sync_all()?;
        drop(file);
        std::fs::hard_link(tmp_path, path)
    })();
    let _ = std::fs::remove_file(tmp_path);
    result?;
    // Make the new directory entry durable (best effort; not every platform allows it).
    if let Some(parent) = parent
        && let Ok(dir) = std::fs::File::open(parent)
    {
        let _ = dir.sync_all();
    }
    Ok(())
}

/// Locks `data_dir` (creating it if needed) for as long as the returned file stays
/// open: two nodes on one data directory would corrupt each other's `blocks.log`.
pub fn lock_data_dir(data_dir: &Path) -> Result<std::fs::File, NodeError> {
    std::fs::create_dir_all(data_dir)?;
    let path = data_dir.join(LOCK_FILE_NAME);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => Err(format!(
            "data directory {} is in use by another plauthium-node",
            data_dir.display()
        )
        .into()),
        Err(std::fs::TryLockError::Error(e)) => {
            Err(format!("cannot lock {}: {e}", path.display()).into())
        }
    }
}

/// `--reset`: deletes everything in `data_dir` except the key file at `key_path` (so
/// the node keeps its identity across resets) and the lock file.
///
/// Directories on the way to the key file are emptied instead of deleted. Symlinks
/// are never followed: one leading to the key file or to a directory holding it is
/// kept, any other is removed as a link.
pub fn reset_data_dir(data_dir: &Path, key_path: &Path) -> Result<(), NodeError> {
    let keep: Vec<PathBuf> = [key_path.to_path_buf(), data_dir.join(LOCK_FILE_NAME)]
        .iter()
        .filter_map(|p| p.canonicalize().ok())
        .collect();
    clear_dir(data_dir, &keep)?;
    Ok(())
}

fn clear_dir(dir: &Path, keep: &[PathBuf]) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?; // does not follow symlinks
        if let Ok(canonical) = path.canonicalize()
            && keep.iter().any(|k| k.starts_with(&canonical))
        {
            // A kept file itself, or a directory or symlink on the way to one.
            if file_type.is_dir() && !keep.contains(&canonical) {
                clear_dir(&path, keep)?;
            }
            continue;
        }
        if file_type.is_dir() {
            std::fs::remove_dir_all(&path)?;
        } else {
            std::fs::remove_file(&path)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "plau-keystore-{label}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn key_is_created_once_then_loaded_and_survives_reset() {
        let dir = temp_dir("persist");
        let key_path = dir.join(KEY_FILE_NAME);

        let (created, was_new) = load_or_create(&key_path).unwrap();
        assert!(was_new);
        let bytes = std::fs::read(&key_path).unwrap();
        assert_eq!(bytes.len(), KEY_FILE_LEN);
        assert!(bytes.starts_with(KEY_FILE_MAGIC));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&key_path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "the key file is private");
        }

        let (loaded, was_new) = load_or_create(&key_path).unwrap();
        assert!(!was_new);
        assert_eq!(loaded.key_id(), created.key_id());
        assert_eq!(loaded.public_key(), created.public_key());

        // --reset wipes the chain data but keeps the key and the lock file.
        std::fs::write(dir.join("blocks.log"), b"chain data").unwrap();
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let lock = lock_data_dir(&dir).unwrap();
        reset_data_dir(&dir, &key_path).unwrap();
        assert!(!dir.join("blocks.log").exists());
        assert!(!dir.join("sub").exists());
        assert!(dir.join(LOCK_FILE_NAME).exists());
        // While it is locked, a second node cannot use the directory.
        assert!(lock_data_dir(&dir).is_err());
        drop(lock);
        drop(lock_data_dir(&dir).unwrap());
        let (after_reset, was_new) = load_or_create(&key_path).unwrap();
        assert!(!was_new);
        assert_eq!(after_reset.key_id(), created.key_id());

        // Two fresh key files hold two different identities.
        let other_path = dir.join("other.bin");
        let (other, _) = load_or_create(&other_path).unwrap();
        assert_ne!(other.key_id(), created.key_id());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reset_keeps_a_nested_key_and_never_follows_symlinks() {
        let dir = temp_dir("nested");
        let key_path = dir.join("keys").join("miner.bin");
        let (created, _) = load_or_create(&key_path).unwrap();
        std::fs::write(dir.join("keys").join("other"), b"x").unwrap();
        std::fs::write(dir.join("blocks.log"), b"chain data").unwrap();

        // Outside data: a directory reached through a symlink must survive the reset.
        let outside = temp_dir("nested-outside");
        std::fs::write(outside.join("precious"), b"keep me").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, dir.join("link")).unwrap();

        reset_data_dir(&dir, &key_path).unwrap();
        assert!(key_path.exists(), "the nested key file survives");
        assert!(!dir.join("keys").join("other").exists());
        assert!(!dir.join("blocks.log").exists());
        assert!(
            outside.join("precious").exists(),
            "symlinks are not followed"
        );
        #[cfg(unix)]
        assert!(std::fs::symlink_metadata(dir.join("link")).is_err());
        let (loaded, was_new) = load_or_create(&key_path).unwrap();
        assert!(!was_new);
        assert_eq!(loaded.key_id(), created.key_id());
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn an_existing_key_is_never_overwritten() {
        let dir = temp_dir("exclusive");
        let key_path = dir.join(KEY_FILE_NAME);
        let (first, _) = load_or_create(&key_path).unwrap();
        let before = std::fs::read(&key_path).unwrap();

        // Publishing over an existing key fails instead of replacing it...
        let err = publish_key_file(&key_path, &[9; SEED_LEN]).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&key_path).unwrap(), before);
        // ...and leaves no temporary file behind.
        let leftovers = std::fs::read_dir(&dir).unwrap().count();
        assert_eq!(leftovers, 1);
        assert_eq!(
            load_or_create(&key_path).unwrap().0.key_id(),
            first.key_id()
        );

        // A symlink to a missing file is refused rather than replaced.
        #[cfg(unix)]
        {
            let link = dir.join("dangling.bin");
            std::os::unix::fs::symlink(dir.join("missing"), &link).unwrap();
            assert!(load_or_create(&link).is_err());
            assert!(
                std::fs::symlink_metadata(&link)
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_damaged_key_file_is_rejected_and_kept() {
        let dir = temp_dir("damaged");
        let key_path = dir.join(KEY_FILE_NAME);
        std::fs::write(&key_path, b"not a key").unwrap();
        assert!(load_or_create(&key_path).is_err());
        assert_eq!(std::fs::read(&key_path).unwrap(), b"not a key");

        let mut wrong_magic = vec![0u8; KEY_FILE_LEN];
        wrong_magic[..4].copy_from_slice(b"PLAU");
        std::fs::write(&key_path, &wrong_magic).unwrap();
        assert!(load_or_create(&key_path).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
