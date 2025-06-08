// Copyright 2020 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Defines state structures for saving/restoring a Firecracker microVM.

use std::fmt::Debug;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::mem::forget;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::{Arc, Mutex};

use semver::Version;
use serde::{Deserialize, Serialize};
use userfaultfd::{FeatureFlags, Uffd, UffdBuilder};
use vmm_sys_util::sock_ctrl_msg::ScmSocket;

#[cfg(target_arch = "aarch64")]
use crate::arch::aarch64::vcpu::get_manufacturer_id_from_host;
use crate::builder::{self, BuildMicrovmFromSnapshotError};
use crate::cpu_config::templates::StaticCpuTemplate;
#[cfg(target_arch = "x86_64")]
use crate::cpu_config::x86_64::cpuid::CpuidTrait;
#[cfg(target_arch = "x86_64")]
use crate::cpu_config::x86_64::cpuid::common::get_vendor_id_from_host;
use crate::device_manager::persist::{ACPIDeviceManagerState, DevicePersistError, DeviceStates};
use crate::logger::{info, warn};
use crate::resources::VmResources;
use crate::seccomp::BpfThreadMap;
use crate::snapshot::Snapshot;
use crate::utils::u64_to_usize;
use crate::vmm_config::boot_source::BootSourceConfig;
use crate::vmm_config::instance_info::InstanceInfo;
use crate::vmm_config::machine_config::{HugePageConfig, MachineConfigError, MachineConfigUpdate};
use crate::vmm_config::snapshot::{CreateSnapshotParams, LoadSnapshotParams, MemBackendType};
use crate::vstate::kvm::KvmState;
use crate::vstate::memory;
use crate::vstate::memory::{GuestMemoryState, GuestRegionMmap, MemoryError};
use crate::vstate::vcpu::{VcpuSendEventError, VcpuState};
use crate::vstate::vm::VmState;
use crate::{EventManager, Vmm, vstate};

/// Holds information related to the VM that is not part of VmState.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct VmInfo {
    /// Guest memory size.
    pub mem_size_mib: u64,
    /// smt information
    pub smt: bool,
    /// CPU template type
    pub cpu_template: StaticCpuTemplate,
    /// Boot source information.
    pub boot_source: BootSourceConfig,
    /// Huge page configuration
    pub huge_pages: HugePageConfig,
}

impl From<&VmResources> for VmInfo {
    fn from(value: &VmResources) -> Self {
        Self {
            mem_size_mib: value.machine_config.mem_size_mib as u64,
            smt: value.machine_config.smt,
            cpu_template: StaticCpuTemplate::from(&value.machine_config.cpu_template),
            boot_source: value.boot_source.config.clone(),
            huge_pages: value.machine_config.huge_pages,
        }
    }
}

/// Contains the necesary state for saving/restoring a microVM.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct MicrovmState {
    /// Miscellaneous VM info.
    pub vm_info: VmInfo,
    /// KVM KVM state.
    pub kvm_state: KvmState,
    /// VM KVM state.
    pub vm_state: VmState,
    /// Vcpu states.
    pub vcpu_states: Vec<VcpuState>,
    /// Device states.
    pub device_states: DeviceStates,
    /// ACPI devices state.
    pub acpi_dev_state: ACPIDeviceManagerState,
}

/// This describes the mapping between Firecracker base virtual address and
/// offset in the buffer or file backend for a guest memory region. It is used
/// to tell an external process/thread where to populate the guest memory data
/// for this range.
///
/// E.g. Guest memory contents for a region of `size` bytes can be found in the
/// backend at `offset` bytes from the beginning, and should be copied/populated
/// into `base_host_address`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct GuestRegionUffdMapping {
    /// Base host virtual address where the guest memory contents for this
    /// region should be copied/populated.
    pub base_host_virt_addr: u64,
    /// Region size.
    pub size: usize,
    /// Offset in the backend file/buffer where the region contents are.
    pub offset: u64,
    /// The configured page size for this memory region.
    pub page_size: usize,
    /// The configured page size **in bytes** for this memory region. The name is
    /// wrong but cannot be changed due to being API, so this field is deprecated,
    /// to be removed in 2.0.
    #[deprecated]
    pub page_size_kib: usize,
}

/// Errors related to saving and restoring Microvm state.
#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum MicrovmStateError {
    /// Operation not allowed: {0}
    NotAllowed(String),
    /// Cannot restore devices: {0}
    RestoreDevices(DevicePersistError),
    /// Cannot save Vcpu state: {0}
    SaveVcpuState(vstate::vcpu::VcpuError),
    /// Cannot save Vm state: {0}
    SaveVmState(vstate::vm::ArchVmError),
    /// Cannot signal Vcpu: {0}
    SignalVcpu(VcpuSendEventError),
    /// Vcpu is in unexpected state.
    UnexpectedVcpuResponse,
}

/// Error definitions for the snapshot creation.
#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum CreateSnapshotError {
    /// Cannot get dirty bitmap: {0}
    DirtyBitmap(#[from] vmm_sys_util::errno::Error),
    /// Cannot write memory file: {0}
    Memory(#[from] MemoryError),
    /// Cannot perform {0} on the memory backing file: {1}
    MemoryBackingFile(&'static str, io::Error),
    /// Cannot save the microVM state: {0}
    MicrovmState(MicrovmStateError),
    /// Cannot serialize the microVM state: {0}
    SerializeMicrovmState(#[from] crate::snapshot::SnapshotError),
    /// Cannot perform {0} on the snapshot backing file: {1}
    SnapshotBackingFile(&'static str, io::Error),
    /// Cannot create free pages bitmap: {0}
    FreePagesbitmap(#[from] crate::snapshot::free_pages::FreePagesError),
}

/// Snapshot version
pub const SNAPSHOT_VERSION: Version = Version::new(7, 0, 0);

/// Creates a Microvm snapshot.
pub fn create_snapshot(
    vmm: &mut Vmm,
    vm_info: &VmInfo,
    params: &CreateSnapshotParams,
) -> Result<(), CreateSnapshotError> {
    let microvm_state = vmm
        .save_state(vm_info)
        .map_err(CreateSnapshotError::MicrovmState)?;

    snapshot_state_to_file(&microvm_state, &params.snapshot_path)?;

    vmm.vm
        .snapshot_memory_to_file(&params.mem_file_path, params.snapshot_type)?;

    // For full snapshots, collect free pages and zero them in memory file for better compression
    if matches!(params.snapshot_type, crate::vmm_config::snapshot::SnapshotType::Full) {
        let free_pages_bitmap = collect_free_pages_for_zeroing(vmm, vm_info)?;
        // Only zero pages if there are actually any free pages to optimize
        if free_pages_bitmap.free_page_count() > 0 {
            zero_free_pages_in_memory_file(&params.mem_file_path, &free_pages_bitmap)?;
        }
    }

    // We need to mark queues as dirty again for all activated devices. The reason we
    // do it here is because we don't mark pages as dirty during runtime
    // for queue objects.
    // SAFETY:
    // This should never fail as we only mark pages only if device has already been activated,
    // and the address validation was already performed on device activation.
    vmm.mmio_device_manager
        .for_each_virtio_device(|_, _, _, dev| {
            let d = dev.lock().unwrap();
            if d.is_activated() {
                d.mark_queue_memory_dirty(vmm.vm.guest_memory())
            } else {
                Ok(())
            }
        })
        .unwrap();

    Ok(())
}

/// Collects free pages information from balloon devices for memory zeroing optimization.
/// This creates a temporary bitmap for zeroing but doesn't save it to a file.
fn collect_free_pages_for_zeroing(
    vmm: &Vmm,
    vm_info: &VmInfo,
) -> Result<crate::snapshot::free_pages::FreePagesbitmap, CreateSnapshotError> {
    use crate::snapshot::free_pages::create_free_pages_bitmap;
    use std::collections::HashSet;

    // Collect inflated pages from all balloon devices
    let mut all_inflated_pages = HashSet::new();

    vmm.mmio_device_manager
        .for_each_virtio_device(|_, _, device_type, dev| {
            if device_type == &crate::devices::virtio::TYPE_BALLOON {
                let d = dev.lock().unwrap();
                if let Some(balloon) = d.as_any().downcast_ref::<crate::devices::virtio::balloon::Balloon>() {
                    let inflated_pages = balloon.inflated_pages();
                    all_inflated_pages.extend(inflated_pages.iter());

                    // Log memory overhead information
                    if let Some(overhead) = balloon.tracking_memory_overhead() {
                        crate::logger::info!(
                            "Balloon device inflated pages tracking overhead: {} KB, {} pages tracked",
                            overhead / 1024,
                            inflated_pages.len()
                        );
                    }
                }
            }
            Ok(())
        })
        .unwrap();

    // Calculate total memory pages (assuming 4KB pages)
    const PAGE_SIZE: u32 = 4096;
    let total_memory_bytes = vm_info.mem_size_mib * 1024 * 1024;
    let total_pages = total_memory_bytes / PAGE_SIZE as u64;

    // Create the bitmap for zeroing (but don't save to file)
    let bitmap = create_free_pages_bitmap(&all_inflated_pages, total_pages, PAGE_SIZE)?;

    crate::logger::info!(
        "Collected free pages for memory zeroing: {} free pages out of {} total pages ({:.2}% free)",
        bitmap.free_page_count(),
        total_pages,
        (bitmap.free_page_count() as f64 / total_pages as f64) * 100.0
    );

    Ok(bitmap)
}

/// Efficiently zeros out free pages in the memory file to improve compression.
/// Uses buffered I/O and batch processing for optimal performance.
fn zero_free_pages_in_memory_file(
    mem_file_path: &std::path::Path,
    bitmap: &crate::snapshot::free_pages::FreePagesbitmap,
) -> Result<(), CreateSnapshotError> {
    use std::fs::OpenOptions;
    use std::io::{BufWriter, Seek, SeekFrom, Write};

    const PAGE_SIZE: u64 = 4096;
    const ZERO_BUFFER_SIZE: usize = 64 * 1024; // 64KB buffer for efficient I/O
    const BATCH_SIZE: usize = ZERO_BUFFER_SIZE / PAGE_SIZE as usize; // 16 pages per batch

    // Open the memory file for read/write
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(mem_file_path)
        .map_err(|err| CreateSnapshotError::MemoryBackingFile("open", err))?;

    let mut writer = BufWriter::new(file);
    let zero_page = vec![0u8; PAGE_SIZE as usize];
    let mut pages_zeroed = 0;
    let mut bytes_zeroed = 0u64;

    // Process free pages in batches for efficiency
    let mut current_batch = Vec::new();

    // Collect all free pages into batches
    for page_num in 0..bitmap.total_pages {
        if bitmap.is_page_free(page_num) {
            current_batch.push(page_num);

            // Process batch when full or at end
            if current_batch.len() >= BATCH_SIZE {
                let batch_result = zero_pages_batch(&mut writer, &current_batch, &zero_page)?;
                pages_zeroed += batch_result.0;
                bytes_zeroed += batch_result.1;
                current_batch.clear();
            }
        }
    }

    // Process remaining pages in the last batch
    if !current_batch.is_empty() {
        let batch_result = zero_pages_batch(&mut writer, &current_batch, &zero_page)?;
        pages_zeroed += batch_result.0;
        bytes_zeroed += batch_result.1;
    }

    // Ensure all writes are flushed
    writer.flush()
        .map_err(|err| CreateSnapshotError::MemoryBackingFile("flush", err))?;

    crate::logger::info!(
        "Zeroed {} free pages ({:.2} MB) in memory file for improved compression",
        pages_zeroed,
        bytes_zeroed as f64 / (1024.0 * 1024.0)
    );

    Ok(())
}

/// Efficiently zeros a batch of pages using optimized I/O operations.
/// Returns (pages_processed, bytes_written).
fn zero_pages_batch(
    writer: &mut BufWriter<std::fs::File>,
    page_numbers: &[u32],
    zero_page: &[u8],
) -> Result<(usize, u64), CreateSnapshotError> {
    const PAGE_SIZE: u64 = 4096;

    // Sort pages to minimize seek operations
    let mut sorted_pages = page_numbers.to_vec();
    sorted_pages.sort_unstable();

    let mut bytes_written = 0u64;
    let mut consecutive_start = None;
    let mut consecutive_count = 0;

    for (i, &page_num) in sorted_pages.iter().enumerate() {
        let page_offset = page_num as u64 * PAGE_SIZE;

        // Check if this page is consecutive with previous ones
        let is_consecutive = consecutive_start
            .map(|start: u64| page_offset == start + (consecutive_count * PAGE_SIZE))
            .unwrap_or(false);

        if is_consecutive {
            consecutive_count += 1;
        } else {
            // Write previous consecutive batch if exists
            if let Some(start_offset) = consecutive_start {
                bytes_written += write_consecutive_zeros(writer, start_offset, consecutive_count, zero_page)?;
            }

            // Start new consecutive batch
            consecutive_start = Some(page_offset);
            consecutive_count = 1;
        }

        // Write final batch if this is the last page
        if i == sorted_pages.len() - 1 {
            if let Some(start_offset) = consecutive_start {
                bytes_written += write_consecutive_zeros(writer, start_offset, consecutive_count, zero_page)?;
            }
        }
    }

    Ok((page_numbers.len(), bytes_written))
}

/// Writes consecutive zero pages efficiently by minimizing seek operations.
fn write_consecutive_zeros(
    writer: &mut BufWriter<std::fs::File>,
    start_offset: u64,
    page_count: u64,
    zero_page: &[u8],
) -> Result<u64, CreateSnapshotError> {
    const PAGE_SIZE: u64 = 4096;

    // Seek to the start position
    writer.get_mut().seek(SeekFrom::Start(start_offset))
        .map_err(|err| CreateSnapshotError::MemoryBackingFile("seek", err))?;

    // Write consecutive zero pages
    let total_bytes = page_count * PAGE_SIZE;
    for _ in 0..page_count {
        writer.write_all(zero_page)
            .map_err(|err| CreateSnapshotError::MemoryBackingFile("write", err))?;
    }

    Ok(total_bytes)
}

fn snapshot_state_to_file(
    microvm_state: &MicrovmState,
    snapshot_path: &Path,
) -> Result<(), CreateSnapshotError> {
    use self::CreateSnapshotError::*;
    let mut snapshot_file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(snapshot_path)
        .map_err(|err| SnapshotBackingFile("open", err))?;

    let snapshot = Snapshot::new(SNAPSHOT_VERSION);
    snapshot.save(&mut snapshot_file, microvm_state)?;
    snapshot_file
        .flush()
        .map_err(|err| SnapshotBackingFile("flush", err))?;
    snapshot_file
        .sync_all()
        .map_err(|err| SnapshotBackingFile("sync_all", err))
}

/// Validates that snapshot CPU vendor matches the host CPU vendor.
///
/// # Errors
///
/// When:
/// - Failed to read host vendor.
/// - Failed to read snapshot vendor.
#[cfg(target_arch = "x86_64")]
pub fn validate_cpu_vendor(microvm_state: &MicrovmState) {
    let host_vendor_id = get_vendor_id_from_host();
    let snapshot_vendor_id = microvm_state.vcpu_states[0].cpuid.vendor_id();
    match (host_vendor_id, snapshot_vendor_id) {
        (Ok(host_id), Some(snapshot_id)) => {
            info!("Host CPU vendor ID: {host_id:?}");
            info!("Snapshot CPU vendor ID: {snapshot_id:?}");
            if host_id != snapshot_id {
                warn!("Host CPU vendor ID differs from the snapshotted one",);
            }
        }
        (Ok(host_id), None) => {
            info!("Host CPU vendor ID: {host_id:?}");
            warn!("Snapshot CPU vendor ID: couldn't get from the snapshot");
        }
        (Err(_), Some(snapshot_id)) => {
            warn!("Host CPU vendor ID: couldn't get from the host");
            info!("Snapshot CPU vendor ID: {snapshot_id:?}");
        }
        (Err(_), None) => {
            warn!("Host CPU vendor ID: couldn't get from the host");
            warn!("Snapshot CPU vendor ID: couldn't get from the snapshot");
        }
    }
}

/// Validate that Snapshot Manufacturer ID matches
/// the one from the Host
///
/// The manufacturer ID for the Snapshot is taken from each VCPU state.
/// # Errors
///
/// When:
/// - Failed to read host vendor.
/// - Failed to read snapshot vendor.
#[cfg(target_arch = "aarch64")]
pub fn validate_cpu_manufacturer_id(microvm_state: &MicrovmState) {
    let host_cpu_id = get_manufacturer_id_from_host();
    let snapshot_cpu_id = microvm_state.vcpu_states[0].regs.manifacturer_id();
    match (host_cpu_id, snapshot_cpu_id) {
        (Ok(host_id), Some(snapshot_id)) => {
            info!("Host CPU manufacturer ID: {host_id:?}");
            info!("Snapshot CPU manufacturer ID: {snapshot_id:?}");
            if host_id != snapshot_id {
                warn!("Host CPU manufacturer ID differs from the snapshotted one",);
            }
        }
        (Ok(host_id), None) => {
            info!("Host CPU manufacturer ID: {host_id:?}");
            warn!("Snapshot CPU manufacturer ID: couldn't get from the snapshot");
        }
        (Err(_), Some(snapshot_id)) => {
            warn!("Host CPU manufacturer ID: couldn't get from the host");
            info!("Snapshot CPU manufacturer ID: {snapshot_id:?}");
        }
        (Err(_), None) => {
            warn!("Host CPU manufacturer ID: couldn't get from the host");
            warn!("Snapshot CPU manufacturer ID: couldn't get from the snapshot");
        }
    }
}
/// Error type for [`snapshot_state_sanity_check`].
#[derive(Debug, thiserror::Error, displaydoc::Display, PartialEq, Eq)]
pub enum SnapShotStateSanityCheckError {
    /// No memory region defined.
    NoMemory,
}

/// Performs sanity checks against the state file and returns specific errors.
pub fn snapshot_state_sanity_check(
    microvm_state: &MicrovmState,
) -> Result<(), SnapShotStateSanityCheckError> {
    // Check if the snapshot contains at least 1 mem region.
    // Upper bound check will be done when creating guest memory by comparing against
    // KVM max supported value kvm_context.max_memslots().
    if microvm_state.vm_state.memory.regions.is_empty() {
        return Err(SnapShotStateSanityCheckError::NoMemory);
    }

    #[cfg(target_arch = "x86_64")]
    validate_cpu_vendor(microvm_state);
    #[cfg(target_arch = "aarch64")]
    validate_cpu_manufacturer_id(microvm_state);

    Ok(())
}

/// Error type for [`restore_from_snapshot`].
#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum RestoreFromSnapshotError {
    /// Failed to get snapshot state from file: {0}
    File(#[from] SnapshotStateFromFileError),
    /// Invalid snapshot state: {0}
    Invalid(#[from] SnapShotStateSanityCheckError),
    /// Failed to load guest memory: {0}
    GuestMemory(#[from] RestoreFromSnapshotGuestMemoryError),
    /// Failed to build microVM from snapshot: {0}
    Build(#[from] BuildMicrovmFromSnapshotError),
}
/// Sub-Error type for [`restore_from_snapshot`] to contain either [`GuestMemoryFromFileError`] or
/// [`GuestMemoryFromUffdError`] within [`RestoreFromSnapshotError`].
#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum RestoreFromSnapshotGuestMemoryError {
    /// Error creating guest memory from file: {0}
    File(#[from] GuestMemoryFromFileError),
    /// Error creating guest memory from uffd: {0}
    Uffd(#[from] GuestMemoryFromUffdError),
}

/// Loads a Microvm snapshot producing a 'paused' Microvm.
pub fn restore_from_snapshot(
    instance_info: &InstanceInfo,
    event_manager: &mut EventManager,
    seccomp_filters: &BpfThreadMap,
    params: &LoadSnapshotParams,
    vm_resources: &mut VmResources,
) -> Result<Arc<Mutex<Vmm>>, RestoreFromSnapshotError> {
    let mut microvm_state = snapshot_state_from_file(&params.snapshot_path)?;
    for entry in &params.network_overrides {
        let net_devices = &mut microvm_state.device_states.net_devices;
        if let Some(device) = net_devices
            .iter_mut()
            .find(|x| x.device_state.id == entry.iface_id)
        {
            device
                .device_state
                .tap_if_name
                .clone_from(&entry.host_dev_name);
        } else {
            return Err(SnapshotStateFromFileError::UnknownNetworkDevice.into());
        }
    }
    let track_dirty_pages = params.enable_diff_snapshots;

    let vcpu_count = microvm_state
        .vcpu_states
        .len()
        .try_into()
        .map_err(|_| MachineConfigError::InvalidVcpuCount)
        .map_err(BuildMicrovmFromSnapshotError::VmUpdateConfig)?;

    vm_resources
        .update_machine_config(&MachineConfigUpdate {
            vcpu_count: Some(vcpu_count),
            mem_size_mib: Some(u64_to_usize(microvm_state.vm_info.mem_size_mib)),
            smt: Some(microvm_state.vm_info.smt),
            cpu_template: Some(microvm_state.vm_info.cpu_template),
            track_dirty_pages: Some(track_dirty_pages),
            huge_pages: Some(microvm_state.vm_info.huge_pages),
            #[cfg(feature = "gdb")]
            gdb_socket_path: None,
        })
        .map_err(BuildMicrovmFromSnapshotError::VmUpdateConfig)?;

    // Some sanity checks before building the microvm.
    snapshot_state_sanity_check(&microvm_state)?;

    let mem_backend_path = &params.mem_backend.backend_path;
    let mem_state = &microvm_state.vm_state.memory;

    let (guest_memory, uffd) = match params.mem_backend.backend_type {
        MemBackendType::File => {
            if vm_resources.machine_config.huge_pages.is_hugetlbfs() {
                return Err(RestoreFromSnapshotGuestMemoryError::File(
                    GuestMemoryFromFileError::HugetlbfsSnapshot,
                )
                .into());
            }
            (
                guest_memory_from_file(mem_backend_path, mem_state, track_dirty_pages)
                    .map_err(RestoreFromSnapshotGuestMemoryError::File)?,
                None,
            )
        }
        MemBackendType::Uffd => guest_memory_from_uffd(
            mem_backend_path,
            mem_state,
            track_dirty_pages,
            vm_resources.machine_config.huge_pages,
        )
        .map_err(RestoreFromSnapshotGuestMemoryError::Uffd)?,
    };
    builder::build_microvm_from_snapshot(
        instance_info,
        event_manager,
        microvm_state,
        guest_memory,
        uffd,
        seccomp_filters,
        vm_resources,
    )
    .map_err(RestoreFromSnapshotError::Build)
}

/// Error type for [`snapshot_state_from_file`]
#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum SnapshotStateFromFileError {
    /// Failed to open snapshot file: {0}
    Open(std::io::Error),
    /// Failed to read snapshot file metadata: {0}
    Meta(std::io::Error),
    /// Failed to load snapshot state from file: {0}
    Load(#[from] crate::snapshot::SnapshotError),
    /// Unknown Network Device.
    UnknownNetworkDevice,
}

fn snapshot_state_from_file(
    snapshot_path: &Path,
) -> Result<MicrovmState, SnapshotStateFromFileError> {
    let snapshot = Snapshot::new(SNAPSHOT_VERSION);
    let mut snapshot_reader =
        File::open(snapshot_path).map_err(SnapshotStateFromFileError::Open)?;
    let metadata = std::fs::metadata(snapshot_path).map_err(SnapshotStateFromFileError::Meta)?;
    let snapshot_len = u64_to_usize(metadata.len());
    let state: MicrovmState = snapshot
        .load_with_version_check(&mut snapshot_reader, snapshot_len)
        .map_err(SnapshotStateFromFileError::Load)?;
    Ok(state)
}

/// Error type for [`guest_memory_from_file`].
#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum GuestMemoryFromFileError {
    /// Failed to load guest memory: {0}
    File(#[from] std::io::Error),
    /// Failed to restore guest memory: {0}
    Restore(#[from] MemoryError),
    /// Cannot restore hugetlbfs backed snapshot by mapping the memory file. Please use uffd.
    HugetlbfsSnapshot,
}

fn guest_memory_from_file(
    mem_file_path: &Path,
    mem_state: &GuestMemoryState,
    track_dirty_pages: bool,
) -> Result<Vec<GuestRegionMmap>, GuestMemoryFromFileError> {
    let mem_file = File::open(mem_file_path)?;
    let guest_mem = memory::snapshot_file(mem_file, mem_state.regions(), track_dirty_pages)?;
    Ok(guest_mem)
}

/// Error type for [`guest_memory_from_uffd`]
#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum GuestMemoryFromUffdError {
    /// Failed to restore guest memory: {0}
    Restore(#[from] MemoryError),
    /// Failed to UFFD object: {0}
    Create(userfaultfd::Error),
    /// Failed to register memory address range with the userfaultfd object: {0}
    Register(userfaultfd::Error),
    /// Failed to connect to UDS Unix stream: {0}
    Connect(#[from] std::io::Error),
    /// Failed to sends file descriptor: {0}
    Send(#[from] vmm_sys_util::errno::Error),
}

fn guest_memory_from_uffd(
    mem_uds_path: &Path,
    mem_state: &GuestMemoryState,
    track_dirty_pages: bool,
    huge_pages: HugePageConfig,
) -> Result<(Vec<GuestRegionMmap>, Option<Uffd>), GuestMemoryFromUffdError> {
    let (guest_memory, backend_mappings) =
        create_guest_memory(mem_state, track_dirty_pages, huge_pages)?;

    let mut uffd_builder = UffdBuilder::new();

    // We only make use of this if balloon devices are present, but we can enable it unconditionally
    // because the only place the kernel checks this is in a hook from madvise, e.g. it doesn't
    // actively change the behavior of UFFD, only passively. Without balloon devices
    // we never call madvise anyway, so no need to put this into a conditional.
    uffd_builder.require_features(FeatureFlags::EVENT_REMOVE);

    let uffd = uffd_builder
        .close_on_exec(true)
        .non_blocking(true)
        .user_mode_only(false)
        .create()
        .map_err(GuestMemoryFromUffdError::Create)?;

    for mem_region in guest_memory.iter() {
        uffd.register(mem_region.as_ptr().cast(), mem_region.size() as _)
            .map_err(GuestMemoryFromUffdError::Register)?;
    }

    send_uffd_handshake(mem_uds_path, &backend_mappings, &uffd)?;

    Ok((guest_memory, Some(uffd)))
}

fn create_guest_memory(
    mem_state: &GuestMemoryState,
    track_dirty_pages: bool,
    huge_pages: HugePageConfig,
) -> Result<(Vec<GuestRegionMmap>, Vec<GuestRegionUffdMapping>), GuestMemoryFromUffdError> {
    let guest_memory = memory::anonymous(mem_state.regions(), track_dirty_pages, huge_pages)?;
    let mut backend_mappings = Vec::with_capacity(guest_memory.len());
    let mut offset = 0;
    for mem_region in guest_memory.iter() {
        #[allow(deprecated)]
        backend_mappings.push(GuestRegionUffdMapping {
            base_host_virt_addr: mem_region.as_ptr() as u64,
            size: mem_region.size(),
            offset,
            page_size: huge_pages.page_size(),
            page_size_kib: huge_pages.page_size(),
        });
        offset += mem_region.size() as u64;
    }

    Ok((guest_memory, backend_mappings))
}

fn send_uffd_handshake(
    mem_uds_path: &Path,
    backend_mappings: &[GuestRegionUffdMapping],
    uffd: &impl AsRawFd,
) -> Result<(), GuestMemoryFromUffdError> {
    // This is safe to unwrap() because we control the contents of the vector
    // (i.e GuestRegionUffdMapping entries).
    let backend_mappings = serde_json::to_string(backend_mappings).unwrap();

    let socket = UnixStream::connect(mem_uds_path)?;
    socket.send_with_fd(
        backend_mappings.as_bytes(),
        // In the happy case we can close the fd since the other process has it open and is
        // using it to serve us pages.
        //
        // The problem is that if other process crashes/exits, firecracker guest memory
        // will simply revert to anon-mem behavior which would lead to silent errors and
        // undefined behavior.
        //
        // To tackle this scenario, the page fault handler can notify Firecracker of any
        // crashes/exits. There is no need for Firecracker to explicitly send its process ID.
        // The external process can obtain Firecracker's PID by calling `getsockopt` with
        // `libc::SO_PEERCRED` option like so:
        //
        // let mut val = libc::ucred { pid: 0, gid: 0, uid: 0 };
        // let mut ucred_size: u32 = mem::size_of::<libc::ucred>() as u32;
        // libc::getsockopt(
        //      socket.as_raw_fd(),
        //      libc::SOL_SOCKET,
        //      libc::SO_PEERCRED,
        //      &mut val as *mut _ as *mut _,
        //      &mut ucred_size as *mut libc::socklen_t,
        // );
        //
        // Per this linux man page: https://man7.org/linux/man-pages/man7/unix.7.html,
        // `SO_PEERCRED` returns the credentials (PID, UID and GID) of the peer process
        // connected to this socket. The returned credentials are those that were in effect
        // at the time of the `connect` call.
        //
        // Moreover, Firecracker holds a copy of the UFFD fd as well, so that even if the
        // page fault handler process does not tear down Firecracker when necessary, the
        // uffd will still be alive but with no one to serve faults, leading to guest freeze.
        uffd.as_raw_fd(),
    )?;

    // We prevent Rust from closing the socket file descriptor to avoid a potential race condition
    // between the mappings message and the connection shutdown. If the latter arrives at the UFFD
    // handler first, the handler never sees the mappings.
    forget(socket);

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixListener;

    use vmm_sys_util::tempfile::TempFile;

    use super::*;
    use crate::Vmm;
    #[cfg(target_arch = "x86_64")]
    use crate::builder::tests::insert_vmgenid_device;
    use crate::builder::tests::{
        CustomBlockConfig, default_kernel_cmdline, default_vmm, insert_balloon_device,
        insert_block_devices, insert_net_device, insert_vsock_device,
    };
    #[cfg(target_arch = "aarch64")]
    use crate::construct_kvm_mpidrs;
    use crate::devices::virtio::block::CacheType;
    use crate::snapshot::Persist;
    use crate::vmm_config::balloon::BalloonDeviceConfig;
    use crate::vmm_config::net::NetworkInterfaceConfig;
    use crate::vmm_config::vsock::tests::default_config;
    use crate::vstate::memory::GuestMemoryRegionState;

    fn default_vmm_with_devices() -> Vmm {
        let mut event_manager = EventManager::new().expect("Cannot create EventManager");
        let mut vmm = default_vmm();
        let mut cmdline = default_kernel_cmdline();

        // Add a balloon device.
        let balloon_config = BalloonDeviceConfig {
            amount_mib: 0,
            deflate_on_oom: false,
            stats_polling_interval_s: 0,
        };
        insert_balloon_device(&mut vmm, &mut cmdline, &mut event_manager, balloon_config);

        // Add a block device.
        let drive_id = String::from("root");
        let block_configs = vec![CustomBlockConfig::new(
            drive_id,
            true,
            None,
            true,
            CacheType::Unsafe,
        )];
        insert_block_devices(&mut vmm, &mut cmdline, &mut event_manager, block_configs);

        // Add net device.
        let network_interface = NetworkInterfaceConfig {
            iface_id: String::from("netif"),
            host_dev_name: String::from("hostname"),
            guest_mac: None,
            rx_rate_limiter: None,
            tx_rate_limiter: None,
        };
        insert_net_device(
            &mut vmm,
            &mut cmdline,
            &mut event_manager,
            network_interface,
        );

        // Add vsock device.
        let mut tmp_sock_file = TempFile::new().unwrap();
        tmp_sock_file.remove().unwrap();
        let vsock_config = default_config(&tmp_sock_file);

        insert_vsock_device(&mut vmm, &mut cmdline, &mut event_manager, vsock_config);

        #[cfg(target_arch = "x86_64")]
        insert_vmgenid_device(&mut vmm);

        vmm
    }

    #[test]
    fn test_microvm_state_snapshot() {
        let vmm = default_vmm_with_devices();
        let states = vmm.mmio_device_manager.save();

        // Only checking that all devices are saved, actual device state
        // is tested by that device's tests.
        assert_eq!(states.block_devices.len(), 1);
        assert_eq!(states.net_devices.len(), 1);
        assert!(states.vsock_device.is_some());
        assert!(states.balloon_device.is_some());

        let vcpu_states = vec![VcpuState::default()];
        #[cfg(target_arch = "aarch64")]
        let mpidrs = construct_kvm_mpidrs(&vcpu_states);
        let microvm_state = MicrovmState {
            device_states: states,
            vcpu_states,
            kvm_state: Default::default(),
            vm_info: VmInfo {
                mem_size_mib: 1u64,
                ..Default::default()
            },
            #[cfg(target_arch = "aarch64")]
            vm_state: vmm.vm.save_state(&mpidrs).unwrap(),
            #[cfg(target_arch = "x86_64")]
            vm_state: vmm.vm.save_state().unwrap(),
            acpi_dev_state: vmm.acpi_device_manager.save(),
        };

        let mut buf = vec![0; 10000];
        Snapshot::serialize(&mut buf.as_mut_slice(), &microvm_state).unwrap();

        let restored_microvm_state: MicrovmState =
            Snapshot::deserialize(&mut buf.as_slice()).unwrap();

        assert_eq!(restored_microvm_state.vm_info, microvm_state.vm_info);
        assert_eq!(
            restored_microvm_state.device_states,
            microvm_state.device_states
        )
    }

    #[test]
    fn test_create_guest_memory() {
        let mem_state = GuestMemoryState {
            regions: vec![GuestMemoryRegionState {
                base_address: 0,
                size: 0x20000,
            }],
        };

        let (_, uffd_regions) =
            create_guest_memory(&mem_state, false, HugePageConfig::None).unwrap();

        assert_eq!(uffd_regions.len(), 1);
        assert_eq!(uffd_regions[0].size, 0x20000);
        assert_eq!(uffd_regions[0].offset, 0);
        assert_eq!(uffd_regions[0].page_size, HugePageConfig::None.page_size());
    }

    #[test]
    fn test_send_uffd_handshake() {
        #[allow(deprecated)]
        let uffd_regions = vec![
            GuestRegionUffdMapping {
                base_host_virt_addr: 0,
                size: 0x100000,
                offset: 0,
                page_size: HugePageConfig::None.page_size(),
                page_size_kib: HugePageConfig::None.page_size(),
            },
            GuestRegionUffdMapping {
                base_host_virt_addr: 0x100000,
                size: 0x200000,
                offset: 0,
                page_size: HugePageConfig::Hugetlbfs2M.page_size(),
                page_size_kib: HugePageConfig::Hugetlbfs2M.page_size(),
            },
        ];

        let uds_path = TempFile::new().unwrap();
        let uds_path = uds_path.as_path();
        std::fs::remove_file(uds_path).unwrap();

        let listener = UnixListener::bind(uds_path).expect("Cannot bind to socket path");

        send_uffd_handshake(uds_path, &uffd_regions, &std::io::stdin()).unwrap();

        let (stream, _) = listener.accept().expect("Cannot listen on UDS socket");

        let mut message_buf = vec![0u8; 1024];
        let (bytes_read, _) = stream
            .recv_with_fd(&mut message_buf[..])
            .expect("Cannot recv_with_fd");
        message_buf.resize(bytes_read, 0);

        let deserialized: Vec<GuestRegionUffdMapping> =
            serde_json::from_slice(&message_buf).unwrap();

        assert_eq!(uffd_regions, deserialized);
    }

    #[test]
    fn test_zero_pages_batch_efficiency() {
        use std::collections::HashSet;
        use std::fs::File;
        use std::io::{BufWriter, Read, Write};
        use tempfile::NamedTempFile;

        // Create a temporary file with test data
        let mut temp_file = NamedTempFile::new().unwrap();
        let test_data = vec![0xAA; 16384]; // 4 pages of 0xAA
        temp_file.write_all(&test_data).unwrap();
        temp_file.flush().unwrap();

        // Create a bitmap with some free pages
        let mut free_pages = HashSet::new();
        free_pages.insert(0); // First page
        free_pages.insert(2); // Third page

        const PAGE_SIZE: u32 = 4096;
        let bitmap = crate::snapshot::free_pages::FreePagesbitmap::new(&free_pages, 4, PAGE_SIZE).unwrap();

        // Zero out the free pages
        zero_free_pages_in_memory_file(temp_file.path(), &bitmap).unwrap();

        // Verify the results
        let mut file = File::open(temp_file.path()).unwrap();
        let mut buffer = vec![0u8; 16384];
        file.read_exact(&mut buffer).unwrap();

        // Page 0 should be zeros
        assert!(buffer[0..4096].iter().all(|&b| b == 0));
        // Page 1 should still be 0xAA
        assert!(buffer[4096..8192].iter().all(|&b| b == 0xAA));
        // Page 2 should be zeros
        assert!(buffer[8192..12288].iter().all(|&b| b == 0));
        // Page 3 should still be 0xAA
        assert!(buffer[12288..16384].iter().all(|&b| b == 0xAA));
    }

    #[test]
    fn test_consecutive_pages_optimization() {
        use std::collections::HashSet;
        use std::fs::File;
        use std::io::{BufWriter, Read, Write};
        use tempfile::NamedTempFile;

        // Create test file
        let mut temp_file = NamedTempFile::new().unwrap();
        let test_data = vec![0xFF; 32768]; // 8 pages of 0xFF
        temp_file.write_all(&test_data).unwrap();

        let file = File::open(temp_file.path()).unwrap();
        let mut writer = BufWriter::new(file);
        let zero_page = vec![0u8; 4096];

        // Test consecutive pages (should be optimized)
        let consecutive_pages = vec![0, 1, 2, 3]; // 4 consecutive pages
        let (pages_processed, bytes_written) = zero_pages_batch(&mut writer, &consecutive_pages, &zero_page).unwrap();

        assert_eq!(pages_processed, 4);
        assert_eq!(bytes_written, 16384); // 4 * 4096

        writer.flush().unwrap();
        drop(writer);

        // Verify all 4 pages are zeroed
        let mut file = File::open(temp_file.path()).unwrap();
        let mut buffer = vec![0u8; 16384];
        file.read_exact(&mut buffer).unwrap();
        assert!(buffer.iter().all(|&b| b == 0));
    }
}
