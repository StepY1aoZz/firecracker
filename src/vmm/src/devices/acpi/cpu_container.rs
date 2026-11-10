// Copyright 2024 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Copyright © 2019 Intel Corporation

use std::ops::Range;
use std::sync::{Arc, Barrier};

use acpi_tables::madt::LocalAPIC;
use acpi_tables::{Aml, aml};
use serde::{Deserialize, Serialize};
use vm_allocator::AllocPolicy;
use vm_memory::GuestAddress;
use vm_superio::Trigger;
use vmm_sys_util::eventfd::EventFd;
use zerocopy::IntoBytes;

use crate::device_manager::mmio::MMIO_LEN;
use crate::devices::legacy::EventFdTrigger;
use crate::logger::{debug, error};
use crate::snapshot::Persist;
use crate::vstate::bus::BusDevice;
use crate::vstate::resources::ResourceAllocator;
use crate::vstate::vm::MAX_SUPPORTED_KVM_VCPUS;

/// Size of the CPU hotplug MMIO operation region.
pub const CPU_CONTAINER_ACPI_SIZE: usize = 0x0c;

const CPU_ENABLE_BIT: u8 = 1 << 0;
const CPU_INSERTING_BIT: u8 = 1 << 1;

const CPU_SELECTION_OFFSET: u64 = 0;
const CPU_STATUS_OFFSET: u64 = 4;
const CPU_COMMAND_OFFSET: u64 = 5;
const CPU_DATA_OFFSET: u64 = 8;

/// ACPI CPU hotplug controller.
///
/// The controller exposes every CPU supported by Firecracker in AML. Boot CPUs
/// start enabled, while hot-added CPUs move from inserting to enabled under
/// control of the guest's AML CPU scan method.
#[derive(Debug)]
pub struct CpuContainer {
    /// Interrupt line for notifying the guest about CPU changes.
    pub acpi_interrupt_evt: EventFdTrigger,
    /// GSI used for guest notifications.
    pub gsi: u32,
    /// Guest physical address of the MMIO operation region.
    pub mmio_address: GuestAddress,
    /// Status of every possible CPU, indexed by APIC ID.
    cpu_devices: Vec<CpuDevice>,
    /// Currently selected APIC ID. This is 32 bits because CSEL is a DWord.
    selected_cpu: u32,
}

#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum CpuContainerError {
    /// Error with CpuContainer interrupt: {0}
    Interrupt(#[from] std::io::Error),
    /// Failed to allocate requested resource: {0}
    Allocator(#[from] vm_allocator::Error),
    /// Invalid boot vCPU count: {0}
    InvalidBootVcpuCount(u8),
    /// Invalid or non-contiguous new vCPU range: {start}..{end}
    InvalidVcpuRange {
        /// First APIC ID in the requested range.
        start: u8,
        /// Exclusive end of the requested range.
        end: u8,
    },
    /// vCPU {0} is already enabled or being inserted
    VcpuAlreadyAdded(u8),
    /// CPU container is not attached
    NotAttached,
}

impl CpuContainer {
    /// Construct a controller from previously allocated resources and state.
    pub fn from_parts(
        mmio_address: GuestAddress,
        gsi: u32,
        selected_cpu: u32,
        cpu_devices: Vec<CpuDevice>,
    ) -> Result<Self, CpuContainerError> {
        let acpi_interrupt_evt = EventFdTrigger::new(EventFd::new(libc::EFD_NONBLOCK)?);

        Ok(Self {
            acpi_interrupt_evt,
            gsi,
            mmio_address,
            cpu_devices,
            selected_cpu,
        })
    }

    /// Allocate one MMIO region and one notification GSI for a new controller.
    pub fn new(resource_allocator: &mut ResourceAllocator) -> Result<Self, CpuContainerError> {
        let gsi = resource_allocator.allocate_gsi_legacy(1)?[0];
        let mmio_address = resource_allocator.mmio32_memory.allocate(
            MMIO_LEN,
            MMIO_LEN,
            AllocPolicy::FirstMatch,
        )?;
        let max_vcpus = u8::try_from(MAX_SUPPORTED_KVM_VCPUS).unwrap();
        let cpu_devices = (0..max_vcpus)
            .map(|cpu_id| CpuDevice {
                cpu_id,
                active: false,
                inserting: false,
            })
            .collect();

        Self::from_parts(GuestAddress(mmio_address.start()), gsi, 0, cpu_devices)
    }

    /// Mark the CPUs present at boot as enabled.
    ///
    /// This only enables CPUs and is therefore safe to call while generating
    /// the boot ACPI tables without providing a hot-unplug operation.
    pub fn enable_boot_vcpus(&mut self, boot_count: u8) -> Result<(), CpuContainerError> {
        if usize::from(boot_count) > self.cpu_devices.len() {
            return Err(CpuContainerError::InvalidBootVcpuCount(boot_count));
        }

        for cpu in self.cpu_devices.iter_mut().take(usize::from(boot_count)) {
            cpu.active = true;
        }
        Ok(())
    }

    /// Mark a contiguous range of newly created vCPUs as inserting and notify
    /// the guest to run its CPU scan method.
    ///
    /// The range is end-exclusive and must start at the first CPU that is
    /// neither enabled nor already being inserted. No CPU state is changed if
    /// range validation fails.
    pub fn hot_add_vcpus(&mut self, new_vcpus: Range<u8>) -> Result<(), CpuContainerError> {
        let start = usize::from(new_vcpus.start);
        let end = usize::from(new_vcpus.end);
        if new_vcpus.start >= new_vcpus.end || end > self.cpu_devices.len() {
            return Err(CpuContainerError::InvalidVcpuRange {
                start: new_vcpus.start,
                end: new_vcpus.end,
            });
        }

        let first_available = self
            .cpu_devices
            .iter()
            .position(|cpu| !cpu.active && !cpu.inserting)
            .unwrap_or(self.cpu_devices.len());
        let devices =
            self.cpu_devices
                .get_mut(start..end)
                .ok_or(CpuContainerError::InvalidVcpuRange {
                    start: new_vcpus.start,
                    end: new_vcpus.end,
                })?;

        if let Some(cpu) = devices.iter().find(|cpu| cpu.active || cpu.inserting) {
            return Err(CpuContainerError::VcpuAlreadyAdded(cpu.cpu_id));
        }
        if start != first_available {
            return Err(CpuContainerError::InvalidVcpuRange {
                start: new_vcpus.start,
                end: new_vcpus.end,
            });
        }

        for cpu in devices {
            cpu.inserting = true;
        }
        self.notify_guest()?;
        Ok(())
    }

    /// Re-notify the guest about insertions that were pending in a snapshot.
    pub fn notify_pending_vcpus(&self) -> Result<(), std::io::Error> {
        if self.cpu_devices.iter().any(|cpu| cpu.inserting) {
            self.notify_guest()?;
        }
        Ok(())
    }

    /// Notify the guest that CPU devices need to be re-enumerated.
    pub fn notify_guest(&self) -> Result<(), std::io::Error> {
        self.acpi_interrupt_evt
            .trigger()
            .inspect_err(|err| error!("hotplug: could not send guest notification: {err}"))?;
        debug!("hotplug: notifying guest about new vCPUs available");
        Ok(())
    }

    fn selected_cpu_index(&self) -> Option<usize> {
        usize::try_from(self.selected_cpu)
            .ok()
            .filter(|&index| index < self.cpu_devices.len())
    }
}

impl BusDevice for CpuContainer {
    fn read(&mut self, _base: u64, offset: u64, data: &mut [u8]) {
        data.fill(0);
        match (offset, data.len()) {
            (CPU_SELECTION_OFFSET, 4) => {
                data.copy_from_slice(&self.selected_cpu.to_le_bytes());
            }
            (CPU_STATUS_OFFSET, 1) => {
                let Some(index) = self.selected_cpu_index() else {
                    error!("Out of range vCPU id: {}", self.selected_cpu);
                    return;
                };
                let cpu = &self.cpu_devices[index];
                if cpu.active {
                    data[0] |= CPU_ENABLE_BIT;
                }
                if cpu.inserting {
                    data[0] |= CPU_INSERTING_BIT;
                }
            }
            // CCMD and CDAT are currently reserved by the AML interface.
            (CPU_COMMAND_OFFSET, 1) | (CPU_DATA_OFFSET, 4) => {}
            _ => error!(
                "Unexpected CPU container read: offset {offset:#x}, width {}",
                data.len()
            ),
        }
    }

    fn write(&mut self, _base: u64, offset: u64, data: &[u8]) -> Option<Arc<Barrier>> {
        let value = match (offset, data.len()) {
            (CPU_SELECTION_OFFSET, 4) => {
                self.selected_cpu = u32::from_le_bytes(data.try_into().unwrap());
                return None;
            }
            (CPU_STATUS_OFFSET, 1) => data[0],
            // CCMD and CDAT are currently reserved by the AML interface.
            (CPU_COMMAND_OFFSET, 1) | (CPU_DATA_OFFSET, 4) => return None,
            _ => {
                error!(
                    "Unexpected CPU container write: offset {offset:#x}, width {}",
                    data.len()
                );
                return None;
            }
        };

        let Some(index) = self.selected_cpu_index() else {
            error!("Out of range vCPU id: {}", self.selected_cpu);
            return None;
        };
        let cpu = &mut self.cpu_devices[index];

        // CPEN and CINS only advance an insertion that the VMM initiated. A
        // guest cannot enable an arbitrary offline CPU or clear CINS before
        // the corresponding CPU has been enabled.
        if cpu.inserting {
            if value & CPU_ENABLE_BIT != 0 {
                cpu.active = true;
            }
            if value & CPU_INSERTING_BIT != 0 && cpu.active {
                cpu.inserting = false;
            }
        }

        None
    }
}

/// Serializable CPU controller state.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CpuContainerState {
    /// Guest physical address of the MMIO operation region.
    pub mmio_address: u64,
    /// Notification GSI.
    pub gsi: u32,
    /// Status of every possible CPU.
    pub cpu_devices: Vec<CpuDevice>,
    /// Full 32-bit CSEL register value.
    pub selected_cpu: u32,
}

impl<'a> Persist<'a> for CpuContainer {
    type State = CpuContainerState;
    type ConstructorArgs = ();
    type Error = CpuContainerError;

    fn save(&self) -> Self::State {
        Self::State {
            mmio_address: self.mmio_address.0,
            gsi: self.gsi,
            cpu_devices: self.cpu_devices.clone(),
            selected_cpu: self.selected_cpu,
        }
    }

    fn restore(
        _constructor_args: Self::ConstructorArgs,
        state: &Self::State,
    ) -> Result<Self, Self::Error> {
        Self::from_parts(
            GuestAddress(state.mmio_address),
            state.gsi,
            state.selected_cpu,
            state.cpu_devices.clone(),
        )
    }
}

impl Aml for CpuContainer {
    fn append_aml_bytes(&self, v: &mut Vec<u8>) -> Result<(), aml::AmlError> {
        aml::Device::new(
            "_SB_.PRES".try_into()?,
            vec![
                &aml::Name::new("_HID".try_into()?, &aml::EisaName::new("PNP0A06")?)?,
                &aml::Name::new("_UID".try_into()?, &"CPU Hotplug Controller")?,
                // Protect the CSEL/status sequence from concurrent AML callers.
                &aml::Mutex::new("CPLK".try_into()?, 0),
                &aml::Name::new(
                    "_CRS".try_into()?,
                    &aml::ResourceTemplate::new(vec![&aml::AddressSpace::new_memory(
                        aml::AddressSpaceCacheable::NotCacheable,
                        true,
                        self.mmio_address.0,
                        self.mmio_address.0 + u64::try_from(CPU_CONTAINER_ACPI_SIZE).unwrap() - 1,
                    )?]),
                )?,
                &aml::OpRegion::new(
                    "PRST".try_into()?,
                    aml::OpRegionSpace::SystemMemory,
                    usize::try_from(self.mmio_address.0).unwrap(),
                    CPU_CONTAINER_ACPI_SIZE,
                ),
                &aml::Field::new(
                    "PRST".try_into()?,
                    aml::FieldAccessType::Byte,
                    aml::FieldUpdateRule::WriteAsZeroes,
                    vec![
                        aml::FieldEntry::Reserved(32),
                        aml::FieldEntry::Named(*b"CPEN", 1),
                        aml::FieldEntry::Named(*b"CINS", 1),
                        aml::FieldEntry::Reserved(6),
                        aml::FieldEntry::Named(*b"CCMD", 8),
                    ],
                ),
                &aml::Field::new(
                    "PRST".try_into()?,
                    aml::FieldAccessType::DWord,
                    aml::FieldUpdateRule::Preserve,
                    vec![
                        aml::FieldEntry::Named(*b"CSEL", 32),
                        aml::FieldEntry::Reserved(32),
                        aml::FieldEntry::Named(*b"CDAT", 32),
                    ],
                ),
            ],
        )
        .append_aml_bytes(v)?;

        let hid = aml::Name::new("_HID".try_into()?, &"ACPI0010")?;
        let cid = aml::Name::new("_CID".try_into()?, &aml::EisaName::new("PNP0A05")?)?;
        let methods = CpuMethods {
            max_vcpus: u8::try_from(MAX_SUPPORTED_KVM_VCPUS).unwrap(),
        };
        let mut cpu_data: Vec<&dyn Aml> = vec![&hid, &cid, &methods];
        cpu_data.extend(self.cpu_devices.iter().map(|cpu| cpu as &dyn Aml));
        aml::Device::new("_SB_.CPUS".try_into()?, cpu_data).append_aml_bytes(v)
    }
}

/// Status of one possible CPU.
#[derive(Default, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CpuDevice {
    /// APIC ID of this CPU.
    cpu_id: u8,
    /// Whether the CPU is enabled.
    active: bool,
    /// Whether the CPU is waiting for the guest to finish insertion.
    inserting: bool,
}

impl CpuDevice {
    fn generate_mat(&self) -> Vec<u8> {
        LocalAPIC::new(self.cpu_id, true).as_bytes().to_owned()
    }
}

impl Aml for CpuDevice {
    fn append_aml_bytes(&self, v: &mut Vec<u8>) -> Result<(), aml::AmlError> {
        let mat_data = self.generate_mat();
        aml::Device::new(
            format!("C{:03X}", self.cpu_id).as_str().try_into()?,
            vec![
                &aml::Name::new("_HID".try_into()?, &"ACPI0007")?,
                &aml::Name::new("_UID".try_into()?, &self.cpu_id)?,
                &aml::Method::new(
                    "_STA".try_into()?,
                    0,
                    false,
                    vec![&aml::Return::new(&aml::MethodCall::new(
                        "CSTA".try_into()?,
                        vec![&self.cpu_id],
                    ))],
                ),
                &aml::Name::new("_MAT".try_into()?, &aml::Buffer::new(mat_data))?,
            ],
        )
        .append_aml_bytes(v)
    }
}

struct CpuNotify {
    cpu_id: u8,
}

impl Aml for CpuNotify {
    fn append_aml_bytes(&self, v: &mut Vec<u8>) -> Result<(), aml::AmlError> {
        let object = aml::Path::new(&format!("C{:03X}", self.cpu_id))?;
        aml::If::new(
            &aml::Equal::new(&aml::Arg(0), &self.cpu_id),
            vec![&aml::Notify::new(&object, &1u8)],
        )
        .append_aml_bytes(v)
    }
}

struct CpuMethods {
    max_vcpus: u8,
}

impl Aml for CpuMethods {
    fn append_aml_bytes(&self, v: &mut Vec<u8>) -> Result<(), aml::AmlError> {
        aml::Method::new(
            "CSTA".try_into()?,
            1,
            true,
            vec![
                &aml::Acquire::new("\\_SB_.PRES.CPLK".try_into()?, 0xffff),
                &aml::Store::new(&aml::Path::new("\\_SB_.PRES.CSEL")?, &aml::Arg(0)),
                &aml::Store::new(&aml::Local(0), &aml::ZERO),
                &aml::If::new(
                    &aml::Equal::new(&aml::Path::new("\\_SB_.PRES.CPEN")?, &aml::ONE),
                    vec![&aml::Store::new(&aml::Local(0), &0x0fu8)],
                ),
                &aml::Release::new("\\_SB_.PRES.CPLK".try_into()?),
                &aml::Return::new(&aml::Local(0)),
            ],
        )
        .append_aml_bytes(v)?;

        let cpu_notifies: Vec<_> = (0..self.max_vcpus)
            .map(|cpu_id| CpuNotify { cpu_id })
            .collect();
        let cpu_notify_refs: Vec<&dyn Aml> = cpu_notifies
            .iter()
            .map(|notify| notify as &dyn Aml)
            .collect();
        aml::Method::new("CTFY".try_into()?, 2, true, cpu_notify_refs).append_aml_bytes(v)?;

        aml::Method::new(
            "CSCN".try_into()?,
            0,
            true,
            vec![
                &aml::Acquire::new("\\_SB_.PRES.CPLK".try_into()?, 0xffff),
                &aml::Store::new(&aml::Local(0), &aml::ZERO),
                &aml::While::new(
                    &aml::LessThan::new(&aml::Local(0), &self.max_vcpus),
                    vec![
                        &aml::Store::new(&aml::Path::new("\\_SB_.PRES.CSEL")?, &aml::Local(0)),
                        &aml::If::new(
                            &aml::Equal::new(&aml::Path::new("\\_SB_.PRES.CINS")?, &aml::ONE),
                            vec![
                                &aml::Store::new(&aml::Path::new("\\_SB_.PRES.CPEN")?, &aml::ONE),
                                &aml::MethodCall::new(
                                    "CTFY".try_into()?,
                                    vec![&aml::Local(0), &aml::ONE],
                                ),
                                &aml::Store::new(&aml::Path::new("\\_SB_.PRES.CINS")?, &aml::ONE),
                            ],
                        ),
                        &aml::Add::new(&aml::Local(0), &aml::Local(0), &aml::ONE),
                    ],
                ),
                &aml::Release::new("\\_SB_.PRES.CPLK".try_into()?),
            ],
        )
        .append_aml_bytes(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::Persist;

    fn container_with_boot_vcpus(boot_count: u8) -> CpuContainer {
        let mut allocator = ResourceAllocator::new();
        let mut container = CpuContainer::new(&mut allocator).unwrap();
        container.enable_boot_vcpus(boot_count).unwrap();
        container
    }

    fn select_cpu(container: &mut CpuContainer, cpu_id: u32) {
        let result = container.write(0, CPU_SELECTION_OFFSET, &cpu_id.to_le_bytes());
        assert!(result.is_none());
    }

    fn selected_status(container: &mut CpuContainer) -> u8 {
        let mut status = [0xff];
        container.read(0, CPU_STATUS_OFFSET, &mut status);
        status[0]
    }

    #[test]
    fn test_boot_vcpus_are_enabled() {
        let mut container = container_with_boot_vcpus(3);
        assert_eq!(
            container.cpu_devices.len(),
            usize::from(MAX_SUPPORTED_KVM_VCPUS)
        );
        assert!(container.cpu_devices[..3].iter().all(|cpu| cpu.active));
        assert!(container.cpu_devices[3..].iter().all(|cpu| !cpu.active));
        assert!(container.cpu_devices.iter().all(|cpu| !cpu.inserting));

        container
            .enable_boot_vcpus(u8::try_from(MAX_SUPPORTED_KVM_VCPUS).unwrap() + 1)
            .unwrap_err();
    }

    #[test]
    fn test_hot_add_range_and_notification() {
        let mut container = container_with_boot_vcpus(2);
        container.hot_add_vcpus(2..5).unwrap();
        assert_eq!(container.acpi_interrupt_evt.read().unwrap(), 1);
        assert!(container.cpu_devices[2..5].iter().all(|cpu| cpu.inserting));
        assert!(container.cpu_devices[2..5].iter().all(|cpu| !cpu.active));

        assert!(matches!(
            container.hot_add_vcpus(2..4),
            Err(CpuContainerError::VcpuAlreadyAdded(2))
        ));
        assert!(matches!(
            container.hot_add_vcpus(6..7),
            Err(CpuContainerError::InvalidVcpuRange { start: 6, end: 7 })
        ));
        assert!(matches!(
            container.hot_add_vcpus(5..5),
            Err(CpuContainerError::InvalidVcpuRange { start: 5, end: 5 })
        ));
        assert!(matches!(
            container.hot_add_vcpus(5..33),
            Err(CpuContainerError::InvalidVcpuRange { start: 5, end: 33 })
        ));
    }

    #[test]
    fn test_pending_insertion_renotification() {
        let mut container = container_with_boot_vcpus(2);
        container.notify_pending_vcpus().unwrap();
        assert_eq!(
            container.acpi_interrupt_evt.read().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );

        container.hot_add_vcpus(2..3).unwrap();
        assert_eq!(container.acpi_interrupt_evt.read().unwrap(), 1);
        container.notify_pending_vcpus().unwrap();
        assert_eq!(container.acpi_interrupt_evt.read().unwrap(), 1);

        select_cpu(&mut container, 2);
        let _ = container.write(0, CPU_STATUS_OFFSET, &[CPU_ENABLE_BIT]);
        let _ = container.write(0, CPU_STATUS_OFFSET, &[CPU_INSERTING_BIT]);
        container.notify_pending_vcpus().unwrap();
        assert_eq!(
            container.acpi_interrupt_evt.read().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn test_guest_completes_cpu_insertion() {
        let mut container = container_with_boot_vcpus(2);
        container.hot_add_vcpus(2..3).unwrap();
        select_cpu(&mut container, 2);
        assert_eq!(selected_status(&mut container), CPU_INSERTING_BIT);

        let _ = container.write(0, CPU_STATUS_OFFSET, &[CPU_ENABLE_BIT]);
        assert_eq!(
            selected_status(&mut container),
            CPU_ENABLE_BIT | CPU_INSERTING_BIT
        );

        let _ = container.write(0, CPU_STATUS_OFFSET, &[CPU_INSERTING_BIT]);
        assert_eq!(selected_status(&mut container), CPU_ENABLE_BIT);
        assert!(container.cpu_devices[2].active);
        assert!(!container.cpu_devices[2].inserting);
    }

    #[test]
    fn test_mmio_rejects_invalid_widths_and_selection() {
        let mut container = container_with_boot_vcpus(1);
        let original_state = container.save();

        for data in [&mut [0xff; 0][..], &mut [0xff; 1][..], &mut [0xff; 8][..]] {
            container.read(0, CPU_SELECTION_OFFSET, data);
            assert!(data.iter().all(|&byte| byte == 0));
        }
        let _ = container.write(0, CPU_SELECTION_OFFSET, &[]);
        let _ = container.write(0, CPU_SELECTION_OFFSET, &[1]);
        let _ = container.write(0, CPU_STATUS_OFFSET, &[CPU_ENABLE_BIT, 0, 0, 0]);
        let _ = container.write(0, 0x100, &[CPU_ENABLE_BIT]);
        assert_eq!(container.save(), original_state);

        select_cpu(&mut container, u32::MAX);
        assert_eq!(selected_status(&mut container), 0);
        let _ = container.write(0, CPU_STATUS_OFFSET, &[CPU_ENABLE_BIT | CPU_INSERTING_BIT]);
        assert_eq!(container.cpu_devices, original_state.cpu_devices);

        // An offline CPU that the VMM has not started inserting cannot be
        // enabled directly by the guest.
        select_cpu(&mut container, 1);
        let _ = container.write(0, CPU_STATUS_OFFSET, &[CPU_ENABLE_BIT]);
        assert!(!container.cpu_devices[1].active);
        assert_eq!(selected_status(&mut container), 0);

        let mut selection = [0xff; 4];
        container.read(0, CPU_SELECTION_OFFSET, &mut selection);
        assert_eq!(u32::from_le_bytes(selection), 1);
    }

    #[test]
    fn test_state_round_trip_preserves_resources_and_cpu_state() {
        let mut container = container_with_boot_vcpus(2);
        container.hot_add_vcpus(2..4).unwrap();
        select_cpu(&mut container, 3);

        let state = container.save();
        let serialized = bitcode::serialize(&state).unwrap();
        let decoded: CpuContainerState = bitcode::deserialize(&serialized).unwrap();
        let restored = CpuContainer::restore((), &decoded).unwrap();

        assert_eq!(restored.save(), state);
        assert_eq!(restored.mmio_address, container.mmio_address);
        assert_eq!(restored.gsi, container.gsi);
        assert_eq!(restored.selected_cpu, 3);
        assert!(restored.cpu_devices[2..4].iter().all(|cpu| cpu.inserting));
    }

    #[test]
    fn test_aml_contains_controller_and_all_cpu_devices() {
        let container = container_with_boot_vcpus(1);
        let bytes = container.to_aml_bytes().unwrap();
        for name in [
            &b"PRES"[..],
            &b"CPUS"[..],
            &b"CSEL"[..],
            &b"CPEN"[..],
            &b"CINS"[..],
            &b"CSCN"[..],
            &b"C000"[..],
            &b"C01F"[..],
        ] {
            assert!(
                bytes.windows(name.len()).any(|window| window == name),
                "missing AML object {:?}",
                std::str::from_utf8(name).unwrap()
            );
        }
    }
}
