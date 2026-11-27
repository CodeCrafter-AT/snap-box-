use std::{ffi::CString, os::unix::ffi::OsStrExt, path::Path};

use serde::{Deserialize, Serialize};

/// Real host OS telemetry queried directly from kernel interfaces (never simulated).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostTelemetry {
    pub cpu_count: usize,
    pub load_avg_1m: Option<f64>,
    pub total_memory_bytes: Option<u64>,
    pub available_memory_bytes: Option<u64>,
    pub disk_total_bytes: Option<u64>,
    pub disk_available_bytes: Option<u64>,
}

/// Collect real OS CPU, memory, and filesystem storage metrics for `data_root`.
pub fn collect_host_telemetry(data_root: &Path) -> HostTelemetry {
    let cpu_count = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);

    let load_avg_1m = {
        let mut loads = [0.0f64; 3];
        // SAFETY: `loads` has capacity for 3 f64 elements.
        let rc = unsafe { libc::getloadavg(loads.as_mut_ptr(), 3) };
        if rc >= 1 {
            Some(loads[0])
        } else {
            None
        }
    };

    let (total_memory_bytes, available_memory_bytes) = query_memory_bytes();
    let (disk_total_bytes, disk_available_bytes) = query_statvfs(data_root);

    HostTelemetry {
        cpu_count,
        load_avg_1m,
        total_memory_bytes,
        available_memory_bytes,
        disk_total_bytes,
        disk_available_bytes,
    }
}

fn query_statvfs(path: &Path) -> (Option<u64>, Option<u64>) {
    let Ok(c_path) = CString::new(path.as_os_str().as_bytes()) else {
        return (None, None);
    };
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) };
    if rc != 0 {
        return (None, None);
    }
    let frsize = if stat.f_frsize > 0 {
        stat.f_frsize as u64
    } else {
        stat.f_bsize as u64
    };
    let total = (stat.f_blocks as u64).saturating_mul(frsize);
    let avail = (stat.f_bavail as u64).saturating_mul(frsize);
    (Some(total), Some(avail))
}

#[cfg(target_os = "macos")]
fn query_memory_bytes() -> (Option<u64>, Option<u64>) {
    // Query total physical RAM via sysctl hw.memsize
    let mut mib = [libc::CTL_HW, libc::HW_MEMSIZE];
    let mut total: u64 = 0;
    let mut len = std::mem::size_of::<u64>();
    let total_rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            2,
            (&mut total as *mut u64).cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    let total_opt = if total_rc == 0 { Some(total) } else { None };

    // Query Mach VM page statistics for free + inactive pages
    #[repr(C)]
    #[derive(Default)]
    struct VmStatistics64 {
        free_count: u32,
        active_count: u32,
        inactive_count: u32,
        wire_count: u32,
        zero_fill_count: u64,
        reactivations: u64,
        pageins: u64,
        pageouts: u64,
        faults: u64,
        cow_faults: u64,
        lookups: u64,
        hits: u64,
        purges: u64,
        purgeable_count: u32,
        speculative_count: u32,
        decompressions: u64,
        compressions: u64,
        swapins: u64,
        swapouts: u64,
        compressor_page_count: u32,
        throttled_count: u32,
        external_page_count: u32,
        internal_page_count: u32,
        total_uncompressed_pages_in_compressor: u64,
    }

    const HOST_VM_INFO64: libc::c_int = 4;
    const HOST_VM_INFO64_COUNT: u32 =
        (std::mem::size_of::<VmStatistics64>() / std::mem::size_of::<u32>()) as u32;

    extern "C" {
        fn mach_host_self() -> libc::c_uint;
        fn host_statistics64(
            host_priv: libc::c_uint,
            flavor: libc::c_int,
            host_info_out: *mut libc::c_int,
            host_info_out_cnt: *mut u32,
        ) -> libc::c_int;
    }

    let mut vm_stat = VmStatistics64::default();
    let mut count = HOST_VM_INFO64_COUNT;
    let rc = unsafe {
        host_statistics64(
            mach_host_self(),
            HOST_VM_INFO64,
            (&mut vm_stat as *mut VmStatistics64).cast(),
            &mut count,
        )
    };
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64;
    let avail_opt = if rc == 0 && page_size > 0 {
        let reclaimable_pages = (vm_stat.free_count as u64)
            .saturating_add(vm_stat.inactive_count as u64)
            .saturating_add(vm_stat.purgeable_count as u64);
        Some(reclaimable_pages.saturating_mul(page_size))
    } else {
        None
    };

    (total_opt, avail_opt)
}

#[cfg(target_os = "linux")]
fn query_memory_bytes() -> (Option<u64>, Option<u64>) {
    let Ok(meminfo) = std::fs::read_to_string("/proc/meminfo") else {
        return (None, None);
    };
    let mut total_kb: Option<u64> = None;
    let mut avail_kb: Option<u64> = None;
    for line in meminfo.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            total_kb = rest
                .split_whitespace()
                .next()
                .and_then(|s| s.parse::<u64>().ok());
        } else if let Some(rest) = line.strip_prefix("MemAvailable:") {
            avail_kb = rest
                .split_whitespace()
                .next()
                .and_then(|s| s.parse::<u64>().ok());
        }
    }
    (
        total_kb.map(|k| k.saturating_mul(1024)),
        avail_kb.map(|k| k.saturating_mul(1024)),
    )
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn query_memory_bytes() -> (Option<u64>, Option<u64>) {
    (None, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_telemetry_reports_real_values() {
        let info = collect_host_telemetry(Path::new("/tmp"));
        assert!(info.cpu_count >= 1);
        assert!(info.total_memory_bytes.unwrap_or(0) > 0);
        assert!(info.disk_total_bytes.unwrap_or(0) > 0);
    }
}
