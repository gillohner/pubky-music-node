//! DSF and DFF (DSDIFF) readers for native DSD playback, plus writers for
//! tests.
//!
//! Both are read without any conversion of the audio bits. Output blocks use
//! the `pm_model::block` DSD layout: one byte per channel interleaved
//! (`L R L R …`), eight 1-bit samples per byte, **oldest sample in the most
//! significant bit**. DFF already stores that. DSF stores each channel in
//! blocks of `block_size` bytes (normally 4096) with the oldest sample in the
//! **least** significant bit, so the reader reverses the bits of every byte
//! and de-interleaves the blocks; the writer does the inverse. Reversing bit
//! order is a permutation, so a DSF → DFF → DSF trip is lossless.
//!
//! Tags: DSF carries an ID3v2 blob at the metadata pointer; DFF may carry an
//! `ID3 ` chunk at the top level or inside `DIIN`. The raw bytes are exposed
//! through [`DsdReader::id3_bytes`] and parsed by `pm-library`.

use crate::error::AudioError;
use pm_model::{AudioBlock, AudioFormat};
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

/// Frames (bytes per channel) per block the reader yields.
const FRAMES_PER_BLOCK: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DsdContainer {
    Dsf,
    Dff,
}

impl DsdContainer {
    pub fn as_str(&self) -> &'static str {
        match self {
            DsdContainer::Dsf => "dsf",
            DsdContainer::Dff => "dff",
        }
    }
}

pub struct DsdReader {
    file: BufReader<File>,
    container: DsdContainer,
    format: AudioFormat,
    /// Total frames (bytes per channel) of audio.
    total_frames: u64,
    frames_read: u64,
    data_start: u64,
    id3: Option<Vec<u8>>,
    /// DSF only: bytes per channel per block, and whether bytes are LSB-first.
    dsf_block_size: usize,
    dsf_lsb_first: bool,
    /// DSF only: frames to drop from the next block after a seek inside a block.
    skip_frames: usize,
}

impl DsdReader {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, AudioError> {
        let path = path.as_ref();
        let file = File::open(path).map_err(|source| AudioError::Open {
            path: path.to_path_buf(),
            source,
        })?;
        let mut file = BufReader::new(file);
        let mut magic = [0u8; 4];
        file.read_exact(&mut magic)?;
        file.seek(SeekFrom::Start(0))?;
        match &magic {
            b"DSD " => Self::open_dsf(file),
            b"FRM8" => Self::open_dff(file),
            _ => Err(AudioError::Unsupported(format!(
                "{}: not a DSF or DFF file",
                path.display()
            ))),
        }
    }

    fn open_dsf(mut file: BufReader<File>) -> Result<Self, AudioError> {
        // DSD chunk
        let mut hdr = [0u8; 28];
        file.read_exact(&mut hdr)?;
        let chunk_size = u64::from_le_bytes(hdr[4..12].try_into().unwrap());
        let metadata_ptr = u64::from_le_bytes(hdr[20..28].try_into().unwrap());
        if chunk_size != 28 {
            return Err(AudioError::Malformed("dsf: DSD chunk size".into()));
        }
        // fmt chunk
        let mut fmt = [0u8; 52];
        file.read_exact(&mut fmt)?;
        if &fmt[0..4] != b"fmt " {
            return Err(AudioError::Malformed("dsf: missing fmt chunk".into()));
        }
        let le32 = |o: usize| u32::from_le_bytes(fmt[o..o + 4].try_into().unwrap());
        let format_id = le32(16);
        let channels = le32(24);
        let rate = le32(28);
        let bits = le32(32);
        let sample_count = u64::from_le_bytes(fmt[36..44].try_into().unwrap());
        let block_size = le32(44) as usize;
        if format_id != 0 {
            return Err(AudioError::Unsupported("dsf: compressed format".into()));
        }
        if channels == 0 || channels > 8 {
            return Err(AudioError::Malformed(format!("dsf: channel count {channels}")));
        }
        if block_size == 0 || block_size > 1 << 20 {
            return Err(AudioError::Malformed(format!("dsf: block size {block_size}")));
        }
        let lsb_first = match bits {
            1 => true,
            8 => false,
            other => return Err(AudioError::Malformed(format!("dsf: bits per sample {other}"))),
        };
        let format = AudioFormat::dsd(rate, channels as u8);
        format
            .validate()
            .map_err(|e| AudioError::Malformed(format!("dsf: {e}")))?;
        // data chunk
        let mut data = [0u8; 12];
        file.read_exact(&mut data)?;
        if &data[0..4] != b"data" {
            return Err(AudioError::Malformed("dsf: missing data chunk".into()));
        }
        let data_start = 28 + 52 + 12;
        let total_frames = sample_count.div_ceil(8);

        let id3 = if metadata_ptr != 0 {
            read_id3_at(&mut file, metadata_ptr).ok()
        } else {
            None
        };
        file.seek(SeekFrom::Start(data_start))?;
        Ok(Self {
            file,
            container: DsdContainer::Dsf,
            format,
            total_frames,
            frames_read: 0,
            data_start,
            id3,
            dsf_block_size: block_size,
            dsf_lsb_first: lsb_first,
            skip_frames: 0,
        })
    }

    fn open_dff(mut file: BufReader<File>) -> Result<Self, AudioError> {
        let mut hdr = [0u8; 16];
        file.read_exact(&mut hdr)?;
        let form_size = u64::from_be_bytes(hdr[4..12].try_into().unwrap());
        if &hdr[12..16] != b"DSD " {
            return Err(AudioError::Unsupported("dff: form type is not DSD".into()));
        }
        let end = 12 + form_size;
        let mut pos: u64 = 16;
        let mut rate = None;
        let mut channels = None;
        let mut compressed = false;
        let mut data: Option<(u64, u64)> = None;
        let mut id3 = None;
        while pos + 12 <= end {
            file.seek(SeekFrom::Start(pos))?;
            let mut ch = [0u8; 12];
            if file.read_exact(&mut ch).is_err() {
                break;
            }
            let id = &ch[0..4];
            let size = u64::from_be_bytes(ch[4..12].try_into().unwrap());
            let body = pos + 12;
            match id {
                b"PROP" => {
                    let mut kind = [0u8; 4];
                    file.read_exact(&mut kind)?;
                    if &kind != b"SND " {
                        return Err(AudioError::Unsupported("dff: PROP is not SND".into()));
                    }
                    let mut p = body + 4;
                    let pend = body + size;
                    while p + 12 <= pend {
                        file.seek(SeekFrom::Start(p))?;
                        let mut sc = [0u8; 12];
                        file.read_exact(&mut sc)?;
                        let ssize = u64::from_be_bytes(sc[4..12].try_into().unwrap());
                        match &sc[0..4] {
                            b"FS  " => {
                                let mut b = [0u8; 4];
                                file.read_exact(&mut b)?;
                                rate = Some(u32::from_be_bytes(b));
                            }
                            b"CHNL" => {
                                let mut b = [0u8; 2];
                                file.read_exact(&mut b)?;
                                channels = Some(u16::from_be_bytes(b));
                            }
                            b"CMPR" => {
                                let mut b = [0u8; 4];
                                file.read_exact(&mut b)?;
                                compressed = &b != b"DSD ";
                            }
                            _ => {}
                        }
                        p += 12 + ssize + (ssize & 1);
                    }
                }
                b"DSD " => data = Some((body, size)),
                b"DST " => compressed = true,
                b"ID3 " => id3 = read_exact_vec(&mut file, size).ok(),
                b"DIIN" => {
                    let mut p = body;
                    let pend = body + size;
                    while p + 12 <= pend {
                        file.seek(SeekFrom::Start(p))?;
                        let mut sc = [0u8; 12];
                        file.read_exact(&mut sc)?;
                        let ssize = u64::from_be_bytes(sc[4..12].try_into().unwrap());
                        if &sc[0..4] == b"ID3 " && id3.is_none() {
                            id3 = read_exact_vec(&mut file, ssize).ok();
                        }
                        p += 12 + ssize + (ssize & 1);
                    }
                }
                _ => {}
            }
            pos = body + size + (size & 1);
        }
        if compressed {
            return Err(AudioError::Unsupported("dff: DST compressed audio".into()));
        }
        let rate = rate.ok_or_else(|| AudioError::Malformed("dff: missing FS".into()))?;
        let channels = channels.ok_or_else(|| AudioError::Malformed("dff: missing CHNL".into()))?;
        if channels == 0 || channels > 8 {
            return Err(AudioError::Malformed(format!("dff: channel count {channels}")));
        }
        let (data_start, data_len) =
            data.ok_or_else(|| AudioError::Malformed("dff: missing DSD chunk".into()))?;
        let format = AudioFormat::dsd(rate, channels as u8);
        format
            .validate()
            .map_err(|e| AudioError::Malformed(format!("dff: {e}")))?;
        file.seek(SeekFrom::Start(data_start))?;
        Ok(Self {
            file,
            container: DsdContainer::Dff,
            format,
            total_frames: data_len / channels as u64,
            frames_read: 0,
            data_start,
            id3,
            dsf_block_size: 0,
            dsf_lsb_first: false,
            skip_frames: 0,
        })
    }

    pub fn format(&self) -> AudioFormat {
        self.format
    }

    pub fn container(&self) -> &'static str {
        self.container.as_str()
    }

    pub fn duration_ms(&self) -> Option<u64> {
        Some(self.total_frames * 8 * 1000 / self.format.rate_hz as u64)
    }

    pub fn total_frames(&self) -> u64 {
        self.total_frames
    }

    pub fn id3_bytes(&self) -> Option<&[u8]> {
        self.id3.as_deref()
    }

    /// Rewind to the first frame.
    pub fn rewind(&mut self) -> Result<(), AudioError> {
        self.seek_frames(0)
    }

    /// Exact seek: the next block starts at `frame` (one frame = one byte =
    /// 8 bits per channel). Past the end clamps to the end.
    pub fn seek_frames(&mut self, frame: u64) -> Result<(), AudioError> {
        let frame = frame.min(self.total_frames);
        let ch = self.format.channels as u64;
        match self.container {
            DsdContainer::Dff => {
                self.file.seek(SeekFrom::Start(self.data_start + frame * ch))?;
                self.frames_read = frame;
                self.skip_frames = 0;
            }
            DsdContainer::Dsf => {
                let bs = self.dsf_block_size as u64;
                let block = frame / bs;
                self.file
                    .seek(SeekFrom::Start(self.data_start + block * bs * ch))?;
                self.frames_read = block * bs;
                self.skip_frames = (frame - block * bs) as usize;
            }
        }
        Ok(())
    }

    /// Seek to `ms`, rounded down to the frame.
    pub fn seek_ms(&mut self, ms: u64) -> Result<(), AudioError> {
        self.seek_frames(ms * self.format.rate_hz as u64 / 8_000)
    }

    /// Next block in the interleaved MSB-first layout; `None` at the end.
    pub fn next_block(&mut self) -> Result<Option<AudioBlock>, AudioError> {
        let ch = self.format.channels as usize;
        let remaining = self.total_frames - self.frames_read;
        if remaining == 0 {
            return Ok(None);
        }
        match self.container {
            DsdContainer::Dff => {
                let frames = remaining.min(FRAMES_PER_BLOCK as u64) as usize;
                let mut buf = vec![0u8; frames * ch];
                let n = read_up_to(&mut self.file, &mut buf)?;
                if n == 0 {
                    self.frames_read = self.total_frames;
                    return Ok(None);
                }
                let frames = n / ch;
                buf.truncate(frames * ch);
                self.frames_read += frames as u64;
                Ok(Some(AudioBlock::dsd(self.format, buf)))
            }
            DsdContainer::Dsf => {
                // One block set: block_size bytes for each channel.
                let bs = self.dsf_block_size;
                let mut raw = vec![0u8; bs * ch];
                let n = read_up_to(&mut self.file, &mut raw)?;
                if n == 0 {
                    self.frames_read = self.total_frames;
                    return Ok(None);
                }
                let frames = (remaining.min(bs as u64)) as usize;
                let mut out = vec![0u8; frames * ch];
                for c in 0..ch {
                    let plane = &raw[c * bs..c * bs + frames];
                    for (f, &b) in plane.iter().enumerate() {
                        out[f * ch + c] = if self.dsf_lsb_first { b.reverse_bits() } else { b };
                    }
                }
                self.frames_read += frames as u64;
                if self.skip_frames > 0 {
                    let drop = self.skip_frames.min(frames) * ch;
                    out.drain(..drop);
                    self.skip_frames = 0;
                    if out.is_empty() {
                        return self.next_block();
                    }
                }
                Ok(Some(AudioBlock::dsd(self.format, out)))
            }
        }
    }

    pub fn read_all(&mut self) -> Result<Vec<u8>, AudioError> {
        let mut out = Vec::new();
        while let Some(b) = self.next_block()? {
            if let pm_model::Samples::Dsd(v) = b.samples {
                out.extend_from_slice(&v);
            }
        }
        Ok(out)
    }
}

fn read_up_to<R: Read>(r: &mut R, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut total = 0;
    while total < buf.len() {
        let n = r.read(&mut buf[total..])?;
        if n == 0 {
            break;
        }
        total += n;
    }
    Ok(total)
}

fn read_exact_vec<R: Read>(r: &mut R, len: u64) -> std::io::Result<Vec<u8>> {
    if len > 16 * 1024 * 1024 {
        return Err(std::io::Error::other("tag chunk too large"));
    }
    let mut v = vec![0u8; len as usize];
    r.read_exact(&mut v)?;
    Ok(v)
}

/// Read an ID3v2 tag (header + body) at `offset`.
fn read_id3_at<R: Read + Seek>(r: &mut R, offset: u64) -> std::io::Result<Vec<u8>> {
    r.seek(SeekFrom::Start(offset))?;
    let mut hdr = [0u8; 10];
    r.read_exact(&mut hdr)?;
    if &hdr[0..3] != b"ID3" {
        return Err(std::io::Error::other("no ID3 header at metadata pointer"));
    }
    let size = hdr[6..10]
        .iter()
        .fold(0u64, |acc, &b| (acc << 7) | (b & 0x7f) as u64);
    let footer = if hdr[5] & 0x10 != 0 { 10 } else { 0 };
    let mut body = read_exact_vec(r, size + footer)?;
    let mut out = hdr.to_vec();
    out.append(&mut body);
    Ok(out)
}

/// Writers, for tests and tooling.
#[cfg(any(test, feature = "testutil"))]
pub mod write {
    use super::*;
    use std::io::Write;

    /// Write a DSF file from interleaved MSB-first bytes (`frames * channels`).
    pub fn write_dsf(
        path: &Path,
        format: AudioFormat,
        interleaved: &[u8],
        id3: Option<&[u8]>,
    ) -> Result<(), AudioError> {
        let ch = format.channels as usize;
        let frames = interleaved.len() / ch;
        let block = 4096usize;
        let blocks = frames.div_ceil(block).max(1);
        let data_len = blocks * block * ch;
        let total = 28 + 52 + 12 + data_len as u64 + id3.map(|t| t.len() as u64).unwrap_or(0);
        let mut f = std::io::BufWriter::new(File::create(path)?);
        f.write_all(b"DSD ")?;
        f.write_all(&28u64.to_le_bytes())?;
        f.write_all(&total.to_le_bytes())?;
        f.write_all(
            &(if id3.is_some() {
                28 + 52 + 12 + data_len as u64
            } else {
                0
            })
            .to_le_bytes(),
        )?;
        f.write_all(b"fmt ")?;
        f.write_all(&52u64.to_le_bytes())?;
        f.write_all(&1u32.to_le_bytes())?; // version
        f.write_all(&0u32.to_le_bytes())?; // DSD raw
        let channel_type: u32 = match ch {
            1 => 1,
            2 => 2,
            3 => 3,
            4 => 4,
            5 => 6,
            6 => 7,
            _ => 7,
        };
        f.write_all(&channel_type.to_le_bytes())?;
        f.write_all(&(ch as u32).to_le_bytes())?;
        f.write_all(&format.rate_hz.to_le_bytes())?;
        f.write_all(&1u32.to_le_bytes())?; // 1 bit, LSB first
        f.write_all(&((frames * 8) as u64).to_le_bytes())?;
        f.write_all(&(block as u32).to_le_bytes())?;
        f.write_all(&0u32.to_le_bytes())?;
        f.write_all(b"data")?;
        f.write_all(&(12 + data_len as u64).to_le_bytes())?;
        let mut plane = vec![0u8; block];
        for b in 0..blocks {
            for c in 0..ch {
                plane.fill(0);
                for (i, slot) in plane.iter_mut().enumerate() {
                    let fr = b * block + i;
                    if fr < frames {
                        *slot = interleaved[fr * ch + c].reverse_bits();
                    }
                }
                f.write_all(&plane)?;
            }
        }
        if let Some(t) = id3 {
            f.write_all(t)?;
        }
        f.flush()?;
        Ok(())
    }

    /// Write a DFF file from interleaved MSB-first bytes.
    pub fn write_dff(
        path: &Path,
        format: AudioFormat,
        interleaved: &[u8],
        id3: Option<&[u8]>,
    ) -> Result<(), AudioError> {
        let ch = format.channels as usize;
        let mut body: Vec<u8> = Vec::new();
        body.extend_from_slice(b"DSD ");
        // FVER
        body.extend_from_slice(b"FVER");
        body.extend_from_slice(&4u64.to_be_bytes());
        body.extend_from_slice(&0x0105_0000u32.to_be_bytes());
        // PROP
        let mut prop: Vec<u8> = Vec::new();
        prop.extend_from_slice(b"SND ");
        prop.extend_from_slice(b"FS  ");
        prop.extend_from_slice(&4u64.to_be_bytes());
        prop.extend_from_slice(&format.rate_hz.to_be_bytes());
        let ids: Vec<&[u8; 4]> = match ch {
            1 => vec![b"C   "],
            2 => vec![b"SLFT", b"SRGT"],
            _ => (0..ch).map(|_| b"C   ").collect(),
        };
        prop.extend_from_slice(b"CHNL");
        prop.extend_from_slice(&((2 + 4 * ch) as u64).to_be_bytes());
        prop.extend_from_slice(&(ch as u16).to_be_bytes());
        for id in ids {
            prop.extend_from_slice(id);
        }
        prop.extend_from_slice(b"CMPR");
        let desc = b"not compressed";
        let cmpr_len = 4 + 1 + desc.len();
        prop.extend_from_slice(&(cmpr_len as u64).to_be_bytes());
        prop.extend_from_slice(b"DSD ");
        prop.push(desc.len() as u8);
        prop.extend_from_slice(desc);
        if cmpr_len % 2 == 1 {
            prop.push(0);
        }
        body.extend_from_slice(b"PROP");
        body.extend_from_slice(&(prop.len() as u64).to_be_bytes());
        body.extend_from_slice(&prop);
        // DSD data
        body.extend_from_slice(b"DSD ");
        body.extend_from_slice(&(interleaved.len() as u64).to_be_bytes());
        body.extend_from_slice(interleaved);
        if interleaved.len() % 2 == 1 {
            body.push(0);
        }
        if let Some(t) = id3 {
            body.extend_from_slice(b"ID3 ");
            body.extend_from_slice(&(t.len() as u64).to_be_bytes());
            body.extend_from_slice(t);
            if t.len() % 2 == 1 {
                body.push(0);
            }
        }
        let mut f = std::io::BufWriter::new(File::create(path)?);
        f.write_all(b"FRM8")?;
        f.write_all(&(body.len() as u64).to_be_bytes())?;
        f.write_all(&body)?;
        f.flush()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::write::{write_dff, write_dsf};
    use super::*;

    fn pattern(frames: usize, ch: usize) -> Vec<u8> {
        let mut v = Vec::with_capacity(frames * ch);
        let mut x: u32 = 0x1234_5678;
        for _ in 0..frames * ch {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            v.push((x & 0xff) as u8);
        }
        v
    }

    fn fake_id3() -> Vec<u8> {
        // Minimal ID3v2.4 header with a 4-byte body.
        let mut v = b"ID3\x04\x00\x00".to_vec();
        v.extend_from_slice(&[0, 0, 0, 4]);
        v.extend_from_slice(&[1, 2, 3, 4]);
        v
    }

    #[test]
    fn dsf_and_dff_roundtrip_identical() {
        for (rate, ch, frames) in [
            (AudioFormat::DSD64_RATE, 2, 10_000usize),
            (AudioFormat::DSD128_RATE, 1, 4096),
            (AudioFormat::DSD256_RATE, 6, 5000),
            (AudioFormat::DSD512_RATE, 2, 1),
        ] {
            let fmt = AudioFormat::dsd(rate, ch);
            let data = pattern(frames, ch as usize);
            let dir = tempfile::tempdir().unwrap();
            let dsf = dir.path().join("a.dsf");
            let dff = dir.path().join("a.dff");
            write_dsf(&dsf, fmt, &data, Some(&fake_id3())).unwrap();
            write_dff(&dff, fmt, &data, Some(&fake_id3())).unwrap();

            let mut r1 = DsdReader::open(&dsf).unwrap();
            assert_eq!(r1.container(), "dsf");
            assert_eq!(r1.format(), fmt);
            assert_eq!(r1.total_frames(), frames as u64);
            assert_eq!(r1.id3_bytes(), Some(fake_id3().as_slice()));
            let a = r1.read_all().unwrap();

            let mut r2 = DsdReader::open(&dff).unwrap();
            assert_eq!(r2.container(), "dff");
            assert_eq!(r2.format(), fmt);
            assert_eq!(r2.total_frames(), frames as u64);
            assert_eq!(r2.id3_bytes(), Some(fake_id3().as_slice()));
            let b = r2.read_all().unwrap();

            assert_eq!(a, data, "dsf rate={rate} ch={ch}");
            assert_eq!(b, data, "dff rate={rate} ch={ch}");
            assert_eq!(r1.duration_ms(), Some(frames as u64 * 8 * 1000 / rate as u64));

            // Rewind and read again.
            r1.rewind().unwrap();
            assert_eq!(r1.read_all().unwrap(), data);
        }
    }

    #[test]
    fn dsf_blocks_are_bit_reversed_on_disk() {
        let fmt = AudioFormat::dsd(AudioFormat::DSD64_RATE, 2);
        let data = vec![0b1000_0000u8, 0b0000_0001, 0b1100_0000, 0b0000_0011];
        let dir = tempfile::tempdir().unwrap();
        let dsf = dir.path().join("b.dsf");
        write_dsf(&dsf, fmt, &data, None).unwrap();
        let raw = std::fs::read(&dsf).unwrap();
        let data_start = 28 + 52 + 12;
        // Channel 0 block holds frames 0 and 1 of channel 0, LSB-first.
        assert_eq!(raw[data_start], 0b0000_0001);
        assert_eq!(raw[data_start + 1], 0b0000_0011);
        // Channel 1 block starts 4096 bytes later.
        assert_eq!(raw[data_start + 4096], 0b1000_0000);
        assert_eq!(raw[data_start + 4097], 0b1100_0000);
        let mut r = DsdReader::open(&dsf).unwrap();
        assert_eq!(r.read_all().unwrap(), data);
    }

    #[test]
    fn seek_lands_on_the_exact_frame_in_both_containers() {
        let ch = 2usize;
        let fmt = AudioFormat::dsd(AudioFormat::DSD64_RATE, ch as u8);
        let frames = 10_000usize;
        let data = pattern(frames, ch);
        let dir = tempfile::tempdir().unwrap();
        let dsf = dir.path().join("s.dsf");
        let dff = dir.path().join("s.dff");
        write_dsf(&dsf, fmt, &data, None).unwrap();
        write_dff(&dff, fmt, &data, None).unwrap();
        for path in [&dsf, &dff] {
            let mut r = DsdReader::open(path).unwrap();
            // Mid-block, on a block boundary, in the last partial block, past the end.
            for frame in [1u64, 4095, 4096, 4097, 8192, 9_999, 10_000, 20_000] {
                r.seek_frames(frame).unwrap();
                let rest = r.read_all().unwrap();
                let f = frame.min(frames as u64) as usize;
                assert_eq!(rest, &data[f * ch..], "{} frame {frame}", path.display());
            }
            // seek_ms rounds down to the frame: 1 ms of DSD64 = 352.8 frames.
            r.seek_ms(1).unwrap();
            assert_eq!(r.read_all().unwrap().len(), (frames - 352) * ch);
        }
    }

    #[test]
    fn rejects_non_dsd() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.dsf");
        std::fs::write(&p, b"RIFF....WAVE").unwrap();
        assert!(matches!(DsdReader::open(&p), Err(AudioError::Unsupported(_))));
    }
}
