// Copyright © 2026 Tencent Corporation
//
// SPDX-License-Identifier: Apache-2.0

//! Cross-VM memory sharing via MAP_PRIVATE mmap.
//!
//! The `SharedMemfileManager` enables multiple Cloud Hypervisor microVMs to
//! share read-only memory pages through the host page cache. Using `MAP_PRIVATE`
//! means writes trigger copy-on-write (CoW) per process, while read-only pages
//! share physical memory across all VMs mapping the same file.
//!
//! This is the foundation for the three-layer snapshot mechanism:
//! - L0 (infrastructure) and L1 (runtime) memfiles are shared across all VMs
//! - L2 (per-instance) uses a CoW overlay on top of the shared base

use log::{debug, info};
use std::collections::HashMap;
use std::fs::File;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, RwLock};
use thiserror::Error;

/// Page size constant (4KB)
pub const PAGE_SIZE: u64 = 4096;

/// Errors related to shared memfile operations
#[derive(Debug, Error)]
pub enum SharedMemfileError {
    #[error("Failed to open memfile {path}: {source}")]
    OpenFailed {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("Failed to stat memfile {path}: {source}")]
    StatFailed {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("Memfile {path} is empty")]
    EmptyFile { path: String },

    #[error("Failed to mmap memfile {path}: {source}")]
    MmapFailed {
        path: String,
        #[source]
        source: nix::Error,
    },

    #[error("Failed to munmap: {0}")]
    MunmapFailed(#[source] nix::Error),

    #[error("Memfile not found: {0}")]
    NotFound(String),
}

/// Result type for shared memfile operations
pub type Result<T> = std::result::Result<T, SharedMemfileError>;

/// A shared memory-mapped file that can be mapped into multiple VM processes.
///
/// Read-only pages hit the host page cache and are shared across all VMs.
/// Writes trigger copy-on-write (CoW) per process, providing isolation.
#[derive(Debug)]
pub struct SharedMemfile {
    /// Absolute path to the memfile on disk
    pub path: PathBuf,
    /// The file handle
    file: File,
    /// The mmap'd region (MAP_PRIVATE | MAP_POPULATE)
    data: *mut u8,
    /// Size of the mapping in bytes
    pub size: u64,
    /// Reference count for shared mappings
    ref_cnt: AtomicI32,
}

// SAFETY: The mmap'd region is backed by a file and uses MAP_PRIVATE.
// Each process gets its own copy-on-write view. The data pointer is
// valid for the lifetime of the SharedMemfile.
unsafe impl Send for SharedMemfile {}
unsafe impl Sync for SharedMemfile {}

impl SharedMemfile {
    /// Returns a slice of the mmap'd data.
    ///
    /// # Safety
    /// The caller must ensure the data is valid and no mutable aliases exist.
    pub unsafe fn data(&self) -> &[u8] {
        std::slice::from_raw_parts(self.data, self.size as usize)
    }

    /// Returns a mutable slice of the mmap'd data.
    ///
    /// # Safety
    /// The caller must ensure no other aliases exist. Writes will trigger
    /// copy-on-write and are isolated to this process.
    pub unsafe fn data_mut(&self) -> &mut [u8] {
        std::slice::from_raw_parts_mut(self.data, self.size as usize)
    }

    /// Returns the raw pointer to the mmap'd data.
    pub fn as_ptr(&self) -> *mut u8 {
        self.data
    }

    /// Returns the file descriptor.
    pub fn fd(&self) -> i32 {
        self.file.as_raw_fd()
    }
}

impl Drop for SharedMemfile {
    fn drop(&mut self) {
        if !self.data.is_null() {
            unsafe {
                if let Some(non_null) = std::ptr::NonNull::new(self.data as *mut std::ffi::c_void) {
                    if let Err(e) = nix::sys::mman::munmap(non_null, self.size as usize) {
                        log::error!("Failed to munmap shared memfile {}: {}", self.path.display(), e);
                    }
                }
            }
        }
    }
}

/// Manages memory-mapped files that can be shared across multiple VM processes.
///
/// Uses reference counting so that multiple VMs mapping the same L0 or L1
/// memfile path share the same in-memory mapping. The mapping is only
/// munmap'd when the last reference drops.
#[derive(Clone)]
pub struct SharedMemfileManager {
    inner: Arc<RwLock<SharedMemfileManagerInner>>,
}

struct SharedMemfileManagerInner {
    memfiles: HashMap<PathBuf, Arc<SharedMemfile>>,
}

impl SharedMemfileManager {
    /// Creates a new shared memfile manager.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(SharedMemfileManagerInner {
                memfiles: HashMap::new(),
            })),
        }
    }

    /// Maps a memfile for shared access. Multiple callers mapping the same
    /// path share physical pages until a write triggers CoW.
    ///
    /// The mapping uses:
    /// - `MAP_PRIVATE`: writes trigger CoW, read-only pages share page cache
    /// - `MAP_POPULATE`: pre-fault pages to reduce first-access minor faults
    /// - `MADV_SEQUENTIAL`: reduce page cache lock contention for concurrent access
    pub fn map(&self, path: &Path) -> Result<Arc<SharedMemfile>> {
        // Fast path: read lock for existing entry.
        {
            let inner = self.inner.read().unwrap();
            if let Some(existing) = inner.memfiles.get(path) {
                existing.ref_cnt.fetch_add(1, Ordering::SeqCst);
                debug!("SharedMemfileManager: reusing existing mapping for {}", path.display());
                return Ok(existing.clone());
            }
        }

        // Slow path: create new mapping.
        let mut inner = self.inner.write().unwrap();

        // Double-check after acquiring write lock.
        if let Some(existing) = inner.memfiles.get(path) {
            existing.ref_cnt.fetch_add(1, Ordering::SeqCst);
            return Ok(existing.clone());
        }

        let file = File::open(path).map_err(|e| SharedMemfileError::OpenFailed {
            path: path.display().to_string(),
            source: e,
        })?;

        let metadata = file.metadata().map_err(|e| SharedMemfileError::StatFailed {
            path: path.display().to_string(),
            source: e,
        })?;

        let size = metadata.len();
        if size == 0 {
            return Err(SharedMemfileError::EmptyFile {
                path: path.display().to_string(),
            });
        }

        // MAP_PRIVATE: writes trigger CoW, read-only pages are shared in host page cache.
        // MAP_POPULATE: pre-fault pages to reduce first-access minor faults.
        let data = unsafe {
            nix::sys::mman::mmap(
                None,
                std::num::NonZeroUsize::new(size as usize).ok_or_else(|| SharedMemfileError::MmapFailed {
                    path: path.display().to_string(),
                    source: nix::errno::Errno::EINVAL,
                })?,
                nix::sys::mman::ProtFlags::PROT_READ | nix::sys::mman::ProtFlags::PROT_WRITE,
                nix::sys::mman::MapFlags::MAP_PRIVATE | nix::sys::mman::MapFlags::MAP_POPULATE,
                &file,
                0,
            )
            .map_err(|e| SharedMemfileError::MmapFailed {
                path: path.display().to_string(),
                source: e,
            })?
        };

        // MADV_SEQUENTIAL reduces kernel page cache lock contention when
        // multiple VMs concurrently map the same large shared memfile.
        unsafe {
            let _ = nix::sys::mman::madvise(
                data,
                size as usize,
                nix::sys::mman::MmapAdvise::MADV_SEQUENTIAL,
            );
        }

        let memfile = Arc::new(SharedMemfile {
            path: path.to_path_buf(),
            file,
            data: data.as_ptr() as *mut u8,
            size,
            ref_cnt: AtomicI32::new(1),
        });

        inner.memfiles.insert(path.to_path_buf(), memfile.clone());

        info!(
            "SharedMemfileManager: mapped {} ({} bytes)",
            path.display(),
            size
        );

        Ok(memfile)
    }

    /// Unmaps a memfile. The actual munmap happens only when the last
    /// reference is dropped.
    pub fn unmap(&self, path: &Path) -> Result<()> {
        let mut inner = self.inner.write().unwrap();

        if let Some(memfile) = inner.memfiles.get(path) {
            let refs = memfile.ref_cnt.fetch_sub(1, Ordering::SeqCst);
            if refs <= 1 {
                // Last reference — remove from map. The actual munmap
                // happens in SharedMemfile::drop.
                inner.memfiles.remove(path);
                info!("SharedMemfileManager: unmapped {}", path.display());
            }
        }

        Ok(())
    }

    /// Returns the number of tracked shared memfiles.
    pub fn count(&self) -> usize {
        let inner = self.inner.read().unwrap();
        inner.memfiles.len()
    }

    /// Returns the total bytes of tracked shared memfiles.
    pub fn total_bytes(&self) -> u64 {
        let inner = self.inner.read().unwrap();
        inner.memfiles.values().map(|m| m.size).sum()
    }

    /// Closes all tracked memfiles.
    pub fn close(&self) {
        let mut inner = self.inner.write().unwrap();
        inner.memfiles.clear();
        info!("SharedMemfileManager: closed all mappings");
    }
}

impl Default for SharedMemfileManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;

    #[test]
    fn test_shared_memfile_manager() {
        let dir = std::env::temp_dir().join("ch-shared-memfile-test");
        fs::create_dir_all(&dir).unwrap();

        let test_file = dir.join("test.mem");
        let mut f = File::create(&test_file).unwrap();
        f.write_all(&[0u8; 4096]).unwrap();
        f.flush().unwrap();

        let manager = SharedMemfileManager::new();

        // First map
        let memfile1 = manager.map(&test_file).unwrap();
        assert_eq!(memfile1.size, 4096);
        assert_eq!(manager.count(), 1);

        // Second map of same file should reuse
        let memfile2 = manager.map(&test_file).unwrap();
        assert_eq!(manager.count(), 1);
        assert_eq!(memfile1.as_ptr(), memfile2.as_ptr());

        // Unmap one reference
        manager.unmap(&test_file).unwrap();
        assert_eq!(manager.count(), 1);

        // Unmap last reference
        manager.unmap(&test_file).unwrap();
        assert_eq!(manager.count(), 0);

        fs::remove_dir_all(&dir).unwrap();
    }
}
