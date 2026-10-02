use std::{
    fs,
    io::{self, Read},
    path::Path,
};
pub fn material(path: &Path, limit: usize, private: bool) -> io::Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > limit as u64 {
        return Err(io::Error::other("invalid TLS material path or size"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if private && metadata.permissions().mode() & 0o077 != 0 {
            return Err(io::Error::other(
                "TLS private key must not be accessible to group or other users",
            ));
        }
    }
    let _ = private;
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(io::Error::other("TLS material exceeds limit"));
    }
    Ok(bytes)
}
