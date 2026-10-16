// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Samples the instance's CPU, memory, disk and network once a second
//! and sums the samples up in windows of [`WINDOW_SAMPLES`].
//!
//! Everything comes from the kernel: `/proc/stat`, `/proc/meminfo`,
//! `/proc/net/dev`, `statvfs` of the root file system, and the cgroup v2
//! files of the agent's own cgroup, which holds the runner and the job's
//! processes. A file that cannot be read counts as zero, so a missing
//! cgroup controller costs one number, not the window.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

/// Samples in one window, one a second.
pub const WINDOW_SAMPLES: usize = 10;

/// Cumulative CPU time from the `cpu` line of `/proc/stat`, in ticks.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CpuTimes {
    /// All time.
    pub total: u64,
    /// Time spent idle or waiting for I/O.
    pub idle: u64,
    /// Time the hypervisor gave to another guest.
    pub steal: u64,
}

impl CpuTimes {
    /// Busy and steal shares between `earlier` and `self`, in percent.
    fn since(self, earlier: Self) -> (f64, f64) {
        let total = self.total.saturating_sub(earlier.total);
        if total == 0 {
            return (0.0, 0.0);
        }
        let idle = self.idle.saturating_sub(earlier.idle);
        let steal = self.steal.saturating_sub(earlier.steal);
        (
            percent(total.saturating_sub(idle), total),
            percent(steal, total),
        )
    }
}

#[allow(clippy::cast_precision_loss)] // Ticks stay far below 2^52.
fn percent(part: u64, total: u64) -> f64 {
    part as f64 * 100.0 / total as f64
}

/// One reading of the counters and gauges.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Reading {
    pub cpu: CpuTimes,
    /// `MemTotal - MemAvailable`, in bytes.
    pub mem_used: u64,
    /// `MemTotal`, in bytes.
    pub mem_total: u64,
    /// The cgroup's `memory.peak`, in bytes.
    pub cgroup_peak: u64,
    /// The cgroup's `oom_kill` count.
    pub oom_kills: u64,
    /// Bytes used on the root file system.
    pub disk_used: u64,
    /// Bytes received and sent on every interface but loopback.
    pub net_rx: u64,
    pub net_tx: u64,
}

/// A window of samples, as the agent reports it. Byte counts and times
/// are whole numbers sent as JSON numbers.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Window {
    /// When the window ended, in Unix seconds.
    pub at: u64,
    /// Seconds the window covers.
    pub seconds: u64,
    /// Busy CPU, all cores together, in percent: lowest, mean and
    /// highest one-second share.
    pub cpu_min: f64,
    pub cpu_avg: f64,
    pub cpu_max: f64,
    /// Mean share of CPU time stolen by the hypervisor, in percent.
    pub cpu_steal: f64,
    /// Highest memory in use, in bytes.
    pub mem_max: u64,
    pub mem_total: u64,
    /// The cgroup's peak memory at the end of the window, in bytes.
    pub cgroup_peak: u64,
    /// Processes the OOM killer ended during the window.
    pub oom_kills: u64,
    /// Bytes used on the root file system at the end of the window.
    pub disk_used: u64,
    /// Bytes received and sent during the window.
    pub net_rx: u64,
    pub net_tx: u64,
}

/// Collects readings into windows.
pub struct Sampler {
    root: PathBuf,
    cgroup: Option<PathBuf>,
    /// The reading the window started from.
    start: Option<(u64, Reading)>,
    /// The latest reading.
    last: Option<Reading>,
    /// Busy and steal share of each second so far.
    seconds: Vec<(f64, f64)>,
    mem_max: u64,
}

impl Sampler {
    /// A sampler over the files under `root`, `/` on an instance.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let cgroup = std::fs::read_to_string(root.join("proc/self/cgroup"))
            .ok()
            .and_then(|text| cgroup_dir(&text))
            .map(|dir| root.join("sys/fs/cgroup").join(dir));
        Self {
            root,
            cgroup,
            start: None,
            last: None,
            seconds: Vec::with_capacity(WINDOW_SAMPLES),
            mem_max: 0,
        }
    }

    /// Takes a reading. Returns the window it completes, if any.
    pub fn sample(&mut self) -> Option<Window> {
        let reading = self.read();
        self.add(unix_now(), reading)
    }

    /// Ends the window early, as the runner exits. Returns it when it
    /// holds at least one second.
    pub fn finish(&mut self) -> Option<Window> {
        let reading = self.read();
        let now = unix_now();
        self.add(now, reading).or_else(|| self.close(now))
    }

    fn add(&mut self, now: u64, reading: Reading) -> Option<Window> {
        if let Some(last) = self.last {
            self.seconds.push(reading.cpu.since(last.cpu));
            self.mem_max = self.mem_max.max(reading.mem_used);
        }
        self.last = Some(reading);
        if self.start.is_none() {
            self.start = Some((now, reading));
            self.mem_max = reading.mem_used;
        }
        if self.seconds.len() >= WINDOW_SAMPLES {
            return self.close(now);
        }
        None
    }

    fn close(&mut self, now: u64) -> Option<Window> {
        let (started, first) = self.start?;
        let last = self.last?;
        if self.seconds.is_empty() {
            return None;
        }
        #[allow(clippy::cast_precision_loss)] // At most WINDOW_SAMPLES.
        let count = self.seconds.len() as f64;
        let busy = self.seconds.iter().map(|(busy, _)| *busy);
        let window = Window {
            at: now,
            seconds: now.saturating_sub(started).max(1),
            cpu_min: busy.clone().fold(f64::INFINITY, f64::min),
            cpu_avg: busy.clone().sum::<f64>() / count,
            cpu_max: busy.fold(0.0, f64::max),
            cpu_steal: self.seconds.iter().map(|(_, steal)| steal).sum::<f64>() / count,
            mem_max: self.mem_max,
            mem_total: last.mem_total,
            cgroup_peak: last.cgroup_peak,
            oom_kills: last.oom_kills.saturating_sub(first.oom_kills),
            disk_used: last.disk_used,
            net_rx: last.net_rx.saturating_sub(first.net_rx),
            net_tx: last.net_tx.saturating_sub(first.net_tx),
        };
        // The next window starts where this one ended.
        self.start = Some((now, last));
        self.seconds.clear();
        self.mem_max = last.mem_used;
        Some(window)
    }

    fn read(&self) -> Reading {
        let text = |path: &Path| std::fs::read_to_string(path).unwrap_or_default();
        let (mem_used, mem_total) = meminfo(&text(&self.root.join("proc/meminfo")));
        let (net_rx, net_tx) = net_dev(&text(&self.root.join("proc/net/dev")));
        let (cgroup_peak, oom_kills) = self.cgroup.as_ref().map_or((0, 0), |dir| {
            (
                text(&dir.join("memory.peak")).trim().parse().unwrap_or(0),
                oom_kill(&text(&dir.join("memory.events"))),
            )
        });
        Reading {
            cpu: proc_stat(&text(&self.root.join("proc/stat"))),
            mem_used,
            mem_total,
            cgroup_peak,
            oom_kills,
            disk_used: disk_used(&self.root),
            net_rx,
            net_tx,
        }
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

/// The `cpu` line of `/proc/stat`: user, nice, system, idle, iowait,
/// irq, softirq, steal. Guest time is already in user and nice.
fn proc_stat(text: &str) -> CpuTimes {
    let Some(line) = text.lines().find(|line| line.starts_with("cpu ")) else {
        return CpuTimes::default();
    };
    let fields: Vec<u64> = line
        .split_ascii_whitespace()
        .skip(1)
        .take(8)
        .map(|field| field.parse().unwrap_or(0))
        .collect();
    let field = |index: usize| fields.get(index).copied().unwrap_or(0);
    CpuTimes {
        total: fields.iter().sum(),
        idle: field(3) + field(4),
        steal: field(7),
    }
}

/// Used and total memory from `/proc/meminfo`, in bytes.
fn meminfo(text: &str) -> (u64, u64) {
    let kib = |name: &str| {
        text.lines()
            .find_map(|line| line.strip_prefix(name)?.strip_prefix(':'))
            .and_then(|rest| rest.split_ascii_whitespace().next()?.parse::<u64>().ok())
            .unwrap_or(0)
            * 1024
    };
    let total = kib("MemTotal");
    (total.saturating_sub(kib("MemAvailable")), total)
}

/// Bytes received and sent on every interface but loopback, from
/// `/proc/net/dev`.
fn net_dev(text: &str) -> (u64, u64) {
    text.lines()
        .filter_map(|line| line.split_once(':'))
        .filter(|(name, _)| name.trim() != "lo")
        .filter_map(|(_, counters)| {
            let counters: Vec<&str> = counters.split_ascii_whitespace().collect();
            Some((
                counters.first()?.parse().ok()?,
                counters.get(8)?.parse().ok()?,
            ))
        })
        .fold((0, 0), |(rx, tx), (r, t): (u64, u64)| (rx + r, tx + t))
}

/// The cgroup v2 path in `/proc/self/cgroup`, relative to the cgroup
/// mount.
fn cgroup_dir(text: &str) -> Option<String> {
    let path = text.lines().find_map(|line| line.strip_prefix("0::"))?;
    Some(path.trim().trim_start_matches('/').to_owned())
}

/// `oom_kill` from a cgroup's `memory.events`.
fn oom_kill(text: &str) -> u64 {
    text.lines()
        .find_map(|line| line.strip_prefix("oom_kill "))
        .and_then(|count| count.trim().parse().ok())
        .unwrap_or(0)
}

// Block counts are u64 on Linux and u32 on macOS, where the agent is
// only built for development.
#[allow(clippy::useless_conversion)]
fn disk_used(root: &Path) -> u64 {
    nix::sys::statvfs::statvfs(root).map_or(0, |stat| {
        u64::from(stat.blocks().saturating_sub(stat.blocks_free())) * stat.fragment_size()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_cpu_times() {
        let stat = "cpu  100 5 50 800 20 1 2 22 0 0\ncpu0 1 2 3 4 5 6 7 8 0 0\nintr 1\n";
        assert_eq!(
            proc_stat(stat),
            CpuTimes {
                total: 1000,
                idle: 820,
                steal: 22,
            }
        );
        assert_eq!(proc_stat(""), CpuTimes::default());
    }

    #[test]
    fn cpu_shares() {
        let earlier = CpuTimes {
            total: 1000,
            idle: 800,
            steal: 0,
        };
        let later = CpuTimes {
            total: 1200,
            idle: 850,
            steal: 10,
        };
        assert_eq!(later.since(earlier), (75.0, 5.0));
        assert_eq!(earlier.since(earlier), (0.0, 0.0));
    }

    #[test]
    fn reads_memory() {
        let text = "MemTotal:       16000 kB\nMemFree:  1000 kB\nMemAvailable:   12000 kB\n";
        assert_eq!(meminfo(text), (4000 * 1024, 16000 * 1024));
        assert_eq!(meminfo(""), (0, 0));
    }

    #[test]
    fn reads_network_without_loopback() {
        let text = "Inter-|   Receive  |  Transmit\n \
            face |bytes packets errs drop fifo frame compressed multicast|bytes\n    \
            lo: 9000 10 0 0 0 0 0 0 9000 10 0 0 0 0 0 0\n  \
            ens5: 1000 10 0 0 0 0 0 0 300 4 0 0 0 0 0 0\n\
            docker0: 50 1 0 0 0 0 0 0 20 1 0 0 0 0 0 0\n";
        assert_eq!(net_dev(text), (1050, 320));
    }

    #[test]
    fn reads_cgroup_files() {
        assert_eq!(
            cgroup_dir("0::/system.slice/hatchrun-agent.service\n").as_deref(),
            Some("system.slice/hatchrun-agent.service")
        );
        assert_eq!(cgroup_dir("1:name=systemd:/\n"), None);
        assert_eq!(
            oom_kill("low 0\nhigh 0\nmax 3\noom 2\noom_kill 2\noom_group_kill 0\n"),
            2
        );
        assert_eq!(oom_kill(""), 0);
    }

    fn reading(total: u64, idle: u64, mem_used: u64, net_rx: u64, oom_kills: u64) -> Reading {
        Reading {
            cpu: CpuTimes {
                total,
                idle,
                steal: 0,
            },
            mem_used,
            mem_total: 8000,
            cgroup_peak: 5000,
            oom_kills,
            disk_used: 700,
            net_rx,
            net_tx: 0,
        }
    }

    #[test]
    fn windows_sum_up_ten_seconds() {
        let mut sampler = Sampler::new("/nonexistent");
        assert_eq!(sampler.add(100, reading(0, 0, 1000, 0, 0)), None);
        for second in 1..WINDOW_SAMPLES as u64 {
            // Half busy, then fully busy on the fifth second.
            let busy = if second == 5 { 100 } else { 50 };
            let idle = 100 - busy;
            let total = second * 100;
            let earlier_idle = (1..second)
                .map(|s| if s == 5 { 0 } else { 50 })
                .sum::<u64>();
            assert_eq!(
                sampler.add(
                    100 + second,
                    reading(total, earlier_idle + idle, 1000 + second, second * 10, 0)
                ),
                None
            );
        }
        let window = sampler.add(110, reading(1000, 450, 3000, 100, 2)).unwrap();
        assert_eq!(window.at, 110);
        assert_eq!(window.seconds, 10);
        assert_eq!(window.cpu_min, 50.0);
        assert_eq!(window.cpu_max, 100.0);
        assert_eq!(window.cpu_avg, 55.0);
        assert_eq!(window.mem_max, 3000);
        assert_eq!(window.mem_total, 8000);
        assert_eq!(window.net_rx, 100);
        assert_eq!(window.oom_kills, 2);

        // The next window starts from the last reading.
        assert_eq!(sampler.add(111, reading(1100, 500, 2000, 150, 2)), None);
        let rest = sampler.close(111).unwrap();
        assert_eq!(rest.seconds, 1);
        assert_eq!(rest.net_rx, 50);
        assert_eq!(rest.oom_kills, 0);
        assert_eq!(rest.mem_max, 3000);
        assert_eq!(sampler.close(112), None);
    }
}
