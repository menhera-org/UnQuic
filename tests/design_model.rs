//! Executable design checks, not the transport implementation.
//! Tags are deterministic fixtures; production obtains them from getrandom.
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Pair {
    i: u128,
    r: u128,
}

#[derive(Clone, Default)]
struct Initiator {
    active: Option<Pair>,
    pending: Option<Pair>,
    installs: usize,
}
impl Initiator {
    fn begin(&mut self, i: u128) {
        self.pending = Some(Pair { i, r: 0 });
    }
    fn challenge(&mut self, pair: Pair) -> Option<Pair> {
        let pending = self.pending.as_mut()?;
        if pair.i != pending.i
            || pair.r == 0
            || pair.r == pair.i
            || (pending.r != 0 && pending.r != pair.r)
        {
            return None;
        }
        *pending = pair;
        Some(pair)
    }
    fn ready(&mut self, pair: Pair) {
        if self.pending == Some(pair) && pair.r != 0 {
            self.active = Some(pair);
            self.pending = None;
            self.installs += 1;
        }
    }
}

#[derive(Clone)]
struct Responder {
    active: Option<Pair>,
    pending: Option<Pair>,
    next_fixture_tag: u128,
    installs: usize,
}
impl Responder {
    fn new(seed: u128) -> Self {
        Self {
            active: None,
            pending: None,
            next_fixture_tag: seed,
            installs: 0,
        }
    }
    fn hello(&mut self, i: u128) -> Option<Pair> {
        if let Some(active) = self.active.filter(|p| p.i == i) {
            return Some(active);
        }
        if let Some(pending) = self.pending {
            return (pending.i == i).then_some(pending);
        }
        let pair = Pair {
            i,
            r: self.next_fixture_tag,
        };
        assert!(i != 0 && pair.r != 0 && i != pair.r);
        self.next_fixture_tag += 1;
        self.pending = Some(pair);
        Some(pair)
    }
    fn confirm(&mut self, pair: Pair) -> Option<Pair> {
        if self.active == Some(pair) {
            return Some(pair);
        }
        if self.pending != Some(pair) {
            return None;
        }
        self.active = Some(pair);
        self.pending = None;
        self.installs += 1;
        Some(pair)
    }
}

fn establish(i: &mut Initiator, r: &mut Responder, tag: u128) -> Pair {
    i.begin(tag);
    let challenge = r.hello(tag).unwrap();
    let confirm = i.challenge(challenge).unwrap();
    let ready = r.confirm(confirm).unwrap();
    i.ready(ready);
    assert_eq!(i.active, r.active);
    ready
}

#[test]
fn losing_each_flight_and_repeating_messages_commits_once() {
    for lost in 0..4 {
        let mut i = Initiator::default();
        let mut r = Responder::new(100);
        i.begin(1);
        for attempt in 0..3 {
            let pending = i.pending.unwrap();
            let confirm = if pending.r == 0 {
                if attempt == 0 && lost == 0 {
                    continue;
                }
                let challenge = r.hello(pending.i).unwrap();
                assert_eq!(r.hello(pending.i), Some(challenge));
                if attempt == 0 && lost == 1 {
                    continue;
                }
                let confirm = i.challenge(challenge).unwrap();
                assert_eq!(i.challenge(challenge), Some(confirm));
                confirm
            } else {
                pending
            };
            if attempt == 0 && lost == 2 {
                continue;
            }
            let ready = r.confirm(confirm).unwrap();
            assert_eq!(r.confirm(confirm), Some(ready));
            if attempt == 0 && lost == 3 {
                continue;
            }
            i.ready(ready);
            i.ready(ready);
            break;
        }
        assert!(i.active.is_some());
        assert_eq!(i.active, r.active);
        assert_eq!((i.installs, r.installs), (1, 1));
    }
}

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

#[test]
fn every_order_of_old_handshake_messages_preserves_new_connection() {
    let mut i = Initiator::default();
    let mut r = Responder::new(100);
    let old = establish(&mut i, &mut r, 1);
    let current = establish(&mut i, &mut r, 2);
    permutations(&mut [0, 1, 2, 3], 0, &mut |order| {
        let (mut i, mut r) = (i.clone(), r.clone());
        for message in order {
            match message {
                0 => {
                    r.hello(old.i);
                }
                1 => {
                    i.challenge(old);
                }
                2 => {
                    r.confirm(old);
                }
                3 => i.ready(old),
                _ => unreachable!(),
            }
            assert_eq!(i.active, Some(current));
            assert_eq!(r.active, Some(current));
            assert_eq!((i.installs, r.installs), (2, 2));
        }
    });
}

#[test]
fn responder_restart_requires_a_fresh_exchange() {
    let mut i = Initiator::default();
    let mut r = Responder::new(100);
    i.begin(1);
    let old = i.challenge(r.hello(1).unwrap()).unwrap();
    let mut r = Responder::new(1000);
    assert_eq!(r.confirm(old), None);
    let different = r.hello(1).unwrap();
    assert_eq!(i.challenge(different), None);
    // Pending attempts expire; the next attempt uses a fresh initiator tag.
    i.pending = None;
    r.pending = None;
    let current = establish(&mut i, &mut r, 2);
    assert_ne!(old, current);
    assert_eq!(r.confirm(old), None);
}

#[test]
fn replacement_preserves_active_state_until_each_confirmation() {
    let mut i = Initiator::default();
    let mut r = Responder::new(100);
    let old = establish(&mut i, &mut r, 1);
    i.begin(2);
    let pending = r.hello(2).unwrap();
    assert_eq!(r.hello(3), None);
    assert_eq!(r.hello(2), Some(pending));
    assert_eq!((i.active, r.active), (Some(old), Some(old)));
    let confirm = i.challenge(pending).unwrap();
    let ready = r.confirm(confirm).unwrap();
    assert_eq!((i.active, r.active), (Some(old), Some(pending)));
    // Lost READY: repeating CONFIRM does not reset the responder's state.
    assert_eq!(r.confirm(confirm), Some(ready));
    assert_eq!(r.installs, 2);
    i.ready(ready);
    assert_eq!(i.active, Some(pending));
    assert_eq!(i.installs, 2);
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
