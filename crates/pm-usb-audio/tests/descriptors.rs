//! Descriptor parsing against a recorded FiiO KA17 (tests/fixtures/fiio-ka17.bin, rebuilt from a
//! public FreeBSD `usbconfig -lv` dump with tools/usbconfig-to-raw.py: every configuration adds up to
//! its wTotalLength), a UAC1 dongle built here, and hostile input.

use pm_usb_audio::*;

const KA17: &[u8] = include_bytes!("fixtures/fiio-ka17.bin");

/// A RANGE reply for the KA17's clock, constructed from FiiO's published rates (44.1–768 kHz,
/// both families) until one is recorded from the device: one sub-range per rate.
fn ka17_range_reply() -> Vec<u8> {
    let rates = [
        44_100u32, 48_000, 88_200, 96_000, 176_400, 192_000, 352_800, 384_000, 705_600, 768_000,
    ];
    let mut b = (rates.len() as u16).to_le_bytes().to_vec();
    for r in rates {
        b.extend(r.to_le_bytes());
        b.extend(r.to_le_bytes());
        b.extend(0u32.to_le_bytes());
    }
    b
}

#[test]
fn the_fiio_ka17() {
    let d = parse(KA17);
    assert_eq!((d.vendor_id, d.product_id), (0x2972, 0x0093));
    assert_eq!(d.uac, Some(Uac::V2));
    assert_eq!(
        d.clocks,
        [ClockSource {
            id: 0x29,
            programmable: true,
            settable: true
        }]
    );
    assert_eq!(
        d.hardware_volume,
        Some(HardwareVolume {
            unit_id: 0x0a,
            volume: true,
            mute: true
        })
    );
    // Three playback settings: 32-bit PCM, 16-bit PCM, native DSD; asynchronous with feedback
    let shape: Vec<_> = d
        .streams
        .iter()
        .map(|s| (s.alt, s.subslot_bytes, s.bits, s.pcm, s.dsd_raw, s.channels))
        .collect();
    assert_eq!(
        shape,
        [
            (1, 4, 32, true, false, 2),
            (2, 2, 16, true, false, 2),
            (3, 4, 32, false, true, 2)
        ]
    );
    for s in &d.streams {
        assert_eq!(s.interface, 1);
        assert_eq!(s.rates, Rates::FromClock(0x29));
        let e = s.endpoint.as_ref().unwrap();
        assert_eq!(
            (e.address, e.sync, e.feedback, e.interval),
            (0x01, Sync::Async, Some(0x81), 1)
        );
    }
    assert_eq!(d.streams[0].endpoint.as_ref().unwrap().max_packet, 0x0308);
    assert_eq!(d.streams[1].endpoint.as_ref().unwrap().max_packet, 0x0184);

    let ranges = parse_rate_ranges(&ka17_range_reply());
    let s = d.summary("FiiO KA17", &ranges);
    assert_eq!(s.rates.first(), Some(&44_100));
    assert_eq!(s.rates.last(), Some(&768_000));
    assert_eq!((s.max_bits, s.dsd, s.hardware_volume), (32, true, true));
    assert_eq!(
        s.line(),
        "FiiO KA17 · PCM up to 768 kHz / 32 bit · DSD · hardware volume"
    );
    // Before the clock was asked: no rates, the rest still known
    assert_eq!(
        d.summary("FiiO KA17", &[]).line(),
        "FiiO KA17 · DSD · hardware volume"
    );
}

#[test]
fn rate_ranges() {
    // One continuous range with a step, as some DACs answer
    let mut b = 1u16.to_le_bytes().to_vec();
    for v in [44_100u32, 96_000, 100] {
        b.extend(v.to_le_bytes());
    }
    let r = parse_rate_ranges(&b);
    assert_eq!(
        r,
        [RateRange {
            min: 44_100,
            max: 96_000,
            res: 100
        }]
    );
    assert_eq!(rates_in(&r), [44_100, 48_000, 88_200, 96_000]);
    // A step that misses a rate leaves it out
    let coarse = [RateRange {
        min: 44_100,
        max: 96_000,
        res: 3_900,
    }];
    assert_eq!(rates_in(&coarse), [44_100, 48_000]);
    // Short and empty replies
    assert!(parse_rate_ranges(&[]).is_empty());
    assert!(parse_rate_ranges(&[2, 0, 1, 2, 3]).is_empty());
    // A reply saying more sub-ranges than it holds gives the ones it has
    let mut b = 3u16.to_le_bytes().to_vec();
    for v in [48_000u32, 48_000, 0] {
        b.extend(v.to_le_bytes());
    }
    assert_eq!(rates_in(&parse_rate_ranges(&b)), [48_000]);
}

/// A UAC1 dongle: 44.1 and 48 kHz at 16 and 24 bit, adaptive, with a volume and mute.
fn uac1_dongle() -> Vec<u8> {
    let mut b = vec![
        18, 1, 0x00, 0x02, 0, 0, 0, 64, 0xac, 0x05, 0x0a, 0x11, 0x00, 0x01, 1, 2, 3, 1,
    ];
    let mut cfg: Vec<u8> = vec![
        // AudioControl interface, UAC1 (protocol 0)
        9, 4, 0, 0, 0, 1, 1, 0, 0, 9, 0x24, 1, 0x00, 0x01, 0x27, 0, 1, 1, // header
        12, 0x24, 2, 1, 0x01, 0x01, 0, 2, 3, 0, 0, 0, // input terminal (USB streaming)
        10, 0x24, 6, 2, 1, 1, 0x03, 0x00, 0x00, 0, // feature unit: master mute+volume
        9, 0x24, 3, 3, 0x02, 0x04, 0, 2, 0, // output terminal (headphones)
        // Streaming interface: alt 0, then alt 1 (16 bit) and alt 2 (24 bit)
        9, 4, 1, 0, 0, 1, 2, 0, 0, 9, 4, 1, 1, 1, 1, 2, 0, 0, 7, 0x24, 1, 1, 1, 0x01,
        0x00, // AS_GENERAL: PCM
        14, 0x24, 2, 1, 2, 2, 16, 2, 0x44, 0xac, 0x00, 0x80, 0xbb, 0x00, // 2 discrete rates
        9, 5, 0x01, 0x09, 0xc0, 0x00, 1, 0, 0, // iso OUT, adaptive
        7, 0x25, 1, 0, 0, 0, 0, 9, 4, 1, 2, 1, 1, 2, 0, 0, 7, 0x24, 1, 1, 1, 0x01, 0x00, 14, 0x24, 2, 1, 2,
        3, 24, 2, 0x44, 0xac, 0x00, 0x80, 0xbb, 0x00, 9, 5, 0x01, 0x09, 0x20, 0x01, 1, 0, 0, 7, 0x25, 1, 0,
        0, 0, 0,
    ];
    let total = (9 + cfg.len()) as u16;
    let mut head = vec![9, 2];
    head.extend(total.to_le_bytes());
    head.extend([2, 1, 0, 0x80, 50]);
    head.append(&mut cfg);
    b.extend(head);
    b
}

#[test]
fn a_uac1_dongle() {
    let d = parse(&uac1_dongle());
    assert_eq!(
        (d.vendor_id, d.product_id, d.uac),
        (0x05ac, 0x110a, Some(Uac::V1))
    );
    assert_eq!(
        d.hardware_volume,
        Some(HardwareVolume {
            unit_id: 2,
            volume: true,
            mute: true
        })
    );
    assert_eq!(d.streams.len(), 2);
    assert_eq!(d.streams[0].rates, Rates::Discrete(vec![44_100, 48_000]));
    assert_eq!((d.streams[1].subslot_bytes, d.streams[1].bits), (3, 24));
    assert_eq!(d.streams[0].endpoint.as_ref().unwrap().sync, Sync::Adaptive);
    assert_eq!(
        d.summary("USB-C dongle", &[]).line(),
        "USB-C dongle · PCM up to 48 kHz / 24 bit · hardware volume"
    );
}

#[test]
fn hostile_descriptors_never_panic() {
    // Every truncation of the real KA17
    for n in 0..KA17.len() {
        let _ = parse(&KA17[..n]);
    }
    // Lengths of 0 and 1, lengths past the end, and noise
    for bad in [
        &[0u8, 0, 0][..],
        &[1, 4],
        &[255, 4, 0, 0],
        &[9, 4, 0, 0, 0, 1, 1, 0x20, 0, 3, 0x24],
    ] {
        let _ = parse(bad);
    }
    let mut x: u32 = 0x1234_5678;
    for _ in 0..2000 {
        let v: Vec<u8> = (0..64)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                (x & 0xff) as u8
            })
            .collect();
        let _ = parse(&v);
        let _ = parse_rate_ranges(&v);
    }
}
