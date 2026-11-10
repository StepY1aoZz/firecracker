// Copyright 2026 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::vmm_config::machine_config::MachineConfigError;
use crate::vstate::vcpu::VcpuAffinity;
use crate::vstate::vm::MAX_SUPPORTED_KVM_VCPUS;

/// Request to add vCPUs to an existing or restored microVM.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HotplugVcpuConfig {
    /// Number of vCPUs to append after the current vCPU range.
    pub add: u16,
    /// Optional host CPU affinity for newly added vCPUs.
    #[serde(default)]
    pub affinity: Vec<VcpuAffinity>,
}

/// Invalid per-vCPU affinity in a hot-add request.
#[derive(Debug, thiserror::Error, displaydoc::Display, PartialEq, Eq)]
pub enum VcpuAffinityError {
    /// Affinity vCPU ID {0} is outside the newly added range.
    VcpuIdOutOfRange(u8),
    /// Affinity for vCPU {0} is specified more than once.
    DuplicateVcpu(u8),
    /// Affinity for vCPU {0} contains no host CPUs.
    EmptyHostCpuSet(u8),
    /// Host CPU {0} exceeds CPU_SETSIZE.
    HostCpuOutOfRange(u32),
    /// Affinity for vCPU {vcpu_id} contains duplicate host CPU {host_cpu}.
    DuplicateHostCpu {
        /// Guest vCPU ID.
        vcpu_id: u8,
        /// Duplicated host CPU ID.
        host_cpu: u32,
    },
}

impl HotplugVcpuConfig {
    /// Validates the addition and returns the resulting vCPU count.
    pub fn target_count(&self, current_count: u16, smt: bool) -> Result<u16, MachineConfigError> {
        if self.add == 0 {
            return Err(MachineConfigError::InvalidVcpuCount);
        }
        let target = current_count
            .checked_add(self.add)
            .filter(|target| *target <= MAX_SUPPORTED_KVM_VCPUS)
            .ok_or(MachineConfigError::InvalidVcpuCount)?;
        if smt && target > 1 && !target.is_multiple_of(2) {
            return Err(MachineConfigError::InvalidVcpuCount);
        }
        Ok(target)
    }

    /// Validates and indexes affinity by newly added guest vCPU ID.
    pub fn affinity_map(
        &self,
        current_count: u16,
        target_count: u16,
    ) -> Result<HashMap<u8, Vec<u32>>, VcpuAffinityError> {
        let mut result = HashMap::with_capacity(self.affinity.len());
        for affinity in &self.affinity {
            let id = u16::from(affinity.vcpu_id);
            if id < current_count || id >= target_count {
                return Err(VcpuAffinityError::VcpuIdOutOfRange(affinity.vcpu_id));
            }
            if affinity.host_cpus.is_empty() {
                return Err(VcpuAffinityError::EmptyHostCpuSet(affinity.vcpu_id));
            }
            if result
                .insert(affinity.vcpu_id, affinity.host_cpus.clone())
                .is_some()
            {
                return Err(VcpuAffinityError::DuplicateVcpu(affinity.vcpu_id));
            }

            let mut host_cpus = HashSet::with_capacity(affinity.host_cpus.len());
            for &host_cpu in &affinity.host_cpus {
                if host_cpu >= u32::try_from(libc::CPU_SETSIZE).expect("CPU_SETSIZE is positive") {
                    return Err(VcpuAffinityError::HostCpuOutOfRange(host_cpu));
                }
                if !host_cpus.insert(host_cpu) {
                    return Err(VcpuAffinityError::DuplicateHostCpu {
                        vcpu_id: affinity.vcpu_id,
                        host_cpu,
                    });
                }
            }
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_target_count() {
        let config = HotplugVcpuConfig {
            add: 2,
            affinity: vec![],
        };
        assert_eq!(config.target_count(2, false), Ok(4));
        assert_eq!(config.target_count(2, true), Ok(4));
        assert_eq!(
            config.target_count(MAX_SUPPORTED_KVM_VCPUS - 1, false),
            Err(MachineConfigError::InvalidVcpuCount)
        );
        assert_eq!(
            HotplugVcpuConfig::default().target_count(1, false),
            Err(MachineConfigError::InvalidVcpuCount)
        );
        assert_eq!(
            HotplugVcpuConfig {
                add: 1,
                affinity: vec![],
            }
            .target_count(2, true),
            Err(MachineConfigError::InvalidVcpuCount)
        );
    }

    #[test]
    fn test_affinity_map() {
        let config = HotplugVcpuConfig {
            add: 2,
            affinity: vec![VcpuAffinity {
                vcpu_id: 3,
                host_cpus: vec![4, 5],
            }],
        };
        assert_eq!(
            config.affinity_map(2, 4),
            Ok(HashMap::from([(3, vec![4, 5])]))
        );
        assert_eq!(
            config.affinity_map(1, 3),
            Err(VcpuAffinityError::VcpuIdOutOfRange(3))
        );

        let duplicate = HotplugVcpuConfig {
            add: 1,
            affinity: vec![
                VcpuAffinity {
                    vcpu_id: 2,
                    host_cpus: vec![4],
                },
                VcpuAffinity {
                    vcpu_id: 2,
                    host_cpus: vec![5],
                },
            ],
        };
        assert_eq!(
            duplicate.affinity_map(2, 3),
            Err(VcpuAffinityError::DuplicateVcpu(2))
        );
    }
}
