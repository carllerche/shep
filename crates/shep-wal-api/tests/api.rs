use bytes::Bytes;
use shep_wal_api::*;

fn b(s: &str) -> Bytes {
    Bytes::copy_from_slice(s.as_bytes())
}

#[test]
fn point_ranges_hold_one_key() {
    let r = KeyRange::point(b("ab"));
    assert_eq!(r.as_point(), Some(&b("ab")));
    assert!(r.contains(b"ab"));
    assert!(!r.contains(b"a"));
    assert!(!r.contains(b"ab\0"));
    assert!(!r.contains(b"abc"));
    assert_eq!(KeyRange::new(b("a"), b("b")).as_point(), None);
}

#[test]
fn prefix_ranges() {
    let r = KeyRange::prefix(b("user/"));
    assert!(r.contains(b"user/"));
    assert!(r.contains(b"user/\xff\xff"));
    assert!(!r.contains(b"user0"));
    assert!(!r.contains(b"user"));
    // Trailing 0xff bytes carry.
    let r = KeyRange::prefix(Bytes::from_static(b"a\xff"));
    assert!(r.contains(b"a\xff\x00"));
    assert!(!r.contains(b"b"));
    assert_eq!(r.end(), Some(&b("b")));
    // Nothing follows a prefix of only 0xff bytes.
    let r = KeyRange::prefix(Bytes::from_static(b"\xff\xff"));
    assert_eq!(r.end(), None);
    assert!(r.contains(b"\xff\xff\xff\xff"));
}

#[test]
fn intersection() {
    let ab = KeyRange::new(b("a"), b("b"));
    let bc = KeyRange::new(b("b"), b("c"));
    assert!(!ab.intersects(&bc), "ends are exclusive");
    assert!(ab.intersects(&KeyRange::point(b("a"))));
    assert!(ab.intersects(&KeyRange::starting_at(b("aa"))));
    assert!(!ab.intersects(&KeyRange::starting_at(b("b"))));
    let empty = KeyRange::new(b("b"), b("a"));
    assert!(empty.is_empty());
    assert!(!empty.intersects(&KeyRange::starting_at(b(""))));
}

#[test]
fn proposals_round_trip() {
    let p = Proposal {
        idempotency_key: b("key"),
        kind: ProposalKind::Mutation(Mutation {
            read_version: Dvv::new()
                .with_prefix(PartitionId(1), 4)
                .with_dot(Dot::new(PartitionId(1), 9)),
            context: Dvv::new()
                .with_prefix(PartitionId(1), 4)
                .with_prefix(PartitionId(2), 7),
            read_set: vec![KeyRange::point(b("a")), KeyRange::starting_at(b("z"))],
            write_set: vec![KeyRange::prefix(b("p/"))],
            payload: b("delta"),
            reply: b("reply"),
        }),
    };
    let wire: proto::Proposal = (&p).into();
    assert_eq!(Proposal::try_from(wire).unwrap(), p);

    let f = Proposal {
        idempotency_key: b("k"),
        kind: ProposalKind::Failure { reply: b("no") },
    };
    assert_eq!(Proposal::try_from(proto::Proposal::from(&f)).unwrap(), f);
}

#[test]
fn responses_round_trip() {
    let results: Vec<Result<Decided, Rejection>> = vec![
        Ok(Decided {
            dot: Dot::new(PartitionId(3), 12),
            commit_time_us: 99,
            status: DecidedStatus::Failed,
            reply: b("r"),
        }),
        Err(Rejection::Contended),
        Err(Rejection::TooOld),
        Err(Rejection::NotLeader {
            leader: Some(Member {
                node_id: 2,
                addr: "h:1".into(),
            }),
        }),
        Err(Rejection::NotLeader { leader: None }),
        Err(Rejection::UnknownPartition),
        Err(Rejection::UnknownOutcome("m".into())),
        Err(Rejection::Invalid("bad".into())),
    ];
    for r in results {
        let wire: proto::ProposeResponse = (&r).into();
        assert_eq!(Result::<Decided, Rejection>::try_from(wire).unwrap(), r);
    }
}

#[test]
fn log_entries_round_trip() {
    let p = PartitionId(5);
    let entries = [
        EntryKind::Internal,
        EntryKind::Void,
        EntryKind::Failure {
            commit_time_us: 3,
            idempotency_key: b("k"),
        },
        EntryKind::Mutation(MutationEntry {
            context: Dvv::new().with_prefix(p, 2),
            commit_time_us: 4,
            idempotency_key: b("k2"),
            payload: b("delta"),
        }),
    ];
    for (i, kind) in entries.into_iter().enumerate() {
        let e = LogEntry {
            dot: Dot::new(p, i as u64),
            term: 7,
            kind,
        };
        assert_eq!(LogEntry::from_proto(p, e.to_proto()).unwrap(), e);
    }
}

#[test]
fn missing_fields_are_errors() {
    assert!(Proposal::try_from(proto::Proposal::default()).is_err());
    assert!(Result::<Decided, Rejection>::try_from(proto::ProposeResponse::default()).is_err());
    let mutation = proto::Proposal {
        idempotency_key: b("k"),
        kind: Some(proto::proposal::Kind::Mutation(proto::Mutation::default())),
    };
    assert!(Proposal::try_from(mutation).is_err());
}
