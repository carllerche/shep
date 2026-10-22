use std::collections::BTreeSet;

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use shep_version::{Dot, Dvv, PartitionId};

const P1: PartitionId = PartitionId(1);
const P2: PartitionId = PartitionId(2);

fn dot(p: PartitionId, i: u64) -> Dot {
    Dot::new(p, i)
}

#[test]
fn empty_version_holds_only_index_zero() {
    let v = Dvv::new();
    assert!(v.is_empty());
    assert!(v.contains(dot(P1, 0)));
    assert!(!v.contains(dot(P1, 1)));
    assert_eq!(v.prefix(P1), 0);
}

#[test]
fn prefix_membership() {
    let v = Dvv::new().with_prefix(P1, 5);
    assert!((0..=5).all(|i| v.contains(dot(P1, i))));
    assert!(!v.contains(dot(P1, 6)));
    assert!(!v.contains(dot(P2, 1)));
}

#[test]
fn extras_are_exact() {
    let v = Dvv::new().with_prefix(P1, 3).with_dot(dot(P1, 7));
    assert!(v.contains(dot(P1, 7)));
    assert!(!v.contains(dot(P1, 4)));
    assert!(!v.contains(dot(P1, 6)));
    assert!(!v.contains(dot(P1, 8)));
    assert_eq!(v.extras(P1).collect::<Vec<_>>(), vec![7]);
}

#[test]
fn extras_fold_into_the_prefix() {
    let mut v = Dvv::new()
        .with_prefix(P1, 3)
        .with_dot(dot(P1, 5))
        .with_dot(dot(P1, 6));
    assert_eq!(v.prefix(P1), 3);
    v.insert(dot(P1, 4));
    assert_eq!(v.prefix(P1), 6);
    assert_eq!(v.extras(P1).count(), 0);
}

#[test]
fn raising_the_prefix_drops_covered_extras() {
    let mut v = Dvv::new().with_dot(dot(P1, 4)).with_dot(dot(P1, 9));
    v.raise_prefix(P1, 5);
    assert_eq!(v.prefix(P1), 5);
    assert_eq!(v.extras(P1).collect::<Vec<_>>(), vec![9]);
    // Lowering is a no-op.
    v.raise_prefix(P1, 2);
    assert_eq!(v.prefix(P1), 5);
}

#[test]
fn canonical_equality() {
    let a = Dvv::new().with_dot(dot(P1, 1)).with_dot(dot(P1, 2));
    let b = Dvv::new().with_prefix(P1, 2);
    assert_eq!(a, b);
    // Index 0 never makes a partition appear.
    assert_eq!(
        Dvv::new().with_dot(dot(P2, 0)).with_prefix(P2, 0),
        Dvv::new()
    );
}

#[test]
fn merge_is_union_and_inclusion_is_subset() {
    let a = Dvv::new().with_prefix(P1, 4).with_dot(dot(P2, 3));
    let b = Dvv::new().with_prefix(P2, 2).with_dot(dot(P1, 8));
    let mut m = a.clone();
    m.merge(&b);
    assert_eq!(m.prefix(P1), 4);
    assert_eq!(m.extras(P1).collect::<Vec<_>>(), vec![8]);
    assert_eq!(m.prefix(P2), 3);
    assert!(m.includes(&a));
    assert!(m.includes(&b));
    assert!(!a.includes(&m));
    assert!(m.includes(&Dvv::new()));
    // A prefix is included by extras covering it.
    let extras = Dvv::new()
        .with_prefix(P1, 2)
        .with_dot(dot(P1, 4))
        .with_dot(dot(P1, 5));
    assert!(!extras.includes(&Dvv::new().with_prefix(P1, 3)));
    assert!(extras.includes(&Dvv::new().with_prefix(P1, 2).with_dot(dot(P1, 5))));
}

#[test]
fn display() {
    let v = Dvv::new()
        .with_prefix(P1, 1042)
        .with_prefix(P2, 877)
        .with_dot(dot(P1, 1045));
    assert_eq!(v.to_string(), "{p1 ≤ 1042 + 1045, p2 ≤ 877}");
    assert_eq!(dot(P1, 1045).to_string(), "p1:1045");
}

#[test]
fn the_largest_index() {
    let max = u64::MAX;
    let full = Dvv::new().with_prefix(P1, max);
    assert!(full.contains(dot(P1, max)));
    assert_eq!(full.prefix(P1), max);
    // Extras the full prefix covers are dropped.
    let mut v = full.clone().with_dot(dot(P1, 7));
    v.insert(dot(P1, max));
    assert_eq!(v, full);
    // A run of extras folds up to the largest index.
    let run = Dvv::new()
        .with_prefix(P1, max - 3)
        .with_dot(dot(P1, max))
        .with_dot(dot(P1, max - 1))
        .with_dot(dot(P1, max - 2));
    assert_eq!(run, full);
    let mut m = Dvv::new().with_dot(dot(P1, max));
    m.merge(&full);
    assert_eq!(m, full);
    assert!(full.includes(&m));
    assert!(!Dvv::new().with_dot(dot(P1, max)).includes(&full));
}

/// Every operation agrees with a plain set of dots.
#[test]
fn matches_a_set_model() {
    let mut rng = StdRng::seed_from_u64(7);
    let parts = [P1, P2, PartitionId(3)];
    let random = |rng: &mut StdRng| {
        let mut v = Dvv::new();
        let mut model = BTreeSet::new();
        for _ in 0..rng.random_range(0..12) {
            let p = parts[rng.random_range(0..parts.len())];
            if rng.random_bool(0.3) {
                let n = rng.random_range(0..10);
                v.raise_prefix(p, n);
                model.extend((1..=n).map(|i| dot(p, i)));
            } else {
                let i = rng.random_range(1..14);
                v.insert(dot(p, i));
                model.insert(dot(p, i));
            }
        }
        (v, model)
    };
    let all: Vec<Dot> = parts
        .iter()
        .flat_map(|&p| (1..16).map(move |i| dot(p, i)))
        .collect();
    for _ in 0..2000 {
        let (a, ma) = random(&mut rng);
        let (b, mb) = random(&mut rng);
        for &d in &all {
            assert_eq!(a.contains(d), ma.contains(&d), "{a} contains {d}");
        }
        let mut m = a.clone();
        m.merge(&b);
        let mm: BTreeSet<Dot> = ma.union(&mb).copied().collect();
        for &d in &all {
            assert_eq!(m.contains(d), mm.contains(&d), "{m} contains {d}");
        }
        assert_eq!(a.includes(&b), mb.is_subset(&ma), "{a} includes {b}");
        assert_eq!(a == b, ma == mb, "{a} == {b}");
    }
}

#[test]
fn versions_round_trip_through_the_codec() {
    let v = Dvv::new()
        .with_prefix(P1, 7)
        .with_dot(dot(P1, 9))
        .with_prefix(P2, 3);
    let bytes = shep_codec::to_bytes(&v);
    assert_eq!(shep_codec::from_bytes::<Dvv>(bytes).unwrap(), v);
    assert_eq!(
        shep_codec::from_bytes::<Dvv>(shep_codec::to_bytes(&Dvv::new())).unwrap(),
        Dvv::new()
    );
    let d = dot(P2, 4);
    assert_eq!(
        shep_codec::from_bytes::<Dot>(shep_codec::to_bytes(&d)).unwrap(),
        d
    );
}
