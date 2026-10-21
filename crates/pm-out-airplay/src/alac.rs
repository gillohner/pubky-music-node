//! Uncompressed ("verbatim") ALAC frames: what every AirPlay 1 receiver decodes, without an encoder.
//! 16-bit stereo, 352 frames per packet (the RAOP default), as Apple's and FFmpeg's encoders lay
//! out a verbatim channel-pair element.

/// Frames per RTP packet.
pub const FRAMES_PER_PACKET: usize = 352;

struct Bits {
    out: Vec<u8>,
    acc: u64,
    n: u32,
}

impl Bits {
    fn new(cap: usize) -> Self {
        Self {
            out: Vec::with_capacity(cap),
            acc: 0,
            n: 0,
        }
    }

    fn put(&mut self, bits: u32, value: u32) {
        self.acc = (self.acc << bits) | u64::from(value & ((1u64 << bits) - 1) as u32);
        self.n += bits;
        while self.n >= 8 {
            self.n -= 8;
            self.out.push((self.acc >> self.n) as u8);
        }
    }

    fn finish(mut self) -> Vec<u8> {
        if self.n > 0 {
            let pad = 8 - self.n;
            self.put(pad, 0);
        }
        self.out
    }
}

/// One frame from interleaved stereo samples (`[l, r, l, r, …]`, at most 352 pairs).
pub fn encode_verbatim(samples: &[i16]) -> Vec<u8> {
    let frames = samples.len() / 2;
    debug_assert!(frames <= FRAMES_PER_PACKET);
    let mut b = Bits::new(samples.len() * 2 + 16);
    b.put(3, 1); // channel pair element
    b.put(4, 0); // element instance
    b.put(12, 0); // unused
    let short = frames != FRAMES_PER_PACKET;
    b.put(1, short as u32); // the frame carries its sample count
    b.put(2, 0); // no extra bytes (16-bit)
    b.put(1, 1); // verbatim
    if short {
        b.put(32, frames as u32);
    }
    for s in &samples[..frames * 2] {
        b.put(16, *s as u16 as u32);
    }
    b.put(3, 7); // end of frame
    b.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_verbatim_frame_has_the_alac_layout() {
        let f = encode_verbatim(&[0x1234, -1]);
        // 3+4+12+1+2+1 = 23 header bits, 32 bits of count (short frame), 2×16 samples, 3 end bits.
        assert_eq!(f.len(), (23 + 32 + 32 + 3usize).div_ceil(8));
        // 001 0000 000000000000 1 00 1, then the count: 0x20 0x00 0x12 …
        assert_eq!(&f[..3], &[0x20, 0x00, 0x12]);
        let full = encode_verbatim(&vec![0i16; FRAMES_PER_PACKET * 2]);
        assert_eq!(full.len(), (23 + FRAMES_PER_PACKET * 32 + 3).div_ceil(8));
        assert_eq!(&full[..3], &[0x20, 0x00, 0x02]);
    }
}
