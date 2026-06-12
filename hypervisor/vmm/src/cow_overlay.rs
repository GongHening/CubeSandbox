// Copyright © 2026 Tencent Corporation
//
// SPDX-License-Identifier: Apache-2.0

//! Copy-on-Write overlay for per-instance dirty page tracking.
//!
//! The `CoWOverlay` wraps a shared base memfile (L0/L1) with a sparse overlay
//! file for CoW-modified pages (L2). It provides a read-through view:
//! - Reads check the overlay first (via dirty bitmap)
//! - Falls back to the shared base for unmodified pages
//!
//! The overlay file is a sparse file the same size as the base. Only pages
//! explicitly imported consume physical storage.

use crate::shared_memfile::{SharedMemfile, PAGE_SIZE};
use log::{debug, info};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use thiserror::Error;

/// Errors related to CoW overlay operations
#[derive(Debug, Error)]
pub enum CoWOverlayError {
    #[error("Failed to open overlay file {path}: {source}")]
    OpenFailed {
        path: String,
        #[source]
        source: io::Error,
    },

    #[error("Failed to truncate overlay file: {0}")]
    TruncateFailed(#[source] io::Error),

    #[error("Failed to mmap overlay file: {0}")]
    MmapFailed(#[source] nix::Error),

    #[error("Failed to read from overlay: {0}")]
    ReadFailed(#[source] io::Error),

    #[error("Failed to write to overlay: {0}")]
    WriteFailed(#[source] io::Error),

    #[error("Invalid offset: {0}")]
    InvalidOffset(i64),

    #[error("Overlay size mismatch: expected {expected}, got {actual}")]
    SizeMismatch { expected: u64, actual: u64 },

    #[error("Checkpoint error: {0}")]
    Checkpoint(String),
}

/// Result type for CoW overlay operations
pub type Result<T> = std::result::Result<T, CoWOverlayError>;

/// Page state tracking using a simple bitmap.
///
/// Each bit represents a 4KB page. A set bit means the page has been
/// modified (dirty) and exists in the overlay.
#[derive(Debug)]
pub struct PageBitmap {
    /// Bitmap data (1 bit per page)
    data: Vec<u64>,
    /// Number of pages tracked
    num_pages: u64,
}

impl PageBitmap {
    /// Creates a new page bitmap for the given number of pages.
    pub fn new(num_pages: u64) -> Self {
        let words = ((num_pages + 63) / 64) as usize;
        Self {
            data: vec![0u64; words],
            num_pages,
        }
    }

    /// Returns the state of a page (true = dirty).
    pub fn is_set(&self, page_idx: u64) -> bool {
        if page_idx >= self.num_pages {
            return false;
        }
        let word = (page_idx / 64) as usize;
        let bit = page_idx % 64;
        (self.data[word] >> bit) & 1 != 0
    }

    /// Sets a page as dirty.
    pub fn set(&mut self, page_idx: u64) {
        if page_idx >= self.num_pages {
            return;
        }
        let word = (page_idx / 64) as usize;
        let bit = page_idx % 64;
        self.data[word] |= 1 << bit;
    }

    /// Clears a page's dirty state.
    pub fn clear(&mut self, page_idx: u64) {
        if page_idx >= self.num_pages {
            return;
        }
        let word = (page_idx / 64) as usize;
        let bit = page_idx % 64;
        self.data[word] &= !(1 << bit);
    }

    /// Returns the number of dirty pages.
    pub fn dirty_count(&self) -> u64 {
        self.data.iter().map(|w| w.count_ones() as u64).sum()
    }

    /// Returns the total number of pages tracked.
    pub fn total_pages(&self) -> u64 {
        self.num_pages
    }

    /// Serializes the bitmap to bytes.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut result = Vec::with_capacity(self.data.len() * 8);
        for word in &self.data {
            result.extend_from_slice(&word.to_le_bytes());
        }
        result
    }

    /// Deserializes a bitmap from bytes.
    pub fn from_bytes(data: &[u8], num_pages: u64) -> Self {
        let words = ((num_pages + 63) / 64) as usize;
        let mut bitmap = Self::new(num_pages);
        for (i, chunk) in data.chunks_exact(8).enumerate().take(words) {
            if let Ok(arr) = chunk.try_into() {
                bitmap.data[i] = u64::from_le_bytes(arr);
            }
        }
        bitmap
    }

    /// Returns an iterator over dirty page indices.
    pub fn iter_dirty(&self) -> impl Iterator<Item = u64> + '_ {
        let num_pages = self.num_pages;
        self.data.iter().enumerate().flat_map(move |(word_idx, &word)| {
            (0..64).filter_map(move |bit| {
                let page_idx = word_idx as u64 * 64 + bit;
                if page_idx < num_pages && (word >> bit) & 1 != 0 {
                    Some(page_idx)
                } else {
                    None
                }
            })
        })
    }
}

/// CoW overlay wrapping a shared base memfile with a sparse overlay for
/// per-instance dirty pages.
///
/// The overlay provides a read-through view:
/// - Dirty pages (marked in the bitmap) come from the overlay
/// - Clean pages fall through to the shared base
pub struct CoWOverlay {
    /// The shared base memfile (L0/L1)
    base: Arc<SharedMemfile>,
    /// Path to the overlay file
    overlay_path: PathBuf,
    /// The overlay file handle
    overlay_file: File,
    /// The mmap'd overlay region (MAP_SHARED for durability)
    overlay_data: *mut u8,
    /// Dirty page bitmap
    bitmap: Arc<RwLock<PageBitmap>>,
    /// Size of the overlay in bytes
    size: u64,
    /// Statistics
    dirty_pages: AtomicU64,
}

// SAFETY: The overlay data is mmap'd with MAP_SHARED and is backed by a file.
// Access is synchronized through the bitmap RwLock.
unsafe impl Send for CoWOverlay {}
unsafe impl Sync for CoWOverlay {}

impl CoWOverlay {
    /// Creates a new CoW overlay wrapping the given shared base memfile.
    ///
    /// The overlay file is created/truncated as a sparse file matching
    /// base.size, then mmap'd with MAP_SHARED so writes are durable.
    pub fn new(base: Arc<SharedMemfile>, overlay_path: &Path) -> Result<Self> {
        let size = base.size;
        let num_pages = size / PAGE_SIZE;

        let overlay_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(overlay_path)
            .map_err(|e| CoWOverlayError::OpenFailed {
                path: overlay_path.display().to_string(),
                source: e,
            })?;

        overlay_file
            .set_len(size)
            .map_err(CoWOverlayError::TruncateFailed)?;

        let overlay_data = unsafe {
            nix::sys::mman::mmap(
                None,
                std::num::NonZeroUsize::new(size as usize).ok_or_else(|| CoWOverlayError::MmapFailed(nix::errno::Errno::EINVAL))?,
                nix::sys::mman::ProtFlags::PROT_READ | nix::sys::mman::ProtFlags::PROT_WRITE,
                nix::sys::mman::MapFlags::MAP_SHARED,
                &overlay_file,
                0,
            )
            .map_err(CoWOverlayError::MmapFailed)?
        };

        info!(
            "CoWOverlay: created overlay at {} ({} bytes, {} pages)",
            overlay_path.display(),
            size,
            num_pages
        );

        Ok(Self {
            base,
            overlay_path: overlay_path.to_path_buf(),
            overlay_file,
            overlay_data: overlay_data.as_ptr() as *mut u8,
            bitmap: Arc::new(RwLock::new(PageBitmap::new(num_pages))),
            size,
            dirty_pages: AtomicU64::new(0),
        })
    }

    /// Returns a slice of the overlay data.
    ///
    /// # Safety
    /// The caller must ensure no mutable aliases exist.
    pub unsafe fn overlay_data(&self) -> &[u8] {
        std::slice::from_raw_parts(self.overlay_data, self.size as usize)
    }

    /// Returns a mutable slice of the overlay data.
    ///
    /// # Safety
    /// The caller must ensure no other aliases exist.
    pub unsafe fn overlay_data_mut(&self) -> &mut [u8] {
        std::slice::from_raw_parts_mut(self.overlay_data, self.size as usize)
    }

    /// Returns the base memfile.
    pub fn base(&self) -> &Arc<SharedMemfile> {
        &self.base
    }

    /// Returns the size of the overlay in bytes.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// Returns the number of dirty pages.
    pub fn dirty_page_count(&self) -> u64 {
        self.dirty_pages.load(Ordering::Relaxed)
    }

    /// Returns true if at least one page has been modified.
    pub fn has_dirty_pages(&self) -> bool {
        self.dirty_pages.load(Ordering::Relaxed) > 0
    }

    /// Returns the overlay file path.
    pub fn overlay_path(&self) -> &Path {
        &self.overlay_path
    }

    /// Reads a page, preferring the overlay if the page is marked dirty.
    pub fn read_page(&self, page_offset: u64) -> Option<&[u8]> {
        if page_offset >= self.size || page_offset % PAGE_SIZE != 0 {
            return None;
        }

        let page_idx = page_offset / PAGE_SIZE;
        let end = page_offset + PAGE_SIZE;
        if end > self.size {
            return None;
        }

        let bitmap = self.bitmap.read().unwrap();
        if bitmap.is_set(page_idx) {
            Some(unsafe {
                std::slice::from_raw_parts(self.overlay_data.add(page_offset as usize), PAGE_SIZE as usize)
            })
        } else {
            Some(unsafe {
                std::slice::from_raw_parts(self.base.as_ptr().add(page_offset as usize), PAGE_SIZE as usize)
            })
        }
    }

    /// Writes a page to the overlay and marks it as dirty.
    pub fn write_page(&self, page_offset: u64, data: &[u8]) -> Result<()> {
        if page_offset >= self.size || page_offset % PAGE_SIZE != 0 {
            return Err(CoWOverlayError::InvalidOffset(page_offset as i64));
        }

        let page_idx = page_offset / PAGE_SIZE;
        let end = page_offset + PAGE_SIZE;
        if end > self.size {
            return Err(CoWOverlayError::InvalidOffset(page_offset as i64));
        }

        unsafe {
            std::ptr::copy_nonoverlapping(
                data.as_ptr(),
                self.overlay_data.add(page_offset as usize),
                data.len().min(PAGE_SIZE as usize),
            );
        }

        let mut bitmap = self.bitmap.write().unwrap();
        if !bitmap.is_set(page_idx) {
            bitmap.set(page_idx);
            self.dirty_pages.fetch_add(1, Ordering::Relaxed);
        }

        Ok(())
    }

    /// Imports dirty pages from a source buffer. Each page is written to
    /// the overlay and marked dirty.
    pub fn import_dirty_pages(&self, offset: u64, data: &[u8]) -> Result<()> {
        let page_size = PAGE_SIZE as usize;
        let mut page_offset = offset;

        for chunk in data.chunks(page_size) {
            if page_offset >= self.size {
                break;
            }

            let page_idx = page_offset / PAGE_SIZE;
            let copy_len = chunk.len().min(page_size);

            unsafe {
                std::ptr::copy_nonoverlapping(
                    chunk.as_ptr(),
                    self.overlay_data.add(page_offset as usize),
                    copy_len,
                );
            }

            let mut bitmap = self.bitmap.write().unwrap();
            if !bitmap.is_set(page_idx) {
                bitmap.set(page_idx);
                self.dirty_pages.fetch_add(1, Ordering::Relaxed);
            }

            page_offset += PAGE_SIZE as u64;
        }

        Ok(())
    }

    /// Reads data from the overlay, falling back to the base for clean pages.
    pub fn read_at(&self, buf: &mut [u8], offset: u64) -> Result<usize> {
        if offset >= self.size {
            return Ok(0);
        }

        let to_read = (buf.len() as u64).min(self.size - offset);
        let page_size = PAGE_SIZE;
        let mut pos = offset;
        let mut buf_pos = 0;

        while pos < offset + to_read {
            let page_idx = pos / page_size;
            let page_start = page_idx * page_size;
            let page_end = (page_start + page_size).min(self.size);

            let chunk_start = (pos - page_start) as usize;
            let chunk_end = ((offset + to_read - page_start).min(page_size)) as usize;
            let chunk_len = chunk_end - chunk_start;

            let bitmap = self.bitmap.read().unwrap();
            let src = if bitmap.is_set(page_idx) {
                unsafe { self.overlay_data.add(page_start as usize + chunk_start) }
            } else {
                unsafe { self.base.as_ptr().add(page_start as usize + chunk_start) }
            };

            unsafe {
                std::ptr::copy_nonoverlapping(src, buf.as_mut_ptr().add(buf_pos), chunk_len);
            }

            pos += chunk_len as u64;
            buf_pos += chunk_len;
        }

        Ok(to_read as usize)
    }

    /// Saves a checkpoint of the overlay to disk.
    ///
    /// Writes two files:
    /// - `{dst_path}.data`: the sparse overlay data (only dirty pages)
    /// - `{dst_path}.bitmap`: the dirty page bitmap
    pub fn save_checkpoint(&self, dst_path: &Path) -> Result<()> {
        let data_path = dst_path.with_extension("data");
        let bitmap_path = dst_path.with_extension("bitmap");

        // Write sparse data file (only dirty pages)
        let mut data_file = File::create(&data_path)
            .map_err(|e| CoWOverlayError::Checkpoint(format!("create data file: {}", e)))?;

        data_file
            .set_len(self.size)
            .map_err(|e| CoWOverlayError::Checkpoint(format!("truncate data file: {}", e)))?;

        let bitmap = self.bitmap.read().unwrap();
        let page_size = PAGE_SIZE;

        for page_idx in bitmap.iter_dirty() {
            let offset = page_idx * page_size;
            let end = (offset + page_size).min(self.size);
            let len = (end - offset) as usize;

            data_file
                .seek(SeekFrom::Start(offset))
                .map_err(|e| CoWOverlayError::Checkpoint(format!("seek: {}", e)))?;

            unsafe {
                let slice = std::slice::from_raw_parts(self.overlay_data.add(offset as usize), len);
                data_file
                    .write_all(slice)
                    .map_err(|e| CoWOverlayError::Checkpoint(format!("write: {}", e)))?;
            }
        }

        // Write bitmap
        let bitmap_bytes = bitmap.to_bytes();
        std::fs::write(&bitmap_path, &bitmap_bytes)
            .map_err(|e| CoWOverlayError::Checkpoint(format!("write bitmap: {}", e)))?;

        info!(
            "CoWOverlay: saved checkpoint to {} ({} dirty pages)",
            dst_path.display(),
            bitmap.dirty_count()
        );

        Ok(())
    }

    /// Loads a checkpoint from disk into this overlay.
    pub fn load_checkpoint(&mut self, checkpoint_path: &Path) -> Result<()> {
        let data_path = checkpoint_path.with_extension("data");
        let bitmap_path = checkpoint_path.with_extension("bitmap");

        // Read bitmap
        let bitmap_bytes = std::fs::read(&bitmap_path)
            .map_err(|e| CoWOverlayError::Checkpoint(format!("read bitmap: {}", e)))?;

        let num_pages = self.size / PAGE_SIZE;
        let loaded_bitmap = PageBitmap::from_bytes(&bitmap_bytes, num_pages);

        // Read and copy dirty pages
        let mut data_file = File::open(&data_path)
            .map_err(|e| CoWOverlayError::Checkpoint(format!("open data file: {}", e)))?;

        for page_idx in loaded_bitmap.iter_dirty() {
            let offset = page_idx * PAGE_SIZE;
            let end = (offset + PAGE_SIZE).min(self.size);
            let len = (end - offset) as usize;

            data_file
                .seek(SeekFrom::Start(offset))
                .map_err(|e| CoWOverlayError::Checkpoint(format!("seek: {}", e)))?;

            let mut buf = vec![0u8; len];
            data_file
                .read_exact(&mut buf)
                .map_err(|e| CoWOverlayError::Checkpoint(format!("read: {}", e)))?;

            unsafe {
                std::ptr::copy_nonoverlapping(
                    buf.as_ptr(),
                    self.overlay_data.add(offset as usize),
                    len,
                );
            }
        }

        // Update bitmap
        let mut bitmap = self.bitmap.write().unwrap();
        *bitmap = loaded_bitmap;
        self.dirty_pages.store(bitmap.dirty_count(), Ordering::Relaxed);

        info!(
            "CoWOverlay: loaded checkpoint from {} ({} dirty pages)",
            checkpoint_path.display(),
            bitmap.dirty_count()
        );

        Ok(())
    }
}

impl Drop for CoWOverlay {
    fn drop(&mut self) {
        if !self.overlay_data.is_null() {
            unsafe {
                if let Some(non_null) = std::ptr::NonNull::new(self.overlay_data as *mut std::ffi::c_void) {
                    if let Err(e) = nix::sys::mman::munmap(
                        non_null,
                        self.size as usize,
                    ) {
                        log::error!("Failed to munmap CoW overlay: {}", e);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_page_bitmap() {
        let mut bitmap = PageBitmap::new(1024);
        assert_eq!(bitmap.dirty_count(), 0);

        bitmap.set(0);
        bitmap.set(42);
        bitmap.set(1023);

        assert!(bitmap.is_set(0));
        assert!(bitmap.is_set(42));
        assert!(bitmap.is_set(1023));
        assert!(!bitmap.is_set(1));
        assert_eq!(bitmap.dirty_count(), 3);

        bitmap.clear(42);
        assert!(!bitmap.is_set(42));
        assert_eq!(bitmap.dirty_count(), 2);
    }

    #[test]
    fn test_page_bitmap_serialization() {
        let mut bitmap = PageBitmap::new(256);
        bitmap.set(0);
        bitmap.set(100);
        bitmap.set(255);

        let bytes = bitmap.to_bytes();
        let restored = PageBitmap::from_bytes(&bytes, 256);

        assert!(restored.is_set(0));
        assert!(restored.is_set(100));
        assert!(restored.is_set(255));
        assert!(!restored.is_set(1));
    }
}
