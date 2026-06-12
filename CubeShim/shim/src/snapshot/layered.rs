// Copyright (c) 2026 Tencent Inc.
// SPDX-License-Identifier: Apache-2.0
//
// Three-layer snapshot orchestration for CubeSandbox.
//
// This module implements the layered snapshot mechanism inspired by My-E2B:
// - L0: Infrastructure (kernel, agent, base system)
// - L1: Runtime (language runtime, frameworks)
// - L2: Per-instance (private state, dirty pages)
//
// The layers enable:
// - Fast VM startup (no need to load full memory upfront)
// - Memory efficiency (shared pages only stored once in page cache)
// - High concurrency (many VMs share L0/L1 memory)

use crate::common::utils::Utils;
use crate::common::CResult;
use crate::hypervisor::config::VmConfig;
use crate::hypervisor::snapshot::{self, SnapshotInfo};

use cube_hypervisor;
use cube_hypervisor::vmm_config;
use cube_hypervisor::ApiRequest;
use cube_hypervisor::SnapshotConfig;
use cube_hypervisor::{NotifyEvent, SnapshotType, VmmInstance};

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::time::Duration;
use std::{fs, thread};

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

/// Orchestrates the creation of layered snapshots.
///
/// This struct manages the two-phase snapshot creation process:
/// 1. Phase 1: Create L0 snapshot (kernel, agent, base system)
/// 2. Phase 2: Create L1 snapshot (runtime, frameworks)
///
/// Each phase boots a VM, waits for it to be ready, then takes a snapshot.
pub struct LayeredSnapshotOrchestrator {
    /// Base path for storing snapshots
    base_path: PathBuf,
    /// Kernel path
    kernel_path: String,
    /// L0 rootfs path
    l0_rootfs: PathBuf,
    /// L1 rootfs path
    l1_rootfs: PathBuf,
    /// VM resource configuration
    vcpus: u32,
    memory_mb: u32,
    /// Snapshot type to use
    snapshot_type: SnapshotType,
}

impl LayeredSnapshotOrchestrator {
    /// Creates a new layered snapshot orchestrator.
    pub fn new(
        base_path: &Path,
        kernel_path: &str,
        l0_rootfs: &Path,
        l1_rootfs: &Path,
        vcpus: u32,
        memory_mb: u32,
        snapshot_type: SnapshotType,
    ) -> Self {
        Self {
            base_path: base_path.to_path_buf(),
            kernel_path: kernel_path.to_string(),
            l0_rootfs: l0_rootfs.to_path_buf(),
            l1_rootfs: l1_rootfs.to_path_buf(),
            vcpus,
            memory_mb,
            snapshot_type,
        }
    }

    /// Creates the layered snapshots (L0 and L1).
    ///
    /// This method:
    /// 1. Creates L0 snapshot by booting a VM with L0 rootfs
    /// 2. Creates L1 snapshot by booting a VM with L1 rootfs
    /// 3. Returns the layered snapshot metadata
    pub fn create_layered_snapshots(&self) -> CResult<LayeredSnapshotMetadata> {
        // Create output directories
        let l0_dir = self.base_path.join("l0");
        let l1_dir = self.base_path.join("l1");
        fs::create_dir_all(&l0_dir).map_err(|e| e.to_string())?;
        fs::create_dir_all(&l1_dir).map_err(|e| e.to_string())?;

        // Phase 1: Create L0 snapshot
        println!("Phase 1: Creating L0 snapshot...");
        let l0_ref = self.create_layer_snapshot(Layer::L0, &l0_dir, &self.l0_rootfs)?;
        println!("L0 snapshot created: {:?}", l0_ref.memfile_path);

        // Phase 2: Create L1 snapshot
        println!("Phase 2: Creating L1 snapshot...");
        let l1_ref = self.create_layer_snapshot(Layer::L1, &l1_dir, &self.l1_rootfs)?;
        println!("L1 snapshot created: {:?}", l1_ref.memfile_path);

        let metadata = LayeredSnapshotMetadata {
            enabled: true,
            l0: Some(l0_ref),
            l1: Some(l1_ref),
            l2_memfile_size: (self.memory_mb as u64) * 1024 * 1024,
            guest_memory_size: (self.memory_mb as u64) * 1024 * 1024,
        };

        // Save metadata
        let metadata_path = self.base_path.join("layered_metadata.json");
        let metadata_json = serde_json::to_string_pretty(&metadata).map_err(|e| e.to_string())?;
        fs::write(&metadata_path, metadata_json).map_err(|e| e.to_string())?;

        println!("Layered snapshot metadata saved to {:?}", metadata_path);

        Ok(metadata)
    }

    /// Creates a single layer snapshot.
    fn create_layer_snapshot(
        &self,
        layer: Layer,
        output_dir: &Path,
        rootfs: &Path,
    ) -> CResult<LayerRef> {
        // Launch VMM
        cube_hypervisor::set_runtime_seccomp_rules(vec![
            (libc::SYS_mkdir, vec![]),
            (libc::SYS_getsockopt, vec![]),
            (libc::SYS_setsockopt, vec![]),
        ]);

        let mut vmm_config = vmm_config::VmmConfig {
            sandbox_id: format!("snapshot-{}", layer),
            ..Default::default()
        };

        let (sender, receiver) = std::sync::mpsc::channel::<NotifyEvent>();
        let notifier = vmm_config::EventNotifyConfig { notifier: sender };
        vmm_config.event_notifier = Some(notifier);

        let ch = VmmInstance::new(vmm_config)
            .map_err(|e| format!("New vmm instance failed: {}", e))?;

        // Create VM config
        let mut vm_config = crate::hypervisor::config::VmConfig::default();
        vm_config
            .set_kernel(self.kernel_path.clone())
            .set_vcpus(self.vcpus)
            .set_memory(self.memory_mb as u64, true);

        // Add rootfs as disk
        let disk = crate::sandbox::disk::Disk {
            path: rootfs.to_str().unwrap().to_string(),
            ..Default::default()
        };
        vm_config.add_disks(&[disk]);

        // Boot VM
        let b_vm_config = Box::new(vm_config.to_vm_config());
        ch.send_request(ApiRequest::VmCreate(b_vm_config))
            .map_err(|e| format!("Create vm failed: {}", e))?
            .map_err(|e| format!("Create vm failed: {}", e))?;

        ch.send_request(ApiRequest::VmBoot)
            .map_err(|e| format!("Boot vm failed: {}", e))?
            .map_err(|e| format!("Boot vm failed: {}", e))?;

        // Wait for VM to be ready
        let ev = receiver.recv_timeout(Duration::from_secs(10));
        if let Err(e) = ev {
            return Err(format!("Wait vm ready err: {}", e).into());
        }
        let ev = ev.unwrap();
        if ev != NotifyEvent::SysStart {
            return Err(format!(
                "Not an expected event, expected: {:?}, actual: {:?}",
                NotifyEvent::SysStart,
                ev
            )
            .into());
        }

        // Wait a bit for the system to stabilize
        thread::sleep(Duration::from_secs(3));

        // Pause VM
        ch.send_request(ApiRequest::VmPause)
            .map_err(|e| format!("Pause vm failed: {}", e))?
            .map_err(|e| format!("Pause vm failed: {}", e))?;

        // Take snapshot
        let snapshot_path = output_dir.join("snapshot");
        fs::create_dir_all(&snapshot_path).map_err(|e| e.to_string())?;

        let config = SnapshotConfig {
            destination_url: format!("file://{}", snapshot_path.to_str().unwrap()),
            snapshot_type: self.snapshot_type,
            ..Default::default()
        };

        ch.send_request(ApiRequest::VmSnapshot(std::sync::Arc::new(config)))
            .map_err(|e| format!("Snapshot vm failed: {}", e))?
            .map_err(|e| format!("Snapshot vm failed: {}", e))?;

        // Get memfile path
        let memfile_path = snapshot_path.join("memory-ranges");
        let memfile_size = if memfile_path.exists() {
            fs::metadata(&memfile_path).map_err(|e| e.to_string())?.len()
        } else {
            0
        };

        // Get snapfile path
        let snapfile_path = snapshot_path.join("state.json");

        // Get build ID (from rootfs modification time or hash)
        let build_id = self.get_build_id(rootfs)?;

        Ok(LayerRef {
            layer,
            memfile_path,
            snapfile_path,
            memfile_size,
            build_id,
        })
    }

    /// Gets a build ID for a rootfs file.
    fn get_build_id(&self, rootfs: &Path) -> CResult<String> {
        let metadata = fs::metadata(rootfs).map_err(|e| e.to_string())?;
        let modified = metadata.modified().map_err(|e| e.to_string())?;
        let timestamp = modified
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        Ok(format!("{}-{}", rootfs.display(), timestamp))
    }
}

/// Creates a layered snapshot using the CLI tool.
///
/// This is the main entry point for the `create-layered-snapshot` command.
pub fn create_layered_snapshot_cli(
    base_path: &str,
    kernel_path: &str,
    l0_rootfs: &str,
    l1_rootfs: &str,
    vcpus: u32,
    memory_mb: u32,
    snapshot_type: SnapshotType,
) -> CResult<()> {
    let orchestrator = LayeredSnapshotOrchestrator::new(
        Path::new(base_path),
        kernel_path,
        Path::new(l0_rootfs),
        Path::new(l1_rootfs),
        vcpus,
        memory_mb,
        snapshot_type,
    );

    let metadata = orchestrator.create_layered_snapshots()?;

    println!("Layered snapshot created successfully:");
    println!("  L0: {:?}", metadata.l0.map(|l| l.memfile_path));
    println!("  L1: {:?}", metadata.l1.map(|l| l.memfile_path));
    println!("  L2 size: {} bytes", metadata.l2_memfile_size);
    println!("  Guest memory: {} bytes", metadata.guest_memory_size);

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
                memfile_size: 1024 * 1024 * 256,
                build_id: "build-001".to_string(),
            }),
            l1: Some(LayerRef {
                layer: Layer::L1,
                memfile_path: PathBuf::from("/tmp/l1.memfile"),
                snapfile_path: PathBuf::from("/tmp/l1.snapfile"),
                memfile_size: 1024 * 1024 * 128,
                build_id: "build-002".to_string(),
            }),
            l2_memfile_size: 1024 * 1024 * 512,
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
