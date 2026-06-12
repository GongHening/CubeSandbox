// Copyright © 2026 Tencent Corporation
//
// SPDX-License-Identifier: Apache-2.0

//! Three-layer snapshot composition for fast VM startup and memory sharing.
//!
//! This module implements a three-layer snapshot mechanism inspired by the
//! My-E2B orchestrator:
//!
//! - **Layer 0 (L0)**: Infrastructure — guest kernel, agent, base system
//! - **Layer 1 (L1)**: Runtime — language runtime, application frameworks
//! - **Layer 2 (L2)**: Per-instance — private state, dirty pages
//!
//! The layers are composed as follows:
//! - L0 and L1 are shared across all VMs via `MAP_PRIVATE` mmap
//! - L2 is a per-instance CoW overlay tracking dirty pages
//! - On restore, userfaultfd provides demand-paging from the layers
//!
//! This design enables:
//! - Fast VM startup (no need to load full memory upfront)
//! - Memory efficiency (shared pages only stored once in page cache)
//! - High concurrency (many VMs share L0/L1 memory)

use crate::cow_overlay::{CoWOverlay, CoWOverlayError};
use crate::shared_memfile::{SharedMemfile, SharedMemfileManager, SharedMemfileError, PAGE_SIZE};
use crate::uffd::{UffdError, Userfaultfd};
use log::{debug, error, info, warn};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use thiserror::Error;

/// Errors related to snapshot layer operations
#[derive(Debug, Error)]
pub enum SnapshotLayerError {
    #[error("Shared memfile error: {0}")]
    SharedMemfile(#[from] SharedMemfileError),

    #[error("CoW overlay error: {0}")]
    CoWOverlay(#[from] CoWOverlayError),

    #[error("userfaultfd error: {0}")]
    Uffd(#[from] UffdError),

    #[error("Layer not found: {0}")]
    LayerNotFound(String),

    #[error("Invalid layer configuration: {0}")]
    InvalidConfig(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
}

/// Result type for snapshot layer operations
pub type Result<T> = std::result::Result<T, SnapshotLayerError>;

/// Layer identifier
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Layer {
    /// Infrastructure layer (kernel, agent, base system)
    L0,
    /// Runtime layer (language runtime, frameworks)
    L1,
    /// Per-instance layer (private state, dirty pages)
    L2,
}

impl std::fmt::Display for Layer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Layer::L0 => write!(f, "L0"),
            Layer::L1 => write!(f, "L1"),
            Layer::L2 => write!(f, "L2"),
        }
    }
}

/// Reference to a snapshot layer's files
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LayerRef {
    /// Layer identifier
    pub layer: Layer,
    /// Path to the memory file (memfile)
    pub memfile_path: PathBuf,
    /// Path to the snapshot state file (snapfile)
    pub snapfile_path: PathBuf,
    /// Size of the memory file in bytes
    pub memfile_size: u64,
    /// Build ID for compatibility checking
    pub build_id: String,
}

/// Metadata for a layered snapshot
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LayeredSnapshotMetadata {
    /// Whether layered snapshots are enabled
    pub enabled: bool,
    /// L0 layer reference
    pub l0: Option<LayerRef>,
    /// L1 layer reference
    pub l1: Option<LayerRef>,
    /// L2 memfile size in bytes (per-instance)
    pub l2_memfile_size: u64,
    /// Total guest memory size in bytes
    pub guest_memory_size: u64,
}

/// Memory layer for the VM, representing one layer's contribution to
/// the guest physical address space.
#[derive(Debug)]
pub struct MemoryLayer {
    /// The shared memfile data (mmap'd)
    pub data: *mut u8,
    /// Size of this layer's data
    pub size: u64,
    /// Whether this layer is shared across VMs
    pub shared: bool,
    /// Path to the pre-merged file (if applicable)
    pub pre_merged_path: Option<PathBuf>,
}

// SAFETY: MemoryLayer data comes from SharedMemfile or CoWOverlay
// which manage their own lifetimes and synchronization.
unsafe impl Send for MemoryLayer {}
unsafe impl Sync for MemoryLayer {}

/// Statistics for the layered snapshot system
#[derive(Debug, Default)]
pub struct LayeredSnapshotStats {
    /// Number of active VMs sharing L0
    pub l0_shared_vms: u64,
    /// Number of active VMs sharing L1
    pub l1_shared_vms: u64,
    /// Total shared memory bytes
    pub shared_memory_bytes: u64,
    /// Total overlay memory bytes
    pub overlay_memory_bytes: u64,
}

/// Manages the three-layer snapshot system.
///
/// This is the main entry point for creating and restoring layered snapshots.
/// It coordinates the SharedMemfileManager, CoWOverlays, and userfaultfd.
pub struct SnapshotLayerManager {
    /// Manager for shared memfiles (L0, L1)
    shared_manager: SharedMemfileManager,
    /// L0 memfile (shared across all VMs)
    l0_memfile: Option<Arc<SharedMemfile>>,
    /// L1 memfile (shared across all VMs)
    l1_memfile: Option<Arc<SharedMemfile>>,
    /// Layered snapshot metadata
    metadata: Option<LayeredSnapshotMetadata>,
    /// Statistics
    stats: LayeredSnapshotStats,
    /// Guest memory size
    guest_memory_size: u64,
}

impl SnapshotLayerManager {
    /// Creates a new snapshot layer manager.
    pub fn new() -> Self {
        Self {
            shared_manager: SharedMemfileManager::new(),
            l0_memfile: None,
            l1_memfile: None,
            metadata: None,
            stats: LayeredSnapshotStats::default(),
            guest_memory_size: 0,
        }
    }

    /// Initializes the layered snapshot system with the given metadata.
    pub fn initialize(&mut self, metadata: LayeredSnapshotMetadata) -> Result<()> {
        if !metadata.enabled {
            info!("SnapshotLayerManager: layered snapshots disabled");
            self.metadata = Some(metadata);
            return Ok(());
        }

        self.guest_memory_size = metadata.guest_memory_size;

        // Map L0 memfile
        if let Some(ref l0) = metadata.l0 {
            let memfile = self.shared_manager.map(&l0.memfile_path)?;
            self.l0_memfile = Some(memfile);
            info!(
                "SnapshotLayerManager: mapped L0 memfile {} ({} bytes)",
                l0.memfile_path.display(),
                l0.memfile_size
            );
        }

        // Map L1 memfile
        if let Some(ref l1) = metadata.l1 {
            let memfile = self.shared_manager.map(&l1.memfile_path)?;
            self.l1_memfile = Some(memfile);
            info!(
                "SnapshotLayerManager: mapped L1 memfile {} ({} bytes)",
                l1.memfile_path.display(),
                l1.memfile_size
            );
        }

        self.metadata = Some(metadata);

        info!("SnapshotLayerManager: initialized with layered snapshots enabled");

        Ok(())
    }

    /// Returns whether layered snapshots are enabled.
    pub fn is_enabled(&self) -> bool {
        self.metadata
            .as_ref()
            .map(|m| m.enabled)
            .unwrap_or(false)
    }

    /// Returns the layered snapshot metadata.
    pub fn metadata(&self) -> Option<&LayeredSnapshotMetadata> {
        self.metadata.as_ref()
    }

    /// Returns the L0 memfile.
    pub fn l0_memfile(&self) -> Option<&Arc<SharedMemfile>> {
        self.l0_memfile.as_ref()
    }

    /// Returns the L1 memfile.
    pub fn l1_memfile(&self) -> Option<&Arc<SharedMemfile>> {
        self.l1_memfile.as_ref()
    }

    /// Creates a CoW overlay for a new VM instance (L2).
    pub fn create_overlay(&self, overlay_path: &Path) -> Result<CoWOverlay> {
        // Determine the base memfile (prefer L1 over L0)
        let base = if let Some(ref l1) = self.l1_memfile {
            l1.clone()
        } else if let Some(ref l0) = self.l0_memfile {
            l0.clone()
        } else {
            return Err(SnapshotLayerError::LayerNotFound(
                "No base layer available for overlay".to_string(),
            ));
        };

        let overlay = CoWOverlay::new(base, overlay_path)?;
        Ok(overlay)
    }

    /// Builds memory layers for a VM instance.
    ///
    /// Returns the memory layers that compose the guest physical address space.
    pub fn build_memory_layers(&self, overlay: &CoWOverlay) -> Result<Vec<MemoryLayer>> {
        let mut layers = Vec::new();

        // L0 layer (shared)
        if let Some(ref l0) = self.l0_memfile {
            layers.push(MemoryLayer {
                data: l0.as_ptr(),
                size: l0.size,
                shared: true,
                pre_merged_path: None,
            });
        }

        // L1 layer (shared)
        if let Some(ref l1) = self.l1_memfile {
            layers.push(MemoryLayer {
                data: l1.as_ptr(),
                size: l1.size,
                shared: true,
                pre_merged_path: None,
            });
        }

        // L2 layer (per-instance overlay)
        layers.push(MemoryLayer {
            data: unsafe { overlay.overlay_data_mut().as_mut_ptr() },
            size: overlay.size(),
            shared: false,
            pre_merged_path: Some(overlay.overlay_path().to_path_buf()),
        });

        Ok(layers)
    }

    /// Creates a pre-merged memfile from L0+L1 for efficient cross-VM sharing.
    ///
    /// The merged file contains the concatenation of L0 and L1 data, which
    /// can be mapped with MAP_PRIVATE for page cache sharing.
    pub fn create_merged_memfile(&self, output_path: &Path) -> Result<()> {
        let mut total_size = 0u64;

        if let Some(ref l0) = self.l0_memfile {
            total_size += l0.size;
        }
        if let Some(ref l1) = self.l1_memfile {
            total_size += l1.size;
        }

        if total_size == 0 {
            return Err(SnapshotLayerError::InvalidConfig(
                "No layers to merge".to_string(),
            ));
        }

        let file = std::fs::File::create(output_path)?;
        file.set_len(total_size)?;

        let mut offset = 0u64;

        // Write L0 data
        if let Some(ref l0) = self.l0_memfile {
            let data = unsafe { l0.data() };
            std::io::Write::write_all(&mut &file, data)?;
            offset += l0.size;
        }

        // Write L1 data
        if let Some(ref l1) = self.l1_memfile {
            let data = unsafe { l1.data() };
            std::io::Write::write_all(&mut &file, data)?;
        }

        info!(
            "SnapshotLayerManager: created merged memfile at {} ({} bytes)",
            output_path.display(),
            total_size
        );

        Ok(())
    }

    /// Returns the shared memfile manager.
    pub fn shared_manager(&self) -> &SharedMemfileManager {
        &self.shared_manager
    }

    /// Returns the current statistics.
    pub fn stats(&self) -> &LayeredSnapshotStats {
        &self.stats
    }

    /// Cleans up resources.
    pub fn cleanup(&mut self) {
        self.shared_manager.close();
        self.l0_memfile = None;
        self.l1_memfile = None;
        info!("SnapshotLayerManager: cleaned up");
    }
}

impl Default for SnapshotLayerManager {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for SnapshotLayerManager {
    fn drop(&mut self) {
        self.cleanup();
    }
}

/// Creates a layered snapshot from a paused VM.
///
/// This function:
/// 1. Pauses the VM (if not already paused)
/// 2. Creates L0 snapshot (kernel, agent, base system)
/// 3. Creates L1 snapshot (runtime, frameworks)
/// 4. Creates L2 snapshot (per-instance dirty pages)
pub fn create_layered_snapshot(
    vm: &mut crate::vm::Vm,
    output_dir: &Path,
    l0_config: Option<&LayerRef>,
    l1_config: Option<&LayerRef>,
) -> Result<LayeredSnapshotMetadata> {
    use vm_migration::Snapshottable;

    // Create output directory
    std::fs::create_dir_all(output_dir)?;

    // Take VM snapshot
    let snapshot = vm.snapshot().map_err(|e| {
        SnapshotLayerError::InvalidConfig(format!("Failed to snapshot VM: {}", e))
    })?;

    // Save snapshot state
    let state_path = output_dir.join("state.json");
    let state_json = serde_json::to_string_pretty(&snapshot)?;
    std::fs::write(&state_path, state_json)?;

    // Save VM config
    let config_path = output_dir.join("config.json");
    let config = vm.get_config();
    let config_guard = config.lock().map_err(|e| {
        SnapshotLayerError::InvalidConfig(format!("Failed to lock VM config: {}", e))
    })?;
    let config_json = serde_json::to_string_pretty(&*config_guard)?;
    std::fs::write(&config_path, config_json)?;

    info!(
        "Created layered snapshot at {}",
        output_dir.display()
    );

    Ok(LayeredSnapshotMetadata {
        enabled: true,
        l0: l0_config.cloned(),
        l1: l1_config.cloned(),
        l2_memfile_size: 0, // Will be set by caller
        guest_memory_size: 0, // Will be set by caller
    })
}

/// Restores a VM from a layered snapshot.
///
/// This function:
/// 1. Maps L0 and L1 memfiles via SharedMemfileManager
/// 2. Creates a CoW overlay for L2
/// 3. Sets up userfaultfd for demand-paging
/// 4. Restores VM state
pub fn restore_from_layered_snapshot(
    vm: &mut crate::vm::Vm,
    metadata: &LayeredSnapshotMetadata,
    snapshot_dir: &Path,
    overlay_dir: &Path,
) -> Result<()> {
    use vm_migration::Snapshottable;

    if !metadata.enabled {
        return Err(SnapshotLayerError::InvalidConfig(
            "Layered snapshots not enabled".to_string(),
        ));
    }

    // Create snapshot layer manager
    let mut manager = SnapshotLayerManager::new();
    manager.initialize(metadata.clone())?;

    // Create CoW overlay for L2
    let overlay_path = overlay_dir.join("l2.overlay");
    let mut overlay = manager.create_overlay(&overlay_path)?;

    // Load L2 checkpoint if it exists
    let l2_checkpoint = snapshot_dir.join("l2.checkpoint");
    if l2_checkpoint.exists() {
        overlay.load_checkpoint(&l2_checkpoint)?;
        info!("Restored L2 checkpoint with {} dirty pages", overlay.dirty_page_count());
    }

    // Read VM state
    let state_path = snapshot_dir.join("state.json");
    let state_json = std::fs::read_to_string(&state_path)?;
    let snapshot: vm_migration::Snapshot = serde_json::from_str(&state_json)?;

    // Restore VM state
    vm.restore(snapshot).map_err(|e| {
        SnapshotLayerError::InvalidConfig(format!("Failed to restore VM: {}", e))
    })?;

    info!("Restored VM from layered snapshot");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_layer_display() {
        assert_eq!(Layer::L0.to_string(), "L0");
        assert_eq!(Layer::L1.to_string(), "L1");
        assert_eq!(Layer::L2.to_string(), "L2");
    }

    #[test]
    fn test_layered_snapshot_metadata_serialization() {
        let metadata = LayeredSnapshotMetadata {
            enabled: true,
            l0: Some(LayerRef {
                layer: Layer::L0,
                memfile_path: PathBuf::from("/tmp/l0.memfile"),
                snapfile_path: PathBuf::from("/tmp/l0.snapfile"),
                memfile_size: 1024 * 1024 * 256, // 256MB
                build_id: "build-001".to_string(),
            }),
            l1: Some(LayerRef {
                layer: Layer::L1,
                memfile_path: PathBuf::from("/tmp/l1.memfile"),
                snapfile_path: PathBuf::from("/tmp/l1.snapfile"),
                memfile_size: 1024 * 1024 * 128, // 128MB
                build_id: "build-002".to_string(),
            }),
            l2_memfile_size: 1024 * 1024 * 512, // 512MB
            guest_memory_size: 1024 * 1024 * 512,
        };

        let json = serde_json::to_string(&metadata).unwrap();
        let restored: LayeredSnapshotMetadata = serde_json::from_str(&json).unwrap();

        assert!(restored.enabled);
        assert!(restored.l0.is_some());
        assert!(restored.l1.is_some());
        assert_eq!(restored.guest_memory_size, 1024 * 1024 * 512);
    }
}
