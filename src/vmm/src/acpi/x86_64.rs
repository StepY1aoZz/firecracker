// Copyright 2024 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::mem::size_of;

use acpi_tables::fadt::IAPC_BOOT_ARG_FLAGS_VGA_NOT_PRESENT;
use acpi_tables::madt::{IoAPIC, LocalAPIC};
use acpi_tables::{Fadt, aml};
use vm_memory::GuestAddress;
use zerocopy::IntoBytes;

use crate::arch::x86_64::layout;
use crate::device_manager::legacy::PortIODeviceManager;
use crate::vstate::vm::MAX_SUPPORTED_KVM_VCPUS;

#[inline(always)]
pub(crate) fn setup_interrupt_controllers(nr_vcpus: u8) -> Vec<u8> {
    let max_vcpus = u8::try_from(MAX_SUPPORTED_KVM_VCPUS).unwrap();
    let mut ic =
        Vec::with_capacity(size_of::<IoAPIC>() + usize::from(max_vcpus) * size_of::<LocalAPIC>());

    ic.extend_from_slice(IoAPIC::new(0, layout::IOAPIC_ADDR).as_bytes());
    for cpu_id in 0..max_vcpus {
        ic.extend_from_slice(LocalAPIC::new(cpu_id, cpu_id < nr_vcpus).as_bytes());
    }
    ic
}

#[inline(always)]
pub(crate) fn setup_arch_fadt(fadt: &mut Fadt) {
    // Let the guest kernel know that there is not VGA hardware present
    // neither do we support ASPM, or MSI type of interrupts.
    // More info here:
    // https://uefi.org/specs/ACPI/6.5/05_ACPI_Software_Programming_Model.html?highlight=0a06#ia-pc-boot-architecture-flags
    fadt.setup_iapc_flags(1 << IAPC_BOOT_ARG_FLAGS_VGA_NOT_PRESENT);
}

#[inline(always)]
pub(crate) fn setup_arch_dsdt(dsdt_data: &mut Vec<u8>) -> Result<(), aml::AmlError> {
    PortIODeviceManager::append_aml_bytes(dsdt_data)
}

pub(crate) const fn apic_addr() -> u32 {
    layout::APIC_ADDR
}

pub(crate) const fn rsdp_addr() -> GuestAddress {
    GuestAddress(layout::RSDP_ADDR)
}

#[cfg(test)]
mod tests {
    use std::mem::size_of;

    use acpi_tables::madt::{IoAPIC, LocalAPIC};

    use super::setup_interrupt_controllers;
    use crate::vstate::vm::MAX_SUPPORTED_KVM_VCPUS;

    #[test]
    fn test_interrupt_controllers_include_all_possible_vcpus() {
        let boot_vcpus = 3;
        let controllers = setup_interrupt_controllers(boot_vcpus);
        let max_vcpus = u8::try_from(MAX_SUPPORTED_KVM_VCPUS).unwrap();
        assert_eq!(
            controllers.len(),
            size_of::<IoAPIC>() + usize::from(max_vcpus) * size_of::<LocalAPIC>()
        );

        let local_apics = &controllers[size_of::<IoAPIC>()..];
        for cpu_id in 0..max_vcpus {
            let start = usize::from(cpu_id) * size_of::<LocalAPIC>();
            let entry = &local_apics[start..start + size_of::<LocalAPIC>()];
            assert_eq!(entry[0], 0, "local APIC entry type");
            assert_eq!(entry[1], 8, "local APIC entry length");
            assert_eq!(entry[2], cpu_id, "processor UID");
            assert_eq!(entry[3], cpu_id, "APIC ID");
            let flags = u32::from_le_bytes(entry[4..8].try_into().unwrap());
            let expected_flags = if cpu_id < boot_vcpus { 3 } else { 2 };
            assert_eq!(flags, expected_flags, "flags for CPU {cpu_id}");
        }
    }
}
