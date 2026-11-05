//! Frame-window design checks, retained until stage 2 replaces them with real types.
use std::collections::BTreeMap;

fn permutations<T: Copy>(items: &mut [T], at: usize, check: &mut impl FnMut(&[T])) {
    if at == items.len() {
        check(items);
        return;
    }
    for j in at..items.len() {
        items.swap(at, j);
        permutations(items, at + 1, check);
        items.swap(at, j);
    }
}

#[derive(Clone, Copy)]
enum Frame {
    Pieces(u8),
    Delivered,
    Expired,
}
struct ReceiveLane {
    id: u16,
    retired: bool,
    floor: u64,
    width: u64,
    frames: BTreeMap<u64, Frame>,
    deliveries: usize,
}
impl ReceiveLane {
    fn new(id: u16) -> Self {
        Self {
            id,
            retired: false,
            floor: 1,
            width: 4,
            frames: BTreeMap::new(),
            deliveries: 0,
        }
    }
    // Two pieces per frame suffice to exercise completion and duplicate history.
    // The return value is packet-ACK eligibility, not frame delivery.
    fn piece(&mut self, frame_lane: u16, seq: u64, part: u8, can_reserve: bool) -> bool {
        if self.retired || frame_lane != self.id || seq == 0 {
            return false;
        }
        if seq < self.floor {
            return true;
        }
        let pieces = match self.frames.get(&seq) {
            Some(Frame::Delivered | Frame::Expired) => return true,
            Some(Frame::Pieces(bits)) => *bits,
            None => 0,
        };
        if pieces & part == part {
            return true;
        }
        if !can_reserve {
            return false;
        }
        if seq - self.floor >= self.width {
            self.floor = seq - self.width + 1;
            self.frames.retain(|&seq, _| seq >= self.floor);
        }
        let bits = pieces | part;
        let state = if bits == 3 {
            self.deliveries += 1;
            Frame::Delivered
        } else {
            Frame::Pieces(bits)
        };
        self.frames.insert(seq, state);
        assert!(self.frames.len() as u64 <= self.width);
        true
    }
    fn expire(&mut self, seq: u64) {
        if matches!(self.frames.get(&seq), Some(Frame::Pieces(_))) {
            self.frames.insert(seq, Frame::Expired);
        }
    }
    fn abandon(&mut self, seq: u64) {
        if self.retired || seq < self.floor {
            return;
        }
        if seq - self.floor >= self.width {
            self.floor = seq - self.width + 1;
            self.frames.retain(|&seq, _| seq >= self.floor);
        }
        if !matches!(self.frames.get(&seq), Some(Frame::Delivered)) {
            self.frames.insert(seq, Frame::Expired);
        }
        assert!(self.frames.len() as u64 <= self.width);
    }

    fn retire(&mut self) {
        self.retired = true;
        self.frames.clear();
    }
}

#[test]
fn originals_and_repairs_deliver_once_in_every_order() {
    permutations(&mut [1, 1, 2, 2], 0, &mut |parts| {
        let mut lane = ReceiveLane::new(1);
        for &part in parts {
            assert!(lane.piece(1, 1, part, true));
        }
        assert_eq!(lane.deliveries, 1);
    });
}

#[test]
fn forgotten_terminal_bits_do_not_reopen_old_frames() {
    let mut lane = ReceiveLane::new(1);
    assert!(lane.piece(1, 1, 3, true));
    assert!(lane.piece(1, 2, 1, true));
    lane.expire(2);
    assert!(lane.piece(1, 2, 2, true));
    assert!(lane.piece(1, 10, 1, true));
    assert!(!lane.frames.contains_key(&1));
    assert!(!lane.frames.contains_key(&2));
    // New transmission numbers cannot resurrect old frame identities.
    assert!(lane.piece(1, 1, 3, true));
    assert!(lane.piece(1, 2, 3, true));
    assert_eq!(lane.deliveries, 1);
}

#[test]
fn pressure_rejects_without_ack_or_window_mutation() {
    let mut lane = ReceiveLane::new(1);
    assert!(lane.piece(1, 1, 1, true));
    assert!(!lane.piece(1, 100, 1, false));
    assert_eq!(lane.floor, 1);
    assert!(lane.frames.contains_key(&1));
    assert!(!lane.piece(1, 1, 2, false));
    assert_eq!(lane.deliveries, 0);
    assert!(lane.piece(1, 1, 2, true));
    assert_eq!(lane.deliveries, 1);
}

#[test]
fn retirement_and_lane_identity_prevent_cross_worker_redelivery() {
    let mut old = ReceiveLane::new(1);
    let mut replacement = ReceiveLane::new(2);
    assert!(old.piece(1, 1, 3, true));
    old.retire();
    assert!(!old.piece(1, 1, 3, true));
    assert!(!replacement.piece(1, 1, 3, true));
    assert_eq!(old.deliveries + replacement.deliveries, 1);
    assert!(replacement.piece(2, 1, 3, true));
    assert_eq!(replacement.deliveries, 1);
}

#[test]
fn abandonment_before_first_piece_remains_terminal_after_window_advance() {
    let mut lane = ReceiveLane::new(1);
    lane.abandon(1);
    assert!(lane.piece(1, 1, 1, true));
    assert!(lane.piece(1, 1, 2, true));
    lane.abandon(10);
    assert!(lane.piece(1, 1, 3, true));
    assert!(lane.piece(1, 10, 3, true));
    assert_eq!(lane.deliveries, 0);
    assert!(lane.piece(1, 11, 3, true));
    lane.abandon(11);
    assert!(lane.piece(1, 11, 3, true));
    assert_eq!(lane.deliveries, 1);
}
