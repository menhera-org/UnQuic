//! Reserved feedback storage and fair, bounded control transmission.
use super::*;
use crate::wire::{AckRange, AckRanges, MAX_ACK_RANGES};

pub(super) const BUFFERS: usize = 4;
pub(super) const REPLIES: usize = 16;

pub(crate) struct Feedback {
    origin: usize,
    session: usize,
    tag: u128,
    lane: u16,
    slot: usize,
    count: usize,
    delay_us: u32,
    created: Duration,
    sent: Option<Duration>,
    due: Duration,
    ranges: [AckRange; MAX_ACK_RANGES],
}

impl Feedback {
    pub(super) fn new(origin: usize) -> Self {
        Self {
            origin,
            session: 0,
            tag: 0,
            lane: 0,
            slot: 0,
            count: 0,
            delay_us: 0,
            created: Duration::ZERO,
            sent: None,
            due: Duration::ZERO,
            ranges: [AckRange { start: 0, end: 0 }; MAX_ACK_RANGES],
        }
    }
}

pub(super) enum Pending {
    Handshake(usize),
    Reply {
        session: usize,
        tag: u128,
        lane: u16,
        packet: u64,
        length: u16,
        due: Duration,
    },
    Ack(Box<Feedback>),
}

impl Worker {
    pub(super) fn queue_handshake(&mut self, session: usize, now: Duration) {
        let control = &mut self.controls[session];
        control.send_due = now;
        if !control.queued {
            control.queued = true;
            self.pending_control.push_back(Pending::Handshake(session));
        }
    }

    pub(super) fn queue_reply(
        &mut self,
        session: usize,
        tag: u128,
        lane: u16,
        packet: u64,
        length: u16,
        now: Duration,
    ) {
        if self.control_replies == REPLIES {
            self.counters.queue_drops = self.counters.queue_drops.wrapping_add(1);
            let totals = &mut self.sessions[session].totals;
            totals.queue_drops = totals.queue_drops.wrapping_add(1);
            return;
        }
        self.control_replies += 1;
        self.pending_control.push_back(Pending::Reply {
            session,
            tag,
            lane,
            packet,
            length,
            due: now,
        });
    }

    pub(super) fn receive_feedback(&mut self, feedback: Box<Feedback>) -> io::Result<()> {
        if feedback.count != 0 && self.id == 0 {
            self.pending_control.push_back(Pending::Ack(feedback));
        } else if feedback.count == 0 && feedback.origin == self.id {
            if self.matches(feedback.session, feedback.tag) {
                let local = &mut self.sessions[feedback.session];
                if let Some(lane) = local.rx[feedback.slot].as_mut()
                    && lane.id() == feedback.lane
                {
                    local.ack_inflight &= !(1 << feedback.slot);
                    if let Some(sent) = feedback.sent {
                        lane.ack_transmitted(sent);
                    }
                }
            }
            self.feedback_free.push(feedback);
        } else {
            return Err(io::Error::other("feedback delivered to the wrong owner"));
        }
        Ok(())
    }

    pub(super) fn collect_acks(&mut self, now: Duration) -> io::Result<()> {
        if now < self.feedback_retry {
            return Ok(());
        }
        let slots = self.sessions.len() * LANES;
        for _ in 0..slots.min(BATCH_SIZE) {
            if self.feedback_free.is_empty() {
                break;
            }
            if self.id != 0 && self.outgoing[0].as_mut().is_none_or(Producer::is_full) {
                self.feedback_retry = now.saturating_add(RETRY);
                break;
            }
            let index = self.feedback_cursor;
            self.feedback_cursor = (index + 1) % slots;
            let (session, slot) = (index / LANES, index % LANES);
            let local = &mut self.sessions[session];
            if local.ack_inflight & (1 << slot) != 0 || local.returning & (1 << slot) != 0 {
                continue;
            }
            let Some(connection) = local.connection else {
                continue;
            };
            let Some(lane) = local.rx[slot].as_mut() else {
                continue;
            };
            if lane.ack_deadline().is_none_or(|due| due > now)
                || self.shared.owners[session][usize::from(lane.id())].load(Ordering::Acquire)
                    != self.id as u8 + 1
            {
                continue;
            }
            let Some(mut feedback) = self.feedback_free.pop() else {
                break;
            };
            let Some(Record::Ack {
                lane,
                delay_us,
                ranges,
            }) = lane.take_ack(now, &mut feedback.ranges)
            else {
                self.feedback_free.push(feedback);
                continue;
            };
            feedback.count = ranges.len();
            feedback.lane = lane;
            feedback.delay_us = delay_us;
            feedback.session = session;
            feedback.slot = slot;
            feedback.tag = connection.local_tag().get();
            feedback.created = now;
            feedback.sent = None;
            feedback.due = now;
            local.ack_inflight |= 1 << slot;
            if self.id == 0 {
                self.pending_control.push_back(Pending::Ack(feedback));
            } else if let Err(Event::Feedback(_)) = self.push(0, Event::Feedback(feedback)) {
                return Err(io::Error::other("reserved feedback queue slot disappeared"));
            }
        }
        Ok(())
    }

    fn return_feedback(&mut self, mut feedback: Box<Feedback>, now: Duration) -> io::Result<()> {
        feedback.count = 0;
        if feedback.origin == self.id {
            self.receive_feedback(feedback)?;
        } else if let Err(Event::Feedback(mut feedback)) =
            self.push(feedback.origin, Event::Feedback(feedback))
        {
            feedback.due = now.saturating_add(RETRY);
            self.pending_control.push_back(Pending::Ack(feedback));
        }
        Ok(())
    }

    fn control_due(&self, session: usize, now: Duration) -> io::Result<Duration> {
        let peer = self
            .shared
            .config
            .peer_for_session(session)
            .ok_or(io::ErrorKind::InvalidInput)?;
        let group = self
            .shared
            .config
            .path_group_for_peer(peer)
            .ok_or(io::ErrorKind::InvalidInput)?;
        Ok(self.control_budgets[group]
            .deadline()
            .unwrap_or(now)
            .max(now))
    }

    pub(super) fn flush_control(&mut self, now: Duration) -> io::Result<()> {
        for _ in 0..self.pending_control.len().min(BATCH_SIZE) {
            let now = self.io_clock(now)()?;
            let Some(job) = self.pending_control.pop_front() else {
                break;
            };
            let due = match &job {
                Pending::Handshake(session) => self.controls[*session].send_due,
                Pending::Reply { due, .. } => *due,
                Pending::Ack(feedback) => feedback.due,
            };
            if due > now {
                self.pending_control.push_back(job);
                continue;
            }
            match job {
                Pending::Handshake(session) => {
                    let peer = self
                        .shared
                        .config
                        .peer_for_session(session)
                        .ok_or(io::ErrorKind::InvalidInput)?;
                    if self.shared.peer_bound(peer) && !self.controls[session].barrier.is_active() {
                        self.flush_handshake(session, now)?;
                    } else {
                        self.controls[session].send_due = now.saturating_add(RETRY);
                    }
                    if self.controls[session].pending_send.is_some() {
                        self.pending_control.push_back(Pending::Handshake(session));
                    } else {
                        self.controls[session].queued = false;
                    }
                }
                Pending::Reply {
                    session,
                    tag,
                    lane,
                    packet,
                    length,
                    ..
                } => {
                    if self.matches(session, tag) {
                        let due = self.control_due(session, now)?;
                        if due > now {
                            self.pending_control.push_back(Pending::Reply {
                                session,
                                tag,
                                lane,
                                packet,
                                length,
                                due,
                            });
                            continue;
                        }
                        self.send_record(
                            session,
                            Record::ProbeReply {
                                lane,
                                packet_number: packet,
                                payload_len: length,
                            },
                            now,
                        )?;
                    }
                    self.control_replies -= 1;
                }
                Pending::Ack(mut feedback) => {
                    if feedback.count != 0
                        && self.matches(feedback.session, feedback.tag)
                        && self.shared.owners[feedback.session][usize::from(feedback.lane)]
                            .load(Ordering::Acquire)
                            == feedback.origin as u8 + 1
                    {
                        let due = self.control_due(feedback.session, now)?;
                        if due > now {
                            feedback.due = due;
                            self.pending_control.push_back(Pending::Ack(feedback));
                            continue;
                        }
                        let queued =
                            now.saturating_sub(feedback.created).as_micros().min(1000) as u32;
                        let ranges = AckRanges::new(&feedback.ranges[..feedback.count])
                            .map_err(io::Error::other)?;
                        if let Some((_, _, started)) = self.send_record(
                            feedback.session,
                            Record::Ack {
                                lane: feedback.lane,
                                delay_us: (feedback.delay_us + queued).min(1000),
                                ranges,
                            },
                            now,
                        )? {
                            feedback.sent = Some(started);
                        } else {
                            self.counters.ack_send_deferred =
                                self.counters.ack_send_deferred.wrapping_add(1);
                            let totals = &mut self.sessions[feedback.session].totals;
                            totals.ack_send_deferred = totals.ack_send_deferred.wrapping_add(1);
                            // take_ack consumed the schedule; retain its snapshot until submitted.
                            feedback.due = self
                                .control_due(feedback.session, now)?
                                .max(now.saturating_add(RETRY));
                            self.pending_control.push_back(Pending::Ack(feedback));
                            continue;
                        }
                    }
                    self.return_feedback(feedback, now)?;
                }
            }
        }
        Ok(())
    }

    pub(super) fn feedback_deadline(&self) -> Option<Duration> {
        self.pending_control
            .iter()
            .map(|job| match job {
                Pending::Handshake(session) => self.controls[*session].send_due,
                Pending::Reply { due, .. } => *due,
                Pending::Ack(feedback) => feedback.due,
            })
            .min()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::worker::tests::{receive_bytes, receive_piece, receive_worker, test_workers};

    fn socket(worker: &mut Worker) -> ReceiveSocket {
        let rx = ReceiveSocket::bind("[::1]:40001".parse().unwrap(), None).unwrap();
        worker.transmit[0].paths[0].lanes[0].socket =
            Some(TransmitSocket::connect(&rx, rx.local_addr()).unwrap());
        rx
    }

    #[test]
    #[ignore = "kernel descriptor test; run in a disposable container"]
    fn failed_ack_submission_keeps_snapshot_and_yields_until_retry() {
        for stale in [false, true] {
            let (mut worker, _tap) = receive_worker();
            let mut bytes = [0; BASE_UDP_PAYLOAD];
            let length = receive_bytes(2, &[receive_piece(1, 0, &[1; 60])], &mut bytes);
            worker.sessions[0].rx[0]
                .as_mut()
                .unwrap()
                .admit(&Datagram::decode(&bytes[..length]).unwrap(), Duration::ZERO);
            worker.collect_acks(Duration::ZERO).unwrap();
            // No transmit socket: the snapshot must outlive this unsuccessful attempt.
            worker.flush_control(Duration::ZERO).unwrap();
            assert_eq!(worker.counters.ack_send_deferred, 1);
            assert_eq!(worker.sessions[0].totals.ack_send_deferred, 1);
            assert_eq!(worker.feedback_deadline(), Some(RETRY));
            assert_eq!(worker.feedback_free.len(), BUFFERS - 1);
            assert_eq!(worker.sessions[0].ack_inflight, 1);
            let rx = socket(&mut worker);
            worker
                .flush_control(RETRY - Duration::from_nanos(1))
                .unwrap();
            assert_eq!(worker.counters.ack_send_deferred, 1);
            assert_eq!(
                rx.recv(&mut bytes).unwrap_err().kind(),
                io::ErrorKind::WouldBlock
            );
            if stale {
                worker.shared.owners[0][7].store(255, Ordering::Release);
            }
            worker.flush_control(RETRY).unwrap();
            assert!(worker.pending_control.is_empty());
            assert_eq!(worker.feedback_free.len(), BUFFERS);
            assert_eq!(worker.sessions[0].ack_inflight, 0);
            if stale {
                assert_eq!(
                    rx.recv(&mut bytes).unwrap_err().kind(),
                    io::ErrorKind::WouldBlock
                );
            } else {
                let length = rx.recv(&mut bytes).unwrap().0;
                let packet = Datagram::decode(&bytes[..length]).unwrap();
                let Some(Record::Ack {
                    ranges, delay_us, ..
                }) = packet.records().next()
                else {
                    panic!("missing retained ACK");
                };
                assert_eq!(
                    ranges.iter().collect::<Vec<_>>(),
                    [AckRange { start: 2, end: 2 }]
                );
                assert_eq!(delay_us, 1000);
                worker.flush_control(RETRY * 2).unwrap();
                assert_eq!(
                    rx.recv(&mut bytes).unwrap_err().kind(),
                    io::ErrorKind::WouldBlock
                );
            }
        }
    }

    #[test]
    #[ignore = "kernel descriptor test; run in a disposable container"]
    fn emitted_ack_excludes_pressure_rejection_even_when_its_tail_was_retained() {
        let (mut worker, _) = receive_worker();
        let rx = socket(&mut worker);
        let mut bytes = [0; BASE_UDP_PAYLOAD];
        let lane = worker.sessions[0].rx[0].as_mut().unwrap();
        for number in 1..=reassembly::FRAME_CAPACITY as u64 {
            let length = receive_bytes(number, &[receive_piece(number, 0, &[1; 30])], &mut bytes);
            assert_eq!(
                lane.admit(&Datagram::decode(&bytes[..length]).unwrap(), Duration::ZERO),
                Admission::Fresh
            );
        }
        let rejected = reassembly::FRAME_CAPACITY as u64 + 1;
        let length = receive_bytes(
            rejected,
            &[
                receive_piece(1, 30, &[2; 30]),
                receive_piece(rejected, 0, &[3; 30]),
            ],
            &mut bytes,
        );
        assert_eq!(
            lane.admit(&Datagram::decode(&bytes[..length]).unwrap(), Duration::ZERO),
            Admission::Pressure
        );
        assert_eq!(lane.ready_count(), 1);
        worker.collect_acks(Duration::ZERO).unwrap();
        assert_eq!(worker.feedback_free.len(), BUFFERS - 1);
        assert_eq!(worker.sessions[0].ack_inflight, 1);
        worker.flush_control(Duration::from_micros(500)).unwrap();
        assert_eq!(worker.feedback_free.len(), BUFFERS);
        assert_eq!(worker.sessions[0].ack_inflight, 0);
        let length = rx.recv(&mut bytes).unwrap().0;
        let packet = Datagram::decode(&bytes[..length]).unwrap();
        assert_eq!(packet.header().receive_tag, 2);
        assert!(!packet.records().any(|record| record.ack_eliciting()));
        let Record::Ack {
            lane,
            delay_us,
            ranges,
        } = packet.records().next().unwrap()
        else {
            panic!("missing ACK");
        };
        assert_eq!((lane, delay_us, ranges.len()), (7, 500, 1));
        assert_eq!(
            ranges.iter().next().unwrap(),
            AckRange {
                start: 1,
                end: rejected - 1
            }
        );
        assert_eq!(
            rx.recv(&mut bytes).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
    }

    #[test]
    #[ignore = "kernel descriptor test; run in a disposable container"]
    fn delayed_ack_includes_queue_time_and_spaces_duplicate_feedback_from_send() {
        let (mut worker, _tap) = receive_worker();
        let rx = socket(&mut worker);
        let mut bytes = [0; BASE_UDP_PAYLOAD];
        let length = receive_bytes(2, &[receive_piece(1, 0, &[1; 60])], &mut bytes);
        let packet = Datagram::decode(&bytes[..length]).unwrap();
        let lane = worker.sessions[0].rx[0].as_mut().unwrap();
        assert_eq!(lane.admit(&packet, Duration::ZERO), Admission::Fresh);
        worker.collect_acks(Duration::ZERO).unwrap();
        std::thread_local! {
            static CLOCK_STEP: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
        }
        CLOCK_STEP.set(0);
        worker.io_time = Some(|_| {
            let step = CLOCK_STEP.replace(CLOCK_STEP.get() + 1);
            Ok(Duration::from_micros([200, 300, 700][step]))
        });
        worker.flush_control(Duration::from_micros(100)).unwrap();
        let mut output = [0; BASE_UDP_PAYLOAD];
        let length = rx.recv(&mut output).unwrap().0;
        assert!(matches!(
            Datagram::decode(&output[..length])
                .unwrap()
                .records()
                .next(),
            Some(Record::Ack { delay_us: 700, .. })
        ));
        let lane = worker.sessions[0].rx[0].as_mut().unwrap();
        assert_eq!(
            lane.admit(&packet, Duration::from_micros(800)),
            Admission::Duplicate
        );
        assert!(lane.ack_deadline().unwrap() >= Duration::from_micros(1700));
    }

    #[test]
    #[ignore = "kernel descriptor test; run in a disposable container"]
    fn control_queue_shares_released_slots_between_handshakes_replies_and_acks() {
        let (mut worker, _) = receive_worker();
        worker
            .command(1, worker.sessions[0].installed.unwrap(), 0)
            .unwrap();
        worker.shared.bound[0].store(3, Ordering::Release);
        let rx = socket(&mut worker);
        let now = Duration::from_millis(10);
        let reply = Record::ProbeReply {
            lane: 7,
            packet_number: 1,
            payload_len: 52,
        };
        let mut bytes = [0; BASE_UDP_PAYLOAD];
        for _ in 0..16 {
            assert!(worker.send_record(1, reply, now).unwrap().is_some());
            rx.recv(&mut bytes).unwrap();
        }
        let effects = worker.controls[0].state.on_timer(now, Tag::new(9)).unwrap();
        worker.effects(0, effects, now).unwrap();
        worker.queue_reply(1, 1, 7, 1, 52, now);
        let length = receive_bytes(1, &[receive_piece(1, 0, &[0; 60])], &mut bytes);
        assert_eq!(
            worker.sessions[0].rx[0]
                .as_mut()
                .unwrap()
                .admit(&Datagram::decode(&bytes[..length]).unwrap(), now - RETRY),
            Admission::Fresh
        );
        worker.collect_acks(now).unwrap();
        worker.flush_control(now).unwrap();
        assert_eq!(worker.pending_control.len(), 3);
        assert_eq!(
            worker.feedback_deadline(),
            worker.control_budgets[0].deadline()
        );
        for expected in 0..3 {
            let due = worker.feedback_deadline().unwrap();
            assert!(due > now);
            worker.flush_control(due).unwrap();
            let length = rx.recv(&mut bytes).unwrap().0;
            let record = Datagram::decode(&bytes[..length])
                .unwrap()
                .records()
                .next()
                .unwrap();
            assert!(matches!(
                (expected, record),
                (0, Record::Hello { .. })
                    | (1, Record::ProbeReply { .. })
                    | (2, Record::Ack { .. })
            ));
            assert_eq!(
                rx.recv(&mut bytes).unwrap_err().kind(),
                io::ErrorKind::WouldBlock
            );
        }
        assert!(worker.pending_control.is_empty());
        assert_eq!(worker.feedback_free.len(), BUFFERS);
        assert_eq!(worker.control_replies, 0);
    }

    #[test]
    #[ignore = "kernel descriptor test; run in a disposable container"]
    fn feedback_reserves_handoff_space_and_returns_through_backpressure_and_replacement() {
        let (shared, mut workers) = test_workers();
        for from in 0..2 {
            let (tx, rx) = crate::spsc::channel();
            workers[from].outgoing[1 - from] = Some(tx);
            workers[1 - from].incoming[from] = Some(rx);
        }
        let (fixture, _) = receive_worker();
        let connection = fixture.sessions[0].connection.unwrap();
        drop(fixture);
        let command = Command {
            generation: 1,
            phase: Phase::Install,
            connection: Some(connection),
        };
        for worker in &mut workers {
            worker.command(0, command, 0).unwrap();
        }
        let mut lane = workers[0].sessions[0].free.pop().unwrap();
        lane.reset(7, 9022).unwrap();
        let mut bytes = [0; BASE_UDP_PAYLOAD];
        let length = receive_bytes(1, &[receive_piece(1, 0, &[0; 60])], &mut bytes);
        lane.admit(&Datagram::decode(&bytes[..length]).unwrap(), Duration::ZERO);
        workers[1].sessions[0].rx[3] = Some(lane);
        workers[1].sessions[0].rx_index[7] = 4;
        shared.owners[0][7].store(2, Ordering::Release);
        let rx = socket(&mut workers[0]);
        let now = RETRY;
        for _ in 0..crate::spsc::CAPACITY {
            assert!(
                workers[1]
                    .push(
                        0,
                        Event::Retire {
                            session: 0,
                            tag: 0,
                            lane: 7
                        }
                    )
                    .is_ok()
            );
        }
        workers[1].collect_acks(now).unwrap();
        assert_eq!(workers[1].feedback_free.len(), BUFFERS);
        assert_eq!(workers[1].sessions[0].ack_inflight, 0);
        assert_eq!(
            workers[1].sessions[0].rx[3]
                .as_ref()
                .unwrap()
                .ack_deadline(),
            Some(now)
        );
        assert_eq!(workers[1].feedback_retry, now + RETRY);
        while workers[0].incoming[1].as_mut().unwrap().try_pop().is_some() {}
        workers[1].collect_acks(now + RETRY).unwrap();
        assert_eq!(workers[1].feedback_free.len(), BUFFERS - 1);
        workers[0].drain_queues(now + RETRY).unwrap();
        for _ in 0..crate::spsc::CAPACITY {
            assert!(
                workers[0]
                    .push(
                        1,
                        Event::Retire {
                            session: 0,
                            tag: 0,
                            lane: 7
                        }
                    )
                    .is_ok()
            );
        }
        workers[0].flush_control(now + RETRY).unwrap();
        let length = rx.recv(&mut bytes).unwrap().0;
        assert!(matches!(
            Datagram::decode(&bytes[..length]).unwrap().records().next(),
            Some(Record::Ack {
                lane: 7,
                delay_us: 1000,
                ..
            })
        ));
        assert_eq!(workers[0].pending_control.len(), 1);
        assert_eq!(workers[1].sessions[0].ack_inflight, 1 << 3);
        let replacement = Connection {
            initiator: Tag::new(3).unwrap(),
            responder: Tag::new(4).unwrap(),
            ..connection
        };
        workers[1]
            .command(
                0,
                Command {
                    generation: 2,
                    connection: Some(replacement),
                    ..command
                },
                0,
            )
            .unwrap();
        workers[1].sessions[0].ack_inflight = 1 << 3;
        while workers[1].incoming[0].as_mut().unwrap().try_pop().is_some() {}
        workers[0].flush_control(now + 2 * RETRY).unwrap();
        workers[1].drain_queues(now + 2 * RETRY).unwrap();
        assert_eq!(workers[1].feedback_free.len(), BUFFERS);
        assert_eq!(workers[1].sessions[0].ack_inflight, 1 << 3);
        assert!(workers[0].pending_control.is_empty());
        assert_eq!(
            rx.recv(&mut bytes).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
    }
}
