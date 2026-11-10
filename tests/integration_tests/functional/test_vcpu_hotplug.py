# Copyright 2026 Amazon.com, Inc. or its affiliates. All Rights Reserved.
# SPDX-License-Identifier: Apache-2.0
"""Tests for x86_64 vCPU hot-add."""

import time

import pytest

from framework.artifacts import ACPI_GUEST_KERNELS, pin_guest_kernel
from framework.utils import CpuMap
from framework.vm_backend import VM_BACKEND_KVM, pin_vm_backend

pytestmark = [pin_vm_backend(VM_BACKEND_KVM), pin_guest_kernel(ACPI_GUEST_KERNELS)]


@pytest.fixture
def uvm_configured(uvm_configured):
    """Disable fixed VMM overhead thresholds for vCPU hot-add tests."""
    uvm_configured.memory_monitor = None
    return uvm_configured


def wait_for_online_cpus(vm, expected, timeout=30):
    """Wait until Linux reports the expected number of online CPUs."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        online = vm.ssh.check_output("getconf _NPROCESSORS_ONLN").stdout.strip()
        if online == str(expected):
            return
        time.sleep(0.1)
    pytest.fail(f"expected {expected} online vCPUs, got {online}")


def affinity_for(first_vcpu, count):
    """Assign distinct allowed host CPUs to a new guest vCPU range."""
    return [
        {"vcpu_id": vcpu_id, "host_cpus": [CpuMap(vcpu_id)]}
        for vcpu_id in range(first_vcpu, first_vcpu + count)
    ]


def online_cpus(vm, first_vcpu, count, timeout=30):
    """Wait for hot-added CPU devices and ask Linux to bring them online."""
    deadline = time.monotonic() + timeout
    for vcpu_id in range(first_vcpu, first_vcpu + count):
        online_path = f"/sys/devices/system/cpu/cpu{vcpu_id}/online"
        while vm.ssh.run(f"test -f {online_path}").returncode != 0:
            if time.monotonic() >= deadline:
                pytest.fail(f"CPU {vcpu_id} did not appear in the guest")
            time.sleep(0.1)
        vm.ssh.check_output(f"echo 1 > {online_path}")


def test_vcpu_hotplug_online(uvm_any):
    """Hot-add works after boot and after an exact snapshot restore."""
    vm = uvm_any
    vm.memory_monitor = None
    old_count = vm.vcpus_count
    add = 2
    vm.api.vcpu_hotplug.patch(add=add, affinity=affinity_for(old_count, add))

    assert vm.api.machine_config.get().json()["vcpu_count"] == old_count + add
    online_cpus(vm, old_count, add)
    wait_for_online_cpus(vm, old_count + add)
    for vcpu_id in range(old_count, old_count + add):
        assert (
            vm.ssh.check_output(
                f"cat /sys/devices/system/cpu/cpu{vcpu_id}/online"
            ).stdout.strip()
            == "1"
        )
    vm.vcpus_count += add


def test_vcpu_hotplug_snapshot_cycle(uvm_configured, microvm_factory):
    """Hot-added CPUs survive snapshots, including a pending insertion."""
    vm = uvm_configured
    vm.add_net_iface()
    vm.start()

    first_add = 2
    vm.api.vcpu_hotplug.patch(
        add=first_add, affinity=affinity_for(vm.vcpus_count, first_add)
    )
    online_cpus(vm, vm.vcpus_count, first_add)
    vm.vcpus_count += first_add
    wait_for_online_cpus(vm, vm.vcpus_count)

    vm.pause()
    pending_add = 2
    first_pending = vm.vcpus_count
    vm.api.vcpu_hotplug.patch(
        add=pending_add, affinity=affinity_for(first_pending, pending_add)
    )
    assert vm.state == "Paused"
    vm.vcpus_count += pending_add
    snapshot = vm.snapshot_full()
    vm.kill()

    restored = microvm_factory.build_from_snapshot(snapshot)
    restored.memory_monitor = None
    assert restored.vcpus_count == first_pending + pending_add
    online_cpus(restored, first_pending, pending_add)
    wait_for_online_cpus(restored, restored.vcpus_count)

    final_add = 2
    first_final = restored.vcpus_count
    restored.api.vcpu_hotplug.patch(
        add=final_add, affinity=affinity_for(first_final, final_add)
    )
    online_cpus(restored, first_final, final_add)
    restored.vcpus_count += final_add
    wait_for_online_cpus(restored, restored.vcpus_count)

    final_snapshot = restored.snapshot_full()
    restored.kill()
    restored_again = microvm_factory.build_from_snapshot(final_snapshot)
    restored_again.memory_monitor = None
    wait_for_online_cpus(restored_again, first_final + final_add)


@pytest.mark.parametrize("resume_at_restore", [True, False])
def test_vcpu_hotplug_during_restore(
    uvm_configured, microvm_factory, resume_at_restore
):
    """Additional vCPUs are initialized before the restored guest resumes."""
    vm = uvm_configured
    vm.add_net_iface()
    vm.start()
    old_count = vm.vcpus_count
    add = 2
    snapshot = vm.snapshot_full()
    vm.kill()

    restored = microvm_factory.build()
    restored.memory_monitor = None
    restored.spawn()
    restored.restore_from_snapshot(
        snapshot,
        resume=resume_at_restore,
        vcpu_hotplug={"add": add, "affinity": affinity_for(old_count, add)},
    )
    if not resume_at_restore:
        assert restored.state == "Paused"
        restored.resume()

    assert restored.api.machine_config.get().json()["vcpu_count"] == old_count + add
    online_cpus(restored, old_count, add)
    wait_for_online_cpus(restored, old_count + add)

    first_runtime = restored.vcpus_count
    restored.api.vcpu_hotplug.patch(
        add=add, affinity=affinity_for(first_runtime, add)
    )
    online_cpus(restored, first_runtime, add)
    restored.vcpus_count += add
    wait_for_online_cpus(restored, restored.vcpus_count)
