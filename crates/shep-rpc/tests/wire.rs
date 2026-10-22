//! The frame layout and checksums, pinned byte for byte.

use shep_rpc::wire::{self, COMPRESSION_NONE, HEADER_SIZE, Header, VERSION};
use shep_rpc::{Status, method_id};

fn sample() -> Header {
    Header {
        version: 0x01,
        header_checksum: 0x0403_0201,
        compression: 0x05,
        payload_size: 0x0908_0706,
        meta: 0x0d0c_0b0a,
        correlation_id: 0x1110_0f0e,
        payload_checksum: 0x1918_1716_1514_1312,
    }
}

#[test]
fn header_layout() {
    // Redpanda's rpc::header: 26 bytes, packed, little-endian, in field order.
    assert_eq!(HEADER_SIZE, 26);
    let bytes = sample().encode();
    let expected: [u8; 26] = [
        0x01, // version
        0x01, 0x02, 0x03, 0x04, // header_checksum
        0x05, // compression
        0x06, 0x07, 0x08, 0x09, // payload_size
        0x0a, 0x0b, 0x0c, 0x0d, // meta
        0x0e, 0x0f, 0x10, 0x11, // correlation_id
        0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, // payload_checksum
    ];
    assert_eq!(bytes, expected);
    assert_eq!(Header::decode(&bytes), sample());
}

#[test]
fn golden_request_frame() {
    let header = Header::new(method_id(7, 1), 42, b"hello");
    assert_eq!(header.version, VERSION);
    assert_eq!(header.compression, COMPRESSION_NONE);
    assert_eq!(header.payload_size, 5);
    assert_eq!(header.meta, 0x0007_0001);
    assert_eq!(header.correlation_id, 42);
    assert_eq!(header.payload_checksum, 0x26c7_827d_889f_6da3);
    assert!(header.checksum_ok());
    assert_eq!(
        header.encode(),
        [
            0x01, // version
            0x80, 0x4d, 0xcf, 0xcf, // header_checksum
            0x00, // compression
            0x05, 0x00, 0x00, 0x00, // payload_size
            0x01, 0x00, 0x07, 0x00, // meta
            0x2a, 0x00, 0x00, 0x00, // correlation_id
            0xa3, 0x6d, 0x9f, 0x88, 0x7d, 0x82, 0xc7, 0x26, // payload_checksum
        ]
    );
}

#[test]
fn payload_checksum_is_xxh64_seed_0() {
    // Reference vectors from the xxHash spec.
    assert_eq!(wire::payload_checksum(b""), 0xef46_db37_51d8_e999);
    assert_eq!(wire::payload_checksum(b"abc"), 0x44bc_2cf5_ad77_0999);
}

#[test]
fn header_checksum_is_crc32c_over_version_and_tail() {
    let header = sample();
    let bytes = header.encode();
    let mut covered = vec![bytes[0]];
    covered.extend_from_slice(&bytes[5..]);
    assert_eq!(header.compute_checksum(), crc32c::crc32c(&covered));
    // Sanity check the crc32c (Castagnoli) reference vector.
    assert_eq!(crc32c::crc32c(b"123456789"), 0xe306_9283);
}

#[test]
fn header_checksum_covers_every_field_but_itself() {
    let base = Header::new(method_id(1, 2), 3, b"body");
    let tweaks: [fn(&mut Header); 6] = [
        // Covered in Shep; Redpanda leaves the version out.
        |h| h.version ^= 1,
        |h| h.compression ^= 1,
        |h| h.payload_size ^= 1,
        |h| h.meta ^= 1,
        |h| h.correlation_id ^= 1,
        |h| h.payload_checksum ^= 1,
    ];
    for tweak in tweaks {
        let mut h = base;
        tweak(&mut h);
        assert!(!h.checksum_ok(), "{h:?}");
    }
    let mut h = base;
    h.header_checksum ^= 1;
    assert!(!h.checksum_ok());
    assert_eq!(base.compute_checksum(), base.header_checksum);
}

/// Adapted from Redpanda src/v/rpc/test/netbuf_tests.cc (netbuf_pod): a frame
/// built for a body parses back to the same body.
#[test]
fn frame_round_trip() {
    let body = b"\x58\x00\x58\x00\x00\x00\x58\x00\x00\x00\x00\x00\x00\x00";
    let header = Header::new(method_id(0, 66), 42, body);
    let mut frame = header.encode().to_vec();
    frame.extend_from_slice(body);

    let parsed = Header::decode(frame[..HEADER_SIZE].try_into().unwrap());
    assert_eq!(parsed, header);
    assert!(parsed.checksum_ok());
    let parsed_body = &frame[HEADER_SIZE..];
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
}

#[test]
fn status_codes() {
    let all = [
        (Status::Success, 200),
        (Status::MethodNotFound, 404),
        (Status::RequestTimeout, 408),
        (Status::ServerError, 500),
        (Status::ServiceUnavailable, 503),
        (Status::VersionNotSupported, 505),
    ];
    for (status, code) in all {
        assert_eq!(status.as_u32(), code);
        assert_eq!(Status::from_u32(code), Some(status));
    }
    assert_eq!(Status::from_u32(0), None);
    assert_eq!(Status::from_u32(201), None);
}
