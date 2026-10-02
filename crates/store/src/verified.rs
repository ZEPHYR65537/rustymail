//! Successful EOF means the bytes read match the durable blob metadata.
use crate::{StoreError, blob::reject_symlink};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{self, Read},
    path::Path,
};

pub(crate) struct VerifiedReader {
    file: File,
    expected_size: u64,
    expected_hash: String,
    read_bytes: u64,
    hash: Sha256,
    verified: bool,
    failed: bool,
}

impl VerifiedReader {
    pub(crate) fn open(
        path: &Path,
        expected_size: u64,
        expected_hash: String,
    ) -> Result<Self, StoreError> {
        reject_symlink(path)?;
        let file = File::open(path)?;
        if !file.metadata()?.is_file() {
            return Err(StoreError::UnsafePath);
        }
        Ok(Self {
            file,
            expected_size,
            expected_hash,
            read_bytes: 0,
            hash: Sha256::new(),
            verified: false,
            failed: false,
        })
    }
}

impl Read for VerifiedReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() || self.verified {
            return Ok(0);
        }
        if self.failed {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "blob integrity mismatch",
            ));
        }
        let count = self.file.read(buffer)?;
        self.read_bytes = self
            .read_bytes
            .checked_add(count as u64)
            .ok_or_else(|| io::Error::other("blob byte counter overflow"))?;
        self.hash.update(&buffer[..count]);
        if self.read_bytes > self.expected_size
            || (count == 0
                && (self.read_bytes != self.expected_size
                    || format!("{:x}", self.hash.clone().finalize()) != self.expected_hash))
        {
            self.failed = true;
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "blob integrity mismatch",
            ));
        }
        self.verified = count == 0;
        Ok(count)
    }
}
