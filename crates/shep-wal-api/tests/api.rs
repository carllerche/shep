use shep_wal_api::*;

fn b(s: &str) -> Vec<u8> {
    s.as_bytes().to_vec()
}

#[test]
fn point_ranges_hold_one_key() {
    let r = KeyRange::point(b("ab"));
    assert_eq!(r.as_point(), Some(&b"ab"[..]));
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
    let r = KeyRange::prefix(b"a\xff".to_vec());
    assert!(r.contains(b"a\xff\x00"));
    assert!(!r.contains(b"b"));
    assert_eq!(r.end(), Some(&b"b"[..]));
    // Nothing follows a prefix of only 0xff bytes.
    let r = KeyRange::prefix(b"\xff\xff".to_vec());
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

fn round_trip<T: shep_codec::Encode + shep_codec::Decode + PartialEq + std::fmt::Debug>(v: &T) {
    assert_eq!(
        &shep_codec::from_bytes::<T>(&shep_codec::to_bytes(v)).unwrap(),
        v
    );
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
    round_trip(&ProposeRequest {
        partition: PartitionId(3),
        proposal: p,
    });
    round_trip(&Proposal {
        idempotency_key: b("k"),
        kind: ProposalKind::Failure { reply: b("no") },
    });
}

#[test]
fn replies_round_trip() {
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
        let reply = ProposeReply::from(r.clone());
        round_trip(&reply);
        assert_eq!(Result::<Decided, Rejection>::from(reply), r);
    }
}

#[test]
fn log_entries_round_trip() {
    let p = PartitionId(5);
    let kinds = [
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
    let entries: Vec<_> = kinds
        .into_iter()
        .enumerate()
        .map(|(i, kind)| LogEntry {
            dot: Dot::new(p, i as u64),
            term: 7,
            kind,
        })
        .collect();
    round_trip(&FetchReply::Entries {
        entries,
        applied_index: Some(3),
    });
    round_trip(&FetchReply::OutOfRange { start: 10 });
    round_trip(&FetchRequest {
        partition: p,
        from_index: 4,
        max_bytes: 1 << 20,
        max_wait_ms: 50,
    });
}

#[test]
fn admin_and_status_round_trip() {
    round_trip(&StatusReply::Status(PartitionStatus {
        node_id: 1,
        term: 2,
        leader: Some(Member {
            node_id: 1,
            addr: "a:1".into(),
        }),
        last_log_index: 9,
        applied_index: None,
        voters: vec![1, 2, 3],
    }));
    round_trip(&InitializePartitionRequest {
        partition: PartitionId(1),
        members: vec![Member {
            node_id: 1,
            addr: "a:1".into(),
        }],
    });
    round_trip(&AdminReply::Failed("x".into()));
}

#[test]
fn method_ids_belong_to_the_wal_service() {
    for m in [
        method::PROPOSE,
        method::FETCH,
        method::STATUS,
        method::CREATE_PARTITION,
        method::INITIALIZE_PARTITION,
    ] {
        assert!(method::is_wal(m));
    }
    assert!(!method::is_wal(shep_rpc::method_id(1, 1)));
}
