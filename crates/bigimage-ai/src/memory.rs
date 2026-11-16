// SPDX-License-Identifier: GPL-3.0-or-later
//! Admission check before a model loads or runs.
//!
//! The native engines are small (0.1 to 0.7 GB measured at their largest
//! working sets, see each module's budget), so the old fixed 8 GiB rule
//! for ONNX Runtime is gone: a run is admitted when the memory it is
//! estimated to need fits in what is available right now, taking the
//! smallest of `MemAvailable`, every cgroup v2 `memory.max` above this
//! process and the address-space rlimit. Swap never counts. This is an
//! admission check, not a quota or a guarantee against OOM.

use crate::AiError;

pub(crate) const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;

fn kilobytes(text: &str, key: &str) -> Option<u64> {
    let rest = text.lines().find_map(|line| line.strip_prefix(key))?;
    let mut fields = rest.split_whitespace();
    let value: u64 = fields.next()?.parse().ok()?;
    if fields.next()? != "kB" {
        return None;
    }
    value.checked_mul(1024)
}

fn decide(model: &str, available: u64, need: u64) -> Result<(), AiError> {
    if available < need {
        return Err(AiError::Memory(format!(
            "{model} precisa de cerca de {:.2} GiB de RAM livre nesta etapa; disponíveis agora: \
             {:.2} GiB. Feche outros programas ou use uma imagem menor. Swap não conta como RAM \
             livre.",
            need as f64 / GIB as f64,
            available as f64 / GIB as f64,
        )));
    }
    Ok(())
}

fn unknown(error: impl std::fmt::Display) -> AiError {
    AiError::Memory(format!("não foi possível conferir o orçamento de memória: {error}"))
}

fn process_value(key: &str) -> Result<u64, AiError> {
    let status = std::fs::read_to_string("/proc/self/status").map_err(unknown)?;
    kilobytes(&status, key).ok_or_else(|| unknown("/proc/self/status incompleto"))
}

/// Memory this process may still allocate without swapping.
pub(crate) fn available() -> Result<u64, AiError> {
    use std::path::{Component, Path};
    let info = std::fs::read_to_string("/proc/meminfo").map_err(unknown)?;
    let total = kilobytes(&info, "MemTotal:").ok_or_else(|| unknown("MemTotal ausente"))?;
    let mut available =
        kilobytes(&info, "MemAvailable:").ok_or_else(|| unknown("MemAvailable ausente"))?;
    // Respect every finite ancestor limit in cgroup v2, including a 4 GiB
    // container on a host with much more RAM. Never count swap as capacity.
    let groups = std::fs::read_to_string("/proc/self/cgroup").map_err(unknown)?;
    if let Some(relative) = groups.lines().find_map(|line| line.strip_prefix("0::")) {
        if Path::new(relative)
            .components()
            .any(|part| !matches!(part, Component::RootDir | Component::Normal(_)))
        {
            return Err(unknown("cgroup inválido"));
        }
        let base = Path::new("/sys/fs/cgroup");
        let mut directory = base.join(relative.trim_start_matches('/'));
        loop {
            match std::fs::read_to_string(directory.join("memory.max")) {
                Ok(limit) if limit.trim() != "max" => {
                    let maximum: u64 = limit.trim().parse().map_err(unknown)?;
                    let current: u64 = std::fs::read_to_string(directory.join("memory.current"))
                        .map_err(unknown)?
                        .trim()
                        .parse()
                        .map_err(unknown)?;
                    available = available.min(maximum.saturating_sub(current));
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(unknown(error)),
            }
            if directory == base {
                break;
            }
            if !directory.pop() || !directory.starts_with(base) {
                return Err(unknown("cgroup fora do mount"));
            }
        }
    }
    if let Some(limit) = rustix::process::getrlimit(rustix::process::Resource::As).current {
        available = available.min(limit.saturating_sub(process_value("VmSize:")?));
    }
    Ok(available.min(total))
}

/// Refuse to start when `need` bytes are not available now.
pub(crate) fn admit(model: &str, need: u64) -> Result<(), AiError> {
    decide(model, available()?, need)
}

/// Resident memory of this process (`VmRSS`), for measurements.
#[allow(dead_code)]
pub(crate) fn resident_bytes() -> Result<u64, AiError> {
    process_value("VmRSS:")
}

/// Peak resident memory of this process (`VmHWM`), for measurements.
#[allow(dead_code)]
pub(crate) fn peak_bytes() -> Result<u64, AiError> {
    process_value("VmHWM:")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admission_compares_the_estimate_with_what_is_free() {
        assert!(decide("X", 2 * GIB, GIB).is_ok());
        assert!(decide("X", GIB, GIB).is_ok());
        let error = decide("BiRefNet-lite", GIB / 2, GIB).unwrap_err();
        let text = error.to_string();
        assert!(text.contains("BiRefNet-lite") && text.contains("1.00 GiB"), "{text}");
        assert!(text.contains("0.50 GiB"), "{text}");
    }

    #[test]
    fn meminfo_values_need_kilobyte_units() {
        assert_eq!(
            kilobytes("MemAvailable: 4096 kB\nSwapFree: 9000000 kB", "MemAvailable:"),
            Some(4 * 1024 * 1024)
        );
        assert_eq!(kilobytes("MemAvailable: 4096 MB", "MemAvailable:"), None);
        assert_eq!(kilobytes("MemAvailable: 18446744073709551615 kB", "MemAvailable:"), None);
    }

    #[test]
    fn this_machine_reports_some_available_memory() {
        // Build containers may hide parts of the cgroup tree: an error is
        // a refusal at run time, not a test failure.
        match available() {
            Ok(available) => assert!(available > 0),
            Err(error) => eprintln!("memory budget unknown here: {error}"),
        }
        assert!(resident_bytes().unwrap() > 0 && peak_bytes().unwrap() > 0);
    }
}
