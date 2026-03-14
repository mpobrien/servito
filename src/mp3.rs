use crate::CHUNK_MS;

pub struct Frame {
    pub start: usize,
    pub end:   usize,
}

pub fn parse_frames(data: &[u8]) -> (Vec<Frame>, u32) {
    let br1: [u32; 16] = [0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 0]
        .map(|k| k * 1000);
    let br2: [u32; 16] = [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160, 0]
        .map(|k| k * 1000);
    let sr1:  [u32; 3] = [44100, 48000, 32000];
    let sr2:  [u32; 3] = [22050, 24000, 16000];
    let sr25: [u32; 3] = [11025, 12000,  8000];

    let mut i = if data.len() > 10 && &data[0..3] == b"ID3" {
        let sz = ((data[6] as usize) << 21) | ((data[7] as usize) << 14)
               | ((data[8] as usize) <<  7) |  data[9] as usize;
        sz + 10
    } else {
        0
    };

    let mut frames      = Vec::new();
    let mut sample_rate = 44100u32;

    while i + 4 <= data.len() {
        if data[i] != 0xFF || (data[i + 1] & 0xE0) != 0xE0 { i += 1; continue; }

        let b1      = data[i + 1];
        let b2      = data[i + 2];
        let version = (b1 >> 3) & 0x3;
        let layer   = (b1 >> 1) & 0x3;
        let br_idx  = ((b2 >> 4) & 0xF) as usize;
        let sr_idx  = ((b2 >> 2) & 0x3) as usize;
        let padding = ((b2 >> 1) & 0x1) as usize;

        if layer != 0x1 || br_idx == 0 || br_idx == 15 || sr_idx >= 3 { i += 1; continue; }

        let (bitrate, sr, mpeg1) = match version {
            0x3 => (br1[br_idx], sr1[sr_idx],  true),
            0x2 => (br2[br_idx], sr2[sr_idx],  false),
            0x0 => (br2[br_idx], sr25[sr_idx], false),
            _   => { i += 1; continue; }
        };

        let frame_size = if mpeg1 {
            144 * bitrate as usize / sr as usize + padding
        } else {
            72 * bitrate as usize / sr as usize + padding
        };

        if frame_size < 4 || i + frame_size > data.len() { i += 1; continue; }

        sample_rate = sr;
        frames.push(Frame { start: i, end: i + frame_size });
        i += frame_size;
    }

    (frames, sample_rate)
}

pub fn frames_per_chunk_for(sample_rate: u32) -> usize {
    // ceil(CHUNK_MS * sample_rate / (1152 * 1000))
    (CHUNK_MS as usize * sample_rate as usize + 1151 * 1000) / (1152 * 1000)
}
