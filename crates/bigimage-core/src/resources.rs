// SPDX-License-Identifier: GPL-3.0-or-later
//! Conservative concurrency budget shared by the CLI and Prisma.
use std::path::PathBuf;

/// Choose batch concurrency from CPUs, image dimensions and available memory.
/// Leaves half of available RAM to GNOME and other applications, uses at most
/// four jobs, and estimates 32 bytes/pixel for decoded/converted/scratch buffers.
/// This is a scheduling heuristic, not a hard process memory limit.
pub fn batch_worker_count(files: &[PathBuf]) -> usize {
    let cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    let working_set = files
        .iter()
        .filter_map(|p| crate::image_dimensions(p).ok())
        .map(|(w, h)| (u64::from(w) * u64::from(h)).saturating_mul(32))
        .max()
        .unwrap_or(512 * 1024 * 1024)
        .max(64 * 1024 * 1024);
    let available = std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|s| {
            s.lines().find_map(|line| {
                line.strip_prefix("MemAvailable:")
                    .and_then(|rest| rest.split_whitespace().next())
                    .and_then(|number| number.parse::<u64>().ok())
                    .map(|kb| kb.saturating_mul(1024))
            })
        })
        .unwrap_or(1024 * 1024 * 1024);
    budget(cpus, files.len(), available, working_set)
}

fn budget(cpus: usize, jobs: usize, available: u64, working_set: u64) -> usize {
    let memory_jobs = (available / 2 / working_set.max(1)).clamp(1, 4) as usize;
    cpus.saturating_sub(1).max(1).min(jobs.max(1)).min(memory_jobs)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn budget_reserves_cpu_and_ram_and_handles_low_memory() {
        assert_eq!(budget(16, 100, 4096, 1024), 2);
        assert_eq!(budget(16, 100, 16384, 1024), 4);
        assert_eq!(budget(2, 100, 16384, 1024), 1);
        assert_eq!(budget(16, 1, 16384, 1024), 1);
        assert_eq!(budget(1, 0, 0, 1024), 1);
    }
}
