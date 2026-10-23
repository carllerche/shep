use std::collections::BTreeMap;

use shep_raft::{Admit, Entry, Machine};
use shep_wal_api::PartitionId;
use shep_wal_controller::*;

fn node(id: u64) -> NodeInfo {
    NodeInfo {
        node_id: id,
        addr: format!("10.0.0.{id}:7000"),
    }
}

fn rules() -> LogSetRules {
    LogSetRules::default()
}

fn founded(n: u64) -> ControllerState {
    let mut s = ControllerState::default();
    let members = (1..=n).map(node).collect();
    assert_eq!(
        s.apply(
            &rules(),
            1,
            Command::Found {
                cluster_id: b"c".to_vec(),
                members
            }
        ),
        Outcome::Ok
    );
    s
}

fn create(name: &str, partitions: Vec<Vec<u64>>) -> Command {
    Command::CreateLogSet {
        name: name.into(),
        partitions,
    }
}

#[test]
fn founding_records_the_members_once() {
    let mut s = founded(3);
    assert!(s.is_founded());
    assert_eq!(s.members.len(), 3);
    assert_eq!(s.members[&2].revision, 1);
    let again = s.apply(
        &rules(),
        2,
        Command::Found {
            cluster_id: b"d".to_vec(),
            members: vec![node(9)],
        },
    );
    assert!(matches!(again, Outcome::Rejected(_)));
    assert_eq!(s.cluster_id.as_deref(), Some(&b"c"[..]));
    assert_eq!(s.revision, 2, "every command moves the revision");
}

#[test]
fn nodes_join_once() {
    let mut s = founded(1);
    assert_eq!(s.apply(&rules(), 2, Command::AddNode(node(2))), Outcome::Ok);
    assert!(matches!(
        s.apply(&rules(), 3, Command::AddNode(node(2))),
        Outcome::Rejected(_)
    ));
    assert_eq!(s.addrs()[&2], "10.0.0.2:7000");
}

#[test]
fn log_sets_get_partitions_with_increasing_ids() {
    let mut s = founded(3);
    let partitions = s.plan(&rules(), 3, 3).unwrap();
    let out = s.apply(&rules(), 5, create("prod", partitions));
    let Outcome::LogSetCreated(prod) = out else {
        panic!("{out:?}")
    };
    let set = &s.log_sets[&prod];
    assert_eq!(
        set.partitions,
        vec![PartitionId(1), PartitionId(2), PartitionId(3)],
        "partition ids start after the controller's group"
    );
    assert_eq!(s.partitions[&PartitionId(1)].log_set, prod);
    assert_eq!(s.partitions[&PartitionId(2)].revision, 5);
    assert_eq!(s.log_set_by_name(&rules(), "prod").unwrap().0, prod);

    let partitions = s.plan(&rules(), 1, 1).unwrap();
    let Outcome::LogSetCreated(dev) = s.apply(&rules(), 6, create("dev", partitions)) else {
        panic!()
    };
    assert_ne!(dev, prod);
    assert_eq!(s.log_sets[&dev].partitions, vec![PartitionId(4)]);
    // Names are unique.
    let partitions = s.plan(&rules(), 1, 1).unwrap();
    assert!(matches!(
        s.apply(&rules(), 7, create("dev", partitions)),
        Outcome::Rejected(_)
    ));
}

#[test]
fn replicas_must_be_members() {
    let mut s = founded(2);
    let out = s.apply(&rules(), 2, create("u", vec![vec![1, 7]]));
    assert!(matches!(out, Outcome::Rejected(_)), "{out:?}");
    assert!(s.log_sets.is_empty() && s.partitions.is_empty());
    let mut unfounded = ControllerState::default();
    assert!(matches!(
        unfounded.apply(&rules(), 1, create("u", vec![vec![1]])),
        Outcome::Rejected(_)
    ));
}

/// Replicas spread so every member holds about as many.
#[test]
fn allocation_balances_replicas() {
    let mut s = founded(5);
    for i in 0..4 {
        let partitions = s.plan(&rules(), 5, 3).unwrap();
        s.apply(&rules(), 10 + i, create(&format!("u{i}"), partitions));
    }
    let mut load: BTreeMap<u64, usize> = BTreeMap::new();
    for p in s.partitions.values() {
        assert_eq!(p.replicas.len(), 3);
        let mut r = p.replicas.clone();
        r.dedup();
        assert_eq!(r.len(), 3, "replicas are distinct nodes");
        for n in &p.replicas {
            *load.entry(*n).or_default() += 1;
        }
    }
    let (min, max) = (load.values().min().unwrap(), load.values().max().unwrap());
    assert!(max - min <= 1, "{load:?}");
    assert!(
        s.allocate(6, &BTreeMap::new()).is_err(),
        "more replicas than members"
    );
    assert!(s.allocate(0, &BTreeMap::new()).is_err());
}

#[test]
fn assigned_to_lists_a_nodes_partitions() {
    let mut s = founded(3);
    s.apply(
        &rules(),
        2,
        create("u", vec![vec![1, 2], vec![2, 3], vec![1, 3]]),
    );
    let on2: Vec<_> = s.assigned_to(2).map(|(p, _)| p).collect();
    assert_eq!(on2, vec![PartitionId(1), PartitionId(2)]);
}

fn entry(index: u64, command: Command) -> Entry<Command> {
    Entry {
        index,
        term: 1,
        commit_time_us: index,
        key: None,
        command,
    }
}

fn admit() -> Admit {
    Admit {
        term: 1,
        seq: 0,
        commit_time_us: 0,
        next_index: 0,
        window_start_us: 0,
        window_entries: 0,
    }
}

/// As a machine: the leader admits a proposal by placing it, applying
/// reports revisions, and a snapshot restores the same state.
#[test]
fn the_machine_admits_applies_and_snapshots() {
    let mut sm = ControllerSm::new(rules());
    let rev = sm.subscribe();
    let found = sm
        .admit(
            &admit(),
            Proposal::Found {
                cluster_id: b"c".to_vec(),
                members: vec![node(1), node(2)],
            },
        )
        .unwrap();
    assert_eq!(sm.apply(&entry(1, found)), Outcome::Ok);

    let cmd = sm
        .admit(
            &admit(),
            Proposal::CreateLogSet {
                name: "u".into(),
                partitions: 2,
                replication: 2,
            },
        )
        .unwrap();
    assert_eq!(cmd, create("u", vec![vec![1, 2], vec![1, 2]]));
    assert!(matches!(
        sm.apply(&entry(2, cmd)),
        Outcome::LogSetCreated(_)
    ));
    assert_eq!(*rev.borrow(), 2);
    // Refused before anything is placed.
    for (partitions, replication) in [(0, 1), (1, 3), (1025, 1), (u32::MAX, 1)] {
        let r = sm.admit(
            &admit(),
            Proposal::CreateLogSet {
                name: "v".into(),
                partitions,
                replication,
            },
        );
        assert!(r.is_err(), "{partitions} × {replication}: {r:?}");
    }

    let mut copy = ControllerSm::new(rules());
    copy.restore(sm.snapshot());
    assert_eq!(copy.state(), sm.state());
}

/// A log set's size is bounded before anything is planned, and again when
/// the command applies, whoever admitted it.
#[test]
fn log_sets_are_bounded() {
    let rules = LogSetRules {
        max_partitions: 8,
        names: NameRule::Any,
    };
    let mut s = founded(3);
    assert_eq!(s.plan(&rules, 8, 3).unwrap().len(), 8);
    assert!(s.plan(&rules, 9, 1).is_err());
    assert!(s.plan(&rules, 0, 1).is_err());
    // Rejected at once, not after allocating billions.
    assert!(s.plan(&rules, u32::MAX as usize, 1).is_err());
    assert!(s.plan(&rules, 1, 4).is_err());

    let rejected = [
        create("u", vec![vec![1]; 9]),
        create("u", vec![]),
        create("", vec![vec![1]]),
        create(&"x".repeat(MAX_NAME + 1), vec![vec![1]]),
        create("u", vec![vec![]]),
        create("u", vec![vec![1, 1]]),
        create("u", vec![vec![1], vec![2, 3, 2]]),
    ];
    for (i, cmd) in rejected.into_iter().enumerate() {
        let out = s.apply(&rules, 2 + i as u64, cmd);
        assert!(matches!(out, Outcome::Rejected(_)), "{i}: {out:?}");
    }
    assert!(s.log_sets.is_empty() && s.partitions.is_empty());
}

/// Under [`NameRule::Domain`], names are DNS domains, compared without
/// case (Shep's universes, §14).
#[test]
fn domain_names() {
    let rule = NameRule::Domain;
    let label = "a".repeat(MAX_DOMAIN_LABEL);
    // Four labels of 63 and their dots make 255; trim to the limit.
    let longest = format!("{label}.{label}.{label}.{}", "a".repeat(61));
    assert_eq!(longest.len(), MAX_DOMAIN_NAME);
    for ok in ["prod", "a", "my-app.prod", "x1.y-2.z", &label, &longest] {
        assert_eq!(rule.check(ok), Ok(ok.to_string()), "{ok:?}");
    }
    assert_eq!(rule.check("My-App.Prod"), Ok("my-app.prod".into()));
    let too_long_label = "a".repeat(MAX_DOMAIN_LABEL + 1);
    let too_long = format!("{longest}a");
    for bad in [
        "",
        ".",
        "prod.",
        ".prod",
        "a..b",
        "-prod",
        "prod-",
        "a.-b",
        "a.b-",
        "pro_d",
        "pro d",
        "prød",
        &too_long_label,
        &too_long,
    ] {
        assert!(rule.check(bad).is_err(), "{bad:?}");
    }
    // Any name goes under the default rule, case and all.
    assert_eq!(NameRule::Any.check("Pro_d"), Ok("Pro_d".into()));

    // Applying normalizes, and duplicates differ only in case.
    let rules = LogSetRules {
        names: NameRule::Domain,
        ..LogSetRules::default()
    };
    let mut s = founded(1);
    let one = || vec![vec![1]];
    let Outcome::LogSetCreated(id) = s.apply(&rules, 2, create("Prod.Example", one())) else {
        panic!()
    };
    assert_eq!(s.log_sets[&id].name, "prod.example");
    assert_eq!(s.log_set_by_name(&rules, "PROD.example").unwrap().0, id);
    assert!(matches!(
        s.apply(&rules, 3, create("prod.EXAMPLE", one())),
        Outcome::Rejected(_)
    ));
    assert!(matches!(
        s.apply(&rules, 4, create("bad_name", one())),
        Outcome::Rejected(_)
    ));
    assert_eq!(s.log_sets.len(), 1);
}
