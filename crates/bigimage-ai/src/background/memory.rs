// SPDX-License-Identifier: GPL-3.0-or-later
//! Admission policy for the measured BiRefNet FP32 working set.
use super::BgError;
const GIB: u64 = 1024 * 1024 * 1024;
const REQUIRED: u64 = 8 * GIB;
#[cfg(feature = "onnx")]
const MAX_CREDIT: u64 = 13 * GIB / 2;

fn kilobytes(text: &str, key: &str) -> Option<u64> {
    let rest = text.lines().find_map(|line| line.strip_prefix(key))?;
    let mut fields = rest.split_whitespace();
    let value: u64 = fields.next()?.parse().ok()?;
    if fields.next()? != "kB" {
        return None;
    }
    value.checked_mul(1024)
}
fn admit(total: u64, available: u64, credited: u64, workspace: u64) -> Result<(), BgError> {
    let required = REQUIRED.saturating_sub(credited).saturating_add(workspace);
    if total < REQUIRED || available < required {
        return Err(BgError::Memory(format!(
            "BiRefNet FP32 requer um orçamento inicial de 8 GiB de RAM livre; \
             disponíveis agora: {:.2} GiB, necessários nesta etapa: {:.2} GiB. \
             Visualizar e converter imagens continuam disponíveis. Swap não conta como RAM livre.",
            available as f64 / GIB as f64,
            required as f64 / GIB as f64,
        )));
    }
    Ok(())
}
#[cfg(feature = "onnx")]
fn unknown(error: impl std::fmt::Display) -> BgError {
    BgError::Memory(format!("não foi possível conferir o orçamento de memória: {error}"))
}
#[cfg(feature = "onnx")]
fn process_value(key: &str) -> Result<u64, BgError> {
    let status = std::fs::read_to_string("/proc/self/status").map_err(unknown)?;
    kilobytes(&status, key).ok_or_else(|| unknown("/proc/self/status incompleto"))
}
#[cfg(feature = "onnx")]
pub(super) fn resident_bytes() -> Result<u64, BgError> {
    process_value("VmRSS:")
}

#[cfg(feature = "onnx")]
fn capacity() -> Result<(u64, u64), BgError> {
    use std::path::{Component, Path};
    let info = std::fs::read_to_string("/proc/meminfo").map_err(unknown)?;
    let mut total = kilobytes(&info, "MemTotal:").ok_or_else(|| unknown("MemTotal ausente"))?;
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
            let limit_path = directory.join("memory.max");
            match std::fs::read_to_string(&limit_path) {
                Ok(limit) if limit.trim() != "max" => {
                    let maximum: u64 = limit.trim().parse().map_err(unknown)?;
                    let current: u64 = std::fs::read_to_string(directory.join("memory.current"))
                        .map_err(unknown)?
                        .trim()
                        .parse()
                        .map_err(unknown)?;
                    total = total.min(maximum);
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
        total = total.min(limit);
        available = available.min(limit.saturating_sub(process_value("VmSize:")?));
    }
    Ok((total, available.min(total)))
}

#[cfg(feature = "onnx")]
pub(super) struct MemoryGuard {
    credited: u64,
}
#[cfg(feature = "onnx")]
impl MemoryGuard {
    pub(super) fn new() -> Result<Self, BgError> {
        let guard = Self { credited: 0 };
        guard.check(0)?;
        Ok(guard)
    }
    pub(super) fn check(&self, workspace: u64) -> Result<(), BgError> {
        let (total, available) = capacity()?;
        admit(total, available, self.credited.min(resident_bytes()?), workspace)
    }
    pub(super) fn account_native_growth(&mut self, before: u64) -> Result<(), BgError> {
        let after = resident_bytes()?;
        self.credited = if after >= before {
            self.credited.saturating_add(after - before)
        } else {
            self.credited.saturating_sub(before - after)
        }
        .min(MAX_CREDIT);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn four_gib_is_refused_even_with_swap_or_residency_credit() {
        assert!(admit(4 * GIB, 3 * GIB, 0, 0).is_err());
        assert!(admit(4 * GIB, 3 * GIB, 6 * GIB, 0).is_err());
    }
    #[test]
    fn reused_native_memory_does_not_require_a_second_full_budget() {
        assert!(admit(16 * GIB, 8 * GIB, 0, 0).is_ok());
        assert!(admit(16 * GIB, 2 * GIB, 6 * GIB, 0).is_ok());
        assert!(admit(16 * GIB, GIB, 6 * GIB, 0).is_err());
        assert!(admit(16 * GIB, 2 * GIB, 6 * GIB, GIB).is_err());
    }
    #[test]
    fn low_available_memory_and_missing_units_are_rejected() {
        assert!(admit(32 * GIB, 7 * GIB, 0, 0).is_err());
        assert_eq!(
            kilobytes("MemAvailable: 4096 kB\nSwapFree: 9000000 kB", "MemAvailable:"),
            Some(4 * 1024 * 1024)
        );
        assert_eq!(kilobytes("MemAvailable: 4096 MB", "MemAvailable:"), None);
        assert_eq!(kilobytes("MemAvailable: 18446744073709551615 kB", "MemAvailable:"), None);
    }
}
