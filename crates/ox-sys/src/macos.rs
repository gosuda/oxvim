//! Audited macOS system-information queries used by the safe runtime.
//!
//! Each entry point mirrors the libuv Darwin implementation its safe caller
//! reimplements: `uv_get_total_memory` reads `hw.memsize`, `uv_get_free_memory`
//! scales the `vm_statistics64` free count by the page size, `uv_uptime`
//! subtracts `kern.boottime` from the wall clock, `uv_cpu_info` reads
//! per-processor load ticks, and `uv_resident_set_memory` reads
//! `MACH_TASK_BASIC_INFO`. Portable Unix queries live in [`crate::unix`].

use std::ffi::CStr;
use std::io;
use std::ptr;

/// Physical memory measured in bytes at one instant.
#[derive(Clone, Copy, Debug)]
pub struct PhysicalMemory {
    /// Total usable physical memory.
    pub total: u64,
    /// Physical memory currently available.
    pub available: u64,
}

/// Cumulative busy-state counters for one logical processor, in kernel clock
/// ticks (`hz`, 100 per second on current XNU kernels).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ProcessorTicks {
    /// Ticks spent running user code.
    pub user: u64,
    /// Ticks spent running user code at lowered priority.
    pub nice: u64,
    /// Ticks spent in the kernel.
    pub system: u64,
    /// Ticks idle.
    pub idle: u64,
}

/// One process's identity as `nvim_get_proc` reports it (upstream resolves
/// processes through libproc on Darwin).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcInfo {
    /// Process id.
    pub pid: i64,
    /// Parent process id.
    pub ppid: i64,
    /// Executable name (`p_comm`, at most `MAXCOMLEN` bytes).
    pub comm: String,
}

/// `proc_pidinfo(PROC_PIDTBSDINFO)`: the BSD record for one pid — libproc's
/// own data source — or `Ok(None)` when it has already exited.
///
/// # Errors
///
/// Returns the `proc_pidinfo` errno on failure.
pub fn proc_info(pid: i32) -> io::Result<Option<ProcInfo>> {
    // SAFETY: plain-data struct; zero is a valid pattern and `proc_pidinfo`
    // writes at most `buffersize` bytes on success.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    // SAFETY: `info` is a writable `proc_bsdinfo` of the advertised size.
    let size = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            ptr::from_mut(&mut info).cast::<libc::c_void>(),
            size_of::<libc::proc_bsdinfo>() as libc::c_int,
        )
    };
    if size <= 0 {
        // A dead pid reports zero bytes rather than an error; anything else
        // is the real errno, except ESRCH which also means "gone".
        let error = io::Error::last_os_error();
        if size == 0 || error.raw_os_error() == Some(libc::ESRCH) {
            return Ok(None);
        }
        return Err(error);
    }
    let comm = &info.pbi_comm;
    let end = comm.iter().position(|byte| *byte == 0).unwrap_or(comm.len());
    let bytes = comm[..end]
        .iter()
        .map(|byte| *byte as u8)
        .collect::<Vec<_>>();
    Ok(Some(ProcInfo {
        pid: i64::from(info.pbi_pid),
        ppid: i64::from(info.pbi_ppid),
        comm: String::from_utf8_lossy(&bytes).into_owned(),
    }))
}

/// `proc_listchildpids(ppid)`: the live child pids of `ppid` — the same
/// libproc enumeration upstream uses on Darwin.
///
/// # Errors
///
/// Returns the `proc_listchildpids` errno on failure.
pub fn child_pids(ppid: i32) -> io::Result<Vec<i64>> {
    let mut size = 64_usize;
    loop {
        let mut buffer = vec![0 as libc::pid_t; size];
        // SAFETY: `buffer` holds `size` `pid_t` slots; the call writes at
        // most `size` entries and returns how many were filled.
        let count = unsafe {
            libc::proc_listchildpids(
                ppid,
                buffer.as_mut_ptr().cast::<libc::c_void>(),
                (size * size_of::<libc::pid_t>()) as libc::c_int,
            )
        };
        if count < 0 {
            return Err(io::Error::last_os_error());
        }
        let count = usize::try_from(count).unwrap_or(0);
        if count < size {
            return Ok(buffer
                .into_iter()
                .take(count)
                .map(i64::from)
                .collect());
        }
        // A full buffer may have missed late arrivals; grow and retry.
        size = size.saturating_mul(2);
    }
}

fn mach_error(operation: &'static str, status: libc::kern_return_t) -> io::Error {
    io::Error::other(format!("{operation} failed with kern_return {status}"))
}

// `mach_host_self`/`mach_task_self` carry a deprecation note in libc pointing
// at the mach2 crate; they are still the canonical libSystem entry points and
// pulling in a second Mach binding for two integer ports is not justified.
#[allow(deprecated)]
fn host_port() -> libc::mach_port_t {
    // SAFETY: no pointers cross; the returned port is a plain integer handle.
    unsafe { libc::mach_host_self() }
}

#[allow(deprecated)]
fn task_port() -> libc::mach_port_t {
    // SAFETY: reads the process-global task port; cannot fail.
    unsafe { libc::mach_task_self() }
}

/// Reads a fixed-size `sysctlbyname` value.
fn read_sysctl<T>(name: &CStr, out: &mut T) -> io::Result<()> {
    let mut size = size_of::<T>();
    // SAFETY: `out` is a writable buffer whose advertised length is its size;
    // `sysctlbyname` writes at most `size` bytes and retains no pointer.
    let result = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            ptr::from_mut(out).cast::<libc::c_void>(),
            &raw mut size,
            ptr::null_mut(),
            0,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Reads a NUL-terminated `sysctlbyname` string.
fn read_sysctl_string(name: &CStr) -> io::Result<String> {
    let mut size = 0_usize;
    // SAFETY: a null `oldp` asks for the length only; `size` is writable.
    let result = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            ptr::null_mut(),
            &raw mut size,
            ptr::null_mut(),
            0,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    let mut buffer = vec![0_u8; size];
    // SAFETY: `buffer` is writable for `size` bytes; the C call writes a
    // terminated string of at most `size` bytes into it.
    let result = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            buffer.as_mut_ptr().cast::<libc::c_void>(),
            &raw mut size,
            ptr::null_mut(),
            0,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    while matches!(buffer.last(), Some(&0)) {
        buffer.pop();
    }
    String::from_utf8(buffer).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

/// Queries physical memory like libuv's `uv_get_total_memory` and
/// `uv_get_free_memory`.
///
/// # Errors
///
/// Returns the sysctl error when `hw.memsize` is unavailable, or a Mach error
/// when `host_statistics64` fails.
pub fn physical_memory() -> io::Result<PhysicalMemory> {
    let mut total = 0_u64;
    read_sysctl(c"hw.memsize", &mut total)?;

    // SAFETY: `vm_statistics64` is a plain C struct; zero is a valid pattern
    // and `host_statistics64` writes every field on success.
    let mut statistics: libc::vm_statistics64 = unsafe { std::mem::zeroed() };
    let mut count = libc::HOST_VM_INFO64_COUNT;
    // SAFETY: `statistics` is a fully writable `vm_statistics64`; `count`
    // advertises its size in integer_t units to the kernel.
    let status = unsafe {
        libc::host_statistics64(
            host_port(),
            libc::HOST_VM_INFO64,
            (&raw mut statistics).cast::<libc::integer_t>(),
            &raw mut count,
        )
    };
    if status != libc::KERN_SUCCESS {
        return Err(mach_error("host_statistics64", status));
    }
    // SAFETY: `_SC_PAGESIZE` is always implemented; the return is an index into
    // `sysconf`'s value space and only -1 signals an error.
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    let page_size = u64::try_from(page_size.max(0)).unwrap_or(0);
    Ok(PhysicalMemory {
        total,
        available: u64::from(statistics.free_count).saturating_mul(page_size),
    })
}

/// Resident set size of the current process in bytes, like libuv's
/// `uv_resident_set_memory` on Darwin.
///
/// # Errors
///
/// Returns a Mach error when `task_info` refuses the query.
pub fn resident_set() -> io::Result<u64> {
    // SAFETY: `mach_task_basic_info` is a plain C struct; zero is a valid
    // pattern and `task_info` writes every field on success.
    let mut info: libc::mach_task_basic_info = unsafe { std::mem::zeroed() };
    let mut count = libc::MACH_TASK_BASIC_INFO_COUNT;
    // SAFETY: `info` is a writable `mach_task_basic_info`; `count` advertises
    // its size in natural_t units.
    let status = unsafe {
        libc::task_info(
            task_port(),
            libc::MACH_TASK_BASIC_INFO,
            (&raw mut info).cast::<libc::integer_t>(),
            &raw mut count,
        )
    };
    if status != libc::KERN_SUCCESS {
        return Err(mach_error("task_info", status));
    }
    Ok(info.resident_size)
}

/// Process resource usage from `getrusage(RUSAGE_SELF)`, matching
/// `uv_getrusage` (field units stay platform-native: `ru_maxrss` is bytes on
/// macOS, kilobytes on Linux, exactly as libuv passes it through).
///
/// # Errors
///
/// Returns the `getrusage` errno on failure.
pub fn resource_usage() -> io::Result<libc::rusage> {
    // SAFETY: plain-data struct; `getrusage` writes all fields on success.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: `usage` is a writable `struct rusage` of the advertised type.
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, &raw mut usage) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(usage)
}

/// System load averages over 1, 5, and 15 minutes via `getloadavg`; zeros when
/// the kernel declines, matching libuv's best-effort fill.
#[must_use]
pub fn load_avg() -> (f64, f64, f64) {
    let mut averages = [0.0_f64; 3];
    // SAFETY: `averages` is three writable doubles; `getloadavg` writes at
    // most that many.
    if unsafe { libc::getloadavg(averages.as_mut_ptr(), 3) } < 0 {
        return (0.0, 0.0, 0.0);
    }
    (averages[0], averages[1], averages[2])
}

/// Seconds since boot, `kern.boottime` subtracted from the wall clock — the
/// same quantity `uv_uptime` reports on Darwin.
///
/// # Errors
///
/// Returns the sysctl error when `kern.boottime` is unavailable.
pub fn uptime() -> io::Result<f64> {
    let mut boot = libc::timeval {
        tv_sec: 0,
        tv_usec: 0,
    };
    read_sysctl(c"kern.boottime", &mut boot)?;
    // SAFETY: `time` takes an optional out-pointer and cannot fail.
    let now = unsafe { libc::time(ptr::null_mut()) };
    Ok((now - boot.tv_sec) as f64)
}

/// Per-processor cumulative tick counters via `host_processor_info`; the
/// kernel-allocated result array is always deallocated before returning.
///
/// # Errors
///
/// Returns a Mach error when the host refuses the query.
pub fn processor_ticks() -> io::Result<Vec<ProcessorTicks>> {
    let mut cpu_count: libc::natural_t = 0;
    let mut info: libc::processor_info_array_t = ptr::null_mut();
    let mut info_count: libc::mach_msg_type_number_t = 0;
    // SAFETY: the out-parameters are writable and re-read only on success;
    // the kernel allocates `info` out-of-line when the call succeeds.
    let status = unsafe {
        libc::host_processor_info(
            host_port(),
            libc::PROCESSOR_CPU_LOAD_INFO,
            &raw mut cpu_count,
            &raw mut info,
            &raw mut info_count,
        )
    };
    if status != libc::KERN_SUCCESS {
        return Err(mach_error("host_processor_info", status));
    }
    // SAFETY: on success `info` is a kernel-allocated array holding
    // `info_count` integer_t units, which hosts `cpu_count` consecutive
    // `processor_cpu_load_info` records.
    let ticks = unsafe {
        std::slice::from_raw_parts(
            info.cast::<libc::processor_cpu_load_info>(),
            usize::try_from(cpu_count).unwrap_or(0),
        )
        .iter()
        .map(|cpu| ProcessorTicks {
            user: u64::from(cpu.cpu_ticks[libc::CPU_STATE_USER as usize]),
            nice: u64::from(cpu.cpu_ticks[libc::CPU_STATE_NICE as usize]),
            system: u64::from(cpu.cpu_ticks[libc::CPU_STATE_SYSTEM as usize]),
            idle: u64::from(cpu.cpu_ticks[libc::CPU_STATE_IDLE as usize]),
        })
        .collect::<Vec<_>>()
    };
    let size = usize::try_from(info_count).unwrap_or(0) * size_of::<libc::integer_t>();
    // SAFETY: `info`/`size` name the out-of-line region the kernel handed over;
    // the Vec was collected before the region is released.
    let _ = unsafe { libc::vm_deallocate(task_port(), info as libc::vm_address_t, size) };
    Ok(ticks)
}

/// CPU marketing name. Intel macs publish `machdep.cpu.brand_string`; Apple
/// Silicon omits it, so the `hw.model` identifier stands in (libuv leaves the
/// model empty there).
#[must_use]
pub fn cpu_model() -> String {
    read_sysctl_string(c"machdep.cpu.brand_string")
        .or_else(|_| read_sysctl_string(c"hw.model"))
        .unwrap_or_default()
}

/// Advertised CPU frequency in Hz (`hw.cpufrequency`). Apple Silicon does not
/// publish the key, so `0` is the honest answer, matching libuv.
#[must_use]
pub fn cpu_frequency() -> u64 {
    let mut hz = 0_u64;
    drop(read_sysctl(c"hw.cpufrequency", &mut hz));
    hz
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::{
        cpu_frequency, cpu_model, load_avg, physical_memory, processor_ticks, resident_set,
        resource_usage, uptime,
    };

    #[test]
    fn system_queries_return_real_kernel_values() -> io::Result<()> {
        let memory = physical_memory()?;
        assert!(memory.total > 0);
        assert!(memory.available <= memory.total);

        assert!(resident_set()? > 0);

        let usage = resource_usage()?;
        assert!(usage.ru_utime.tv_sec >= 0);

        let (one, five, fifteen) = load_avg();
        assert!(one >= 0.0 && five >= 0.0 && fifteen >= 0.0);

        assert!(uptime()? > 0.0);
        assert!(!processor_ticks()?.is_empty());
        assert!(cpu_frequency() == 0 || cpu_frequency() > 10_000_000);
        let _ = cpu_model();
        Ok(())
    }
}
