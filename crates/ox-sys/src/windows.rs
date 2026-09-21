//! Audited Windows system-information queries used by the safe runtime.

use std::io;

use windows_sys::Wdk::System::SystemServices::RtlGetVersion;
use windows_sys::Win32::Foundation::{ERROR_SUCCESS, RtlNtStatusToDosError};
use windows_sys::Win32::System::Registry::{
    HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RRF_SUBKEY_WOW6464KEY, RegGetValueW,
};
use windows_sys::Win32::System::SystemInformation::{
    GetSystemInfo, GlobalMemoryStatusEx, MEMORYSTATUSEX, OSVERSIONINFOW,
    PROCESSOR_ARCHITECTURE_ALPHA, PROCESSOR_ARCHITECTURE_ALPHA64, PROCESSOR_ARCHITECTURE_AMD64,
    PROCESSOR_ARCHITECTURE_ARM, PROCESSOR_ARCHITECTURE_ARM64, PROCESSOR_ARCHITECTURE_IA32_ON_WIN64,
    PROCESSOR_ARCHITECTURE_IA64, PROCESSOR_ARCHITECTURE_INTEL, PROCESSOR_ARCHITECTURE_MIPS,
    PROCESSOR_ARCHITECTURE_PPC, PROCESSOR_ARCHITECTURE_SHX, SYSTEM_INFO,
};
use windows_sys::core::w;

/// Owned system identity, with the field meanings of libuv's `uv_os_uname`.
#[derive(Debug)]
pub struct SystemIdentity {
    /// Kernel name.
    pub sysname: String,
    /// Major, minor, and build numbers.
    pub release: String,
    /// Product name and optional service-pack description.
    pub version: String,
    /// Process-visible processor architecture.
    pub machine: String,
}

/// Physical memory measured in bytes at one instant.
#[derive(Clone, Copy, Debug)]
pub struct PhysicalMemory {
    /// Total usable physical memory.
    pub total: u64,
    /// Physical memory currently available.
    pub available: u64,
}

#[expect(
    clippy::cast_possible_truncation,
    reason = "the compile-time size assertion bounds the cast"
)]
const fn dword_size<T>() -> u32 {
    assert!(size_of::<T>() <= u32::MAX as usize);
    size_of::<T>() as u32
}

fn utf16_z(value: &[u16]) -> io::Result<String> {
    let end = value
        .iter()
        .position(|unit| *unit == 0)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "unterminated Windows string"))?;
    String::from_utf16(&value[..end])
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

/// Queries the running Windows kernel and its product description.
///
/// # Errors
///
/// Returns the translated OS error if the version query fails, or invalid-data
/// errors for malformed UTF-16. An unavailable registry product name is empty,
/// matching libuv; it never replaces the actual kernel release.
pub fn system_identity() -> io::Result<SystemIdentity> {
    let mut version = OSVERSIONINFOW {
        dwOSVersionInfoSize: dword_size::<OSVERSIONINFOW>(),
        ..OSVERSIONINFOW::default()
    };
    // SAFETY: the initialized structure is writable and advertises its exact
    // size. RtlGetVersion retains no pointer after returning.
    let status = unsafe { RtlGetVersion(&raw mut version) };
    if status < 0 {
        // SAFETY: any NTSTATUS is valid input; this translation takes no pointers.
        let error = unsafe { RtlNtStatusToDosError(status) };
        return Err(io::Error::from_raw_os_error(i32::from_ne_bytes(
            error.to_ne_bytes(),
        )));
    }

    let mut product = [0_u16; 256];
    let mut length = dword_size::<[u16; 256]>();
    // SAFETY: predefined HKLM needs no close. Constant keys are terminated;
    // the writable buffer and byte length agree. RegGetValueW is synchronous.
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            w!("SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion"),
            w!("ProductName"),
            RRF_RT_REG_SZ | RRF_SUBKEY_WOW6464KEY,
            std::ptr::null_mut(),
            product.as_mut_ptr().cast(),
            &raw mut length,
        )
    };
    let mut product = if status == ERROR_SUCCESS {
        utf16_z(&product)?
    } else {
        // libuv permits the product registry key to be missing or inaccessible.
        String::new()
    };
    if version.dwMajorVersion == 10
        && version.dwBuildNumber >= 22_000
        && product.starts_with("Windows 10")
    {
        product.replace_range(9..10, "1");
    }
    let service_pack = utf16_z(&version.szCSDVersion)?;
    if !service_pack.is_empty() {
        if !product.is_empty() {
            product.push(' ');
        }
        product.push_str(&service_pack);
    }

    // Match libuv's uv_os_uname: this is the process architecture, including
    // i686 under WOW64. GetNativeSystemInfo would change that contract.
    let mut system = SYSTEM_INFO::default();
    // SAFETY: GetSystemInfo fully initializes the writable structure and has
    // no failure return. It writes the documented architecture union member.
    unsafe { GetSystemInfo(&raw mut system) };
    let architecture = unsafe { system.Anonymous.Anonymous.wProcessorArchitecture };
    let machine = match architecture {
        PROCESSOR_ARCHITECTURE_INTEL => format!("i{}86", system.wProcessorLevel.clamp(3, 6)),
        PROCESSOR_ARCHITECTURE_AMD64 => "x86_64".to_owned(),
        PROCESSOR_ARCHITECTURE_IA64 => "ia64".to_owned(),
        PROCESSOR_ARCHITECTURE_IA32_ON_WIN64 => "i686".to_owned(),
        PROCESSOR_ARCHITECTURE_MIPS => "mips".to_owned(),
        PROCESSOR_ARCHITECTURE_ALPHA | PROCESSOR_ARCHITECTURE_ALPHA64 => "alpha".to_owned(),
        PROCESSOR_ARCHITECTURE_PPC => "powerpc".to_owned(),
        PROCESSOR_ARCHITECTURE_SHX => "sh".to_owned(),
        PROCESSOR_ARCHITECTURE_ARM => "arm".to_owned(),
        PROCESSOR_ARCHITECTURE_ARM64 => "arm64".to_owned(),
        _ => "unknown".to_owned(),
    };
    Ok(SystemIdentity {
        sysname: "Windows_NT".to_owned(),
        release: format!(
            "{}.{}.{}",
            version.dwMajorVersion, version.dwMinorVersion, version.dwBuildNumber
        ),
        version: product,
        machine,
    })
}

/// Queries physical memory without relying on environment variables.
///
/// # Errors
///
/// Returns the Windows error when `GlobalMemoryStatusEx` fails.
pub fn physical_memory() -> io::Result<PhysicalMemory> {
    let mut memory = MEMORYSTATUSEX {
        dwLength: dword_size::<MEMORYSTATUSEX>(),
        ..MEMORYSTATUSEX::default()
    };
    // SAFETY: the fully initialized, writable buffer advertises its exact size
    // and remains live for the synchronous query.
    if unsafe { GlobalMemoryStatusEx(&raw mut memory) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(PhysicalMemory {
        total: memory.ullTotalPhys,
        available: memory.ullAvailPhys,
    })
}

#[cfg(test)]
mod tests {
    use super::{physical_memory, system_identity, utf16_z};

    #[test]
    fn system_queries_return_real_kernel_and_memory_values() -> std::io::Result<()> {
        let identity = system_identity()?;
        assert_eq!(identity.sysname, "Windows_NT");
        assert_eq!(identity.release.split('.').count(), 3);
        assert_ne!(identity.machine, "unknown");
        let memory = physical_memory()?;
        assert!(memory.total > 0);
        assert!(memory.available <= memory.total);
        Ok(())
    }

    #[cfg(target_arch = "x86")]
    #[test]
    fn x86_uname_keeps_libuv_process_architecture() -> std::io::Result<()> {
        // The i686 CI target runs under WOW64 on a 64-bit Windows host.
        // GetNativeSystemInfo would incorrectly change this to x86_64.
        assert_eq!(system_identity()?.machine, "i686");
        Ok(())
    }

    #[test]
    fn windows_strings_reject_bad_surrogates_and_missing_terminators() {
        assert!(utf16_z(&[0xd800, 0]).is_err());
        assert!(utf16_z(&[65]).is_err());
        assert_eq!(utf16_z(&[65, 0]).ok().as_deref(), Some("A"));
    }
}
