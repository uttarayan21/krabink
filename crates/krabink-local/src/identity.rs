//! The node's long-lived ed25519 identity: its public key is the
//! [`EndpointId`](iroh::EndpointId) peers dial.

use std::path::Path;

use iroh::SecretKey;

use crate::{Error, Result};

/// Read the key at `path` (32 bytes, hex) or generate and store a new one
/// with owner-only permissions.
pub fn load_or_create_secret_key(path: &Path) -> Result<SecretKey> {
    if let Ok(text) = std::fs::read_to_string(path) {
        let bytes = hex::decode(text.trim()).map_err(|err| Error::Key(err.to_string()))?;
        let bytes: [u8; 32] = bytes
            .try_into()
            .map_err(|_| Error::Key(format!("{} is not a 32-byte key", path.display())))?;
        return Ok(SecretKey::from_bytes(&bytes));
    }
    let key = SecretKey::generate();
    let io = |source| Error::Io {
        context: format!("writing {}", path.display()),
        source,
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(io)?;
    }
    std::fs::write(path, hex::encode(key.to_bytes())).map_err(io)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(io)?;
    }
    Ok(key)
}
