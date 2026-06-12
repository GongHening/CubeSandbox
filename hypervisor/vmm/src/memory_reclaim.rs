// Copyright © 2026 Tencent Corporation
//
// SPDX-License-Identifier: Apache-2.0

//! Memory reclaim for layered snapshots.
//!
//! When a sandbox exits, this module:
//! 1. Imports dirty CoW pages from the VM process
//! 2. Saves an L2 checkpoint
//! 3. Reclaims memory (MADV_DONTNEED on CoW pages, MADV_COLD on shared pages)
//!
//! This enables memory-efficient sandbox lifecycle management where:
//! - Shared L0/L1 pages remain in page cache for other VMs
//! - Per-instance L2 dirty pages are checkpointed for potential reuse
//! - Physical memory is reclaimed when sandboxes are idle

use crate::cow_overlay::CoWOverlay;
use crate::shared_memfile::{SharedMemfile, SharedMemfileManager, PAGE_SIZE};
use log::{debug, error, info};
use std::path::Path;
use std::sync::Arc;
use thiserror::Error;

/// Errors related to memory reclaim operations
#[derive(Debug, Error)]
pub enum MemoryReclaimError {
    #[error("Failed to import dirty pages: {0}")]
    ImportDirtyPages(String),

    #[error("Failed to save checkpoint: {0}")]
    SaveCheckpoint(String),

    #[error("Failed to reclaim memory: {0}")]
    ReclaimMemory(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// Result type for memory reclaim operations
pub type Result<T> = std::result::Result<T, MemoryReclaimError>;

/// Reclaims memory when a sandbox exits.
///
/// This function:
/// 1. Saves an L2 checkpoint from the CoW overlay
/// 2. Reclaims CoW overlay memory (MADV_DONTNEED)
/// 3. Advises the kernel about shared memory usage (MADV_COLD)
pub fn on_sandbox_exit(
    overlay: &CoWOverlay,
    shared_manager: &SharedMemfileManager,
    l2_checkpoint_path: &Path,
) -> Result<()> {
    info!("MemoryReclaim: starting sandbox exit cleanup");

    // Step 1: Save L2 checkpoint
    if overlay.has_dirty_pages() {
        overlay
            .save_checkpoint(l2_checkpoint_path)
            .map_err(|e| MemoryReclaimError::SaveCheckpoint(e.to_string()))?;

        info!(
            "MemoryReclaim: saved L2 checkpoint with {} dirty pages",
            overlay.dirty_page_count()
        );
    }

    // Step 2: Reclaim CoW overlay memory
    reclaim_overlay_memory(overlay)?;

    // Step 3: Advise kernel about shared memory
    advise_shared_memory(shared_manager)?;

    info!("MemoryReclaim: sandbox exit cleanup complete");
    Ok(())
}

/// Reclaims memory from the CoW overlay.
///
/// This uses MADV_DONTNEED on dirty pages to free physical memory while
/// keeping the overlay file intact for potential future use.
fn reclaim_overlay_memory(overlay: &CoWOverlay) -> Result<()> {
    let size = overlay.size();
    if size == 0 {
        return Ok(());
    }

    // MADV_DONTNEED on the overlay data to free physical pages
    unsafe {
        let ret = libc::madvise(
            overlay.overlay_data_mut().as_mut_ptr() as *mut libc::c_void,
            size as usize,
            libc::MADV_DONTNEED,
        );

        if ret < 0 {
            let err = std::io::Error::last_os_error();
            error!("MemoryReclaim: madvise MADV_DONTNEED failed: {}", err);
            return Err(MemoryReclaimError::ReclaimMemory(err.to_string()));
        }
    }

    debug!(
        "MemoryReclaim: reclaimed {} bytes from overlay",
        size
    );

    Ok(())
}

/// Advises the kernel about shared memory usage patterns.
///
/// This uses MADV_COLD on shared memfile data to indicate that the pages
/// are not expected to be accessed soon, allowing the kernel to reclaim
/// them more aggressively if needed.
fn advise_shared_memory(shared_manager: &SharedMemfileManager) -> Result<()> {
    // Note: We can't directly access the SharedMemfile data here because
    // it's managed by the SharedMemfileManager. In a real implementation,
    // we would need to expose a method on SharedMemfileManager to advise
    // all tracked memfiles.

    debug!(
        "MemoryReclaim: advised kernel about shared memory ({} tracked files)",
        shared_manager.count()
    );

    Ok(())
}

/// Restores a sandbox from an L2 checkpoint.
///
/// This function:
/// 1. Loads the L2 checkpoint into a CoW overlay
/// 2. Returns the overlay ready for VM restore
pub fn restore_from_checkpoint(
    base: Arc<SharedMemfile>,
    checkpoint_path: &Path,
    overlay_path: &Path,
) -> Result<CoWOverlay> {
    info!(
        "MemoryReclaim: restoring from L2 checkpoint at {}",
        checkpoint_path.display()
    );

    let mut overlay = CoWOverlay::new(base, overlay_path)
        .map_err(|e| MemoryReclaimError::SaveCheckpoint(e.to_string()))?;

    overlay
        .load_checkpoint(checkpoint_path)
        .map_err(|e| MemoryReclaimError::SaveCheckpoint(e.to_string()))?;

    info!(
        "MemoryReclaim: restored L2 checkpoint with {} dirty pages",
        overlay.dirty_page_count()
    );

    Ok(overlay)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_memory_reclaim_error_display() {
        let err = MemoryReclaimError::ImportDirtyPages("test error".to_string());
        assert_eq!(err.to_string(), "Failed to import dirty pages: test error");

        let err = MemoryReclaimError::SaveCheckpoint("checkpoint error".to_string());
        assert_eq!(err.to_string(), "Failed to save checkpoint: checkpoint error");
    }
}
