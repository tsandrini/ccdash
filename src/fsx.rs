//! Owner-only file modes. Unix applies them; on Windows `set_mode` gives an
//! owner-only mode (no group or other bits) a DACL that lets only the user
//! in, and a file created with a mode takes the DACL of its directory,
//! which `set_mode` made owner-only (the state, runtime and edit dirs).

use std::{fs::OpenOptions, io, path::Path};

/// `OpenOptions::mode` on Unix; nothing elsewhere.
pub trait ModeExt {
    fn mode(&mut self, mode: u32) -> &mut Self;
}

impl ModeExt for OpenOptions {
    #[cfg(unix)]
    fn mode(&mut self, mode: u32) -> &mut Self {
        std::os::unix::fs::OpenOptionsExt::mode(self, mode)
    }

    #[cfg(not(unix))]
    fn mode(&mut self, _mode: u32) -> &mut Self {
        self
    }
}

/// Sets the permission bits of `path` on Unix; on Windows an owner-only
/// mode becomes an owner-only DACL, any other leaves the DACL alone.
///
/// # Errors
///
/// `chmod` (or setting the DACL) failed.
pub fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
    }
    #[cfg(windows)]
    {
        if mode & 0o077 == 0 {
            crate::win::owner_only(path)?;
        }
        Ok(())
    }
}
