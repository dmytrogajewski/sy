//! `/proc/diskstats` + `/sys/block/<dev>/queue/*` parser. The
//! `/proc/diskstats` line shape is documented in
//! `Documentation/admin-guide/iostats.rst`:
//! `<major> <minor> <name> <reads> <reads_merged> <sectors_read>
//! <ms_reading> <writes> <writes_merged> <sectors_written>
//! <ms_writing> <io_in_progress> <ms_doing_io> <weighted_ms_doing_io>
//! <discards> <discards_merged> <sectors_discarded> <ms_discarding>
//! <flushes> <ms_flushing>` — fields 14+ landed in Linux 4.18.
//!
//! Like `cpu` and `net`, the sector counters are cumulative since boot;
//! a single sample is meaningless on its own — the aggregator diffs
//! two adjacent samples for IOPS / MB-s. Per-device queue knobs come
//! from sysfs (`logical_block_size`, `nr_requests`, `scheduler`) and
//! live in `sample()` because they vary per-device.

use serde::{Deserialize, Serialize};

/// Cumulative IO counters + in-flight depth for one block device,
/// scraped at one instant. Fields are named after the diskstats spec.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskDevice {
    /// Device name as the kernel prints it (`sda`, `nvme0n1`, `dm-0`,
    /// …). Partitions (`sda1`, `nvme0n1p2`) are kept — the panel can
    /// filter them out if it only wants whole disks.
    pub name: String,
    /// Successful read I/Os completed.
    pub reads: u64,
    /// Successful write I/Os completed.
    pub writes: u64,
    /// 512-byte sectors read (field 6 in iostats.rst).
    pub sectors_read: u64,
    /// 512-byte sectors written (field 10).
    pub sectors_written: u64,
    /// I/Os in progress at the instant of the read (field 12 — the
    /// only non-monotonic field on the line).
    pub io_in_progress: u64,
    /// Logical block size in bytes, read from
    /// `/sys/block/<name>/queue/logical_block_size`. `None` when the
    /// sysfs node is absent (rare on real hardware, common in
    /// containers).
    pub logical_block_size: Option<u32>,
    /// Active queue length cap, read from
    /// `/sys/block/<name>/queue/nr_requests`. `None` when the sysfs
    /// node is absent.
    pub nr_requests: Option<u32>,
}

/// One sensor tick of disk state — every block device the kernel
/// surfaces in `/proc/diskstats`. Order follows the file (major /
/// minor ascending in practice).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskSample {
    pub devices: Vec<DiskDevice>,
}

/// Parse a `/proc/diskstats` blob into per-device counters. Lines
/// with fewer than 14 fields (pre-4.18 kernels' shape) are still
/// accepted as long as the first 12 fields parse; queue-knob fields
/// stay `None` because they are pulled from sysfs in `sample()`.
pub fn parse_diskstats(raw: &str) -> DiskSample {
    let mut devices = Vec::new();
    for line in raw.lines() {
        let fields: Vec<&str> = line.split_ascii_whitespace().collect();
        // major minor name + at least 11 stat fields = 14 tokens.
        if fields.len() < 14 {
            continue;
        }
        let name = fields[2].to_string();
        // Parse the eleven numeric stat fields (offset 3 onwards). A
        // single mis-parse drops the row — better than synthesising a
        // misleading zero.
        let Ok(reads) = fields[3].parse::<u64>() else {
            continue;
        };
        let Ok(sectors_read) = fields[5].parse::<u64>() else {
            continue;
        };
        let Ok(writes) = fields[7].parse::<u64>() else {
            continue;
        };
        let Ok(sectors_written) = fields[9].parse::<u64>() else {
            continue;
        };
        let Ok(io_in_progress) = fields[11].parse::<u64>() else {
            continue;
        };
        devices.push(DiskDevice {
            name,
            reads,
            writes,
            sectors_read,
            sectors_written,
            io_in_progress,
            logical_block_size: None,
            nr_requests: None,
        });
    }
    DiskSample { devices }
}

/// Read a single `/sys/block/<name>/queue/<knob>` u32, returning
/// `None` when the file is absent or malformed. Lives in the I/O
/// section so `parse_diskstats` stays pure.
fn read_queue_u32(name: &str, knob: &str) -> Option<u32> {
    let path = format!("/sys/block/{name}/queue/{knob}");
    std::fs::read_to_string(&path)
        .ok()?
        .trim()
        .parse::<u32>()
        .ok()
}

/// I/O wrapper: reads `/proc/diskstats` once and decorates each
/// device with its `/sys/block/<name>/queue/{logical_block_size,
/// nr_requests}` knobs. The sysfs reads only fire for whole-disk
/// names — partitions don't expose `queue/`.
pub fn sample() -> Option<DiskSample> {
    let raw = std::fs::read_to_string("/proc/diskstats").ok()?;
    let mut s = parse_diskstats(&raw);
    for dev in &mut s.devices {
        dev.logical_block_size = read_queue_u32(&dev.name, "logical_block_size");
        dev.nr_requests = read_queue_u32(&dev.name, "nr_requests");
    }
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::parse_diskstats;

    #[test]
    fn parse_diskstats_handles_lvm() {
        // Mixed fixture: an NVMe whole-disk, a SATA whole-disk with
        // one partition, and an LVM `dm-0` device-mapper target.
        // Field counts are the post-4.18 shape (20 tokens).
        let raw = "\
 259       0 nvme0n1 1000 50 16000 200 500 30 8000 100 0 300 350 0 0 0 0 0 0 0 0
   8       0 sda 200 5 3200 80 100 10 1600 40 1 90 110 0 0 0 0 0 0 0 0
   8       1 sda1 50 1 800 20 25 2 400 10 0 22 28 0 0 0 0 0 0 0 0
 253       0 dm-0 75 0 1200 30 40 0 640 16 0 40 46 0 0 0 0 0 0 0 0
";
        let s = parse_diskstats(raw);
        assert_eq!(s.devices.len(), 4);

        let nvme = &s.devices[0];
        assert_eq!(nvme.name, "nvme0n1");
        assert_eq!(nvme.reads, 1000);
        assert_eq!(nvme.writes, 500);
        assert_eq!(nvme.sectors_read, 16_000);
        assert_eq!(nvme.sectors_written, 8_000);

        let sda = &s.devices[1];
        assert_eq!(sda.name, "sda");
        assert_eq!(sda.io_in_progress, 1);

        let sda1 = &s.devices[2];
        assert_eq!(sda1.name, "sda1");

        let dm = &s.devices[3];
        assert_eq!(dm.name, "dm-0");
        assert_eq!(dm.reads, 75);
        assert_eq!(dm.writes, 40);
        // Queue knobs are sample()'s job, not the parser's.
        assert!(dm.logical_block_size.is_none());
        assert!(dm.nr_requests.is_none());
    }
}
