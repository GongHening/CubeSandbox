// Copyright © 2026 Tencent Corporation
//
// SPDX-License-Identifier: Apache-2.0

//! userfaultfd demand-paging for fast snapshot resume.
//!
//! When resuming from a layered snapshot, guest memory is not fully loaded
//! upfront. Instead, pages are registered with the Linux `userfaultfd`
//! mechanism, and page faults are handled on-demand by the VMM.
//!
//! The handler:
//! 1. Listens on a Unix socket for the uffd file descriptor and memory
//!    region mappings from the hypervisor
//! 2. Polls the uffd fd for `UFFD_EVENT_PAGEFAULT` events
//! 3. For each fault, looks up the page state in the CoW overlay:
//!    - Dirty: page already in overlay, copy via UFFDIO_COPY
//!    - Clean: read from base memfile, copy via UFFDIO_COPY
//! 4. Handles `UFFD_EVENT_REMOVE` for madvise/mremap

use crate::cow_overlay::CoWOverlay;
use crate::shared_memfile::PAGE_SIZE;
use log::{debug, error, info, trace, warn};
use nix::sys::socket::{self, AddressFamily, MsgFlags, SockFlag, SockType, UnixAddr};
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use thiserror::Error;

/// Errors related to userfaultfd operations
#[derive(Debug, Error)]
pub enum UffdError {
    #[error("Failed to create userfaultfd: {0}")]
    CreateFailed(#[source] nix::Error),

    #[error("Failed to register memory region: {0}")]
    RegisterFailed(#[source] nix::Error),

    #[error("Failed to resolve page fault: {0}")]
    PageFaultFailed(#[source] nix::Error),

    #[error("Socket error: {0}")]
    SocketError(#[source] nix::Error),

    #[error("IO error: {0}")]
    IoError(#[source] std::io::Error),

    #[error("Invalid message: {0}")]
    InvalidMessage(String),

    #[error("uffd not initialized")]
    NotInitialized,
}

/// Result type for userfaultfd operations
pub type Result<T> = std::result::Result<T, UffdError>;

// userfaultfd constants
const UFFD_API: u64 = 0xAA;
const UFFD_EVENT_PAGEFAULT: u64 = 18446744073709551612; // 0xFFFFFFFFFFFFFFFC
const UFFD_EVENT_REMOVE: u64 = 18446744073709551611; // 0xFFFFFFFFFFFFFFFB
const UFFDIO_COPY_MODE_WP: u64 = 1;
const UFFDIO_ZEROPAGE_MODE_DONTWAKE: u64 = 1;

// userfaultfd ioctls
const UFFDIO: u64 = 0xAA;
const UFFDIO_API: u64 = 0x03;
const UFFDIO_REGISTER: u64 = 0x00;
const UFFDIO_UNREGISTER: u64 = 0x01;
const UFFDIO_COPY: u64 = 0x03;
const UFFDIO_ZEROPAGE: u64 = 0x04;
const UFFDIO_WRITEPROTECT: u64 = 0x06;

// ioctl encoding macros
const _IOC_NRBITS: u64 = 8;
const _IOC_TYPEBITS: u64 = 8;
const _IOC_SIZEBITS: u64 = 14;
const _IOC_DIRBITS: u64 = 2;
const _IOC_NRSHIFT: u64 = 0;
const _IOC_TYPESHIFT: u64 = _IOC_NRSHIFT + _IOC_NRBITS;
const _IOC_SIZESHIFT: u64 = _IOC_TYPESHIFT + _IOC_TYPEBITS;
const _IOC_DIRSHIFT: u64 = _IOC_SIZESHIFT + _IOC_SIZEBITS;
const _IOC_WRITE: u64 = 1;

fn iow(type_: u64, nr: u64, size: u64) -> u64 {
    (_IOC_WRITE << _IOC_DIRSHIFT) | (type_ << _IOC_TYPESHIFT) | (nr << _IOC_NRSHIFT) | (size << _IOC_SIZESHIFT)
}

/// uffd_msg union (largest member is pagefault)
#[repr(C)]
#[derive(Clone, Copy)]
struct UffdMsg {
    event: u64,
    // pagefault struct
    flags: u64,
    address: u64,
    ptid: u32,
}

/// uffdio_copy struct
#[repr(C)]
#[derive(Clone, Copy)]
struct UffdioCopy {
    dst: u64,
    src: u64,
    len: u64,
    mode: u64,
    copy: i64,
}

/// uffdio_zeropage struct
#[repr(C)]
#[derive(Clone, Copy)]
struct UffdioZeropage {
    start: u64,
    len: u64,
    mode: u64,
    zeropage: i64,
}

/// uffdio_register struct
#[repr(C)]
#[derive(Clone, Copy)]
struct UffdioRegister {
    start: u64,
    len: u64,
    mode: u64,
    ioctls: u64,
}

/// uffdio_api struct
#[repr(C)]
#[derive(Clone, Copy)]
struct UffdioApi {
    api: u64,
    features: u64,
    ioctls: u64,
}

/// Memory region registered with userfaultfd
#[derive(Debug, Clone)]
pub struct UffdRegion {
    /// Start address of the region
    pub start: u64,
    /// Size of the region in bytes
    pub size: u64,
}

/// Statistics for uffd page fault handling
#[derive(Debug, Default)]
pub struct UffdStats {
    pub page_faults: u64,
    pub pages_served: u64,
    pub zero_pages: u64,
    pub remove_events: u64,
}

/// The userfaultfd handler for demand-paging during snapshot resume.
pub struct Userfaultfd {
    /// The uffd file descriptor
    uffd: RawFd,
    /// The CoW overlay for page data
    overlay: Arc<CoWOverlay>,
    /// Memory regions registered with uffd
    regions: Vec<UffdRegion>,
    /// Running flag
    running: Arc<AtomicBool>,
    /// Statistics
    stats: UffdStats,
}

impl Userfaultfd {
    /// Creates a new userfaultfd handler.
    pub fn new(overlay: Arc<CoWOverlay>) -> Result<Self> {
        let uffd = unsafe {
            libc::syscall(libc::SYS_userfaultfd, libc::O_CLOEXEC | libc::O_NONBLOCK)
        };

        if uffd < 0 {
            return Err(UffdError::CreateFailed(nix::errno::Errno::last()));
        }

        let uffd = uffd as RawFd;

        // Enable uffd API
        let api = UffdioApi {
            api: UFFD_API,
            features: 0,
            ioctls: 0,
        };

        let ret = unsafe { libc::ioctl(uffd, iow(UFFDIO, UFFDIO_API, std::mem::size_of::<UffdioApi>() as u64) as _, &api) };
        if ret < 0 {
            unsafe { libc::close(uffd); }
            return Err(UffdError::CreateFailed(nix::errno::Errno::last()));
        }

        info!("Userfaultfd: created with fd={}", uffd);

        Ok(Self {
            uffd,
            overlay,
            regions: Vec::new(),
            running: Arc::new(AtomicBool::new(false)),
            stats: UffdStats::default(),
        })
    }

    /// Returns the uffd file descriptor.
    pub fn fd(&self) -> RawFd {
        self.uffd
    }

    /// Registers a memory region with userfaultfd.
    pub fn register_region(&mut self, start: u64, size: u64) -> Result<()> {
        let reg = UffdioRegister {
            start,
            len: size,
            mode: 1, // UFFDIO_REGISTER_MODE_MISSING
            ioctls: 0,
        };

        let ret = unsafe {
            libc::ioctl(
                self.uffd,
                iow(UFFDIO, UFFDIO_REGISTER, std::mem::size_of::<UffdioRegister>() as u64) as _,
                &reg,
            )
        };

        if ret < 0 {
            return Err(UffdError::RegisterFailed(nix::errno::Errno::last()));
        }

        self.regions.push(UffdRegion { start, size });

        info!(
            "Userfaultfd: registered region 0x{:x} - 0x{:x} ({} bytes)",
            start,
            start + size,
            size
        );

        Ok(())
    }

    /// Starts the page fault handling loop in a background thread.
    ///
    /// Returns a JoinHandle for the background thread.
    pub fn start(self) -> thread::JoinHandle<()> {
        let running = self.running.clone();
        running.store(true, Ordering::SeqCst);

        let uffd = self.uffd;
        let overlay = self.overlay.clone();
        let regions = self.regions.clone();

        thread::spawn(move || {
            let mut stats = UffdStats::default();
            let page_size = PAGE_SIZE;

            info!("Userfaultfd: starting page fault handler");

            while running.load(Ordering::SeqCst) {
                // Poll for events
                let mut pollfds = [libc::pollfd {
                    fd: uffd,
                    events: libc::POLLIN,
                    revents: 0,
                }];

                let ret = unsafe { libc::poll(pollfds.as_mut_ptr(), 1, 100) };

                if ret < 0 {
                    let err = nix::errno::Errno::last();
                    if err == nix::errno::Errno::EINTR {
                        continue;
                    }
                    error!("Userfaultfd: poll error: {}", err);
                    break;
                }

                if ret == 0 {
                    // Timeout, check running flag
                    continue;
                }

                // Read event
                let mut msg = UffdMsg {
                    event: 0,
                    flags: 0,
                    address: 0,
                    ptid: 0,
                };

                let n = unsafe {
                    libc::read(
                        uffd,
                        &mut msg as *mut _ as *mut libc::c_void,
                        std::mem::size_of::<UffdMsg>(),
                    )
                };

                if n < 0 {
                    let err = nix::errno::Errno::last();
                    if err == nix::errno::Errno::EAGAIN {
                        continue;
                    }
                    error!("Userfaultfd: read error: {}", err);
                    break;
                }

                if n == 0 {
                    continue;
                }

                match msg.event {
                    UFFD_EVENT_PAGEFAULT => {
                        stats.page_faults += 1;
                        let addr = msg.address;
                        let page_offset = addr & !(page_size - 1);

                        trace!(
                            "Userfaultfd: page fault at 0x{:x} (page_offset=0x{:x})",
                            addr,
                            page_offset
                        );

                        // Check if the page is in the overlay
                        if let Some(page_data) = overlay.read_page(page_offset) {
                            // Copy page to faulting address
                            let copy = UffdioCopy {
                                dst: addr,
                                src: page_data.as_ptr() as u64,
                                len: page_size,
                                mode: 0,
                                copy: 0,
                            };

                            let ret = unsafe {
                                libc::ioctl(
                                    uffd,
                                    iow(UFFDIO, UFFDIO_COPY, std::mem::size_of::<UffdioCopy>() as u64) as _,
                                    &copy,
                                )
                            };

                            if ret < 0 {
                                error!(
                                    "Userfaultfd: UFFDIO_COPY failed at 0x{:x}: {}",
                                    addr,
                                    nix::errno::Errno::last()
                                );
                            } else {
                                stats.pages_served += 1;
                            }
                        } else {
                            // Page out of bounds, zero-fill
                            let zero = UffdioZeropage {
                                start: page_offset,
                                len: page_size,
                                mode: UFFDIO_ZEROPAGE_MODE_DONTWAKE,
                                zeropage: 0,
                            };

                            let ret = unsafe {
                                libc::ioctl(
                                    uffd,
                                    iow(UFFDIO, UFFDIO_ZEROPAGE, std::mem::size_of::<UffdioZeropage>() as u64) as _,
                                    &zero,
                                )
                            };

                            if ret < 0 {
                                error!(
                                    "Userfaultfd: UFFDIO_ZEROPAGE failed at 0x{:x}: {}",
                                    addr,
                                    nix::errno::Errno::last()
                                );
                            } else {
                                stats.zero_pages += 1;
                            }
                        }
                    }
                    UFFD_EVENT_REMOVE => {
                        stats.remove_events += 1;
                        debug!("Userfaultfd: remove event at 0x{:x}", msg.address);
                    }
                    _ => {
                        warn!("Userfaultfd: unknown event: {}", msg.event);
                    }
                }
            }

            info!(
                "Userfaultfd: handler stopped (faults={}, served={}, zeros={}, removes={})",
                stats.page_faults, stats.pages_served, stats.zero_pages, stats.remove_events
            );
        })
    }

    /// Stops the page fault handling loop.
    pub fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
    }

    /// Returns the current statistics.
    pub fn stats(&self) -> &UffdStats {
        &self.stats
    }
}

impl Drop for Userfaultfd {
    fn drop(&mut self) {
        if self.uffd >= 0 {
            unsafe {
                libc::close(self.uffd);
            }
        }
    }
}

/// Listens on a Unix socket for a uffd file descriptor from the hypervisor.
///
/// This is used when the hypervisor sends the uffd fd after registering
/// guest memory regions.
pub fn listen_for_uffd(socket_path: &Path) -> Result<RawFd> {
    let addr = UnixAddr::new(socket_path).map_err(UffdError::SocketError)?;

    let sock = socket::socket(
        AddressFamily::Unix,
        SockType::Stream,
        SockFlag::SOCK_CLOEXEC,
        None,
    )
    .map_err(UffdError::SocketError)?;

    socket::bind(sock.as_raw_fd(), &addr).map_err(UffdError::SocketError)?;
    socket::listen(&sock, socket::Backlog::new(1).map_err(UffdError::SocketError)?).map_err(UffdError::SocketError)?;

    info!("Uffd listener: waiting for connection on {}", socket_path.display());

    let conn_fd = socket::accept(sock.as_raw_fd()).map_err(UffdError::SocketError)?;

    // Receive the uffd fd via SCM_RIGHTS
    let mut buf = [0u8; 1];
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr() as *mut _,
        iov_len: 1,
    };

    // Receive message with ancillary data
    let mut cmsg_buf = [0u8; 64];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cmsg_buf.as_mut_ptr() as *mut _;
    msg.msg_controllen = cmsg_buf.len();

    let n = unsafe { libc::recvmsg(conn_fd, &mut msg, 0) };
    if n < 0 {
        unsafe { libc::close(conn_fd); }
        unsafe { libc::close(sock.as_raw_fd()); }
        return Err(UffdError::SocketError(nix::errno::Errno::last()));
    }

    // Extract the uffd fd from ancillary data
    let cmsg = unsafe { libc::CMSG_FIRSTHDR(&msg) };
    if cmsg.is_null() {
        unsafe { libc::close(conn_fd); }
        unsafe { libc::close(sock.as_raw_fd()); }
        return Err(UffdError::InvalidMessage("No ancillary data".to_string()));
    }

    let uffd = unsafe {
        let cmsg_data = libc::CMSG_DATA(cmsg) as *const i32;
        *cmsg_data
    };

    unsafe { libc::close(conn_fd); }
    unsafe { libc::close(sock.as_raw_fd()); }

    info!("Uffd listener: received uffd fd={}", uffd);

    Ok(uffd)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_uffd_constants() {
        assert_eq!(UFFD_API, 0xAA);
        assert_eq!(UFFD_EVENT_PAGEFAULT, 0xFFFFFFFFFFFFFFFC_u64);
        assert_eq!(UFFD_EVENT_REMOVE, 0xFFFFFFFFFFFFFFFB_u64);
    }
}
