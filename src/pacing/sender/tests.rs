use super::*;

fn planned(lane: u16, number: u64, bytes: u16) -> Planned {
    Planned {
        id: PacketId {
            session: 2,
            generation: 7,
            lane,
            number,
        },
        bytes,
    }
}

fn claim(bytes: u64) -> Claim {
    Claim {
        group: 0,
        prior: 0,
        bytes,
        at: Duration::from_micros(5),
        state: Published::default(),
    }
}

#[test]
fn prepare_takes_one_handle_per_packet_and_rejects_bad_plans() {
    let mut records = Records::new(3).unwrap();
    let mut flight = Flight::default();
    for bad in [
        &[][..],
        &[planned(1, 2, 1280), planned(1, 2, 1280)],
        &[planned(1, 1, 99)],
        &[planned(1, 1, 1280), planned(2, 2, 1280)],
    ] {
        assert_eq!(flight.prepare(bad, &mut records), Err(FlightError::Invalid));
    }
    assert_eq!(records.available(), 3);
    let plan = [1, 2, 3, 4].map(|n| planned(1, n, 1280));
    assert_eq!(
        flight.prepare(&plan, &mut records),
        Ok(3),
        "short of handles"
    );
    assert_eq!(records.available(), 0);
    assert_eq!(flight.packets().len(), 3);
    assert_eq!(
        flight.prepare(&plan, &mut records),
        Err(FlightError::Blocked),
        "live handles"
    );
    flight.release(&mut records);
    assert_eq!(records.available(), 3);
    assert_eq!(flight.packets().len(), 0);
}

#[test]
fn truncation_returns_suffix_handles_and_claims_fix_the_plan() {
    let mut records = Records::new(8).unwrap();
    let mut flight = Flight::default();
    let plan = [1, 2, 3].map(|n| planned(1, n, 1000));
    flight.prepare(&plan, &mut records).unwrap();
    assert_eq!(
        flight.truncate(999, &mut records),
        Err(FlightError::Blocked)
    );
    assert_eq!(flight.truncate(2500, &mut records), Ok(2000));
    assert_eq!(records.available(), 6);
    assert_eq!(flight.set_claim(claim(3000)), Err(FlightError::Invalid));
    flight.set_claim(claim(2000)).unwrap();
    assert_eq!(
        flight.truncate(1000, &mut records),
        Err(FlightError::Blocked)
    );
    assert_eq!(flight.set_claim(claim(2000)), Err(FlightError::Invalid));
}

#[test]
fn completion_keeps_the_sent_prefix_handles_for_history() {
    let mut records = Records::new(4).unwrap();
    let mut flight = Flight::default();
    let plan = [1, 2, 3].map(|n| planned(1, n, 1000));
    flight.prepare(&plan, &mut records).unwrap();
    let handles = [0, 1, 2].map(|ordinal| flight.handle(ordinal).unwrap());
    assert_eq!(
        flight.complete(1, Some(Duration::from_micros(5)), false),
        Err(FlightError::Invalid),
        "no claim"
    );
    flight.set_claim(claim(3000)).unwrap();
    for (count, started) in [
        (4, Some(Duration::from_micros(5))),
        (1, None),
        (0, Some(Duration::from_micros(5))),
        (1, Some(Duration::from_micros(4))),
    ] {
        assert_eq!(
            flight.complete(count, started, false),
            Err(FlightError::Invalid)
        );
    }
    flight.reidentify(10).unwrap();
    assert_eq!(flight.packets().map(|p| p.id.number).max(), Some(12));
    assert_eq!(flight.reidentify(12), Err(FlightError::Invalid));
    flight
        .complete(2, Some(Duration::from_micros(6)), true)
        .unwrap();
    assert_eq!(
        flight.completion(),
        Some((2, Some(Duration::from_micros(6)), true))
    );
    assert_eq!(flight.packets().len(), 2);
    flight.release(&mut records);
    // The sent prefix stays live for history; only the unsent packet's handle returns.
    assert_eq!(records.available(), 2);
    assert!(records.get(handles[0]).is_some() && records.get(handles[1]).is_some());
    assert!(records.get(handles[2]).is_none());
    assert!(flight.claim().is_none() && flight.completion().is_none());
}
