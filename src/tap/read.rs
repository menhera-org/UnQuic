//! Synchronous batches of nonblocking TAP reads; no borrowed buffer survives a call.
use super::TapQueue;
use std::{
    io,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    sync::atomic::Ordering::{Acquire, Relaxed, Release},
};
pub(crate) const CAPACITY: usize = 64;

#[repr(C)]
#[derive(Default)]
struct SqOffsets {
    head: u32,
    tail: u32,
    mask: u32,
    entries: u32,
    flags: u32,
    dropped: u32,
    array: u32,
    reserved: u32,
    user_addr: u64,
}
#[repr(C)]
#[derive(Default)]
struct CqOffsets {
    head: u32,
    tail: u32,
    mask: u32,
    entries: u32,
    overflow: u32,
    cqes: u32,
    flags: u32,
    reserved: u32,
    user_addr: u64,
}
#[repr(C)]
#[derive(Default)]
struct Params {
    sq_entries: u32,
    cq_entries: u32,
    flags: u32,
    sq_cpu: u32,
    sq_idle: u32,
    features: u32,
    wq_fd: u32,
    reserved: [u32; 3],
    sq: SqOffsets,
    cq: CqOffsets,
}
#[repr(C)]
struct Sqe {
    opcode: u8,
    flags: u8,
    priority: u16,
    fd: i32,
    offset: u64,
    address: u64,
    length: u32,
    rw_flags: u32,
    user: u64,
    unused: [u64; 3],
}
#[repr(C)]
#[derive(Clone, Copy)]
struct Cqe {
    user: u64,
    result: i32,
    flags: u32,
}
const _: () = {
    assert!(size_of::<Params>() == 120);
    assert!(size_of::<Sqe>() == 64);
    assert!(size_of::<Cqe>() == 16);
};
struct Map {
    pointer: *mut u8,
    length: usize,
}
impl Map {
    fn new(fd: i32, length: usize, offset: libc::off_t) -> io::Result<Self> {
        // SAFETY: kernel fd and checked ABI sizes describe the requested mapping.
        let p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                length,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                offset,
            )
        };
        if p == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            pointer: p.cast(),
            length,
        })
    }
    fn at<T>(&self, offset: usize) -> *mut T {
        assert!(
            offset
                .checked_add(size_of::<T>())
                .is_some_and(|end| end <= self.length)
        );
        // SAFETY: bounds checked; callers use kernel offsets aligned for their ABI type.
        let p = unsafe { self.pointer.add(offset).cast::<T>() };
        assert_eq!(p as usize % align_of::<T>(), 0);
        p
    }
    fn atomic(&self, offset: u32) -> &std::sync::atomic::AtomicU32 {
        // SAFETY: ring counters are aligned shared u32s; all concurrent access is atomic.
        unsafe { &*self.at(offset as usize) }
    }
}
impl Drop for Map {
    fn drop(&mut self) {
        // SAFETY: this object owns the mapping and drops after all completions.
        unsafe { libc::munmap(self.pointer.cast(), self.length) };
    }
}

pub(crate) struct Reader {
    fd: OwnedFd,
    sq: Map,
    cq: Map,
    sqes: Map,
    params: Params,
    tail: u32,
    pub(crate) enabled: bool,
}
// SAFETY: calls borrow Reader exclusively and return only after every submitted completion.
unsafe impl Send for Reader {}

pub(crate) struct Batch {
    pub(crate) results: [io::Result<usize>; CAPACITY],
    pub(crate) segments: [Option<super::Segments>; CAPACITY],
    pub(crate) calls: u64,
    pub(crate) requests: u64,
}

// Unwinding with published kernel pointers would free live borrowed storage.
struct Pending(bool);
impl Drop for Pending {
    fn drop(&mut self) {
        if self.0 {
            std::process::abort();
        }
    }
}

impl Reader {
    pub(crate) fn storage_bytes() -> Option<usize> {
        // SAFETY: sysconf has no pointer arguments; account for page-rounded mappings.
        let page = usize::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).ok()?;
        (page >= 4096).then_some(page)?.checked_mul(3)
    }

    pub(crate) fn new() -> io::Result<Self> {
        if Self::storage_bytes().is_none() {
            return Err(io::ErrorKind::Unsupported.into());
        }
        let mut params = Params::default();
        // SAFETY: params is the complete initialized io_uring_params ABI structure.
        let n = unsafe { libc::syscall(libc::SYS_io_uring_setup, CAPACITY as u32, &mut params) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: sole owner of the new kernel descriptor.
        let fd = unsafe { OwnedFd::from_raw_fd(n as i32) };
        if params.sq_entries != CAPACITY as u32
            || !(CAPACITY as u32..=128).contains(&params.cq_entries)
            || (params.sq.array as usize)
                .checked_add(4 * CAPACITY)
                .is_none_or(|end| end > 4096)
            || (params.cq.cqes as usize)
                .checked_add(16 * params.cq_entries as usize)
                .is_none_or(|end| end > 4096)
        {
            return Err(io::ErrorKind::Unsupported.into());
        }
        // SAFETY: fcntl sets the flags of this owned descriptor.
        if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let sq = Map::new(fd.as_raw_fd(), 4096, 0)?;
        let cq = Map::new(fd.as_raw_fd(), 4096, 0x08000000)?;
        let sqes = Map::new(fd.as_raw_fd(), 4096, 0x10000000)?;
        for (map, offsets) in [
            (
                &sq,
                [
                    params.sq.head,
                    params.sq.tail,
                    params.sq.mask,
                    params.sq.dropped,
                ],
            ),
            (
                &cq,
                [
                    params.cq.head,
                    params.cq.tail,
                    params.cq.mask,
                    params.cq.overflow,
                ],
            ),
        ] {
            if offsets.iter().any(|&o| o as usize > 4092 || o % 4 != 0) {
                return Err(io::ErrorKind::Unsupported.into());
            }
            for offset in offsets {
                let _ = map.atomic(offset);
            }
        }
        if params.sq.array % 4 != 0
            || params.cq.cqes % 8 != 0
            || sq.atomic(params.sq.mask).load(Relaxed) != 63
            || cq.atomic(params.cq.mask).load(Relaxed) != params.cq_entries - 1
        {
            return Err(io::ErrorKind::Unsupported.into());
        }
        for i in 0..CAPACITY {
            // SAFETY: no SQEs are published; checked array bounds and alignment.
            unsafe {
                sq.at::<u32>(params.sq.array as usize + 4 * i)
                    .write(i as u32)
            };
        }
        Ok(Self {
            fd,
            sq,
            cq,
            sqes,
            params,
            tail: 0,
            enabled: true,
        })
    }

    pub(crate) fn read(&mut self, tap: &mut TapQueue, buffers: &mut [&mut [u8]]) -> Batch {
        self.read_with(tap, buffers, enter)
    }

    fn read_with(
        &mut self,
        tap: &mut TapQueue,
        buffers: &mut [&mut [u8]],
        mut submit: impl FnMut(i32, u32, u32) -> io::Result<usize>,
    ) -> Batch {
        assert!(self.enabled && buffers.len() <= CAPACITY);
        assert!(buffers.iter().all(|b| b.len() > crate::MAX_FRAME_LEN));
        assert_eq!(self.sq.atomic(self.params.sq.head).load(Acquire), self.tail);
        assert_eq!(
            self.cq.atomic(self.params.cq.head).load(Relaxed),
            self.cq.atomic(self.params.cq.tail).load(Acquire)
        );
        let mut batch = Batch {
            results: std::array::from_fn(|_| Err(io::ErrorKind::WouldBlock.into())),
            segments: [None; CAPACITY],
            calls: 0,
            requests: 0,
        };
        if buffers.is_empty() {
            return batch;
        }
        let mut headers = [[0u8; 10]; CAPACITY];
        let mut iov = [[libc::iovec {
            iov_base: std::ptr::null_mut(),
            iov_len: 0,
        }; 2]; CAPACITY];
        let start = self.tail;
        for (i, buffer) in buffers.iter_mut().enumerate() {
            iov[i][0] = libc::iovec {
                iov_base: headers[i].as_mut_ptr().cast(),
                iov_len: 10,
            };
            iov[i][1] = libc::iovec {
                iov_base: buffer.as_mut_ptr().cast(),
                iov_len: buffer.len(),
            };
            let slot = start.wrapping_add(i as u32) as usize & 63;
            let first = usize::from(!tap.vnet);
            // SAFETY: each unpublished SQE references distinct live buffers and iovec storage.
            unsafe {
                self.sqes.at::<Sqe>(64 * slot).write(Sqe {
                    opcode: 1, // IORING_OP_READV
                    flags: 0,
                    priority: 0,
                    fd: tap.fd.as_raw_fd(),
                    offset: 0,
                    address: iov[i][first..].as_ptr() as u64,
                    length: (2 - first) as u32,
                    rw_flags: libc::RWF_NOWAIT as u32,
                    user: i as u64,
                    unused: [0; 3],
                });
            }
        }
        let mut pending = Pending(true);
        self.tail = start.wrapping_add(buffers.len() as u32);
        self.sq
            .atomic(self.params.sq.tail)
            .store(self.tail, Release);
        let mut seen = 0u64;
        let mut interruptions = 0;
        loop {
            let head = self.sq.atomic(self.params.sq.head).load(Acquire);
            let remaining = self.tail.wrapping_sub(head);
            let accepted = head.wrapping_sub(start);
            assert!(remaining <= buffers.len() as u32 && accepted <= buffers.len() as u32);
            batch.calls += 1;
            let result = submit(
                self.fd.as_raw_fd(),
                remaining,
                if remaining == 0 {
                    accepted - seen.count_ones()
                } else {
                    0
                },
            );
            let head = self.sq.atomic(self.params.sq.head).load(Acquire);
            let accepted = head.wrapping_sub(start);
            assert!(accepted <= buffers.len() as u32);
            let cq_head = self.cq.atomic(self.params.cq.head).load(Relaxed);
            let cq_tail = self.cq.atomic(self.params.cq.tail).load(Acquire);
            assert!(cq_tail.wrapping_sub(cq_head) <= self.params.cq_entries);
            let mask = self.params.cq_entries - 1;
            let count = cq_tail.wrapping_sub(cq_head);
            for index in 0..count {
                // SAFETY: acquired CQ tail publishes this CQE; CQ head is not advanced yet.
                let c = unsafe {
                    self.cq
                        .at::<Cqe>(
                            self.params.cq.cqes as usize
                                + 16 * (cq_head.wrapping_add(index) & mask) as usize,
                        )
                        .read()
                };
                assert!(c.user < u64::from(accepted) && c.flags == 0 && seen & (1 << c.user) == 0);
                seen |= 1 << c.user;
                let i = c.user as usize;
                batch.results[i] = if c.result < 0 {
                    if matches!(-c.result, libc::EOPNOTSUPP | libc::EINVAL | libc::ENOSYS) {
                        self.enabled = false;
                        Err(io::ErrorKind::WouldBlock.into())
                    } else {
                        Err(io::Error::from_raw_os_error(-c.result))
                    }
                } else {
                    tap.finish_input(&headers[i], c.result as usize, buffers[i])
                        .map(|(length, segments)| {
                            batch.segments[i] = segments;
                            length
                        })
                };
            }
            self.cq.atomic(self.params.cq.head).store(cq_tail, Release);
            assert_eq!(self.cq.atomic(self.params.cq.overflow).load(Relaxed), 0);
            assert_eq!(self.sq.atomic(self.params.sq.dropped).load(Relaxed), 0);
            let interrupted = result
                .as_ref()
                .is_err_and(|e| e.kind() == io::ErrorKind::Interrupted);
            if interrupted {
                interruptions += 1;
            }
            let failed = result.is_err() && (!interrupted || interruptions >= 8)
                || matches!(result, Ok(0)) && remaining != 0;
            if failed {
                // No SQPOLL: after enter returns, unpublished work can be withdrawn safely.
                self.tail = head;
                self.sq.atomic(self.params.sq.tail).store(head, Release);
                self.enabled = false;
            }
            if seen.count_ones() == accepted && head == self.tail {
                pending.0 = false;
                batch.requests = u64::from(accepted);
                return batch;
            }
            if failed && seen.count_ones() != accepted {
                // Outstanding kernel pointers cannot be returned after an unrecoverable enter failure.
                std::process::abort();
            }
        }
    }
}

fn enter(fd: i32, count: u32, wait: u32) -> io::Result<usize> {
    // SAFETY: this private ring has only live, bounded READV SQEs; no signal-mask pointer.
    let n = unsafe {
        libc::syscall(
            libc::SYS_io_uring_enter,
            fd,
            count,
            wait,
            1u32,
            std::ptr::null::<libc::sigset_t>(),
            0usize,
        )
    };
    if n < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(n as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixDatagram;

    fn queue(vnet: bool) -> (TapQueue, UnixDatagram) {
        let (a, b) = UnixDatagram::pair().unwrap();
        a.set_nonblocking(true).unwrap();
        (
            TapQueue {
                fd: a.into(),
                frame_limit: crate::MAX_FRAME_LEN,
                vnet,
                uso: false,
                tso: false,
                completed_checksums: 0,
            },
            b,
        )
    }

    #[test]
    #[ignore = "kernel ring test; run in a disposable container"]
    fn read_batches_keep_order_boundaries_and_metadata_without_async_workers() {
        let mut ring = Reader::new().unwrap();
        assert!(std::panic::catch_unwind(|| ring.sq.at::<u32>(usize::MAX)).is_err());
        assert!(std::panic::catch_unwind(|| ring.sq.at::<[u8; 4097]>(0)).is_err());
        let before = std::fs::read_dir("/proc/self/task").unwrap().count();
        for vnet in [false, true] {
            let (mut tap, sender) = queue(vnet);
            let mut storage = vec![vec![0u8; crate::MAX_FRAME_LEN + 1]; CAPACITY];
            let mut buffers: Vec<_> = storage.iter_mut().map(Vec::as_mut_slice).collect();
            for count in [1, 2, 63, 64, 7, 64] {
                for i in 0..count {
                    let mut frame = vec![i as u8; 14 + 31 * i + usize::from(vnet) * 10];
                    if vnet {
                        frame[..10].fill(0);
                    }
                    sender.send(&frame).unwrap();
                }
                let batch = ring.read(&mut tap, &mut buffers[..count]);
                assert_eq!(batch.calls, 1);
                for (i, result) in batch.results.into_iter().take(count).enumerate() {
                    assert_eq!(result.unwrap(), 14 + 31 * i);
                    assert!(buffers[i][..14 + 31 * i].iter().all(|&v| v == i as u8));
                }
                let empty = ring.read(&mut tap, &mut buffers);
                assert_eq!(empty.calls, 1);
                assert!(
                    empty
                        .results
                        .iter()
                        .all(|r| r.as_ref().unwrap_err().kind() == io::ErrorKind::WouldBlock)
                );
            }
            sender
                .send(&vec![0; crate::MAX_FRAME_LEN + 1 + usize::from(vnet) * 10])
                .unwrap();
            let batch = ring.read(&mut tap, &mut buffers[..1]);
            assert_eq!(
                batch.results[0].as_ref().unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
            if vnet {
                let mut frame = [0u8; 70];
                for (flag, gso) in [(1, 0), (4, 0), (128, 0), (0, 5)] {
                    frame[0] = flag;
                    frame[1] = gso;
                    sender.send(&frame).unwrap();
                    let batch = ring.read(&mut tap, &mut buffers[..1]);
                    assert_eq!(
                        batch.results[0].as_ref().unwrap_err().kind(),
                        io::ErrorKind::InvalidData
                    );
                }
            }
        }

        for v6 in [false, true] {
            let (mut tap, sender) = queue(true);
            let expected = super::super::test_common::udp::frame(v6, 7, &[0x5a; 1471]);
            let mut frame = expected.clone();
            let header = super::super::test_common::udp::partial(&mut frame, v6);
            let wire = [header.as_slice(), frame.as_slice()].concat();
            let mut bytes = [0u8; crate::MAX_FRAME_LEN + 1];
            sender.send(&wire).unwrap();
            assert_eq!(tap.read_frame(&mut bytes).unwrap(), expected.len());
            assert_eq!(&bytes[..expected.len()], expected);
            sender.send(&wire).unwrap();
            let batch = ring.read(&mut tap, &mut [&mut bytes]);
            assert_eq!(*batch.results[0].as_ref().unwrap(), expected.len());
            assert_eq!(&bytes[..expected.len()], expected);
            assert_eq!(tap.completed_checksums, 2);
        }
        assert!(ring.enabled);
        assert_eq!(
            std::fs::read_dir("/proc/self/task").unwrap().count(),
            before
        );
    }

    #[test]
    #[ignore = "kernel ring test; run in a disposable container"]
    fn interruptions_partial_submission_and_rejection_preserve_every_frame() {
        for reject in [false, true] {
            let mut ring = Reader::new().unwrap();
            let (mut tap, sender) = queue(false);
            for i in 0..CAPACITY {
                sender.send(&[i as u8; 60]).unwrap();
            }
            let mut storage = vec![vec![0u8; crate::MAX_FRAME_LEN + 1]; CAPACITY];
            let mut buffers: Vec<_> = storage.iter_mut().map(Vec::as_mut_slice).collect();
            let mut calls = 0;
            let batch = ring.read_with(&mut tap, &mut buffers, |fd, count, wait| {
                calls += 1;
                if calls == 1 {
                    assert_eq!(enter(fd, 3, 0).unwrap(), 3);
                    Err(io::Error::from_raw_os_error(if reject {
                        libc::EPERM
                    } else {
                        libc::EINTR
                    }))
                } else if calls == 2 {
                    Err(io::Error::from_raw_os_error(libc::EINTR))
                } else {
                    enter(fd, count, wait)
                }
            });
            let completed = if reject { 3 } else { CAPACITY };
            assert_eq!(batch.calls, if reject { 1 } else { 3 });
            assert_eq!(ring.enabled, !reject);
            for (i, result) in batch.results.into_iter().enumerate() {
                if i < completed {
                    assert_eq!(result.unwrap(), 60);
                    assert_eq!(&buffers[i][..60], &[i as u8; 60]);
                } else {
                    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::WouldBlock);
                    assert_eq!(tap.read_frame(buffers[i]).unwrap(), 60);
                    assert_eq!(&buffers[i][..60], &[i as u8; 60]);
                }
            }
            assert_eq!(
                tap.read_frame(buffers[0]).unwrap_err().kind(),
                io::ErrorKind::WouldBlock
            );
        }

        let mut ring = Reader::new().unwrap();
        // An io_uring READV on an epoll descriptor is rejected without consuming input.
        let n = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
        assert!(n >= 0);
        let mut tap = TapQueue::test_fd(unsafe { OwnedFd::from_raw_fd(n) }, false);
        let mut bytes = [0u8; crate::MAX_FRAME_LEN + 1];
        let batch = ring.read(&mut tap, &mut [&mut bytes]);
        assert!(!ring.enabled);
        assert_eq!(
            batch.results[0].as_ref().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
    }
    #[test]
    #[ignore = "requires disposable container, tun, iproute2 and network capabilities"]
    fn real_tap_reads_preserve_every_frame_and_use_one_enter_per_batch() {
        use super::super::test_common::*;
        // SAFETY: only this test thread and its children enter the private namespace.
        check(unsafe { libc::unshare(libc::CLONE_NEWNET) });
        let mut tap = crate::tap::Tap::create(c"uq-read", 9000, 1, None).unwrap();
        assert!(
            std::process::Command::new("ip")
                .args(["link", "set", "uq-read", "addrgenmode", "none"])
                .status()
                .unwrap()
                .success()
        );
        let control = std::net::UdpSocket::bind("[::]:0").unwrap();
        up(&control, c"uq-read");
        tap.queues[0].set_carrier(true).unwrap();
        let sender = raw_socket(c"uq-read", 0x88b5);
        let mut reader = Reader::new().unwrap();
        let tasks = std::fs::read_dir("/proc/self/task").unwrap().count();
        let mut storage = vec![vec![0u8; crate::MAX_FRAME_LEN + 1]; CAPACITY];
        let mut buffers: Vec<_> = storage.iter_mut().map(Vec::as_mut_slice).collect();
        for size in [60, 1514, 9014] {
            for count in [1, 2, 63, 64] {
                let frames: Vec<_> = (0..count)
                    .map(|i| {
                        let mut frame = vec![i as u8; size];
                        frame[..12].fill(2);
                        frame[12..14].copy_from_slice(&0x88b5u16.to_be_bytes());
                        frame
                    })
                    .collect();
                for frame in &frames {
                    raw_send(&sender, frame);
                }
                let batch = reader.read(&mut tap.queues[0], &mut buffers[..count]);
                assert!(reader.enabled);
                assert_eq!(batch.calls, 1);
                for (i, result) in batch.results.into_iter().take(count).enumerate() {
                    assert_eq!(result.unwrap(), size);
                    assert_eq!(&buffers[i][..size], frames[i]);
                }
            }
        }
        let empty = reader.read(&mut tap.queues[0], &mut buffers);
        assert_eq!(empty.calls, 1);
        assert!(
            empty
                .results
                .iter()
                .all(|r| r.as_ref().unwrap_err().kind() == io::ErrorKind::WouldBlock)
        );
        assert_eq!(std::fs::read_dir("/proc/self/task").unwrap().count(), tasks);
    }
}
