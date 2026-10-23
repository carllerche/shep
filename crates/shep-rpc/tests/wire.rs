//! The frame layout and checksums, pinned byte for byte.

use shep_rpc::wire::{self, COMPRESSION_NONE, HEADER_SIZE, Header};
use shep_rpc::{Status, method_id, service_of};

fn sample() -> Header {
    Header {
        version: 0x01,
        compression: 0x02,
        metadata_size: 0x0403,
        header_checksum: 0x0807_0605,
        payload_size: 0x0c0b_0a09,
        method_or_status: 0x100f_0e0d,
        correlation_id: 0x1413_1211,
        flags: 0x1817_1615,
        payload_checksum: 0x201f_1e1d_1c1b_1a19,
    }
}

#[test]
fn header_layout() {
    // 32 bytes, little-endian, every field at an offset aligned to its size.
    assert_eq!(HEADER_SIZE, 32);
    let bytes = sample().encode();
    let expected: Vec<u8> = (1..=32).collect();
    assert_eq!(bytes[..], expected[..]);
    assert_eq!(Header::decode(&bytes), sample());
}

#[test]
fn new_header() {
    let metadata = [1, 0, 1, 0, 7];
    let header = Header::new(
        3,
        method_id(7, 1),
        42,
        &metadata,
        5,
        wire::payload_checksum(b"hello"),
    );
    assert_eq!(header.version, 3);
    assert_eq!(header.compression, COMPRESSION_NONE);
    assert_eq!(header.metadata_size, 5);
    assert_eq!(header.payload_size, 5);
    assert_eq!(header.method_or_status, 0x0007_0001);
    assert_eq!(header.correlation_id, 42);
    assert_eq!(header.flags, 0);
    assert_eq!(header.payload_checksum, 0x26c7_827d_889f_6da3);
    assert!(header.checksum_ok(&metadata));
}

#[test]
fn payload_checksum_is_xxh64_seed_0() {
    // Reference vectors from the xxHash spec.
    assert_eq!(wire::payload_checksum(b""), 0xef46_db37_51d8_e999);
    assert_eq!(wire::payload_checksum(b"abc"), 0x44bc_2cf5_ad77_0999);
}

#[test]
fn header_checksum_is_crc32c_over_the_header_and_metadata() {
    let header = sample();
    let metadata = b"\x01\x00\x00\x00";
    let mut covered = header.encode().to_vec();
    covered[4..8].fill(0);
    covered.extend_from_slice(metadata);
    assert_eq!(header.compute_checksum(metadata), crc32c::crc32c(&covered));
    // Sanity check the crc32c (Castagnoli) reference vector.
    assert_eq!(crc32c::crc32c(b"123456789"), 0xe306_9283);
}

#[test]
fn header_checksum_covers_every_byte_but_itself() {
    let metadata = [1, 0, 2, 0, 7, 8];
    let base = Header::new(
        0,
        method_id(1, 2),
        3,
        &metadata,
        4,
        wire::payload_checksum(b"body"),
    );
    let bytes = base.encode();
    for i in (0..HEADER_SIZE).filter(|i| !(4..8).contains(i)) {
        let mut flipped = bytes;
        flipped[i] ^= 1;
        assert!(!Header::decode(&flipped).checksum_ok(&metadata), "byte {i}");
    }
    for i in 0..metadata.len() {
        let mut flipped = metadata;
        flipped[i] ^= 1;
        assert!(!base.checksum_ok(&flipped), "metadata byte {i}");
    }
    assert!(!base.checksum_ok(&metadata[..5]));
    let mut h = base;
    h.header_checksum ^= 1;
    assert!(!h.checksum_ok(&metadata));
    assert!(base.checksum_ok(&metadata));
}

/// Adapted from Redpanda src/v/rpc/test/netbuf_tests.cc (netbuf_pod): a frame
/// built for a body parses back to the same body.
#[test]
fn frame_round_trip() {
    let body = b"\x58\x00\x58\x00\x00\x00\x58\x00\x00\x00\x00\x00\x00\x00";
    let metadata = [2, 0, 1, 0, 9];
    let header = Header::new(
        0,
        method_id(0, 66),
        42,
        &metadata,
        body.len(),
        wire::payload_checksum(body),
    );
    let mut frame = header.encode().to_vec();
    frame.extend_from_slice(&metadata);
    frame.extend_from_slice(body);

    let parsed = Header::decode(frame[..HEADER_SIZE].try_into().unwrap());
    assert_eq!(parsed, header);
    let meta_end = HEADER_SIZE + parsed.metadata_size as usize;
    assert!(parsed.checksum_ok(&frame[HEADER_SIZE..meta_end]));
    let parsed_body = &frame[meta_end..];
    assert_eq!(parsed_body.len(), parsed.payload_size as usize);
    assert_eq!(wire::payload_checksum(parsed_body), parsed.payload_checksum);
    assert_eq!(parsed_body, body);
}

#[test]
fn method_ids() {
    assert_eq!(method_id(0, 0), 0);
    assert_eq!(method_id(1, 0), 0x0001_0000);
    assert_eq!(method_id(0, 1), 1);
    assert_eq!(method_id(0xabcd, 0x1234), 0xabcd_1234);
    assert_eq!(method_id(u16::MAX, u16::MAX), u32::MAX);
    assert_eq!(service_of(method_id(0xabcd, 0x1234)), 0xabcd);
}

#[test]
fn status_codes() {
    let all = [
        (Status::Success, 200),
        (Status::Abandoned, 408),
        (Status::PayloadTooLarge, 413),
        (Status::UnsupportedCompression, 415),
        (Status::ServerError, 500),
    ];
    for (status, code) in all {
        assert_eq!(status.as_u32(), code);
        assert_eq!(Status::from_u32(code), Some(status));
        assert!(code < shep_rpc::SERVICE_STATUS_MIN);
    }
    assert_eq!(Status::from_u32(0), None);
    assert_eq!(Status::from_u32(201), None);
    assert_eq!(Status::from_u32(400), None);
    assert_eq!(Status::from_u32(shep_rpc::SERVICE_STATUS_MIN), None);
}
