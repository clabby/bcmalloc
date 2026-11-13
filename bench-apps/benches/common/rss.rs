//! The peak memory of the process.

/// The most memory this process has held, in MiB. On macOS this is the physical footprint, which
/// unlike the resident size does not count pages that were freed with a lazy advice.
#[cfg(target_os = "macos")]
pub(crate) fn peak_resident_mib() -> f64 {
    // SAFETY: proc_pid_rusage fills the struct we pass, and a zeroed one is a valid start.
    let info = unsafe {
        let mut info = std::mem::zeroed::<libc::rusage_info_v4>();
        let buffer = std::ptr::from_mut(&mut info).cast();
        libc::proc_pid_rusage(libc::getpid(), libc::RUSAGE_INFO_V4, buffer);
        info
    };
    info.ri_lifetime_max_phys_footprint as f64 / f64::from(1 << 20)
}

/// The most memory this process has held, in MiB.
#[cfg(target_os = "linux")]
pub(crate) fn peak_resident_mib() -> f64 {
    // SAFETY: getrusage fills the struct we pass and has no other effects.
    let usage = unsafe {
        let mut usage = std::mem::zeroed::<libc::rusage>();
        libc::getrusage(libc::RUSAGE_SELF, &mut usage);
        usage
    };
    usage.ru_maxrss as f64 * 1024.0 / f64::from(1 << 20)
}
