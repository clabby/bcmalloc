//! What machine a run happened on.

use std::{fs, process::Command};

/// The key of the operating system: `macos` or `linux`.
pub const fn key() -> &'static str {
    std::env::consts::OS
}

/// The processor and operating system, for instance "AMD EPYC 9354P 32-Core Processor, Ubuntu
/// 24.04.2 LTS, Linux 6.8".
pub fn describe() -> String {
    let output = |program: &str, args: &[&str]| {
        Command::new(program)
            .args(args)
            .output()
            .ok()
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
            .filter(|text| !text.is_empty())
    };
    if cfg!(target_os = "macos") {
        let cpu = output("sysctl", &["-n", "machdep.cpu.brand_string"]).unwrap_or_default();
        let os = output("sw_vers", &["-productVersion"]).unwrap_or_default();
        format!("{cpu}, macOS {os}")
    } else {
        let cpuinfo = fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
        let cpu = cpuinfo
            .lines()
            .find_map(|line| line.strip_prefix("model name"))
            .and_then(|rest| rest.split(':').nth(1))
            .unwrap_or("")
            .trim()
            .to_string();
        let release = fs::read_to_string("/etc/os-release").unwrap_or_default();
        let os = release
            .lines()
            .find_map(|line| line.strip_prefix("PRETTY_NAME="))
            .unwrap_or("Linux")
            .trim_matches('"')
            .to_string();
        let kernel = output("uname", &["-r"]).unwrap_or_default();
        let kernel = kernel.split('-').next().unwrap_or("");
        format!("{cpu}, {os}, Linux {kernel}")
    }
}

/// Today's date as YYYY-MM-DD, in UTC.
pub fn today() -> String {
    let days = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs() / 86_400) as i64;
    // Days to a civil date, after Howard Hinnant's algorithm.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}
