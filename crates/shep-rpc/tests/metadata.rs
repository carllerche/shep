//! The metadata section against a model: a list of `(key, value)` read the
//! obvious way.
//!
//! For arbitrary bytes, and for well-formed sections with a few mutations,
//! `Metadata::parse` accepts exactly the sections the model reads as
//! well-formed, and then agrees with it on every entry and every lookup.
//! Whatever the builder makes, parse accepts and reads back.

use std::collections::BTreeMap;

use bytes::Bytes;
use hegel::TestCase;
use hegel::generators as gs;
use shep_rpc::{MAX_METADATA, Metadata, MetadataBuilder, MetadataError};

/// The entries of `bytes` if it is a well-formed section.
fn model(bytes: &[u8]) -> Option<Vec<(u16, Vec<u8>)>> {
    if bytes.len() > MAX_METADATA {
        return None;
    }
    let mut entries: Vec<(u16, Vec<u8>)> = Vec::new();
    let mut rest = bytes;
    while !rest.is_empty() {
        if rest.len() < 4 {
            return None;
        }
        let key = u16::from_le_bytes([rest[0], rest[1]]);
        let len = u16::from_le_bytes([rest[2], rest[3]]) as usize;
        let value = rest.get(4..4 + len)?;
        if entries.last().is_some_and(|&(last, _)| key <= last) {
            return None;
        }
        entries.push((key, value.to_vec()));
        rest = &rest[4 + len..];
    }
    Some(entries)
}

fn check(bytes: &[u8], probes: &[u16]) {
    let parsed = Metadata::parse(Bytes::copy_from_slice(bytes));
    match (model(bytes), parsed) {
        (None, Err(_)) => {}
        (Some(entries), Ok(metadata)) => {
            let got: Vec<(u16, Vec<u8>)> = metadata.iter().map(|(k, v)| (k, v.to_vec())).collect();
            assert_eq!(got, entries);
            assert_eq!(metadata.as_bytes(), bytes);
            for &key in probes.iter().chain(entries.iter().map(|(k, _)| k)) {
                let want = entries.iter().find(|(k, _)| *k == key).map(|(_, v)| &v[..]);
                assert_eq!(metadata.get(key), want, "key {key}");
            }
        }
        (want, got) => panic!("model {want:?}, parse {got:?}"),
    }
}

#[hegel::test(test_cases = 500)]
fn arbitrary_bytes(tc: TestCase) {
    let bytes = tc.draw(gs::binary().max_size(48));
    let probes = tc.draw(gs::vecs(gs::integers::<u16>().max_value(8)).max_size(4));
    check(&bytes, &probes);
}

/// Well-formed sections built by hand, then mutated: lengths that overrun,
/// keys out of order, truncation.
#[hegel::test(test_cases = 500)]
fn mutated_sections(tc: TestCase) {
    let count = tc.draw(gs::integers::<usize>().max_value(5));
    let mut bytes = Vec::new();
    let mut key = 0u16;
    for _ in 0..count {
        let step = tc.draw(gs::integers::<u16>().min_value(1).max_value(3));
        key = key.saturating_add(step);
        let value = tc.draw(gs::binary().max_size(6));
        bytes.extend(key.to_le_bytes());
        bytes.extend((value.len() as u16).to_le_bytes());
        bytes.extend(value);
    }
    let mutations = tc.draw(gs::integers::<usize>().max_value(2));
    for _ in 0..mutations {
        if bytes.is_empty() {
            break;
        }
        let at = tc.draw(gs::integers::<usize>().max_value(bytes.len() - 1));
        let kind = tc.draw(gs::integers::<u8>().max_value(2));
        match kind {
            0 => {
                let byte = tc.draw(gs::sampled_from(vec![0u8, 1, 2, 0xff]));
                bytes[at] = byte;
            }
            1 => bytes.truncate(at),
            _ => {
                let byte = tc.draw(gs::integers::<u8>());
                bytes.insert(at, byte);
            }
        }
    }
    let probes = tc.draw(gs::vecs(gs::integers::<u16>().max_value(16)).max_size(4));
    check(&bytes, &probes);
}

/// The builder sorts and deduplicates, and parse reads back what it built.
#[hegel::test(test_cases = 300)]
fn builder_round_trips(tc: TestCase) {
    let count = tc.draw(gs::integers::<usize>().max_value(8));
    let mut builder = MetadataBuilder::new();
    let mut want = BTreeMap::new();
    for _ in 0..count {
        let key = tc.draw(gs::integers::<u16>().max_value(20));
        let value = tc.draw(gs::binary().max_size(10));
        builder = builder.insert(key, value.clone());
        want.insert(key, value);
    }
    let metadata = builder.build().unwrap();
    let parsed = Metadata::parse(metadata.as_bytes().clone()).unwrap();
    assert_eq!(parsed, metadata);
    let got: Vec<(u16, Vec<u8>)> = parsed.iter().map(|(k, v)| (k, v.to_vec())).collect();
    let want: Vec<(u16, Vec<u8>)> = want.into_iter().collect();
    assert_eq!(got, want);
}

#[test]
fn limits() {
    // The largest section that fits, and one byte over.
    let fits = MetadataBuilder::new()
        .insert(1, vec![0; MAX_METADATA - 4])
        .build()
        .unwrap();
    assert_eq!(fits.as_bytes().len(), MAX_METADATA);
    assert_eq!(
        MetadataBuilder::new()
            .insert(1, vec![0; MAX_METADATA - 3])
            .build(),
        Err(MetadataError::TooLarge(MAX_METADATA + 1))
    );
    assert_eq!(
        MetadataBuilder::new().insert(1, vec![0; 70_000]).build(),
        Err(MetadataError::ValueTooLarge(70_000))
    );
    let over = Bytes::from(vec![0; MAX_METADATA + 4]);
    assert_eq!(
        Metadata::parse(over),
        Err(MetadataError::TooLarge(MAX_METADATA + 4))
    );
    assert!(Metadata::parse(Bytes::new()).unwrap().is_empty());
}
