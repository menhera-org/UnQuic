//! Exact per-group PTO bases; packet histories remain on their transmit workers.
use crate::pacing::PacketId;
use std::{io, mem::size_of, time::Duration};

const ABSENT: usize = usize::MAX;

struct Session {
    first: usize,
    count: usize,
    generation: Option<u64>,
    installed: bool,
    probe_lane: usize,
    previous: usize,
    next: usize,
}

struct Lane {
    session: usize,
    lane: u16,
    group: usize,
    outstanding: u32,
    latest: Duration,
    position: usize,
}

pub(super) struct Timers {
    sessions: Box<[Session]>,
    lanes: Box<[Lane]>,
    heaps: Box<[Vec<usize>]>,
    installed: Box<[usize]>,
}

impl Timers {
    /// Descriptors are (session, wire lane, group), ordered by session then lane.
    /// Sessions start at zero; each session's consecutive lanes start at one
    /// and share one group.
    pub(super) fn new(groups: usize, descriptors: &[(usize, u16, usize)]) -> io::Result<Self> {
        let sessions_count = layout(groups, descriptors).ok_or(io::ErrorKind::InvalidInput)?;
        Self::storage_bytes(groups, descriptors).ok_or(io::ErrorKind::InvalidInput)?;
        let mut sessions = Vec::new();
        sessions
            .try_reserve_exact(sessions_count)
            .map_err(io::Error::other)?;
        let mut lanes = Vec::new();
        lanes
            .try_reserve_exact(descriptors.len())
            .map_err(io::Error::other)?;
        for &(session, lane, group) in descriptors {
            if sessions.len() == session {
                sessions.push(Session {
                    first: lanes.len(),
                    count: 0,
                    generation: None,
                    installed: false,
                    probe_lane: 0,
                    previous: ABSENT,
                    next: ABSENT,
                });
            }
            sessions[session].count += 1;
            lanes.push(Lane {
                session,
                lane,
                group,
                outstanding: 0,
                latest: Duration::ZERO,
                position: ABSENT,
            });
        }
        let mut heaps = Vec::new();
        heaps.try_reserve_exact(groups).map_err(io::Error::other)?;
        for group in 0..groups {
            let mut heap = Vec::new();
            heap.try_reserve_exact(descriptors.iter().filter(|d| d.2 == group).count())
                .map_err(io::Error::other)?;
            heaps.push(heap);
        }
        let mut installed = Vec::new();
        installed
            .try_reserve_exact(groups)
            .map_err(io::Error::other)?;
        installed.resize(groups, ABSENT);
        Ok(Self {
            installed: installed.into_boxed_slice(),
            sessions: sessions.into_boxed_slice(),
            lanes: lanes.into_boxed_slice(),
            heaps: heaps.into_boxed_slice(),
        })
    }

    pub(super) fn storage_bytes(
        groups: usize,
        descriptors: &[(usize, u16, usize)],
    ) -> Option<usize> {
        let sessions = layout(groups, descriptors)?;
        size_of::<Self>()
            .checked_add(sessions.checked_mul(size_of::<Session>())?)?
            .checked_add(
                descriptors
                    .len()
                    .checked_mul(size_of::<Lane>() + size_of::<usize>())?,
            )?
            .checked_add(groups.checked_mul(size_of::<Vec<usize>>() + size_of::<usize>())?)
    }

    /// Duplicate installation preserves accounting. Replacements require settled lanes.
    pub(super) fn install(&mut self, session: usize, generation: u64) -> io::Result<()> {
        let state = self
            .sessions
            .get(session)
            .ok_or(io::ErrorKind::InvalidInput)?;
        if state.installed && state.generation == Some(generation) {
            return Ok(());
        }
        if state.generation.is_some_and(|old| generation <= old) {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let range = state.first..state.first + state.count;
        if self.lanes[range.clone()]
            .iter()
            .any(|lane| lane.outstanding != 0)
        {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        for lane in &mut self.lanes[range] {
            lane.latest = Duration::ZERO;
        }
        if !self.sessions[session].installed {
            let group = self.lanes[self.sessions[session].first].group;
            let next = self.installed[group];
            if next != ABSENT {
                self.sessions[next].previous = session;
            }
            self.sessions[session].next = next;
            self.installed[group] = session;
        }
        self.sessions[session].probe_lane = 0;
        self.sessions[session].generation = Some(generation);
        self.sessions[session].installed = true;
        Ok(())
    }

    pub(super) fn quiesce(&mut self, session: usize, generation: u64) -> io::Result<()> {
        let state = self
            .sessions
            .get_mut(session)
            .ok_or(io::ErrorKind::InvalidInput)?;
        if state.generation != Some(generation) {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        if self.lanes[state.first..state.first + state.count]
            .iter()
            .any(|lane| lane.outstanding != 0)
        {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        if state.installed {
            let group = self.lanes[state.first].group;
            let (previous, next) = (state.previous, state.next);
            state.installed = false;
            state.previous = ABSENT;
            state.next = ABSENT;
            if previous == ABSENT {
                self.installed[group] = next;
            } else {
                self.sessions[previous].next = next;
            }
            if next != ABSENT {
                self.sessions[next].previous = previous;
            }
        }
        Ok(())
    }

    /// An installed lane for backed-off probes when no eliciting charge remains.
    pub(super) fn installed(&self, group: usize) -> Option<PacketId> {
        let session = *self.installed.get(group)?;
        let state = self.sessions.get(session)?;
        Some(PacketId {
            session,
            generation: state.generation?,
            lane: 1,
            number: 0,
        })
    }

    /// Rotate sessions before lanes so one unanswered peer cannot monopolize stalled PTO.
    pub(super) fn next_probe(&mut self, group: usize, previous: Option<usize>) -> Option<PacketId> {
        let head = *self.installed.get(group)?;
        let session = previous
            .and_then(|index| self.sessions.get(index))
            .filter(|state| state.installed && self.lanes[state.first].group == group)
            .map_or(head, |state| {
                if state.next == ABSENT {
                    head
                } else {
                    state.next
                }
            });
        let state = self.sessions.get_mut(session)?;
        let lane = state.probe_lane;
        state.probe_lane = (lane + 1) % state.count;
        Some(PacketId {
            session,
            generation: state.generation?,
            lane: lane as u16 + 1,
            number: 0,
        })
    }

    /// Packet number is irrelevant to this installed lane identity.
    pub(super) fn matches(&self, id: PacketId) -> bool {
        self.index(id.session, id.lane, id.generation).is_some()
    }

    pub(super) fn submitted(
        &mut self,
        session: usize,
        lane: u16,
        generation: u64,
        count: u32,
        at: Duration,
    ) -> io::Result<()> {
        let index = self
            .index(session, lane, generation)
            .ok_or(io::ErrorKind::InvalidInput)?;
        let entry = &self.lanes[index];
        if count == 0 || at < entry.latest {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let outstanding = entry
            .outstanding
            .checked_add(count)
            .ok_or(io::ErrorKind::InvalidInput)?;
        let group = entry.group;
        let position = entry.position;
        self.lanes[index].outstanding = outstanding;
        self.lanes[index].latest = at;
        if position == ABSENT {
            let position = self.heaps[group].len();
            self.heaps[group].push(index);
            self.lanes[index].position = position;
            self.up(group, position);
        } else {
            // Successful send times only advance, so an existing key only moves down.
            self.down(group, position);
        }
        Ok(())
    }

    /// Outstanding eliciting transmissions on one installed lane.
    pub(super) fn outstanding(&self, session: usize, lane: u16, generation: u64) -> Option<u32> {
        Some(self.lanes[self.index(session, lane, generation)?].outstanding)
    }

    /// Count only first settlements of outstanding eliciting transmissions.
    pub(super) fn settled(
        &mut self,
        session: usize,
        lane: u16,
        generation: u64,
        count: u32,
    ) -> io::Result<()> {
        let index = self
            .index(session, lane, generation)
            .ok_or(io::ErrorKind::InvalidInput)?;
        let entry = &self.lanes[index];
        let remaining = entry
            .outstanding
            .checked_sub(count)
            .ok_or(io::ErrorKind::InvalidInput)?;
        if remaining == entry.outstanding {
            return Ok(());
        }
        let group = entry.group;
        let position = entry.position;
        self.lanes[index].outstanding = remaining;
        if remaining != 0 {
            return Ok(());
        }
        self.heaps[group].swap_remove(position);
        self.lanes[index].position = ABSENT;
        if let Some(&moved) = self.heaps[group].get(position) {
            self.lanes[moved].position = position;
            if position != 0 && self.less(moved, self.heaps[group][(position - 1) / 2]) {
                self.up(group, position);
            } else {
                self.down(group, position);
            }
        }
        Ok(())
    }

    /// Earliest eligible lane's latest send. Packet number is zero in this summary.
    pub(super) fn earliest(&self, group: usize) -> Option<(PacketId, Duration)> {
        let lane = self.lanes.get(*self.heaps.get(group)?.first()?)?;
        Some((
            PacketId {
                session: lane.session,
                generation: self.sessions.get(lane.session)?.generation?,
                lane: lane.lane,
                number: 0,
            },
            lane.latest,
        ))
    }

    fn index(&self, session: usize, lane: u16, generation: u64) -> Option<usize> {
        let state = self.sessions.get(session)?;
        let lane = usize::from(lane).checked_sub(1)?;
        (state.installed && state.generation == Some(generation) && lane < state.count)
            .then_some(state.first + lane)
    }

    fn less(&self, a: usize, b: usize) -> bool {
        (self.lanes[a].latest, a) < (self.lanes[b].latest, b)
    }

    fn swap(&mut self, group: usize, a: usize, b: usize) {
        self.heaps[group].swap(a, b);
        self.lanes[self.heaps[group][a]].position = a;
        self.lanes[self.heaps[group][b]].position = b;
    }

    fn up(&mut self, group: usize, mut position: usize) {
        while position != 0 {
            let parent = (position - 1) / 2;
            if !self.less(self.heaps[group][position], self.heaps[group][parent]) {
                break;
            }
            self.swap(group, position, parent);
            position = parent;
        }
    }

    fn down(&mut self, group: usize, mut position: usize) {
        while let Some(left) = position.checked_mul(2).and_then(|p| p.checked_add(1)) {
            if left >= self.heaps[group].len() {
                break;
            }
            let right = left + 1;
            let child = if right < self.heaps[group].len()
                && self.less(self.heaps[group][right], self.heaps[group][left])
            {
                right
            } else {
                left
            };
            if !self.less(self.heaps[group][child], self.heaps[group][position]) {
                break;
            }
            self.swap(group, position, child);
            position = child;
        }
    }
}

fn layout(groups: usize, descriptors: &[(usize, u16, usize)]) -> Option<usize> {
    if groups == 0 || descriptors.is_empty() {
        return None;
    }
    let mut previous: Option<(usize, u16, usize)> = None;
    for &(session, lane, group) in descriptors {
        if group >= groups
            || match previous {
                None => session != 0 || lane != 1,
                Some((old_session, old_lane, old_group)) if session == old_session => {
                    Some(lane) != old_lane.checked_add(1) || group != old_group
                }
                Some((old_session, _, _)) => session != old_session + 1 || lane != 1,
            }
        {
            return None;
        }
        previous = Some((session, lane, group));
    }
    descriptors.last()?.0.checked_add(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(session: usize, lane: u16, generation: u64) -> PacketId {
        PacketId {
            session,
            lane,
            generation,
            number: 123,
        }
    }

    #[test]
    fn replacement_and_rejections_preserve_live_counts() {
        let mut timers = Timers::new(2, &[(0, 1, 0), (0, 2, 0), (1, 1, 1)]).unwrap();
        assert!(!timers.matches(id(0, 1, 1)));
        timers.install(0, 1).unwrap();
        timers.install(1, 8).unwrap();
        assert!(timers.matches(id(0, 1, 1)));
        assert!(!timers.matches(id(0, 0, 1)));
        assert!(!timers.matches(id(0, 3, 1)));
        assert!(!timers.matches(id(2, 1, 1)));
        timers
            .submitted(0, 1, 1, 2, Duration::from_millis(10))
            .unwrap();
        timers
            .submitted(0, 2, 1, 1, Duration::from_millis(20))
            .unwrap();
        let before = timers.earliest(0);
        timers.install(0, 1).unwrap();
        for result in [
            timers.install(0, 2),
            timers.quiesce(0, 1),
            timers.submitted(0, 1, 0, 1, Duration::from_millis(30)),
            timers.submitted(0, 1, 1, 1, Duration::ZERO),
            timers.submitted(0, 1, 1, 0, Duration::from_millis(30)),
            timers.settled(0, 1, 1, 3),
        ] {
            assert!(result.is_err());
            assert_eq!(timers.earliest(0), before);
        }
        timers.settled(0, 1, 1, 1).unwrap();
        assert_eq!(timers.earliest(0), before);
        timers
            .submitted(0, 1, 1, 1, Duration::from_millis(30))
            .unwrap();
        assert_eq!(timers.earliest(0).unwrap().0.lane, 2);
        timers.settled(0, 2, 1, 1).unwrap();
        assert_eq!(timers.earliest(0).unwrap().0.lane, 1);
        timers.settled(0, 1, 1, 2).unwrap();
        assert_eq!(timers.earliest(0), None);
        timers.quiesce(0, 1).unwrap();
        timers.quiesce(0, 1).unwrap();
        assert!(!timers.matches(id(0, 1, 1)));
        assert!(timers.install(0, 1).is_err());
        timers.install(0, 2).unwrap();
        assert!(timers.matches(id(0, 1, 2)));
        assert!(timers.settled(0, 1, 1, 0).is_err());
        timers.submitted(0, 1, 2, u32::MAX, Duration::MAX).unwrap();
        assert!(timers.submitted(0, 1, 2, 1, Duration::MAX).is_err());
        timers.settled(0, 1, 2, u32::MAX).unwrap();
        assert_eq!(timers.earliest(0), None);
        assert_eq!(timers.earliest(1), None);
        assert_eq!(timers.earliest(2), None);
    }

    #[test]
    fn interleaved_heap_updates_and_removals_match_an_independent_scan() {
        let descriptors: Vec<_> = (0..4)
            .flat_map(|session| (1..=16).map(move |lane| (session, lane, session % 3)))
            .collect();
        let mut timers = Timers::new(3, &descriptors).unwrap();
        for session in 0..4 {
            timers.install(session, session as u64).unwrap();
        }
        let mut expected = [(0_u32, Duration::ZERO); 64];
        let capacities: Vec<_> = timers.heaps.iter().map(Vec::capacity).collect();
        let mut random = 7_u64;
        for step in 0..10_000 {
            random = random.wrapping_mul(6364136223846793005).wrapping_add(1);
            let index = (random >> 32) as usize % descriptors.len();
            let (session, lane, _) = descriptors[index];
            if step % 3 == 0 && expected[index].0 != 0 {
                let count = if step % 2 == 0 { expected[index].0 } else { 1 };
                timers
                    .settled(session, lane, session as u64, count)
                    .unwrap();
                expected[index].0 -= count;
            } else {
                let at = Duration::from_micros(step / 4); // Include equal keys.
                timers
                    .submitted(session, lane, session as u64, 1, at)
                    .unwrap();
                expected[index].0 += 1;
                expected[index].1 = at;
            }
            for (group, capacity) in capacities.iter().enumerate() {
                let first = expected
                    .iter()
                    .enumerate()
                    .filter(|(index, state)| state.0 != 0 && descriptors[*index].2 == group)
                    .min_by_key(|(index, state)| (state.1, *index))
                    .map(|(index, state)| {
                        let (session, lane, _) = descriptors[index];
                        (
                            PacketId {
                                number: 0,
                                ..id(session, lane, session as u64)
                            },
                            state.1,
                        )
                    });
                assert_eq!(timers.earliest(group), first);
                assert_eq!(timers.heaps[group].capacity(), *capacity);
            }
            for (index, entry) in timers.lanes.iter().enumerate() {
                assert_eq!(entry.outstanding, expected[index].0);
                if entry.outstanding == 0 {
                    assert_eq!(entry.position, ABSENT);
                } else {
                    assert_eq!(timers.heaps[entry.group][entry.position], index);
                }
            }
        }
    }

    #[test]
    fn installed_fallback_survives_middle_head_and_tail_replacements() {
        let mut timers = Timers::new(1, &[(0, 1, 0), (1, 1, 0), (2, 1, 0)]).unwrap();
        assert_eq!(timers.installed(0), None);
        assert_eq!(timers.installed(1), None);
        for session in 0..3 {
            timers.install(session, session as u64 + 1).unwrap();
        }
        assert_eq!(timers.installed(0).unwrap().session, 2);
        timers.quiesce(1, 2).unwrap();
        assert_eq!(timers.installed(0).unwrap().session, 2);
        timers.quiesce(2, 3).unwrap();
        assert_eq!(timers.installed(0).unwrap().session, 0);
        timers.install(1, 3).unwrap();
        timers.install(1, 3).unwrap();
        timers.quiesce(0, 1).unwrap();
        assert_eq!(
            timers.installed(0),
            Some(PacketId {
                number: 0,
                ..id(1, 1, 3)
            })
        );
        timers.install(2, 4).unwrap();
        timers.quiesce(2, 4).unwrap();
        assert_eq!(timers.installed(0).unwrap().session, 1);
        timers.quiesce(1, 3).unwrap();
        assert_eq!(timers.installed(0), None);
        assert_eq!(timers.earliest(0), None);
    }

    #[test]
    fn stalled_probe_rotation_visits_sessions_before_their_other_lanes() {
        let descriptors = [(0, 1, 0), (0, 2, 0), (0, 3, 0), (1, 1, 0), (2, 1, 1)];
        let mut timers = Timers::new(2, &descriptors).unwrap();
        assert!(timers.next_probe(0, None).is_none());
        for session in 0..3 {
            timers.install(session, 1).unwrap();
        }
        let mut previous = None;
        for (session, lane) in [
            (1, 1),
            (0, 1),
            (1, 1),
            (0, 2),
            (1, 1),
            (0, 3),
            (1, 1),
            (0, 1),
        ] {
            let probe = timers.next_probe(0, previous).unwrap();
            assert_eq!((probe.session, probe.lane), (session, lane));
            previous = Some(probe.session);
        }
        assert_eq!(timers.next_probe(1, previous).unwrap().session, 2);
        timers.quiesce(0, 1).unwrap();
        assert_eq!(timers.next_probe(0, previous).unwrap().session, 1);
        timers.quiesce(1, 1).unwrap();
        assert!(timers.next_probe(0, previous).is_none());
        timers.install(0, 2).unwrap();
        assert_eq!(
            timers.next_probe(0, Some(usize::MAX)).unwrap(),
            PacketId {
                number: 0,
                ..id(0, 1, 2)
            }
        );
    }

    #[test]
    fn descriptor_validation_and_reserved_storage_are_exact() {
        for descriptors in [
            vec![],
            vec![(1, 1, 0)],
            vec![(0, 0, 0)],
            vec![(0, 1, 1)],
            vec![(0, 1, 0), (0, 3, 0)],
            vec![(0, 1, 0), (2, 1, 0)],
            vec![(0, 1, 0), (0, 1, 0)],
        ] {
            assert!(Timers::storage_bytes(1, &descriptors).is_none());
            assert!(Timers::new(1, &descriptors).is_err());
        }
        let descriptors = [(0, 1, 0), (1, 1, 1), (1, 2, 1)];
        assert!(Timers::storage_bytes(0, &descriptors).is_none());
        assert!(Timers::storage_bytes(usize::MAX, &descriptors).is_none());
        let timers = Timers::new(3, &descriptors).unwrap();
        let actual = size_of::<Timers>()
            + timers.sessions.len() * size_of::<Session>()
            + timers.lanes.len() * size_of::<Lane>()
            + timers.heaps.len() * size_of::<Vec<usize>>()
            + timers.installed.len() * size_of::<usize>()
            + timers
                .heaps
                .iter()
                .map(|heap| heap.capacity() * size_of::<usize>())
                .sum::<usize>();
        assert_eq!(Timers::storage_bytes(3, &descriptors), Some(actual));
    }
}
