//! Opt-in timeout confirmation. Comparable-volume controls bypass the Linux
//! page cache on the exact pinned file, on explicitly supported filesystems.
use super::*;
use serde::Serialize;

pub(super) const CONTROL_BYTES: usize = 4096;

pub(super) trait RangeReader: Send + Sync + 'static {
    fn read(&self, file: &File, offset: u64, count: usize) -> TimedRead;
    fn control(&self, file: &File, offset: u64, count: usize) -> TimedRead;
}

pub(super) struct FileRangeReader;

impl RangeReader for FileRangeReader {
    fn read(&self, file: &File, offset: u64, count: usize) -> TimedRead {
        timed_read(file, offset, count)
    }

    fn control(&self, file: &File, offset: u64, count: usize) -> TimedRead {
        // Opening our own pinned descriptor creates a separate open file
        // description. Never change O_DIRECT on the original descriptor while
        // a timed-out read might still be running on it.
        let started = Instant::now();
        let result = (|| -> io::Result<usize> {
            let direct = open_control(file)?;
            allocated_control_range(&direct, offset, count)?;
            #[repr(align(4096))]
            struct Aligned([u8; CONTROL_BYTES]);
            let mut data = Aligned([0; CONTROL_BYTES]);
            for relative in (0..count).step_by(CONTROL_BYTES) {
                let read = direct.read_at(&mut data.0, offset + relative as u64)?;
                // A sparse/unwritten extent can return zeros without data I/O.
                // Reject each zero block even when seek metadata reports data.
                if read != CONTROL_BYTES || data.0.iter().all(|byte| *byte == 0) {
                    return Err(io::Error::other("control range unavailable"));
                }
            }
            Ok(count)
        })();
        control_result(offset, count, result, started.elapsed())
    }
}

fn control_result(
    offset: u64,
    count: usize,
    result: io::Result<usize>,
    elapsed: Duration,
) -> TimedRead {
    let (read_bytes, outcome) = match result {
        Ok(count) => (count as u64, ProbeOutcome::Passed),
        Err(_) => (0, ProbeOutcome::Unavailable),
    };
    TimedRead {
        offset_bytes: offset,
        requested_bytes: count as u64,
        read_bytes,
        first_byte_ms: (read_bytes > 0).then(|| millis(elapsed)),
        elapsed_ms: millis(elapsed),
        timeout_ms: 0,
        bytes_per_second: ((u128::from(read_bytes) * 1_000_000_000) / elapsed.as_nanos().max(1))
            .try_into()
            .unwrap_or(u64::MAX),
        outcome,
    }
}

fn supported_filesystem(kind: u64, inode_flags: Option<u32>) -> bool {
    // Linux EXT2/3/4_SUPER_MAGIC and XFS_SUPER_MAGIC. FUSE, network, overlay,
    // memory-backed and unknown filesystems cannot certify a backing read.
    // ext2/3 share ext4's magic: require EXT4_EXTENTS_FL to exclude legacy
    // block-mapped inodes whose generic hole seeking can approximate data.
    // ext4_dio_alignment can select buffered I/O for encryption, journaled
    // data, verity, or inline data. Keep all four inode features unsupported.
    const EXTENTS: u32 = 0x0008_0000;
    const BUFFERED_FEATURES: u32 = 0x1010_4800;
    match kind {
        0xef53 => {
            inode_flags.is_some_and(|flags| flags & EXTENTS != 0 && flags & BUFFERED_FEATURES == 0)
        }
        0x58465342 => true,
        _ => false,
    }
}

fn supported_direct_alignment(
    mask: u32,
    memory_alignment: u32,
    offset_alignment: u32,
    offset: u64,
    count: usize,
) -> bool {
    mask & rustix::fs::StatxFlags::DIOALIGN.bits() != 0
        && memory_alignment != 0
        && offset_alignment != 0
        && CONTROL_BYTES.is_multiple_of(memory_alignment as usize)
        && CONTROL_BYTES.is_multiple_of(offset_alignment as usize)
        && offset.is_multiple_of(u64::from(offset_alignment))
        && count.is_multiple_of(offset_alignment as usize)
}

fn direct_alignment(file: &File, offset: u64, count: usize) -> io::Result<bool> {
    use rustix::fs::{AtFlags, StatxFlags, statx};
    // Inode flags alone miss ext4's mount-wide data=journal fallback. Require
    // positive kernel DIO capability on this descriptor, including alignment.
    // Missing support on older kernels is inconclusive, never a buffered retry.
    let stat = statx(file, "", AtFlags::EMPTY_PATH, StatxFlags::DIOALIGN)?;
    Ok(supported_direct_alignment(
        stat.stx_mask,
        stat.stx_dio_mem_align,
        stat.stx_dio_offset_align,
        offset,
        count,
    ))
}

fn allocated_control_range(file: &File, offset: u64, count: usize) -> io::Result<()> {
    use rustix::fs::{SeekFrom, fstatfs, seek};
    let kind = fstatfs(file)?.f_type as u64;
    let flags = if kind == 0xef53 {
        rustix::fs::ioctl_getflags(file)
            .ok()
            .map(|flags| flags.bits())
    } else {
        None
    };
    if !supported_filesystem(kind, flags) {
        return Err(io::Error::other("unsupported control filesystem"));
    }
    if !direct_alignment(file, offset, count)? {
        return Err(io::Error::other("unsupported control direct I/O"));
    }
    let end = offset
        .checked_add(count as u64)
        .ok_or_else(|| io::Error::other("invalid control range"))?;
    if seek(file, SeekFrom::Data(offset))? != offset || seek(file, SeekFrom::Hole(offset))? < end {
        return Err(io::Error::other("control range contains a hole"));
    }
    Ok(())
}

fn open_control(file: &File) -> io::Result<File> {
    let fd = rustix::fs::open(
        format!("/proc/self/fd/{}", file.as_raw_fd()),
        OFlags::RDONLY
            .union(OFlags::DIRECT)
            .union(OFlags::NONBLOCK)
            .union(OFlags::CLOEXEC),
        Mode::empty(),
    )?;
    Ok(File::from(fd))
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum ConfirmationStatus {
    NoControlRange,
    ControlUnavailable,
    RetryNotTimedOut,
    CapacityExhausted,
    Confirmed,
}

/// Local-only audit data. Construction is private: only bounded observations
/// over one pinned descriptor can produce a confirmed result.
#[derive(Debug, Serialize)]
pub(crate) struct ReadTimeoutConfirmation {
    required_attempts: u8,
    control_timeout_ms: u64,
    attempts: Vec<TimedRead>,
    controls: Vec<TimedRead>,
    status: ConfirmationStatus,
}

impl ReadTimeoutConfirmation {
    pub(crate) fn confirmed(&self) -> bool {
        matches!(self.status, ConfirmationStatus::Confirmed)
    }
}

/// An aligned control at least as large as, and disjoint from, the target range.
/// Small files or invalid arithmetic cannot establish independent evidence.
fn control_range(size: u64, target: &TimedRead) -> Option<(u64, usize)> {
    let block = CONTROL_BYTES as u64;
    let count = target.requested_bytes.checked_next_multiple_of(block)?;
    let last = size.checked_sub(count)? / block * block;
    let end = target.offset_bytes.checked_add(target.requested_bytes)?;
    let offset = if target.offset_bytes >= count {
        0
    } else if last >= end {
        last
    } else {
        return None;
    };
    Some((offset, count.try_into().ok()?))
}

impl MediaAgent {
    pub(super) async fn read_range<R: RangeReader>(
        &self,
        reader: Arc<R>,
        file: Arc<File>,
        offset: u64,
        count: usize,
        control: bool,
    ) -> Result<TimedRead, MediaError> {
        let deadline = if control {
            Duration::from_millis(self.config.read_timeout_policy.control_timeout_ms)
        } else {
            self.config.read_timeout
        };
        let started = Instant::now();
        let result = self
            .workers
            .run(deadline, move || {
                if control {
                    reader.control(&file, offset, count)
                } else {
                    reader.read(&file, offset, count)
                }
            })
            .await?;
        let mut read = result.unwrap_or(TimedRead {
            offset_bytes: offset,
            requested_bytes: count as u64,
            read_bytes: 0,
            first_byte_ms: None,
            elapsed_ms: millis(started.elapsed()),
            timeout_ms: millis(deadline),
            bytes_per_second: 0,
            outcome: ProbeOutcome::TimedOut,
        });
        read.timeout_ms = millis(deadline);
        Ok(read)
    }

    pub(super) async fn confirm_read_timeout<R: RangeReader>(
        &self,
        file: Arc<File>,
        size: u64,
        first: TimedRead,
        reader: Arc<R>,
    ) -> ReadTimeoutConfirmation {
        let policy = &self.config.read_timeout_policy;
        let offset = first.offset_bytes;
        let count = first.requested_bytes;
        let control = control_range(size, &first);
        let mut confirmation = ReadTimeoutConfirmation {
            required_attempts: policy.attempts,
            control_timeout_ms: policy.control_timeout_ms,
            attempts: vec![first],
            controls: Vec::new(),
            status: ConfirmationStatus::NoControlRange,
        };
        let Some((control_at, control_count)) = control else {
            return confirmation;
        };
        for attempt in 1..=policy.attempts {
            let control = match self
                .read_range(
                    Arc::clone(&reader),
                    Arc::clone(&file),
                    control_at,
                    control_count,
                    true,
                )
                .await
            {
                Ok(control) => control,
                Err(_) => {
                    confirmation.status = ConfirmationStatus::CapacityExhausted;
                    return confirmation;
                }
            };
            let prompt = control.outcome == ProbeOutcome::Passed
                && control.read_bytes == control_count as u64
                && control.elapsed_ms < policy.control_timeout_ms;
            confirmation.controls.push(control);
            if !prompt {
                confirmation.status = ConfirmationStatus::ControlUnavailable;
                return confirmation;
            }
            if attempt == policy.attempts {
                confirmation.status = ConfirmationStatus::Confirmed;
                return confirmation;
            }
            let retry = match self
                .read_range(
                    Arc::clone(&reader),
                    Arc::clone(&file),
                    offset,
                    count as usize,
                    false,
                )
                .await
            {
                Ok(retry) => retry,
                Err(_) => {
                    confirmation.status = ConfirmationStatus::CapacityExhausted;
                    return confirmation;
                }
            };
            let timed_out = retry.outcome == ProbeOutcome::TimedOut;
            confirmation.attempts.push(retry);
            if !timed_out {
                confirmation.status = ConfirmationStatus::RetryNotTimedOut;
                return confirmation;
            }
        }
        confirmation
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_timings_preserve_latency_throughput_eof_and_errors() {
        let read = control_result(
            4096,
            CONTROL_BYTES,
            Ok(CONTROL_BYTES),
            Duration::from_millis(2),
        );
        assert_eq!(read.first_byte_ms, Some(2));
        assert_eq!(read.elapsed_ms, 2);
        assert_eq!(read.bytes_per_second, 2_048_000);
        let read = control_result(0, CONTROL_BYTES, Ok(1), Duration::ZERO);
        assert_eq!(read.bytes_per_second, 1_000_000_000);
        let read = control_result(0, CONTROL_BYTES, Ok(usize::MAX), Duration::ZERO);
        assert_eq!(read.bytes_per_second, u64::MAX);
        for (result, outcome) in [
            (Ok(0), ProbeOutcome::Passed),
            (
                Err(io::Error::other("unsupported")),
                ProbeOutcome::Unavailable,
            ),
        ] {
            let read = control_result(4096, CONTROL_BYTES, result, Duration::from_millis(7));
            assert_eq!(read.read_bytes, 0);
            assert_eq!(read.first_byte_ms, None);
            assert_eq!(read.elapsed_ms, 7);
            assert_eq!(read.bytes_per_second, 0);
            assert_eq!(read.outcome, outcome);
        }
    }

    #[test]
    fn control_range_is_aligned_complete_and_disjoint() {
        let target = |offset, count| timed_read_with(offset, count, |_, _| Ok(0));
        for (size, offset, count, expected) in [
            (4095, 4096, 1, None),
            (4096, 0, 4096, None),
            (8191, 0, 1, None),
            (8192, 0, 4096, Some(4096)),
            (8192, 0, 4097, None),
            (16383, 0, 4096, Some(8192)),
            (8192, 4095, 1, Some(4096)),
            (8192, 4096, 1, Some(0)),
            (8192, 4097, 1, Some(0)),
            (32768, 0, 8193, Some(20480)),
            (32768, 16384, 8193, Some(0)),
            (32768, 0, 16385, None),
            (u64::MAX, u64::MAX, 1, None),
        ] {
            assert_eq!(
                control_range(size, &target(offset, count)),
                expected.map(|offset| (offset, count.next_multiple_of(CONTROL_BYTES))),
                "size {size}, target {offset}+{count}"
            );
        }
    }

    #[test]
    fn control_uses_an_independent_read_only_direct_descriptor_without_fallback() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("synthetic.bin");
        let mut bytes = vec![7; 3 * CONTROL_BYTES];
        bytes[..CONTROL_BYTES].fill(0);
        fs::write(&path, bytes).unwrap();
        let file = File::open(&path).unwrap();
        let original_flags = rustix::fs::fcntl_getfl(&file).unwrap();
        let control = open_control(&file);
        let read = FileRangeReader.control(&file, CONTROL_BYTES as u64, 2 * CONTROL_BYTES);
        assert_eq!(rustix::fs::fcntl_getfl(&file).unwrap(), original_flags);
        match control {
            Ok(control) => {
                assert_eq!(
                    Stamp::from(&control.metadata().unwrap()),
                    Stamp::from(&file.metadata().unwrap())
                );
                let flags = rustix::fs::fcntl_getfl(&control).unwrap();
                assert_eq!(flags & OFlags::ACCMODE, OFlags::RDONLY);
                assert!(flags.contains(OFlags::DIRECT));
                assert!(flags.contains(OFlags::NONBLOCK));
                assert!(
                    rustix::io::fcntl_getfd(&control)
                        .unwrap()
                        .contains(rustix::io::FdFlags::CLOEXEC)
                );
                let kind = rustix::fs::fstatfs(&file).unwrap().f_type as u64;
                let inode_flags = rustix::fs::ioctl_getflags(&file)
                    .ok()
                    .map(|flags| flags.bits());
                if !supported_filesystem(kind, inode_flags)
                    || !direct_alignment(&control, CONTROL_BYTES as u64, 2 * CONTROL_BYTES)
                        .unwrap_or(false)
                {
                    assert_eq!(read.outcome, ProbeOutcome::Unavailable);
                    return;
                }
                assert_eq!(read.outcome, ProbeOutcome::Passed);
                assert_eq!(read.offset_bytes, CONTROL_BYTES as u64);
                assert_eq!(read.requested_bytes, 2 * CONTROL_BYTES as u64);
                assert_eq!(read.read_bytes, 2 * CONTROL_BYTES as u64);
                assert!(read.first_byte_ms.is_some());
                assert_eq!(read.first_byte_ms, Some(read.elapsed_ms));
                assert!(read.bytes_per_second > 0);
            }
            Err(_) => assert_eq!(read.outcome, ProbeOutcome::Unavailable),
        }
        let write_only = fs::OpenOptions::new().write(true).open(&path).unwrap();
        // An ordinary read through the real adapter retains its original shape.
        assert_eq!(FileRangeReader.read(&file, 3, 7).read_bytes, 7);
        assert_eq!(
            FileRangeReader.read(&write_only, 3, 7).outcome,
            ProbeOutcome::Failed
        );
        let read = FileRangeReader.control(&file, 3 * CONTROL_BYTES as u64, CONTROL_BYTES);
        assert_eq!(read.read_bytes, 0);
        assert_eq!(read.first_byte_ms, None);
        assert_eq!(read.bytes_per_second, 0);
        assert_eq!(read.timeout_ms, 0);
    }

    #[test]
    fn control_rejects_zero_and_sparse_storage_samples() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("synthetic.bin");
        fs::write(&path, vec![0; 2 * CONTROL_BYTES]).unwrap();
        let file = File::open(&path).unwrap();
        assert_eq!(
            FileRangeReader.control(&file, 0, CONTROL_BYTES).outcome,
            ProbeOutcome::Unavailable
        );
        let path = temp.path().join("sparse.bin");
        let file = File::create(&path).unwrap();
        file.set_len((2 * CONTROL_BYTES) as u64).unwrap();
        let file = File::open(&path).unwrap();
        assert_eq!(
            FileRangeReader.control(&file, 0, CONTROL_BYTES).outcome,
            ProbeOutcome::Unavailable
        );
        // An allocated prefix cannot hide a later hole in a larger control.
        let path = temp.path().join("partial.bin");
        fs::write(&path, vec![7; CONTROL_BYTES]).unwrap();
        let file = fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.set_len((4 * CONTROL_BYTES) as u64).unwrap();
        file.sync_all().unwrap();
        let file = File::open(&path).unwrap();
        assert!(allocated_control_range(&file, 0, 2 * CONTROL_BYTES).is_err());
        assert!(allocated_control_range(&file, CONTROL_BYTES as u64, CONTROL_BYTES).is_err());
        assert!(allocated_control_range(&file, u64::MAX, CONTROL_BYTES).is_err());
        assert_eq!(
            FileRangeReader.control(&file, 0, 2 * CONTROL_BYTES).outcome,
            ProbeOutcome::Unavailable
        );
        // Fully allocated zeros also fail; SEEK_DATA alone is insufficient.
        fs::write(
            &path,
            [vec![7; CONTROL_BYTES], vec![0; CONTROL_BYTES]].concat(),
        )
        .unwrap();
        let file = File::open(&path).unwrap();
        assert_eq!(
            FileRangeReader.control(&file, 0, 2 * CONTROL_BYTES).outcome,
            ProbeOutcome::Unavailable
        );
    }

    #[test]
    fn direct_alignment_matches_the_descriptor_capability_independent_of_filesystem_policy() {
        use rustix::fs::{AtFlags, StatxFlags, statx};
        let file = tempfile::tempfile().unwrap();
        let capability = statx(&file, "", AtFlags::EMPTY_PATH, StatxFlags::DIOALIGN);
        match capability {
            Ok(stat) => {
                for (offset, count) in [(0, CONTROL_BYTES), (1, CONTROL_BYTES), (0, 1)] {
                    assert_eq!(
                        direct_alignment(&file, offset, count).unwrap(),
                        supported_direct_alignment(
                            stat.stx_mask,
                            stat.stx_dio_mem_align,
                            stat.stx_dio_offset_align,
                            offset,
                            count,
                        )
                    );
                }
            }
            Err(_) => assert!(direct_alignment(&file, 0, CONTROL_BYTES).is_err()),
        }
    }

    #[test]
    fn direct_controls_require_positive_capability_and_compatible_alignment() {
        let reported = rustix::fs::StatxFlags::DIOALIGN.bits();
        for (mask, memory, position, offset, count) in [
            (0, 512, 512, 4096, 8192),
            (reported, 0, 512, 4096, 8192),
            (reported, 512, 0, 4096, 8192),
            (reported, 0, 0, 4096, 8192),
            (reported, 8192, 512, 4096, 8192),
            (reported, 512, 8192, 8192, 8192),
            (reported, 512, 512, 4097, 8192),
            (reported, 512, 512, 4096, 8193),
            (reported, 3, 512, 4096, 8192),
        ] {
            assert!(!supported_direct_alignment(
                mask, memory, position, offset, count
            ));
        }
        for alignment in [1, 512, 4096] {
            assert!(supported_direct_alignment(
                reported, alignment, alignment, 4096, 8192
            ));
        }
        assert!(supported_direct_alignment(reported, 4096, 512, 512, 512));
    }

    #[test]
    fn ext4_features_that_can_fall_back_to_buffered_reads_are_refused() {
        for special in [0x0000_0800, 0x0000_4000, 0x0010_0000, 0x1000_0000] {
            assert!(
                !supported_filesystem(0xef53, Some(0x0008_0000 | special)),
                "unsafe ext4 inode flag {special:#x}"
            );
        }
    }

    #[test]
    fn only_known_local_filesystems_can_confirm_backing_reads() {
        for flags in [None, Some(0), Some(0x0004_0000), Some(0x0010_0000)] {
            assert!(!supported_filesystem(0xef53, flags));
        }
        assert!(supported_filesystem(0xef53, Some(0x0008_0000)));
        assert!(supported_filesystem(0xef53, Some(0x0008_0080)));
        assert!(supported_filesystem(0x58465342, None));
        for filesystem in [0, 0x6969, 0x65735546, 0x794c7630, 0x01021994, u64::MAX] {
            assert!(!supported_filesystem(filesystem, Some(0x0008_0000)));
        }
    }
}
