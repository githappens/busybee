use super::CoreSample;

use mach2::kern_return::{kern_return_t, KERN_SUCCESS};
use mach2::mach_types::host_t;
use mach2::message::mach_msg_type_number_t;
use mach2::port::mach_port_t;
use mach2::traps::mach_task_self;
use mach2::vm::mach_vm_deallocate;
use mach2::vm_types::{integer_t, mach_vm_address_t, mach_vm_size_t, natural_t};

// PROCESSOR_CPU_LOAD_INFO = 2 (from <mach/processor_info.h>)
const PROCESSOR_CPU_LOAD_INFO: i32 = 2;
const CPU_STATE_MAX: usize = 4;
const CPU_STATE_USER: usize = 0;
const CPU_STATE_SYSTEM: usize = 1;
const CPU_STATE_IDLE: usize = 2;
const CPU_STATE_NICE: usize = 3;

extern "C" {
    /// Returns a port for the current host; does not need to be deallocated.
    fn mach_host_self() -> mach_port_t;

    /// Retrieves per-processor load information for the host.
    fn host_processor_info(
        host: host_t,
        flavor: i32,
        processor_count: *mut natural_t,
        processor_info: *mut *mut integer_t,
        processor_info_count: *mut mach_msg_type_number_t,
    ) -> kern_return_t;
}

/// Sample per-core CPU tick counters via Mach `host_processor_info`.
/// Mirrors cpumon's C implementation; see the task context for the reference.
pub fn sample() -> Vec<CoreSample> {
    unsafe {
        let host: host_t = mach_host_self();
        let mut cpu_count: natural_t = 0;
        let mut cpu_info: *mut integer_t = std::ptr::null_mut();
        let mut cpu_info_count: mach_msg_type_number_t = 0;

        let kr = host_processor_info(
            host,
            PROCESSOR_CPU_LOAD_INFO,
            &mut cpu_count,
            &mut cpu_info,
            &mut cpu_info_count,
        );

        if kr != KERN_SUCCESS {
            return Vec::new();
        }

        let slice = std::slice::from_raw_parts(cpu_info, cpu_info_count as usize);
        let mut out = Vec::with_capacity(cpu_count as usize);
        for i in 0..cpu_count as usize {
            let base = i * CPU_STATE_MAX;
            out.push(CoreSample {
                user: slice[base + CPU_STATE_USER] as u64,
                system: slice[base + CPU_STATE_SYSTEM] as u64,
                idle: slice[base + CPU_STATE_IDLE] as u64,
                nice: slice[base + CPU_STATE_NICE] as u64,
            });
        }

        // Return the memory to the kernel via mach_vm_deallocate.
        // mach_vm_deallocate takes mach_vm_address_t (u64) and mach_vm_size_t (u64).
        let _ = mach_vm_deallocate(
            mach_task_self(),
            cpu_info as mach_vm_address_t,
            (cpu_info_count as mach_vm_size_t) * std::mem::size_of::<integer_t>() as mach_vm_size_t,
        );

        out
    }
}
