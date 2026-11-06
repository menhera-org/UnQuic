use std::ops::Range;
use unquic::{MAX_FRAME_LEN, loss::FrameRecovery};

#[test]
fn lost_originals_repair_only_missing_bytes_and_stop_after_two_attempts() {
    let mut frame = FrameRecovery::new(1514).unwrap();
    assert_eq!(frame.next_repair(1514), None);
    assert!(!frame.submitted(1..100, 0));
    assert!(frame.submitted(0..1164, 0));
    assert!(frame.submitted(1164..1514, 0));
    assert_eq!(frame.next_repair(1514), None);
    assert_eq!(frame.acknowledge(1164..1514, 0), Some(350));
    assert!(frame.lost(0..1164, 0));
    assert_eq!(frame.next_repair(1000), Some((0..1000, 1)));
    assert!(frame.submitted(0..1000, 1));
    // A duplicate loss of the original cannot reopen its newer copy.
    assert!(frame.lost(0..1164, 0));
    assert_eq!(frame.next_repair(1514), Some((1000..1164, 1)));
    assert!(frame.submitted(1000..1164, 1));
    assert!(frame.lost(0..1164, 1));
    assert_eq!(frame.next_repair(1514), Some((0..1164, 2)));
    assert!(frame.submitted(0..1164, 2));
    assert!(frame.lost(0..1164, 2));
    assert_eq!(frame.next_repair(1514), None);
    assert_eq!(frame.remaining(), 1164);
    assert!(!frame.submitted(0..1164, 3));
    assert_eq!(frame.acknowledge(0..1164, 0), Some(1164));
    assert_eq!(frame.remaining(), 0);
    assert_eq!(frame.acknowledge(0..1514, 0), Some(0));
}

#[test]
fn late_ack_cancels_a_repair_plan_and_preserves_other_copies_accounting_boundary() {
    let mut frame = FrameRecovery::new(128).unwrap();
    assert!(frame.submitted(0..128, 0));
    assert!(frame.lost(0..128, 0));
    let plan = frame.next_repair(64).unwrap();
    assert!(frame.can_submit(plan.0.clone(), plan.1));
    assert_eq!(frame.acknowledge(32..64, 0), Some(32));
    assert!(!frame.can_submit(plan.0.clone(), plan.1));
    assert!(!frame.submitted(plan.0, plan.1));
    assert_eq!(frame.next_repair(128), Some((0..32, 1)));
    assert!(frame.submitted(0..32, 1));
    assert_eq!(frame.acknowledge(0..48, 0), Some(32));
    assert!(frame.lost(0..32, 1));
    assert_eq!(frame.next_repair(128), Some((64..128, 1)));
    assert_eq!(frame.remaining(), 64);
}

#[test]
fn boundaries_exhaustion_and_invalid_mappings_leave_state_consistent() {
    assert_eq!(size_of::<FrameRecovery>(), 3392);
    for length in [0, 13, MAX_FRAME_LEN + 1, usize::MAX] {
        assert!(FrameRecovery::new(length).is_none());
    }
    let mut frame = FrameRecovery::new(MAX_FRAME_LEN).unwrap();
    assert!(frame.submitted(0..MAX_FRAME_LEN - 1, 0));
    assert_eq!(frame.next_repair(usize::MAX), None);
    for (start, end) in [(0, 0), (64, 63), (0, MAX_FRAME_LEN), (0, usize::MAX)] {
        let range = start..end;
        assert!(!frame.lost(range.clone(), 0));
        assert_eq!(frame.acknowledge(range, 0), None);
    }
    assert!(!frame.lost(0..64, 2));
    assert_eq!(frame.acknowledge(0..64, 2), None);
    assert!(frame.lost(63..65, 0));
    assert_eq!(frame.next_repair(0), None);
    assert_eq!(frame.next_repair(usize::MAX), Some((63..65, 1)));
    assert!(frame.submitted(63..64, 1));
    // Whole-range validation cannot partially increment byte 64's generation.
    assert!(!frame.submitted(64..66, 1));
    assert_eq!(frame.next_repair(64), Some((64..65, 1)));
    assert!(frame.lost(63..64, 1));
    assert_eq!(frame.next_repair(64), Some((63..64, 2)));
    assert!(frame.submitted(MAX_FRAME_LEN - 1..MAX_FRAME_LEN, 0));
    assert!(frame.lost(65..MAX_FRAME_LEN, 0));
    assert_eq!(frame.acknowledge(0..MAX_FRAME_LEN, 0), Some(MAX_FRAME_LEN));
    assert_eq!(frame.remaining(), 0);
    assert_eq!(frame.next_repair(usize::MAX), None);
}

struct Model {
    generation: Vec<u8>,
    pending: Vec<bool>,
    sent: usize,
}

impl Model {
    fn can_submit(&self, range: &Range<usize>, attempt: u8) -> bool {
        if range.start >= range.end || range.end > self.generation.len() || attempt > 2 {
            return false;
        }
        if attempt == 0 {
            return range.start == self.sent;
        }
        range.end <= self.sent
            && range
                .clone()
                .all(|i| self.generation[i] == attempt - 1 && !self.pending[i])
    }

    fn feedback(&self, range: &Range<usize>, attempt: u8) -> bool {
        range.start < range.end
            && range.end <= self.sent
            && attempt <= 2
            && range.clone().all(|i| self.generation[i] >= attempt)
    }

    fn next(&self, maximum: usize) -> Option<(Range<usize>, u8)> {
        if maximum == 0 {
            return None;
        }
        let start = (0..self.sent).find(|&i| self.generation[i] < 2 && !self.pending[i])?;
        let attempt = self.generation[start] + 1;
        let mut end = start;
        while end < self.sent
            && end - start < maximum
            && self.generation[end] == attempt - 1
            && !self.pending[end]
        {
            end += 1;
        }
        Some((start..end, attempt))
    }
}

#[test]
fn word_masks_match_a_byte_model_through_reordered_partial_transmissions() {
    let mut seed = 17_u64;
    let mut random = || {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        (seed >> 32) as usize
    };
    for length in [14, 63, 64, 65, 1514, MAX_FRAME_LEN] {
        for _ in 0..16 {
            let mut frame = FrameRecovery::new(length).unwrap();
            let mut model = Model {
                generation: vec![0; length],
                pending: vec![false; length],
                sent: 0,
            };
            let mut sent = Vec::<(Range<usize>, u8)>::new();
            for _ in 0..256 {
                let operation = random() % 4;
                let maximum = random() % (length + 1);
                assert_eq!(frame.next_repair(maximum), model.next(maximum));
                let (range, attempt) = match operation {
                    0 => (model.sent..(model.sent + maximum).min(length), 0),
                    1 => model.next(maximum).unwrap_or((0..0, 1)),
                    _ if !sent.is_empty() && random() % 4 != 0 => {
                        sent[random() % sent.len()].clone()
                    }
                    _ => {
                        let start = random() % (length + 2);
                        let end = random() % (length + 2);
                        (start..end, (random() % 4) as u8)
                    }
                };
                if operation < 2 {
                    let valid = model.can_submit(&range, attempt);
                    assert_eq!(frame.can_submit(range.clone(), attempt), valid);
                    assert_eq!(frame.submitted(range.clone(), attempt), valid);
                    if valid {
                        for i in range.clone() {
                            model.generation[i] = attempt;
                            model.pending[i] = true;
                        }
                        if attempt == 0 {
                            model.sent = range.end;
                        }
                        sent.push((range, attempt));
                    }
                } else {
                    let valid = model.feedback(&range, attempt);
                    if operation == 2 {
                        assert_eq!(frame.lost(range.clone(), attempt), valid);
                        if valid {
                            for i in range {
                                if model.generation[i] == attempt {
                                    model.pending[i] = false;
                                }
                            }
                        }
                    } else {
                        let observed = frame.acknowledge(range.clone(), attempt);
                        let new = valid.then(|| {
                            let mut new = 0;
                            for i in range {
                                new += usize::from(model.generation[i] != 3);
                                model.generation[i] = 3;
                                model.pending[i] = false;
                            }
                            new
                        });
                        assert_eq!(observed, new);
                    }
                }
                assert_eq!(
                    frame.remaining(),
                    model.generation.iter().filter(|&&g| g != 3).count()
                );
                assert_eq!(frame.sent(), model.sent);
                assert_eq!(frame.next_repair(usize::MAX), model.next(usize::MAX));
            }
        }
    }
}

#[test]
fn slot_reuse_clears_new_length_and_invalid_reset_preserves_live_state() {
    let mut frame = FrameRecovery::new(MAX_FRAME_LEN).unwrap();
    for length in [MAX_FRAME_LEN, 14, 65, 1514, MAX_FRAME_LEN] {
        assert!(frame.reset(length));
        assert_eq!(frame.sent(), 0);
        assert_eq!(frame.remaining(), length);
        assert_eq!(frame.next_repair(usize::MAX), None);
        assert!(frame.submitted(0..length, 0));
        assert!(frame.lost(0..length, 0));
        for invalid in [0, 13, MAX_FRAME_LEN + 1, usize::MAX] {
            assert!(!frame.reset(invalid));
            assert_eq!(frame.sent(), length);
            assert_eq!(frame.remaining(), length);
            assert_eq!(frame.next_repair(usize::MAX), Some((0..length, 1)));
        }
        assert!(frame.submitted(0..length, 1));
        assert!(frame.lost(0..length, 1));
        assert!(frame.submitted(0..length, 2));
        assert_eq!(frame.acknowledge(0..length, 0), Some(length));
    }
}
