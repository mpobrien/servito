use crate::CHUNK_MS;
use memchr::memchr;

pub struct Frame {
    pub start: usize,
    pub end:   usize,
}

fn id3_skip(data: &[u8]) -> usize {
    if data.len() > 10 && &data[0..3] == b"ID3" {
        let sz = ((data[6] as usize) << 21) | ((data[7] as usize) << 14)
               | ((data[8] as usize) <<  7) |  data[9] as usize;
        sz + 10
    } else {
        0
    }
}

const BR1: [u32; 16] = [0, 32000, 40000, 48000, 56000, 64000, 80000, 96000,
                        112000, 128000, 160000, 192000, 224000, 256000, 320000, 0];
const BR2: [u32; 16] = [0, 8000, 16000, 24000, 32000, 40000, 48000, 56000,
                        64000, 80000, 96000, 112000, 128000, 144000, 160000, 0];
const SR1:  [u32; 3] = [44100, 48000, 32000];
const SR2:  [u32; 3] = [22050, 24000, 16000];
const SR25: [u32; 3] = [11025, 12000,  8000];

/// Decode one MPEG Layer-3 frame header starting at `data[i]`.
/// Returns `Some((frame_size, sample_rate))` or `None` if not a valid header.
#[inline]
fn decode_header(data: &[u8], i: usize) -> Option<(usize, u32)> {
    if i + 4 > data.len() { return None; }
    if data[i] != 0xFF || (data[i + 1] & 0xE0) != 0xE0 { return None; }

    let b1      = data[i + 1];
    let b2      = data[i + 2];
    let version = (b1 >> 3) & 0x3;
    let layer   = (b1 >> 1) & 0x3;
    let br_idx  = ((b2 >> 4) & 0xF) as usize;
    let sr_idx  = ((b2 >> 2) & 0x3) as usize;
    let padding = ((b2 >> 1) & 0x1) as usize;

    if layer != 0x1 || br_idx == 0 || br_idx == 15 || sr_idx >= 3 { return None; }

    let (bitrate, sr, mpeg1) = match version {
        0x3 => (BR1[br_idx], SR1[sr_idx],  true),
        0x2 => (BR2[br_idx], SR2[sr_idx],  false),
        0x0 => (BR2[br_idx], SR25[sr_idx], false),
        _   => return None,
    };

    let frame_size = if mpeg1 {
        144 * bitrate as usize / sr as usize + padding
    } else {
        72  * bitrate as usize / sr as usize + padding
    };

    if frame_size < 4 || i + frame_size > data.len() { return None; }

    Some((frame_size, sr))
}

pub fn parse_frames(data: &[u8]) -> (Vec<Frame>, u32) {
    let mut i           = id3_skip(data);
    let mut frames      = Vec::new();
    let mut sample_rate = 44100u32;

    while i + 4 <= data.len() {
        // Jump to the next 0xFF byte using SIMD memchr instead of byte-by-byte.
        match memchr(0xFF, &data[i..]) {
            None    => break,
            Some(d) => i += d,
        }
        match decode_header(data, i) {
            Some((frame_size, sr)) => {
                sample_rate = sr;
                frames.push(Frame { start: i, end: i + frame_size });
                i += frame_size;
            }
            None => i += 1,
        }
    }

    (frames, sample_rate)
}

/// Probe duration from a small buffer read at the start of the audio data
/// (after any ID3 tag has already been skipped by the caller).
/// `audio_size` is `file_size - id3_size`, used to estimate CBR frame count.
///
/// Returns `Some((frame_count, sample_rate))` or `None` if no valid frame found.
pub fn probe_duration(data: &[u8], audio_size: u64) -> Option<(u64, u32)> {
    let mut i = 0;
    while i + 4 <= data.len() {
        match memchr(0xFF, &data[i..]) {
            None    => break,
            Some(d) => i += d,
        }
        if let Some((frame_size, sr)) = decode_header(data, i) {
            // Side-info size depends on MPEG version and channel mode.
            let version      = (data[i + 1] >> 3) & 0x3;
            let channel_mode = (data[i + 3] >> 6) & 0x3; // 3 = mono
            let side_info: usize = match (version, channel_mode) {
                (0x3, 3) => 17, // MPEG1 mono
                (0x3, _) => 32, // MPEG1 stereo
                (_, 3)   =>  9, // MPEG2/2.5 mono
                _        => 17, // MPEG2/2.5 stereo
            };

            // Xing / Info VBR header (immediately after the side-info block).
            let xing = i + 4 + side_info;
            if xing + 12 <= data.len() {
                let tag = &data[xing..xing + 4];
                if tag == b"Xing" || tag == b"Info" {
                    let flags = u32::from_be_bytes([data[xing+4], data[xing+5], data[xing+6], data[xing+7]]);
                    if flags & 0x1 != 0 {
                        let frames = u32::from_be_bytes([data[xing+8], data[xing+9], data[xing+10], data[xing+11]]) as u64;
                        if frames > 0 { return Some((frames, sr)); }
                    }
                }
            }

            // VBRI header (Fraunhofer) — always at frame_start + 36.
            let vbri = i + 36;
            if vbri + 18 <= data.len() && &data[vbri..vbri+4] == b"VBRI" {
                let frames = u32::from_be_bytes([data[vbri+14], data[vbri+15], data[vbri+16], data[vbri+17]]) as u64;
                if frames > 0 { return Some((frames, sr)); }
            }

            // CBR fallback: estimate from file size and this frame's size.
            let frames = audio_size / frame_size as u64;
            return Some((frames, sr));
        }
        i += 1;
    }
    None
}

pub fn frames_per_chunk_for(sample_rate: u32) -> usize {
    // ceil(CHUNK_MS * sample_rate / (1152 * 1000))
    (CHUNK_MS as usize * sample_rate as usize + 1151 * 1000) / (1152 * 1000)
}
