// Copyright 2024 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

#[cfg(target_arch = "x86_64")]
use std::ops::Range;
#[cfg(target_arch = "x86_64")]
use std::sync::{Arc, Mutex};

#[cfg(target_arch = "x86_64")]
use acpi_tables::{Aml, aml};

#[cfg(target_arch = "x86_64")]
use crate::devices::acpi::cpu_container::{
    CPU_CONTAINER_ACPI_SIZE, CpuContainer, CpuContainerError,
};
use crate::devices::acpi::vmclock::{VmClock, VmClockError};
use crate::devices::acpi::vmgenid::{VmGenId, VmGenIdError};
#[cfg(target_arch = "x86_64")]
use crate::vstate::bus::BusError;
use crate::vstate::memory::GuestMemoryMmap;
use crate::vstate::vm::KvmVm;

#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum ACPIDeviceError {
    /// VMGenID: {0}
    VmGenId(#[from] VmGenIdError),
    /// VMClock: {0}
    VmClock(#[from] VmClockError),
    #[cfg(target_arch = "x86_64")]
    /// CPU container: {0}
    CpuContainer(#[from] CpuContainerError),
    /// Could not register IRQ with KVM: {0}
    RegisterIrq(#[from] kvm_ioctls::Error),
    /// Resource allocator error: {0}
    ResourceAllocator(#[from] vm_allocator::Error),
    #[cfg(target_arch = "x86_64")]
    /// Could not register CPU container on the MMIO bus: {0}
    Bus(#[from] BusError),
}

// Although both VMGenID and VMClock devices are always present, they should be instantiated when
// they are attached to preserve the existing ordering of GSI allocation.
#[derive(Debug, Default)]
pub struct ACPIDeviceManager {
    /// VMGenID device
    vmgenid: Option<VmGenId>,
    /// VMclock device
    vmclock: Option<VmClock>,
    #[cfg(target_arch = "x86_64")]
    /// ACPI CPU hotplug controller
    cpu_container: Option<Arc<Mutex<CpuContainer>>>,
}

impl ACPIDeviceManager {
    /// Create a new ACPIDeviceManager object
    pub fn new(vmgenid: VmGenId, vmclock: VmClock) -> Self {
        ACPIDeviceManager {
            vmgenid: Some(vmgenid),
            vmclock: Some(vmclock),
            #[cfg(target_arch = "x86_64")]
            cpu_container: None,
        }
    }

    pub fn attach_vmgenid(&mut self, vm: &KvmVm) -> Result<(), ACPIDeviceError> {
        self.vmgenid = Some(VmGenId::new(&mut vm.resource_allocator())?);
        Ok(())
    }

    pub fn attach_vmclock(&mut self, vm: &KvmVm) -> Result<(), ACPIDeviceError> {
        self.vmclock = Some(VmClock::new(&mut vm.resource_allocator())?);
        Ok(())
    }

    /// Allocate and attach the boot-time CPU hotplug controller.
    ///
    /// Attaching this after VMGenID and VMClock preserves their existing GSI
    /// assignments. Repeated attachments retain the original controller.
    #[cfg(target_arch = "x86_64")]
    pub fn attach_cpu_container(&mut self, vm: &KvmVm) -> Result<(), ACPIDeviceError> {
        if self.cpu_container.is_some() {
            return Ok(());
        }

        let container = Arc::new(Mutex::new(CpuContainer::new(&mut vm.resource_allocator())?));
        self.attach_cpu_container_device(vm, container)
    }

    /// Attach a constructed CPU container to KVM and the MMIO bus.
    #[cfg(target_arch = "x86_64")]
    pub(crate) fn attach_cpu_container_device(
        &mut self,
        vm: &KvmVm,
        container: Arc<Mutex<CpuContainer>>,
    ) -> Result<(), ACPIDeviceError> {
        {
            let locked_container = container.lock().expect("Poisoned lock");
            vm.register_irq(&locked_container.acpi_interrupt_evt, locked_container.gsi)?;
            vm.common.mmio_bus.insert(
                container.clone(),
                locked_container.mmio_address.0,
                u64::try_from(CPU_CONTAINER_ACPI_SIZE).unwrap(),
            )?;
        }
        self.cpu_container = Some(container);
        Ok(())
    }

    #[cfg(target_arch = "x86_64")]
    pub fn cpu_container(&self) -> Option<&Arc<Mutex<CpuContainer>>> {
        self.cpu_container.as_ref()
    }

    /// Set the enabled CPUs immediately before generating boot ACPI tables.
    #[cfg(target_arch = "x86_64")]
    pub fn enable_boot_vcpus(&mut self, boot_count: u8) -> Result<(), ACPIDeviceError> {
        self.cpu_container
            .as_ref()
            .ok_or(CpuContainerError::NotAttached)?
            .lock()
            .expect("Poisoned lock")
            .enable_boot_vcpus(boot_count)?;
        Ok(())
    }

    /// Mark newly created, contiguous vCPUs as inserting and notify the guest.
    #[cfg(target_arch = "x86_64")]
    pub fn hot_add_vcpus(&mut self, new_vcpus: Range<u8>) -> Result<(), CpuContainerError> {
        self.cpu_container
            .as_ref()
            .ok_or(CpuContainerError::NotAttached)?
            .lock()
            .expect("Poisoned lock")
            .hot_add_vcpus(new_vcpus)
    }

    /// Re-notify the guest about CPU insertions restored in a pending state.
    #[cfg(target_arch = "x86_64")]
    pub fn notify_pending_vcpus(&mut self) -> Result<(), CpuContainerError> {
        self.cpu_container
            .as_ref()
            .ok_or(CpuContainerError::NotAttached)?
            .lock()
            .expect("Poisoned lock")
            .notify_pending_vcpus()?;
        Ok(())
    }

    pub fn vmgenid(&self) -> &VmGenId {
        self.vmgenid.as_ref().expect("Missing VMGenID device")
    }

    pub fn vmclock(&self) -> &VmClock {
        self.vmclock.as_ref().expect("Missing VMClock device")
    }

    pub fn activate_vmgenid(&self, vm: &KvmVm) -> Result<(), ACPIDeviceError> {
        vm.register_irq(&self.vmgenid().interrupt_evt, self.vmgenid().gsi)?;
        self.vmgenid().activate(vm.guest_memory())?;
        Ok(())
    }

    pub fn activate_vmclock(&self, vm: &KvmVm) -> Result<(), ACPIDeviceError> {
        vm.register_irq(&self.vmclock().interrupt_evt, self.vmclock().gsi)?;
        self.vmclock().activate(vm.guest_memory())?;
        Ok(())
    }

    pub fn replay_gsi_allocations(&self, vm: &KvmVm) -> Result<(), ACPIDeviceError> {
        let mut resource_allocator = vm.resource_allocator();
        resource_allocator
            .gsi_legacy_allocator
            .allocate_id_at(self.vmgenid().gsi)?;
        resource_allocator
            .gsi_legacy_allocator
            .allocate_id_at(self.vmclock().gsi)?;
        #[cfg(target_arch = "x86_64")]
        if let Some(container) = &self.cpu_container {
            resource_allocator
                .gsi_legacy_allocator
                .allocate_id_at(container.lock().expect("Poisoned lock").gsi)?;
        }
        Ok(())
    }

    pub fn do_post_restore_vmgenid(&self) -> Result<(), ACPIDeviceError> {
        self.vmgenid().do_post_restore()?;
        Ok(())
    }

    pub fn do_post_restore_vmclock(
        &mut self,
        mem: &GuestMemoryMmap,
    ) -> Result<(), ACPIDeviceError> {
        self.vmclock
            .as_mut()
            .expect("Missing VMClock device")
            .do_post_restore(mem)?;
        Ok(())
    }
}

#[cfg(target_arch = "x86_64")]
#[derive(Clone, Copy)]
enum GedEventAction {
    Notify(&'static str),
    CpuScan,
}

#[cfg(target_arch = "x86_64")]
struct GedEvent {
    // Legacy GSIs fit in u8 on x86_64.
    gsi: u8,
    action: GedEventAction,
}

#[cfg(target_arch = "x86_64")]
impl GedEvent {
    fn new(gsi: u32, action: GedEventAction) -> Self {
        Self {
            #[allow(clippy::cast_possible_truncation)]
            gsi: gsi as u8,
            action,
        }
    }
}

#[cfg(target_arch = "x86_64")]
impl Aml for GedEvent {
    fn append_aml_bytes(&self, v: &mut Vec<u8>) -> Result<(), aml::AmlError> {
        let predicate = aml::Equal::new(&aml::Arg(0), &self.gsi);
        match self.action {
            GedEventAction::Notify(path) => {
                let path = aml::Path::new(path)?;
                let notify = aml::Notify::new(&path, &0x80usize);
                aml::If::new(&predicate, vec![&notify]).append_aml_bytes(v)
            }
            GedEventAction::CpuScan => {
                let scan = aml::MethodCall::new(aml::Path::new("\\_SB_.CPUS.CSCN")?, vec![]);
                aml::If::new(&predicate, vec![&scan]).append_aml_bytes(v)
            }
        }
    }
}

#[cfg(target_arch = "x86_64")]
impl Aml for ACPIDeviceManager {
    fn append_aml_bytes(&self, v: &mut Vec<u8>) -> Result<(), aml::AmlError> {
        // Keep the existing device AML as a prefix when adding CPU hotplug.
        self.vmgenid().append_aml_bytes(v)?;
        self.vmclock().append_aml_bytes(v)?;

        let mut interrupts = vec![
            aml::Interrupt::new(true, true, false, false, self.vmgenid().gsi),
            aml::Interrupt::new(true, true, false, false, self.vmclock().gsi),
        ];
        let mut events = vec![
            GedEvent::new(self.vmgenid().gsi, GedEventAction::Notify("\\_SB_.VGEN")),
            GedEvent::new(self.vmclock().gsi, GedEventAction::Notify("\\_SB_.VCLK")),
        ];

        if let Some(container) = &self.cpu_container {
            let container = container.lock().expect("Poisoned lock");
            interrupts.push(aml::Interrupt::new(true, true, false, false, container.gsi));
            events.push(GedEvent::new(container.gsi, GedEventAction::CpuScan));
            container.append_aml_bytes(v)?;
        }

        let resources = aml::ResourceTemplate::new(
            interrupts
                .iter()
                .map(|interrupt| interrupt as &dyn Aml)
                .collect(),
        );
        let event_refs: Vec<&dyn Aml> = events.iter().map(|event| event as &dyn Aml).collect();

        // Create the AML for the GED interrupt handler.
        aml::Device::new(
            "_SB_.GED_".try_into()?,
            vec![
                &aml::Name::new("_HID".try_into()?, &"ACPI0013")?,
                &aml::Name::new("_CRS".try_into()?, &resources)?,
                &aml::Method::new("_EVT".try_into()?, 1, true, event_refs),
            ],
        )
        .append_aml_bytes(v)
    }
}

#[cfg(test)]
#[cfg(target_arch = "x86_64")]
mod tests {
    use acpi_tables::Aml;

    use super::*;
    use crate::vstate::resources::ResourceAllocator;

    fn manager_with_cpu_container() -> ACPIDeviceManager {
        let mut allocator = ResourceAllocator::new();
        let mut manager = ACPIDeviceManager::new(
            VmGenId::new(&mut allocator).unwrap(),
            VmClock::new(&mut allocator).unwrap(),
        );
        manager.cpu_container = Some(Arc::new(Mutex::new(
            CpuContainer::new(&mut allocator).unwrap(),
        )));
        manager.enable_boot_vcpus(2).unwrap();
        manager
    }

    #[test]
    fn test_ged_preserves_existing_devices_and_adds_cpu_scan() {
        let manager = manager_with_cpu_container();
        let expected_prefix = [
            manager.vmgenid().to_aml_bytes().unwrap(),
            manager.vmclock().to_aml_bytes().unwrap(),
        ]
        .concat();
        let bytes = manager.to_aml_bytes().unwrap();

        assert!(bytes.starts_with(&expected_prefix));
        let container = manager.cpu_container().unwrap();
        let container = container.lock().unwrap();
        let cpu_bytes = container.to_aml_bytes().unwrap();
        let ged = &bytes[expected_prefix.len() + cpu_bytes.len()..];
        assert!(ged.windows(4).any(|window| window == b"GED_"));

        let events = [
            GedEvent::new(manager.vmgenid().gsi, GedEventAction::Notify("\\_SB_.VGEN")),
            GedEvent::new(manager.vmclock().gsi, GedEventAction::Notify("\\_SB_.VCLK")),
            GedEvent::new(container.gsi, GedEventAction::CpuScan),
        ];
        for (gsi, event) in [manager.vmgenid().gsi, manager.vmclock().gsi, container.gsi]
            .into_iter()
            .zip(events)
        {
            let event = event.to_aml_bytes().unwrap();
            assert!(ged.windows(event.len()).any(|window| window == event));
            let interrupt = aml::Interrupt::new(true, true, false, false, gsi)
                .to_aml_bytes()
                .unwrap();
            assert!(
                ged.windows(interrupt.len())
                    .any(|window| window == interrupt)
            );
        }
    }
}
