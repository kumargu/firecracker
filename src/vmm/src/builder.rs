// Copyright 2019 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Enables pre-boot setup, instantiation and booting of a Firecracker VMM.

use std::convert::TryInto;
use std::fmt::{Display, Formatter};
use std::fs::OpenOptions;
use std::io::{self, Read, Seek, SeekFrom};
use std::os::unix::io::{AsRawFd, RawFd};
use std::sync::{Arc, Mutex};

use super::{Error, Vmm};

use arch::InitrdConfig;
#[cfg(target_arch = "x86_64")]
use device_manager::legacy::PortIODeviceManager;
use device_manager::mmio::MMIODeviceManager;
use devices::legacy::Serial;
use devices::virtio::MmioTransport;
use polly::event_manager::{Error as EventManagerError, EventManager};
use seccomp::BpfProgramRef;
use utils::eventfd::EventFd;
use utils::terminal::Terminal;
use utils::time::TimestampUs;
use vm_memory::{Bytes, GuestAddress, GuestMemoryError, GuestMemoryMmap};
use vmm_config;
use vmm_config::boot_source::BootConfig;
use vmm_config::drive::BlockDeviceConfigs;
use vmm_config::net::NetworkInterfaceConfigs;
use vmm_config::vsock::VsockDeviceConfig;
use vstate::{KvmContext, Vcpu, VcpuConfig, Vm};
use {device_manager, VmmEventsObserver};

/// Errors associated with starting the instance.
#[derive(Debug)]
pub enum StartMicrovmError {
    /// Unable to seek the block device backing file due to invalid permissions or
    /// the file was deleted/corrupted.
    CreateBlockDevice(io::Error),
    /// Internal errors are due to resource exhaustion.
    CreateNetDevice(devices::virtio::net::Error),
    /// Failed to create a `RateLimiter` object.
    CreateRateLimiter(io::Error),
    /// Failed to create the backend for the vsock device.
    CreateVsockBackend(devices::virtio::vsock::VsockUnixBackendError),
    /// Failed to create the vsock device.
    CreateVsockDevice(devices::virtio::vsock::VsockError),
    /// Memory regions are overlapping or mmap fails.
    GuestMemoryMmap(GuestMemoryError),
    /// Cannot load initrd due to an invalid memory configuration.
    InitrdLoad,
    /// Cannot load initrd due to an invalid image.
    InitrdRead(io::Error),
    /// Internal error encountered while starting a microVM.
    Internal(Error),
    /// The kernel command line is invalid.
    KernelCmdline(String),
    /// Cannot load kernel due to invalid memory configuration or invalid kernel image.
    KernelLoader(kernel::loader::Error),
    /// Cannot load command line string.
    LoadCommandline(kernel::cmdline::Error),
    /// The start command was issued more than once.
    MicroVMAlreadyRunning,
    /// Cannot start the VM because the kernel was not configured.
    MissingKernelConfig,
    /// The net device configuration is missing the tap device.
    NetDeviceNotConfigured,
    /// Cannot open the block device backing file.
    OpenBlockDevice(io::Error),
    /// Cannot initialize a MMIO Block Device or add a device to the MMIO Bus.
    RegisterBlockDevice(device_manager::mmio::Error),
    /// Cannot register an EventHandler.
    RegisterEvent(EventManagerError),
    /// Cannot initialize a MMIO Network Device or add a device to the MMIO Bus.
    RegisterNetDevice(device_manager::mmio::Error),
    /// Cannot initialize a MMIO Vsock Device or add a device to the MMIO Bus.
    RegisterVsockDevice(device_manager::mmio::Error),
}

/// It's convenient to automatically convert `kernel::cmdline::Error`s
/// to `StartMicrovmError`s.
impl std::convert::From<kernel::cmdline::Error> for StartMicrovmError {
    fn from(e: kernel::cmdline::Error) -> StartMicrovmError {
        StartMicrovmError::KernelCmdline(e.to_string())
    }
}

impl Display for StartMicrovmError {
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        use self::StartMicrovmError::*;
        match *self {
            CreateBlockDevice(ref err) => write!(
                f,
                "Unable to seek the block device backing file due to invalid permissions or \
                 the file was deleted/corrupted. Error number: {}",
                err
            ),
            CreateRateLimiter(ref err) => write!(f, "Cannot create RateLimiter: {}", err),
            CreateVsockBackend(ref err) => {
                write!(f, "Cannot create backend for vsock device: {:?}", err)
            }
            CreateVsockDevice(ref err) => write!(f, "Cannot create vsock device: {:?}", err),
            CreateNetDevice(ref err) => {
                let mut err_msg = format!("{:?}", err);
                err_msg = err_msg.replace("\"", "");

                write!(f, "Cannot create network device. {}", err_msg)
            }
            GuestMemoryMmap(ref err) => {
                // Remove imbricated quotes from error message.
                let mut err_msg = format!("{:?}", err);
                err_msg = err_msg.replace("\"", "");
                write!(f, "Invalid Memory Configuration: {}", err_msg)
            }
            InitrdLoad => write!(
                f,
                "Cannot load initrd due to an invalid memory configuration."
            ),
            InitrdRead(ref err) => write!(f, "Cannot load initrd due to an invalid image: {}", err),
            Internal(ref err) => write!(f, "Internal error while starting microVM: {:?}", err),
            KernelCmdline(ref err) => write!(f, "Invalid kernel command line: {}", err),
            KernelLoader(ref err) => {
                let mut err_msg = format!("{}", err);
                err_msg = err_msg.replace("\"", "");
                write!(
                    f,
                    "Cannot load kernel due to invalid memory configuration or invalid kernel \
                     image. {}",
                    err_msg
                )
            }
            LoadCommandline(ref err) => {
                let mut err_msg = format!("{}", err);
                err_msg = err_msg.replace("\"", "");
                write!(f, "Cannot load command line string. {}", err_msg)
            }
            MicroVMAlreadyRunning => write!(f, "Microvm already running."),
            MissingKernelConfig => write!(f, "Cannot start microvm without kernel configuration."),
            NetDeviceNotConfigured => {
                write!(f, "The net device configuration is missing the tap device.")
            }
            OpenBlockDevice(ref err) => {
                let mut err_msg = format!("{:?}", err);
                err_msg = err_msg.replace("\"", "");

                write!(f, "Cannot open the block device backing file. {}", err_msg)
            }
            RegisterBlockDevice(ref err) => {
                let mut err_msg = format!("{}", err);
                err_msg = err_msg.replace("\"", "");
                write!(
                    f,
                    "Cannot initialize a MMIO Block Device or add a device to the MMIO Bus. {}",
                    err_msg
                )
            }
            RegisterEvent(ref err) => write!(f, "Cannot register EventHandler. {:?}", err),
            RegisterNetDevice(ref err) => {
                let mut err_msg = format!("{}", err);
                err_msg = err_msg.replace("\"", "");

                write!(
                    f,
                    "Cannot initialize a MMIO Network Device or add a device to the MMIO Bus. {}",
                    err_msg
                )
            }
            RegisterVsockDevice(ref err) => {
                let mut err_msg = format!("{}", err);
                err_msg = err_msg.replace("\"", "");

                write!(
                    f,
                    "Cannot initialize a MMIO Vsock Device or add a device to the MMIO Bus. {}",
                    err_msg
                )
            }
        }
    }
}

// Wrapper over io::Stdin that implements `Serial::ReadableFd` and `vmm::VmmEventsObserver`.
struct SerialStdin(io::Stdin);
impl SerialStdin {
    /// Returns a `SerialStdin` wrapper over `io::stdin`.
    pub fn get() -> Self {
        SerialStdin(io::stdin())
    }
}
impl io::Read for SerialStdin {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf)
    }
}
impl AsRawFd for SerialStdin {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}
impl devices::legacy::ReadableFd for SerialStdin {}
impl VmmEventsObserver for SerialStdin {
    fn on_vmm_boot(&mut self) -> std::result::Result<(), utils::errno::Error> {
        // Set raw mode for stdin.
        self.0.lock().set_raw_mode().map_err(|e| {
            warn!("Cannot set raw mode for the terminal. {:?}", e);
            e
        })
    }
    fn on_vmm_stop(&mut self) -> std::result::Result<(), utils::errno::Error> {
        self.0.lock().set_canon_mode().map_err(|e| {
            warn!("Cannot set canonical mode for the terminal. {:?}", e);
            e
        })
    }
}

/// Builds and starts a microVM based on the current Firecracker VmResources configuration.
///
/// This is the default build recipe, one could build other microVM flavors by using the
/// independent functions in this module instead of calling this recipe.
///
/// An `Arc` reference of the built `Vmm` is also plugged in the `EventManager`, while another
/// is returned.
pub fn build_microvm(
    vm_resources: &super::resources::VmResources,
    event_manager: &mut EventManager,
    seccomp_filter: BpfProgramRef,
) -> std::result::Result<Arc<Mutex<Vmm>>, StartMicrovmError> {
    let boot_config = vm_resources
        .boot_source()
        .ok_or(StartMicrovmError::MissingKernelConfig)?;

    // Timestamp for measuring microVM boot duration.
    let request_ts = TimestampUs::default();

    let guest_memory = create_guest_memory(vm_resources.vm_config().mem_size_mib.ok_or(
        StartMicrovmError::GuestMemoryMmap(vm_memory::GuestMemoryError::MemoryNotInitialized),
    )?)?;
    let vcpu_config = vm_resources.vcpu_config();
    let entry_addr = load_kernel(boot_config, &guest_memory)?;
    let initrd = load_initrd_from_config(boot_config, &guest_memory)?;
    // Clone the command-line so that a failed boot doesn't pollute the original.
    #[allow(unused_mut)]
    let mut kernel_cmdline = boot_config.cmdline.clone();
    let mut vm = setup_kvm_vm(&guest_memory)?;

    // On x86_64 always create a serial device,
    // while on aarch64 only create it if 'console=' is specified in the boot args.
    let serial_device = if cfg!(target_arch = "x86_64")
        || (cfg!(target_arch = "aarch64") && kernel_cmdline.as_str().contains("console="))
    {
        Some(setup_serial_device(
            event_manager,
            Box::new(SerialStdin::get()),
            Box::new(io::stdout()),
        )?)
    } else {
        None
    };

    let exit_evt = EventFd::new(libc::EFD_NONBLOCK)
        .map_err(Error::EventFd)
        .map_err(StartMicrovmError::Internal)?;

    #[cfg(target_arch = "x86_64")]
    // Safe to unwrap 'serial_device' as it's always 'Some' on x86_64.
    // x86_64 uses the i8042 reset event as the Vmm exit event.
    let mut pio_device_manager = PortIODeviceManager::new(
        serial_device.unwrap(),
        exit_evt
            .try_clone()
            .map_err(Error::EventFd)
            .map_err(StartMicrovmError::Internal)?,
    )
    .map_err(Error::CreateLegacyDevice)
    .map_err(StartMicrovmError::Internal)?;

    // Instantiate the MMIO device manager.
    // 'mmio_base' address has to be an address which is protected by the kernel
    // and is architectural specific.
    #[allow(unused_mut)]
    let mut mmio_device_manager = MMIODeviceManager::new(
        &mut (arch::MMIO_MEM_START as u64),
        (arch::IRQ_BASE, arch::IRQ_MAX),
    );

    let vcpus;
    // For x86_64 we need to create the interrupt controller before calling `KVM_CREATE_VCPUS`
    // while on aarch64 we need to do it the other way around.
    #[cfg(target_arch = "x86_64")]
    {
        setup_interrupt_controller(&mut vm)?;
        attach_legacy_devices(&vm, &mut pio_device_manager)?;

        vcpus = create_vcpus_x86_64(
            &vm,
            &vcpu_config,
            &guest_memory,
            entry_addr,
            request_ts,
            &pio_device_manager.io_bus,
            &exit_evt,
        )
        .map_err(StartMicrovmError::Internal)?;
    }

    // On aarch64, the vCPUs need to be created (i.e call KVM_CREATE_VCPU) and configured before
    // setting up the IRQ chip because the `KVM_CREATE_VCPU` ioctl will return error if the IRQCHIP
    // was already initialized.
    // Search for `kvm_arch_vcpu_create` in arch/arm/kvm/arm.c.
    #[cfg(target_arch = "aarch64")]
    {
        vcpus = create_vcpus_aarch64(
            &vm,
            &vcpu_config,
            &guest_memory,
            entry_addr,
            request_ts,
            &exit_evt,
        )
        .map_err(StartMicrovmError::Internal)?;

        setup_interrupt_controller(&mut vm, vcpu_config.vcpu_count)?;
        attach_legacy_devices(
            &vm,
            &mut mmio_device_manager,
            &mut kernel_cmdline,
            serial_device,
        )?;
    }

    let mut vmm = Vmm {
        events_observer: Some(Box::new(SerialStdin::get())),
        guest_memory,
        kernel_cmdline,
        vcpus_handles: Vec::new(),
        exit_evt,
        vm,
        mmio_device_manager,
        #[cfg(target_arch = "x86_64")]
        pio_device_manager,
    };

    attach_block_devices(&mut vmm, &vm_resources.block, event_manager)?;
    attach_net_devices(&mut vmm, &vm_resources.network_interface, event_manager)?;
    if let Some(vsock) = vm_resources.vsock.as_ref() {
        attach_vsock_device(&mut vmm, vsock, event_manager)?;
    }

    // Write the kernel command line to guest memory. This is x86_64 specific, since on
    // aarch64 the command line will be specified through the FDT.
    #[cfg(target_arch = "x86_64")]
    load_cmdline(&vmm)?;

    vmm.configure_system(vcpus.as_slice(), &initrd)
        .map_err(StartMicrovmError::Internal)?;
    // Firecracker uses the same seccomp filter for all threads.
    vmm.start_vcpus(vcpus, seccomp_filter.to_vec(), seccomp_filter)
        .map_err(StartMicrovmError::Internal)?;

    let vmm = Arc::new(Mutex::new(vmm));
    event_manager
        .add_subscriber(vmm.clone())
        .map_err(StartMicrovmError::RegisterEvent)?;

    Ok(vmm)
}

/// Creates GuestMemory of `mem_size_mib` MiB in size.
pub fn create_guest_memory(
    mem_size_mib: usize,
) -> std::result::Result<GuestMemoryMmap, StartMicrovmError> {
    let mem_size = mem_size_mib << 20;
    let arch_mem_regions = arch::arch_memory_regions(mem_size);

    Ok(GuestMemoryMmap::from_ranges(&arch_mem_regions)
        .map_err(StartMicrovmError::GuestMemoryMmap)?)
}

fn load_kernel(
    boot_config: &BootConfig,
    guest_memory: &GuestMemoryMmap,
) -> std::result::Result<GuestAddress, StartMicrovmError> {
    let mut kernel_file = boot_config
        .kernel_file
        .try_clone()
        .map_err(|e| StartMicrovmError::Internal(Error::KernelFile(e)))?;

    let entry_addr =
        kernel::loader::load_kernel(guest_memory, &mut kernel_file, arch::get_kernel_start())
            .map_err(StartMicrovmError::KernelLoader)?;

    Ok(entry_addr)
}

fn load_initrd_from_config(
    boot_cfg: &BootConfig,
    vm_memory: &GuestMemoryMmap,
) -> std::result::Result<Option<InitrdConfig>, StartMicrovmError> {
    use self::StartMicrovmError::InitrdRead;

    Ok(match &boot_cfg.initrd_file {
        Some(f) => Some(load_initrd(
            vm_memory,
            &mut f.try_clone().map_err(InitrdRead)?,
        )?),
        None => None,
    })
}

/// Loads the initrd from a file into the given memory slice.
///
/// * `vm_memory` - The guest memory the initrd is written to.
/// * `image` - The initrd image.
///
/// Returns the result of initrd loading
fn load_initrd<F>(
    vm_memory: &GuestMemoryMmap,
    image: &mut F,
) -> std::result::Result<InitrdConfig, StartMicrovmError>
where
    F: Read + Seek,
{
    use self::StartMicrovmError::{InitrdLoad, InitrdRead};

    let size: usize;
    // Get the image size
    match image.seek(SeekFrom::End(0)) {
        Err(e) => return Err(InitrdRead(e)),
        Ok(0) => {
            return Err(InitrdRead(io::Error::new(
                io::ErrorKind::InvalidData,
                "Initrd image seek returned a size of zero",
            )))
        }
        Ok(s) => size = s as usize,
    };
    // Go back to the image start
    image.seek(SeekFrom::Start(0)).map_err(InitrdRead)?;

    // Get the target address
    let address = arch::initrd_load_addr(vm_memory, size).map_err(|_| InitrdLoad)?;

    // Load the image into memory
    vm_memory
        .read_from(GuestAddress(address), image, size)
        .map_err(|_| InitrdLoad)?;

    Ok(InitrdConfig {
        address: GuestAddress(address),
        size,
    })
}

#[cfg(target_arch = "x86_64")]
fn load_cmdline(vmm: &Vmm) -> std::result::Result<(), StartMicrovmError> {
    kernel::loader::load_cmdline(
        vmm.guest_memory(),
        GuestAddress(arch::x86_64::layout::CMDLINE_START),
        &vmm.kernel_cmdline
            .as_cstring()
            .map_err(StartMicrovmError::LoadCommandline)?,
    )
    .map_err(StartMicrovmError::LoadCommandline)
}

pub(crate) fn setup_kvm_vm(
    guest_memory: &GuestMemoryMmap,
) -> std::result::Result<Vm, StartMicrovmError> {
    let kvm = KvmContext::new()
        .map_err(Error::KvmContext)
        .map_err(StartMicrovmError::Internal)?;
    let mut vm = Vm::new(kvm.fd())
        .map_err(Error::Vm)
        .map_err(StartMicrovmError::Internal)?;
    vm.memory_init(&guest_memory, kvm.max_memslots())
        .map_err(Error::Vm)
        .map_err(StartMicrovmError::Internal)?;
    Ok(vm)
}

/// Sets up the irqchip for a x86_64 microVM.
#[cfg(target_arch = "x86_64")]
pub fn setup_interrupt_controller(vm: &mut Vm) -> std::result::Result<(), StartMicrovmError> {
    vm.setup_irqchip()
        .map_err(Error::Vm)
        .map_err(StartMicrovmError::Internal)
}

/// Sets up the irqchip for a aarch64 microVM.
#[cfg(target_arch = "aarch64")]
pub fn setup_interrupt_controller(
    vm: &mut Vm,
    vcpu_count: u8,
) -> std::result::Result<(), StartMicrovmError> {
    vm.setup_irqchip(vcpu_count)
        .map_err(Error::Vm)
        .map_err(StartMicrovmError::Internal)
}

/// Sets up the serial device.
pub fn setup_serial_device(
    event_manager: &mut EventManager,
    input: Box<dyn devices::legacy::ReadableFd + Send>,
    out: Box<dyn io::Write + Send>,
) -> std::result::Result<Arc<Mutex<Serial>>, StartMicrovmError> {
    let interrupt_evt = EventFd::new(libc::EFD_NONBLOCK)
        .map_err(Error::EventFd)
        .map_err(StartMicrovmError::Internal)?;
    let serial = Arc::new(Mutex::new(Serial::new_in_out(interrupt_evt, input, out)));
    if let Err(e) = event_manager.add_subscriber(serial.clone()) {
        // TODO: We just log this message, and immediately return Ok, instead of returning the
        // actual error because this operation always fails with EPERM when adding a fd which
        // has been redirected to /dev/null via dup2 (this may happen inside the jailer).
        // Find a better solution to this (and think about the state of the serial device
        // while we're at it). This also led to commenting out parts of the
        // enable_disable_stdin_test() unit test function.
        warn!("Could not add serial input event to epoll: {:?}", e);
    }
    Ok(serial)
}

#[cfg(target_arch = "x86_64")]
fn attach_legacy_devices(
    vm: &Vm,
    pio_device_manager: &mut PortIODeviceManager,
) -> std::result::Result<(), StartMicrovmError> {
    pio_device_manager
        .register_devices()
        .map_err(Error::LegacyIOBus)
        .map_err(StartMicrovmError::Internal)?;

    macro_rules! register_irqfd_evt {
        ($evt: ident, $index: expr) => {{
            vm.fd()
                .register_irqfd(&pio_device_manager.$evt, $index)
                .map_err(|e| {
                    Error::LegacyIOBus(device_manager::legacy::Error::EventFd(
                        io::Error::from_raw_os_error(e.errno()),
                    ))
                })
                .map_err(StartMicrovmError::Internal)?;
        }};
    }

    register_irqfd_evt!(com_evt_1_3, 4);
    register_irqfd_evt!(com_evt_2_4, 3);
    register_irqfd_evt!(kbd_evt, 1);
    Ok(())
}

#[cfg(target_arch = "aarch64")]
fn attach_legacy_devices(
    vm: &Vm,
    mmio_device_manager: &mut MMIODeviceManager,
    kernel_cmdline: &mut kernel::cmdline::Cmdline,
    serial: Option<Arc<Mutex<Serial>>>,
) -> std::result::Result<(), StartMicrovmError> {
    if let Some(serial) = serial {
        mmio_device_manager
            .register_mmio_serial(vm.fd(), kernel_cmdline, serial)
            .map_err(Error::RegisterMMIODevice)
            .map_err(StartMicrovmError::Internal)?;
    }

    mmio_device_manager
        .register_mmio_rtc(vm.fd())
        .map_err(Error::RegisterMMIODevice)
        .map_err(StartMicrovmError::Internal)?;

    Ok(())
}

#[cfg(target_arch = "x86_64")]
fn create_vcpus_x86_64(
    vm: &Vm,
    vcpu_config: &VcpuConfig,
    guest_mem: &GuestMemoryMmap,
    entry_addr: GuestAddress,
    request_ts: TimestampUs,
    io_bus: &devices::Bus,
    exit_evt: &EventFd,
) -> super::Result<Vec<Vcpu>> {
    let mut vcpus = Vec::with_capacity(vcpu_config.vcpu_count as usize);
    for cpu_index in 0..vcpu_config.vcpu_count {
        let mut vcpu = Vcpu::new_x86_64(
            cpu_index,
            vm.fd(),
            vm.supported_cpuid().clone(),
            vm.supported_msrs().clone(),
            io_bus.clone(),
            exit_evt.try_clone().map_err(Error::EventFd)?,
            request_ts.clone(),
        )
        .map_err(Error::Vcpu)?;

        vcpu.configure_x86_64(guest_mem, entry_addr, vcpu_config)
            .map_err(Error::Vcpu)?;

        vcpus.push(vcpu);
    }
    Ok(vcpus)
}

#[cfg(target_arch = "aarch64")]
fn create_vcpus_aarch64(
    vm: &Vm,
    vcpu_config: &VcpuConfig,
    guest_mem: &GuestMemoryMmap,
    entry_addr: GuestAddress,
    request_ts: TimestampUs,
    exit_evt: &EventFd,
) -> super::Result<Vec<Vcpu>> {
    let mut vcpus = Vec::with_capacity(vcpu_config.vcpu_count as usize);
    for cpu_index in 0..vcpu_config.vcpu_count {
        let mut vcpu = Vcpu::new_aarch64(
            cpu_index,
            vm.fd(),
            exit_evt.try_clone().map_err(Error::EventFd)?,
            request_ts.clone(),
        )
        .map_err(Error::Vcpu)?;

        vcpu.configure_aarch64(vm.fd(), guest_mem, entry_addr)
            .map_err(Error::Vcpu)?;

        vcpus.push(vcpu);
    }
    Ok(vcpus)
}

/// Attaches an MmioTransport device to the device manager.
fn attach_mmio_device(
    vmm: &mut Vmm,
    id: String,
    device: MmioTransport,
) -> std::result::Result<(), device_manager::mmio::Error> {
    let type_id = device
        .device()
        .lock()
        .expect("Poisoned device lock")
        .device_type();
    let cmdline = &mut vmm.kernel_cmdline;

    vmm.mmio_device_manager.register_mmio_device(
        vmm.vm.fd(),
        device,
        cmdline,
        type_id,
        id.as_str(),
    )?;

    Ok(())
}

/// Secondary path for attaching devices to the Bus and EventManager.
/// TODO: Remove this and have a single generic path for all devices.
/// Adds a virtio device to the MmioDeviceManager using the specified transport.
fn attach_block_device(
    vmm: &mut Vmm,
    id: String,
    transport_device: MmioTransport,
    block_device: Arc<Mutex<devices::virtio::Block>>,
) -> std::result::Result<(), StartMicrovmError> {
    let cmdline = &mut vmm.kernel_cmdline;

    vmm.mmio_device_manager
        .register_block_device(
            vmm.vm.fd(),
            transport_device,
            block_device,
            cmdline,
            id.as_str(),
        )
        .map_err(StartMicrovmError::RegisterBlockDevice)?;

    Ok(())
}

fn attach_block_devices(
    vmm: &mut Vmm,
    blocks: &BlockDeviceConfigs,
    event_manager: &mut EventManager,
) -> std::result::Result<(), StartMicrovmError> {
    use self::StartMicrovmError::*;

    // If no PARTUUID was specified for the root device, try with the /dev/vda.
    if blocks.has_root_block_device() && !blocks.has_partuuid_root() {
        let kernel_cmdline = &mut vmm.kernel_cmdline;

        kernel_cmdline.insert_str("root=/dev/vda")?;

        let flags = if blocks.has_read_only_root() {
            "ro"
        } else {
            "rw"
        };

        kernel_cmdline.insert_str(flags)?;
    }

    for drive_config in blocks.config_list.iter() {
        // Add the block device from file.
        let block_file = OpenOptions::new()
            .read(true)
            .write(!drive_config.is_read_only)
            .open(&drive_config.path_on_host)
            .map_err(OpenBlockDevice)?;

        if drive_config.is_root_device && drive_config.get_partuuid().is_some() {
            let kernel_cmdline = &mut vmm.kernel_cmdline;

            kernel_cmdline.insert_str(format!(
                "root=PARTUUID={}",
                //The unwrap is safe as we are firstly checking that partuuid is_some().
                drive_config.get_partuuid().unwrap()
            ))?;

            let flags = if drive_config.is_read_only() {
                "ro"
            } else {
                "rw"
            };

            kernel_cmdline.insert_str(flags)?;
        }

        let rate_limiter = drive_config
            .rate_limiter
            .map(vmm_config::RateLimiterConfig::try_into)
            .transpose()
            .map_err(CreateRateLimiter)?;

        let block_device = Arc::new(Mutex::new(
            devices::virtio::Block::new(
                vmm.guest_memory.clone(),
                block_file,
                drive_config.is_read_only,
                rate_limiter.unwrap_or_default(),
            )
            .map_err(CreateBlockDevice)?,
        ));

        event_manager
            .add_subscriber(block_device.clone())
            .map_err(StartMicrovmError::RegisterEvent)?;

        attach_block_device(
            vmm,
            drive_config.drive_id.clone(),
            MmioTransport::new(vmm.guest_memory().clone(), block_device.clone())
                .map_err(CreateBlockDevice)?,
            block_device,
        )?;
    }

    Ok(())
}

fn attach_net_devices(
    vmm: &mut Vmm,
    network_ifaces: &NetworkInterfaceConfigs,
    event_manager: &mut EventManager,
) -> std::result::Result<(), StartMicrovmError> {
    use self::StartMicrovmError::*;

    for cfg in network_ifaces.iter() {
        let allow_mmds_requests = cfg.allow_mmds_requests();

        let rx_rate_limiter = cfg
            .rx_rate_limiter
            .map(vmm_config::RateLimiterConfig::try_into)
            .transpose()
            .map_err(CreateRateLimiter)?;

        let tx_rate_limiter = cfg
            .tx_rate_limiter
            .map(vmm_config::RateLimiterConfig::try_into)
            .transpose()
            .map_err(CreateRateLimiter)?;

        let tap = cfg.open_tap().map_err(|_| NetDeviceNotConfigured)?;
        let net_device = Arc::new(Mutex::new(
            devices::virtio::net::Net::new_with_tap(
                tap,
                cfg.guest_mac(),
                vmm.guest_memory().clone(),
                rx_rate_limiter.unwrap_or_default(),
                tx_rate_limiter.unwrap_or_default(),
                allow_mmds_requests,
            )
            .map_err(CreateNetDevice)?,
        ));
        event_manager
            .add_subscriber(net_device.clone())
            .map_err(StartMicrovmError::RegisterEvent)?;

        attach_mmio_device(
            vmm,
            cfg.iface_id.clone(),
            MmioTransport::new(vmm.guest_memory().clone(), net_device).map_err(|e| {
                RegisterNetDevice(super::device_manager::mmio::Error::CreateMmioDevice(e))
            })?,
        )
        .map_err(RegisterNetDevice)?;
    }

    Ok(())
}

fn attach_vsock_device(
    vmm: &mut Vmm,
    vsock: &VsockDeviceConfig,
    event_manager: &mut EventManager,
) -> std::result::Result<(), StartMicrovmError> {
    use self::StartMicrovmError::*;
    let backend = devices::virtio::vsock::VsockUnixBackend::new(
        u64::from(vsock.guest_cid),
        vsock.uds_path.clone(),
    )
    .map_err(CreateVsockBackend)?;

    let vsock_device = Arc::new(Mutex::new(
        devices::virtio::Vsock::new(
            u64::from(vsock.guest_cid),
            vmm.guest_memory().clone(),
            backend,
        )
        .map_err(CreateVsockDevice)?,
    ));

    event_manager
        .add_subscriber(vsock_device.clone())
        .map_err(StartMicrovmError::RegisterEvent)?;

    attach_mmio_device(
        vmm,
        vsock.vsock_id.clone(),
        MmioTransport::new(vmm.guest_memory().clone(), vsock_device)
            .map_err(device_manager::mmio::Error::CreateMmioDevice)
            .map_err(RegisterVsockDevice)?,
    )
    .map_err(RegisterVsockDevice)?;

    Ok(())
}

#[cfg(test)]
pub mod tests {
    extern crate vmm_sys_util;
    use self::vmm_sys_util::tempfile::TempFile;

    use std::fs::File;

    use super::*;
    use polly::event_manager::EventManager;

    #[test]
    fn test_stdin_wrapper() {
        let wrapper = SerialStdin(io::stdin());
        assert_eq!(wrapper.as_raw_fd(), io::stdin().as_raw_fd())
    }

    #[test]
    fn test_setup_serial_device() {
        // Wrapper over TempFile that implements `ReadableFd`.
        struct SerialInput(File);
        impl io::Read for SerialInput {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                self.0.read(buf)
            }
        }
        impl AsRawFd for SerialInput {
            fn as_raw_fd(&self) -> RawFd {
                self.0.as_raw_fd()
            }
        }
        impl devices::legacy::ReadableFd for SerialInput {};

        let read_tempfile = TempFile::new().unwrap();
        let read_file = File::open(read_tempfile.as_path()).unwrap();
        let read_handle = SerialInput(read_file);
        let mut event_manager = EventManager::new().expect("Unable to create EventManager");
        setup_serial_device(
            &mut event_manager,
            Box::new(read_handle),
            Box::new(io::stdout()),
        )
        .unwrap();
    }
}