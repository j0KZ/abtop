//! Lightweight host vitals: CPU%, MEM%, 1-min load average, swap, root disk
//! and network throughput.
//!
//! Reads `/proc` directly on Linux and uses `sysinfo` on Windows and macOS.
//! Returns `None` on other platforms (for now); callers should treat absence
//! as "metrics unavailable" and render a graceful fallback.

use serde::Serialize;

#[derive(Debug, Clone, Copy, Serialize)]
pub struct HostMetrics {
    /// Aggregate CPU usage in percent (0.0 - 100.0). Computed across all cores.
    pub cpu_pct: f64,
    /// Used memory in percent (0.0 - 100.0). Used = MemTotal - MemAvailable.
    pub mem_pct: f64,
    /// 1-minute load average.
    pub load1: f64,
    /// Swap usage in percent. `None` when the host has no swap configured,
    /// which is a normal state and must not be drawn as 0%.
    pub swap_pct: Option<f64>,
    /// Usage of the filesystem holding `/`, in percent.
    pub disk_pct: Option<f64>,
    /// Bytes per second across every non-loopback interface, averaged over the
    /// gap since the previous tick. `None` on the first tick, which has no
    /// previous sample to subtract.
    pub net_rx_bps: Option<u64>,
    /// Outbound counterpart of `net_rx_bps`.
    pub net_tx_bps: Option<u64>,
}

/// Stateful sampler that remembers the previous `/proc/stat` snapshot so it
/// can compute CPU usage as a delta between ticks. On Windows and macOS it
/// instead holds a `sysinfo::System` across ticks for the same reason: CPU
/// usage is a delta between two refreshes.
#[derive(Debug, Default)]
pub struct HostSampler {
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    prev: Option<CpuTimes>,
    /// Previous cumulative network byte counters, with the instant they were
    /// read. The kernel counters only ever grow, so a rate needs both.
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    prev_net: Option<(NetCounters, std::time::Instant)>,
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    sys: sysinfo_impl::SysinfoSampler,
}

/// Cumulative byte counters summed across every non-loopback interface.
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
#[derive(Debug, Clone, Copy)]
struct NetCounters {
    rx: u64,
    tx: u64,
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
#[derive(Debug, Clone, Copy)]
struct CpuTimes {
    /// All non-idle jiffies (user + nice + system + irq + softirq + steal).
    busy: u64,
    /// idle + iowait.
    idle: u64,
}

impl HostSampler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Sample current host metrics. Returns `None` if the platform has no
    /// metrics source (a unix that is neither Linux nor macOS, for now).
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    pub fn sample(&mut self) -> Option<HostMetrics> {
        let cpu_pct = self.sample_cpu()?;
        let mem_pct = sample_mem()?;
        let load1 = sample_load()?;
        // Only the three above gate the sample. Swap, disk and network are
        // each optional on their own, so a host with no swap or an unreadable
        // /proc/net/dev still reports the vitals it does have.
        let (net_rx_bps, net_tx_bps) = self.sample_net();
        Some(HostMetrics {
            cpu_pct,
            mem_pct,
            load1,
            swap_pct: sample_swap(),
            disk_pct: sample_disk(),
            net_rx_bps,
            net_tx_bps,
        })
    }

    /// Convert the cumulative counters into a per-second rate. Returns
    /// `(None, None)` on the first tick and whenever the counters go backwards,
    /// which happens when an interface is removed between two reads.
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    fn sample_net(&mut self) -> (Option<u64>, Option<u64>) {
        let now = std::time::Instant::now();
        let Some(counters) = read_net_counters() else {
            return (None, None);
        };
        let out = match self.prev_net {
            Some((prev, prev_at)) => {
                let secs = now.duration_since(prev_at).as_secs_f64();
                if secs > 0.0 && counters.rx >= prev.rx && counters.tx >= prev.tx {
                    (
                        Some(((counters.rx - prev.rx) as f64 / secs) as u64),
                        Some(((counters.tx - prev.tx) as f64 / secs) as u64),
                    )
                } else {
                    (None, None)
                }
            }
            None => (None, None),
        };
        self.prev_net = Some((counters, now));
        out
    }

    /// Windows and macOS: CPU/MEM via `sysinfo`. macOS reports a real 1-min
    /// load average; Windows has none, so there `load1` stays 0.0 (callers
    /// should label it N/A).
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    pub fn sample(&mut self) -> Option<HostMetrics> {
        self.sys.sample()
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    fn sample_cpu(&mut self) -> Option<f64> {
        let now = read_cpu_times()?;
        let pct = match self.prev {
            Some(prev) => {
                let busy_d = now.busy.saturating_sub(prev.busy) as f64;
                let idle_d = now.idle.saturating_sub(prev.idle) as f64;
                let total = busy_d + idle_d;
                if total > 0.0 {
                    (busy_d / total) * 100.0
                } else {
                    0.0
                }
            }
            None => 0.0,
        };
        self.prev = Some(now);
        Some(pct)
    }
}

#[cfg(target_os = "linux")]
fn read_cpu_times() -> Option<CpuTimes> {
    let stat = std::fs::read_to_string("/proc/stat").ok()?;
    let line = stat.lines().next()?;
    let mut fields = line.split_whitespace();
    if fields.next()? != "cpu" {
        return None;
    }
    let nums: Vec<u64> = fields.filter_map(|f| f.parse().ok()).collect();
    // Layout: user nice system idle iowait irq softirq steal guest guest_nice
    if nums.len() < 4 {
        return None;
    }
    let user = nums[0];
    let nice = nums[1];
    let system = nums[2];
    let idle = nums[3];
    let iowait = *nums.get(4).unwrap_or(&0);
    let irq = *nums.get(5).unwrap_or(&0);
    let softirq = *nums.get(6).unwrap_or(&0);
    let steal = *nums.get(7).unwrap_or(&0);
    Some(CpuTimes {
        busy: user + nice + system + irq + softirq + steal,
        idle: idle + iowait,
    })
}

#[cfg(target_os = "linux")]
fn sample_mem() -> Option<f64> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    let mut total = 0u64;
    let mut avail = 0u64;
    for line in meminfo.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            total = parse_kb(rest)?;
        } else if let Some(rest) = line.strip_prefix("MemAvailable:") {
            avail = parse_kb(rest)?;
        }
        if total > 0 && avail > 0 {
            break;
        }
    }
    if total == 0 {
        return None;
    }
    let used = total.saturating_sub(avail) as f64;
    Some((used / total as f64) * 100.0)
}

#[cfg(target_os = "linux")]
fn parse_kb(s: &str) -> Option<u64> {
    s.split_whitespace().next().and_then(|n| n.parse().ok())
}

#[cfg(target_os = "linux")]
fn sample_load() -> Option<f64> {
    let s = std::fs::read_to_string("/proc/loadavg").ok()?;
    s.split_whitespace().next().and_then(|n| n.parse().ok())
}

/// Swap usage from /proc/meminfo. `SwapTotal` of 0 means no swap is configured
/// and yields `None`, so the caller can leave the field out rather than draw a
/// 0% that looks like an idle swap.
#[cfg(target_os = "linux")]
fn sample_swap() -> Option<f64> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    let mut total = 0u64;
    let mut free = 0u64;
    for line in meminfo.lines() {
        if let Some(rest) = line.strip_prefix("SwapTotal:") {
            total = parse_kb(rest)?;
        } else if let Some(rest) = line.strip_prefix("SwapFree:") {
            free = parse_kb(rest)?;
        }
    }
    if total == 0 {
        return None;
    }
    let used = total.saturating_sub(free) as f64;
    Some((used / total as f64) * 100.0)
}

/// Usage of the filesystem holding `/`, via statvfs.
///
/// Deliberately matches what `df` prints, which is used / (used + available),
/// NOT used / total. The two differ by the blocks a filesystem reserves for
/// root: on a 1 TB ext4 with 5% reserved they read 5% and 10%, and the number
/// people cross-check against is the one `df` gives.
#[cfg(target_os = "linux")]
fn sample_disk() -> Option<f64> {
    // SAFETY: statvfs writes into a zeroed struct we own, and the path is a
    // NUL-terminated literal. The return value is checked before reading it.
    let stat = unsafe {
        let mut stat: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(c"/".as_ptr(), &mut stat) != 0 {
            return None;
        }
        stat
    };
    let used = stat.f_blocks.saturating_sub(stat.f_bfree) as f64;
    let avail = stat.f_bavail as f64;
    let usable = used + avail;
    if usable <= 0.0 {
        return None;
    }
    Some((used / usable) * 100.0)
}

/// Sum the cumulative byte counters of every interface in /proc/net/dev except
/// loopback, whose traffic never leaves the machine.
#[cfg(target_os = "linux")]
fn read_net_counters() -> Option<NetCounters> {
    let dev = std::fs::read_to_string("/proc/net/dev").ok()?;
    Some(parse_net_dev(&dev))
}

/// Split out from the read so it can be tested: a container or a network
/// namespace can legitimately have nothing but loopback, in which case the
/// live file exercises none of this.
///
/// A line is "iface: <8 receive fields> <8 transmit fields>", so received
/// bytes sit at index 0 and transmitted bytes at index 8. Lines that do not
/// fit that shape are skipped rather than failing the whole read, since one
/// malformed interface should not blank the network figure.
#[cfg(target_os = "linux")]
fn parse_net_dev(dev: &str) -> NetCounters {
    let mut rx = 0u64;
    let mut tx = 0u64;
    for line in dev.lines().skip(2) {
        let Some((iface, rest)) = line.split_once(':') else {
            continue;
        };
        if iface.trim() == "lo" {
            continue;
        }
        let fields: Vec<&str> = rest.split_whitespace().collect();
        if fields.len() < 9 {
            continue;
        }
        rx = rx.saturating_add(fields[0].parse().unwrap_or(0));
        tx = tx.saturating_add(fields[8].parse().unwrap_or(0));
    }
    NetCounters { rx, tx }
}

#[cfg(all(
    not(target_os = "linux"),
    not(target_os = "windows"),
    not(target_os = "macos")
))]
fn read_cpu_times() -> Option<CpuTimes> {
    None
}
#[cfg(all(
    not(target_os = "linux"),
    not(target_os = "windows"),
    not(target_os = "macos")
))]
fn sample_mem() -> Option<f64> {
    None
}
#[cfg(all(
    not(target_os = "linux"),
    not(target_os = "windows"),
    not(target_os = "macos")
))]
fn sample_load() -> Option<f64> {
    None
}
#[cfg(all(
    not(target_os = "linux"),
    not(target_os = "windows"),
    not(target_os = "macos")
))]
fn sample_swap() -> Option<f64> {
    None
}
#[cfg(all(
    not(target_os = "linux"),
    not(target_os = "windows"),
    not(target_os = "macos")
))]
fn sample_disk() -> Option<f64> {
    None
}
#[cfg(all(
    not(target_os = "linux"),
    not(target_os = "windows"),
    not(target_os = "macos")
))]
fn read_net_counters() -> Option<NetCounters> {
    None
}

/// Host metrics via `sysinfo`, on the platforms with no `/proc` to read:
/// Windows, and macOS where the `/proc`-based path returns nothing.
#[cfg(any(target_os = "windows", target_os = "macos"))]
mod sysinfo_impl {
    use super::HostMetrics;
    use sysinfo::{Disks, Networks, System};

    /// Holds a `System` across ticks: `sysinfo` computes CPU usage as the
    /// delta between two refreshes, so a freshly constructed `System` always
    /// reports 0. The collector tick (~2s) is well above
    /// `sysinfo::MINIMUM_CPU_UPDATE_INTERVAL`.
    pub struct SysinfoSampler {
        sys: System,
        /// Held across ticks: `NetworkData::received()` reports bytes since the
        /// previous refresh, so a fresh `Networks` would always read zero.
        nets: Networks,
        /// The instant of the previous refresh, needed to turn the per-tick
        /// byte delta into a per-second rate.
        prev_net_at: Option<std::time::Instant>,
        /// False until the first refresh has happened; the first sample has
        /// no CPU delta yet, so report 0.0 (mirrors the Linux first-tick
        /// behavior where `prev` is `None`).
        primed: bool,
    }

    impl Default for SysinfoSampler {
        fn default() -> Self {
            Self {
                sys: System::new(),
                nets: Networks::new_with_refreshed_list(),
                prev_net_at: None,
                primed: false,
            }
        }
    }

    impl std::fmt::Debug for SysinfoSampler {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("SysinfoSampler")
                .field("primed", &self.primed)
                .finish()
        }
    }

    impl SysinfoSampler {
        pub fn sample(&mut self) -> Option<HostMetrics> {
            self.sys.refresh_cpu_usage();
            self.sys.refresh_memory();

            let cpu_pct = if self.primed {
                self.sys.global_cpu_usage() as f64
            } else {
                0.0
            };
            self.primed = true;

            let total = self.sys.total_memory();
            if total == 0 {
                return None;
            }
            let mem_pct = (self.sys.used_memory() as f64 / total as f64) * 100.0;

            // macOS has a real load average. Windows has none, and
            // `load_average()` is documented as unsupported there, so keep the
            // wire shape stable by reporting 0.0 instead of an approximation.
            #[cfg(target_os = "macos")]
            let load1 = System::load_average().one;
            #[cfg(target_os = "windows")]
            let load1 = 0.0;

            // A host with no swap reports a total of 0. That is not 0% used, it
            // is "no swap", so it stays absent rather than being drawn as idle.
            let swap_total = self.sys.total_swap();
            let swap_pct = if swap_total > 0 {
                Some((self.sys.used_swap() as f64 / swap_total as f64) * 100.0)
            } else {
                None
            };

            let (net_rx_bps, net_tx_bps) = self.sample_net();

            Some(HostMetrics {
                cpu_pct,
                mem_pct,
                load1,
                swap_pct,
                disk_pct: sample_disk(),
                net_rx_bps,
                net_tx_bps,
            })
        }

        /// Per-second network rate across every interface. `received()` gives
        /// bytes since the previous refresh, so this only divides by the elapsed
        /// time. Returns `(None, None)` on the first tick, which has no gap to
        /// divide by.
        fn sample_net(&mut self) -> (Option<u64>, Option<u64>) {
            let now = std::time::Instant::now();
            self.nets.refresh();
            let out = match self.prev_net_at {
                Some(prev_at) => {
                    let secs = now.duration_since(prev_at).as_secs_f64();
                    if secs > 0.0 {
                        let mut rx = 0u64;
                        let mut tx = 0u64;
                        for (_name, data) in &self.nets {
                            rx = rx.saturating_add(data.received());
                            tx = tx.saturating_add(data.transmitted());
                        }
                        (
                            Some((rx as f64 / secs) as u64),
                            Some((tx as f64 / secs) as u64),
                        )
                    } else {
                        (None, None)
                    }
                }
                None => (None, None),
            };
            self.prev_net_at = Some(now);
            out
        }
    }

    /// Usage of the disk holding `/`. The mount list is rebuilt on every call
    /// rather than cached, because a volume can be mounted or ejected between
    /// ticks, which on macOS is routine.
    fn sample_disk() -> Option<f64> {
        let disks = Disks::new_with_refreshed_list();
        let root = disks
            .list()
            .iter()
            .find(|d| d.mount_point() == std::path::Path::new("/"))?;
        // NOT the same as the Linux branch, and it cannot be: `df` semantics
        // need the free-block count that includes the root reservation, and
        // sysinfo exposes only total and available. So this is used / total,
        // which over-reports by whatever the filesystem reserves. On APFS that
        // reservation is small; on a reserved-heavy filesystem this would read
        // higher than `df`.
        let total = root.total_space();
        if total == 0 {
            return None;
        }
        let used = total.saturating_sub(root.available_space()) as f64;
        Some((used / total as f64) * 100.0)
    }
}

/// Aggregate per-session metrics into a single agent-wide summary.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct AgentAggregate {
    pub mem_mb: u64,
    /// Average context window fill across active sessions (0.0 - 100.0).
    pub avg_ctx_pct: f64,
    pub active_count: usize,
}

impl AgentAggregate {
    pub fn from_sessions(sessions: &[crate::model::AgentSession]) -> Self {
        let mut mem_mb = 0u64;
        let mut ctx_sum = 0.0;
        let mut ctx_n = 0usize;
        let mut active = 0usize;
        for s in sessions {
            mem_mb = mem_mb.saturating_add(s.mem_mb);
            if s.context_percent > 0.0 {
                ctx_sum += s.context_percent;
                ctx_n += 1;
            }
            if s.status.is_active() {
                active += 1;
            }
        }
        let avg_ctx_pct = if ctx_n > 0 {
            ctx_sum / ctx_n as f64
        } else {
            0.0
        };
        Self {
            mem_mb,
            avg_ctx_pct,
            active_count: active,
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    const DEV: &str = "\
Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
    lo: 1111111    9999    0    0    0     0          0         0  2222222    8888    0    0    0     0       0          0
  eth0:  500000    4000    0    0    0     0          0         0    70000    3000    0    0    0     0       0          0
 wlan0:  250000    2000    0    0    0     0          0         0    30000    1000    0    0    0     0       0          0
";

    #[test]
    fn net_dev_sums_interfaces_and_skips_loopback() {
        let c = parse_net_dev(DEV);
        // eth0 + wlan0, with lo's much larger counters deliberately excluded.
        assert_eq!(c.rx, 750_000, "rx should sum eth0+wlan0 only");
        assert_eq!(c.tx, 100_000, "tx should sum eth0+wlan0 only");
    }

    #[test]
    fn net_dev_skips_malformed_lines_without_losing_the_rest() {
        let mixed = format!("{DEV}  bad: 1 2 3\nnocolon here\n");
        let c = parse_net_dev(&mixed);
        assert_eq!(
            c.rx, 750_000,
            "a short or colon-less line must not drop eth0"
        );
    }

    #[test]
    fn net_dev_with_only_loopback_is_zero_not_missing() {
        let only_lo = DEV.lines().take(3).collect::<Vec<_>>().join("\n");
        let c = parse_net_dev(&only_lo);
        assert_eq!((c.rx, c.tx), (0, 0));
    }

    #[test]
    fn first_tick_reports_no_rate() {
        // The counters are cumulative since boot, so a single reading cannot
        // become a rate. Reporting 0 here would look like an idle link.
        let mut s = HostSampler::new();
        let first = s.sample().expect("linux always samples");
        assert_eq!(first.net_rx_bps, None);
        assert_eq!(first.net_tx_bps, None);
    }
}
