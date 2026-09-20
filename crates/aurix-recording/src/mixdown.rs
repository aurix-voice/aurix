//! Post-hoc processing of finished recordings: reading the per-participant Ogg/Opus tracks
//! back, decoding them with their timing gaps intact, mixing several tracks into one channel
//! recording and encoding the result as Ogg/Opus or WAV.
//!
//! Everything here is synchronous, CPU-bound and streaming: tracks are decoded block by
//! block in lockstep, so memory stays proportional to the number of tracks, not to their
//! length. Callers run it on the blocking thread pool.

use crate::ogg::OggOpusWriter;
use aurix_common::error::{AurixError, Result};
use std::io::{self, Seek, SeekFrom, Write};

/// Every decoder in this module runs at the Opus clock rate; mixes are produced at it too.
pub const MIX_SAMPLE_RATE: u32 = 48_000;
/// Block advanced per mixing step (20 ms at 48 kHz), which is also the Opus frame we encode.
pub const BLOCK_SAMPLES: usize = 960;
/// Largest Opus packet we are prepared to decode (RFC 6716 permits 1275 bytes per frame).
const MAX_PACKET_BYTES: usize = 8 * 1275;
/// Guard against corrupt granule positions turning one page into hours of silence.
const MAX_GAP_SAMPLES: u64 = 48_000 * 60;

/// Header of an Ogg/Opus stream (`OpusHead`, RFC 7845 §5.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpusHead {
    pub channels: u8,
    pub pre_skip: u16,
    /// Sample rate the granule positions of *this* file are expressed in. RFC 7845 mandates
    /// 48 kHz; Aurix' own writer uses the encoder input rate stored in the header.
    pub granule_rate: u32,
}

/// One Opus packet pulled out of the Ogg container, with the granule position of the page
/// that completed it when it was the last packet of that page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OggPacket {
    pub data: Vec<u8>,
    pub page_granule: Option<u64>,
}

/// Parses a complete Ogg/Opus file into its header and packets. Pages are CRC-checked;
/// packets may span pages. Files not produced by the Aurix writer are accepted as long as
/// they are a single logical Opus stream.
pub fn parse_ogg_opus(bytes: &[u8]) -> Result<(OpusHead, Vec<OggPacket>)> {
    let mut head: Option<OpusHead> = None;
    let mut packets = Vec::new();
    let mut partial: Vec<u8> = Vec::new();
    let mut serial: Option<u32> = None;
    let mut logical_index = 0usize;
    let mut pos = 0usize;

    while pos < bytes.len() {
        if bytes.len() - pos < 27 || &bytes[pos..pos + 4] != b"OggS" {
            return Err(bad("page capture pattern missing"));
        }
        let hdr = &bytes[pos..];
        let header_type = hdr[5];
        let granule = u64::from_le_bytes(hdr[6..14].try_into().expect("8 bytes"));
        let page_serial = u32::from_le_bytes(hdr[14..18].try_into().expect("4 bytes"));
        let stored_crc = u32::from_le_bytes(hdr[22..26].try_into().expect("4 bytes"));
        let nseg = hdr[26] as usize;
        let header_len = 27 + nseg;
        if hdr.len() < header_len {
            return Err(bad("truncated segment table"));
        }
        let segments = &hdr[27..header_len];
        let body_len: usize = segments.iter().map(|&s| s as usize).sum();
        let page_len = header_len + body_len;
        if hdr.len() < page_len {
            return Err(bad("truncated page body"));
        }
        let mut page = hdr[..page_len].to_vec();
        page[22..26].copy_from_slice(&[0; 4]);
        if crate::ogg::ogg_crc32(&page) != stored_crc {
            return Err(bad("page CRC mismatch"));
        }
        match serial {
            None => serial = Some(page_serial),
            Some(s) if s != page_serial => {
                return Err(bad("multiplexed Ogg streams are not supported"))
            }
            Some(_) => {}
        }
        if header_type & 0x01 == 0 && !partial.is_empty() {
            // A fresh (non-continued) page while a packet is pending: the previous page was
            // the last one of a packet that never completed. Drop the fragment.
            partial.clear();
        }

        let body = &hdr[header_len..page_len];
        let mut off = 0usize;
        let mut completed_on_page: Vec<usize> = Vec::new();
        for &seg in segments {
            let seg = seg as usize;
            partial.extend_from_slice(&body[off..off + seg]);
            off += seg;
            if seg < 255 {
                let data = std::mem::take(&mut partial);
                match logical_index {
                    0 => head = Some(parse_opus_head(&data)?),
                    1 => {
                        if !data.starts_with(b"OpusTags") {
                            return Err(bad("OpusTags header missing"));
                        }
                    }
                    _ => {
                        if data.len() > MAX_PACKET_BYTES {
                            return Err(bad("oversized Opus packet"));
                        }
                        if !data.is_empty() {
                            packets.push(OggPacket {
                                data,
                                page_granule: None,
                            });
                            completed_on_page.push(packets.len() - 1);
                        }
                    }
                }
                logical_index += 1;
            }
        }
        if granule != u64::MAX {
            if let Some(&last) = completed_on_page.last() {
                packets[last].page_granule = Some(granule);
            }
        }
        pos += page_len;
        if header_type & 0x04 != 0 {
            break;
        }
    }

    let head = head.ok_or_else(|| bad("OpusHead header missing"))?;
    Ok((head, packets))
}

fn parse_opus_head(data: &[u8]) -> Result<OpusHead> {
    if data.len() < 19 || !data.starts_with(b"OpusHead") {
        return Err(bad("OpusHead header missing"));
    }
    if data[8] >> 4 != 0 {
        return Err(bad("unsupported OpusHead version"));
    }
    let channels = data[9];
    if !(1..=2).contains(&channels) {
        return Err(bad("only mono and stereo Opus streams are supported"));
    }
    let pre_skip = u16::from_le_bytes([data[10], data[11]]);
    let granule_rate = u32::from_le_bytes(data[12..16].try_into().expect("4 bytes"));
    if !matches!(granule_rate, 8_000 | 12_000 | 16_000 | 24_000 | 48_000) {
        return Err(bad("unsupported OpusHead input sample rate"));
    }
    Ok(OpusHead {
        channels,
        pre_skip,
        granule_rate,
    })
}

fn bad(what: &str) -> AurixError {
    AurixError::Recording(format!("invalid Ogg/Opus recording: {what}"))
}

/// Streams the decoded PCM of one Ogg/Opus track at [`MIX_SAMPLE_RATE`], re-inserting the
/// silences the writer encoded as granule gaps, so the output is on the recording's own
/// timeline (sample `n` was captured `n / 48000` seconds after the first packet).
pub struct TrackDecoder {
    packets: std::vec::IntoIter<OggPacket>,
    decoder: opus::Decoder,
    out_channels: usize,
    granule_rate: u64,
    /// Samples (per channel) emitted so far on the track timeline.
    position: u64,
    /// Leftover interleaved samples from the previous packet.
    buffer: Vec<i16>,
    buffer_off: usize,
    /// Silence still owed before the next packet may play.
    gap_remaining: u64,
    exhausted: bool,
    scratch: Vec<i16>,
}

impl TrackDecoder {
    /// Decodes into `out_channels` (1 or 2) channels regardless of the stream layout — libopus
    /// down-mixes stereo streams to mono and duplicates mono into stereo.
    pub fn new(bytes: &[u8], out_channels: u8) -> Result<Self> {
        let (head, packets) = parse_ogg_opus(bytes)?;
        let channels = match out_channels {
            1 => opus::Channels::Mono,
            2 => opus::Channels::Stereo,
            _ => {
                return Err(AurixError::Recording(
                    "output channels must be 1 or 2".into(),
                ))
            }
        };
        let decoder = opus::Decoder::new(MIX_SAMPLE_RATE, channels)
            .map_err(|e| AurixError::Recording(format!("opus decoder: {e}")))?;
        Ok(Self {
            packets: packets.into_iter(),
            decoder,
            out_channels: out_channels as usize,
            granule_rate: u64::from(head.granule_rate),
            position: 0,
            buffer: Vec::new(),
            buffer_off: 0,
            gap_remaining: 0,
            exhausted: false,
            scratch: vec![0i16; 5760 * 2],
        })
    }

    /// Total length of the track in samples at 48 kHz, derived from the final granule.
    pub fn duration_samples(bytes: &[u8]) -> Result<u64> {
        let (head, packets) = parse_ogg_opus(bytes)?;
        let last = packets
            .iter()
            .rev()
            .find_map(|p| p.page_granule)
            .unwrap_or(0);
        Ok(last * u64::from(MIX_SAMPLE_RATE) / u64::from(head.granule_rate))
    }

    /// Fills `out` (interleaved, `out_channels` wide) with the next block. Returns `false`
    /// once the track is exhausted; the block is then all zeros.
    pub fn next_block(&mut self, out: &mut [i16]) -> bool {
        out.fill(0);
        let frames = out.len() / self.out_channels;
        let mut written = 0usize;
        let mut any = false;
        while written < frames {
            if self.gap_remaining > 0 {
                let n = (frames - written).min(self.gap_remaining as usize);
                self.gap_remaining -= n as u64;
                self.position += n as u64;
                written += n;
                any = true;
                continue;
            }
            if self.buffer_off < self.buffer.len() {
                let avail = (self.buffer.len() - self.buffer_off) / self.out_channels;
                let n = (frames - written).min(avail);
                let src = &self.buffer[self.buffer_off..self.buffer_off + n * self.out_channels];
                out[written * self.out_channels..(written + n) * self.out_channels]
                    .copy_from_slice(src);
                self.buffer_off += n * self.out_channels;
                self.position += n as u64;
                written += n;
                any = true;
                continue;
            }
            if self.exhausted || !self.pull_packet() {
                self.exhausted = true;
                break;
            }
        }
        any
    }

    fn pull_packet(&mut self) -> bool {
        let Some(packet) = self.packets.next() else {
            return false;
        };
        let decoded = match self.decoder.decode(&packet.data, &mut self.scratch, false) {
            Ok(n) => n,
            Err(e) => {
                tracing::debug!("skipping undecodable Opus packet: {e}");
                0
            }
        };
        self.buffer.clear();
        self.buffer
            .extend_from_slice(&self.scratch[..decoded * self.out_channels]);
        self.buffer_off = 0;
        if let Some(granule) = packet.page_granule {
            let end = granule * u64::from(MIX_SAMPLE_RATE) / self.granule_rate;
            let expected_end = self.position + decoded as u64;
            if end > expected_end {
                self.gap_remaining = (end - expected_end).min(MAX_GAP_SAMPLES);
            }
        }
        true
    }
}

/// Where mixed PCM blocks go.
pub trait MixSink {
    fn write_block(&mut self, interleaved: &[i16]) -> Result<()>;
    fn finish(&mut self) -> Result<()>;
}

/// Encodes mixed blocks as Ogg/Opus with the Aurix writer.
pub struct OpusSink<W: Write> {
    encoder: opus::Encoder,
    writer: OggOpusWriter<W>,
    packet: Vec<u8>,
}

impl<W: Write> OpusSink<W> {
    pub fn new(writer: W, channels: u8, bitrate_bps: u32, music: bool) -> Result<Self> {
        let ch = match channels {
            1 => opus::Channels::Mono,
            _ => opus::Channels::Stereo,
        };
        let app = if music {
            opus::Application::Audio
        } else {
            opus::Application::Voip
        };
        let mut encoder = opus::Encoder::new(MIX_SAMPLE_RATE, ch, app)
            .map_err(|e| AurixError::Recording(format!("opus encoder: {e}")))?;
        encoder
            .set_bitrate(opus::Bitrate::Bits(bitrate_bps.clamp(6_000, 510_000) as i32))
            .map_err(|e| AurixError::Recording(format!("opus encoder: {e}")))?;
        let writer = OggOpusWriter::new(writer, rand::random(), MIX_SAMPLE_RATE, channels)
            .map_err(|e| AurixError::Recording(format!("ogg writer: {e}")))?;
        Ok(Self {
            encoder,
            writer,
            packet: vec![0u8; 4000],
        })
    }
}

impl<W: Write> MixSink for OpusSink<W> {
    fn write_block(&mut self, interleaved: &[i16]) -> Result<()> {
        let n = self
            .encoder
            .encode(interleaved, &mut self.packet)
            .map_err(|e| AurixError::Recording(format!("opus encode: {e}")))?;
        self.writer
            .write_packet_with_duration(&self.packet[..n], BLOCK_SAMPLES as u64)
            .map_err(|e| AurixError::Recording(format!("ogg write: {e}")))?;
        Ok(())
    }

    fn finish(&mut self) -> Result<()> {
        self.writer
            .finish()
            .map_err(|e| AurixError::Recording(format!("ogg finish: {e}")))
    }
}

/// Writes 16-bit PCM WAV; the RIFF sizes are patched in on `finish`.
pub struct WavSink<W: Write + Seek> {
    writer: W,
    channels: u16,
    data_bytes: u64,
}

impl<W: Write + Seek> WavSink<W> {
    pub fn new(mut writer: W, channels: u8) -> Result<Self> {
        let channels = u16::from(channels.max(1));
        write_wav_header(&mut writer, MIX_SAMPLE_RATE, channels, 0)
            .map_err(|e| AurixError::Recording(format!("wav header: {e}")))?;
        Ok(Self {
            writer,
            channels,
            data_bytes: 0,
        })
    }
}

fn write_wav_header<W: Write>(
    w: &mut W,
    sample_rate: u32,
    channels: u16,
    data_bytes: u32,
) -> io::Result<()> {
    let block_align = channels * 2;
    w.write_all(b"RIFF")?;
    w.write_all(&(36 + data_bytes).to_le_bytes())?;
    w.write_all(b"WAVE")?;
    w.write_all(b"fmt ")?;
    w.write_all(&16u32.to_le_bytes())?;
    w.write_all(&1u16.to_le_bytes())?;
    w.write_all(&channels.to_le_bytes())?;
    w.write_all(&sample_rate.to_le_bytes())?;
    w.write_all(&(sample_rate * u32::from(block_align)).to_le_bytes())?;
    w.write_all(&block_align.to_le_bytes())?;
    w.write_all(&16u16.to_le_bytes())?;
    w.write_all(b"data")?;
    w.write_all(&data_bytes.to_le_bytes())?;
    Ok(())
}

impl<W: Write + Seek> MixSink for WavSink<W> {
    fn write_block(&mut self, interleaved: &[i16]) -> Result<()> {
        let mut bytes = Vec::with_capacity(interleaved.len() * 2);
        for s in interleaved {
            bytes.extend_from_slice(&s.to_le_bytes());
        }
        self.writer
            .write_all(&bytes)
            .map_err(|e| AurixError::Recording(format!("wav write: {e}")))?;
        self.data_bytes += bytes.len() as u64;
        Ok(())
    }

    fn finish(&mut self) -> Result<()> {
        if self.data_bytes > u64::from(u32::MAX - 36) {
            return Err(AurixError::Recording("WAV output exceeds 4 GiB".into()));
        }
        self.writer
            .seek(SeekFrom::Start(0))
            .and_then(|_| {
                write_wav_header(
                    &mut self.writer,
                    MIX_SAMPLE_RATE,
                    self.channels,
                    self.data_bytes as u32,
                )
            })
            .and_then(|_| self.writer.flush())
            .map_err(|e| AurixError::Recording(format!("wav finish: {e}")))
    }
}

/// One track placed on the mix timeline.
pub struct MixTrack {
    pub decoder: TrackDecoder,
    /// Silence prepended before the track starts, in samples at 48 kHz.
    pub offset_samples: u64,
    /// Linear gain applied to this track.
    pub gain: f32,
}

/// Sums `tracks` onto `sink` block by block, aligned by their offsets, through Opus' soft
/// clipper so overlapping speakers cannot wrap. Returns the mix length in samples.
pub fn mix_tracks(tracks: Vec<MixTrack>, channels: u8, sink: &mut dyn MixSink) -> Result<u64> {
    let ch = channels as usize;
    let block = BLOCK_SAMPLES * ch;
    let mut acc = vec![0f32; block];
    let mut scratch = vec![0i16; block];
    let mut out = vec![0i16; block];
    let mut clip = opus::SoftClip::new(if ch == 1 {
        opus::Channels::Mono
    } else {
        opus::Channels::Stereo
    });
    if tracks.iter().any(|t| t.decoder.out_channels != ch) {
        return Err(AurixError::Recording(
            "every track must be decoded with the mix channel count".into(),
        ));
    }
    let mut states: Vec<(MixTrack, bool)> = tracks.into_iter().map(|t| (t, true)).collect();
    let mut position: u64 = 0;
    let mut total: u64 = 0;

    loop {
        acc.fill(0.0);
        let mut live = false;
        for (track, active) in states.iter_mut() {
            if !*active {
                continue;
            }
            // Blocks are aligned to 20 ms; an offset inside a block shifts the track by up to
            // 10 ms, well below what a listener can perceive between speakers.
            let start_block = track.offset_samples / BLOCK_SAMPLES as u64;
            if position < start_block {
                live = true;
                continue;
            }
            if !track.decoder.next_block(&mut scratch) {
                *active = false;
                continue;
            }
            live = true;
            let g = track.gain;
            for (a, s) in acc.iter_mut().zip(scratch.iter()) {
                *a += f32::from(*s) * g / 32_768.0;
            }
        }
        if !live {
            break;
        }
        clip.apply(&mut acc);
        for (o, a) in out.iter_mut().zip(acc.iter()) {
            *o = (a * 32_767.0).round().clamp(-32_768.0, 32_767.0) as i16;
        }
        sink.write_block(&out)?;
        position += 1;
        total += BLOCK_SAMPLES as u64;
    }
    sink.finish()?;
    Ok(total)
}

/// Decodes a whole track to mono 16-bit PCM at `sample_rate` (8/12/16/24/48 kHz) for speech
/// recognition, gaps included, calling `segment` with consecutive chunks of at most
/// `max_samples` that are cut on silence where possible. The second argument is the chunk's
/// start offset on the track timeline, in milliseconds.
pub fn segment_for_stt(
    bytes: &[u8],
    sample_rate: u32,
    max_samples: usize,
    min_samples: usize,
    mut segment: impl FnMut(Vec<i16>, u64) -> Result<()>,
) -> Result<u64> {
    let mut dec = TrackDecoder::new(bytes, 1)?;
    let ratio = u64::from(MIX_SAMPLE_RATE) / u64::from(sample_rate.max(1));
    if ratio == 0 || ratio * u64::from(sample_rate) != u64::from(MIX_SAMPLE_RATE) {
        return Err(AurixError::Recording(
            "STT sample rate must divide 48000".into(),
        ));
    }
    let step = ratio as usize;
    let silence_gate = 32_768.0 * 0.01;
    // A window this long below the gate is a safe place to cut a segment.
    let quiet_run_needed = (sample_rate as usize) * 3 / 10;

    let mut block = vec![0i16; BLOCK_SAMPLES];
    let mut chunk: Vec<i16> = Vec::with_capacity(max_samples);
    let mut chunk_start_ms: u64 = 0;
    let mut quiet_run = 0usize;
    let mut emitted_samples: u64 = 0;
    let mut total_samples: u64 = 0;

    let mut flush = |chunk: &mut Vec<i16>, start_ms: u64, emitted: &mut u64| -> Result<()> {
        let taken = std::mem::take(chunk);
        *emitted += taken.len() as u64;
        let has_signal = taken.iter().any(|s| (*s as f32).abs() >= silence_gate);
        if taken.len() >= min_samples && has_signal {
            segment(taken, start_ms)?;
        }
        Ok(())
    };

    while dec.next_block(&mut block) {
        // Naive decimation is fine here: the decoder ran at 48 kHz on speech content whose
        // band was already limited by the uplink codec, and STT engines resample anyway.
        for frame in block.chunks(step) {
            let s = frame[0];
            let quiet = (s as f32).abs() < silence_gate;
            if chunk.is_empty() {
                if quiet {
                    total_samples += 1;
                    continue;
                }
                chunk_start_ms = total_samples * 1000 / u64::from(sample_rate);
            }
            chunk.push(s);
            total_samples += 1;
            if quiet {
                quiet_run += 1;
            } else {
                quiet_run = 0;
            }
            let long_enough = chunk.len() >= min_samples.max(sample_rate as usize);
            if chunk.len() >= max_samples || (long_enough && quiet_run >= quiet_run_needed) {
                flush(&mut chunk, chunk_start_ms, &mut emitted_samples)?;
                quiet_run = 0;
            }
        }
    }
    flush(&mut chunk, chunk_start_ms, &mut emitted_samples)?;
    Ok(total_samples * 1000 / u64::from(sample_rate))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn tone(hz: f32, frames: usize, channels: usize, amp: f32) -> Vec<i16> {
        (0..frames * channels)
            .map(|i| {
                let t = (i / channels) as f32 / MIX_SAMPLE_RATE as f32;
                ((t * hz * std::f32::consts::TAU).sin() * amp * 32_767.0) as i16
            })
            .collect()
    }

    /// Encodes `blocks` 20 ms frames of a tone; `gap_after` inserts a gap (in frames) after
    /// block `gap_at` via the granule position, the way the recorder preserves lost time.
    fn ogg_tone(hz: f32, blocks: usize, gap_at: Option<(usize, u64)>) -> Vec<u8> {
        let mut buf = Vec::new();
        let mut w = OggOpusWriter::new(&mut buf, 7, MIX_SAMPLE_RATE, 1).unwrap();
        let mut enc = opus::Encoder::new(
            MIX_SAMPLE_RATE,
            opus::Channels::Mono,
            opus::Application::Voip,
        )
        .unwrap();
        let pcm = tone(hz, BLOCK_SAMPLES * blocks, 1, 0.5);
        let mut out = vec![0u8; 4000];
        for (i, frame) in pcm.chunks(BLOCK_SAMPLES).enumerate() {
            let n = enc.encode(frame, &mut out).unwrap();
            let dur = match gap_at {
                Some((at, gap)) if at == i => BLOCK_SAMPLES as u64 * (gap + 1),
                _ => BLOCK_SAMPLES as u64,
            };
            w.write_packet_with_duration(&out[..n], dur).unwrap();
        }
        w.finish().unwrap();
        drop(w);
        buf
    }

    fn rms(pcm: &[i16]) -> f32 {
        if pcm.is_empty() {
            return 0.0;
        }
        (pcm.iter()
            .map(|s| (*s as f32 / 32_768.0).powi(2))
            .sum::<f32>()
            / pcm.len() as f32)
            .sqrt()
    }

    #[test]
    fn parses_own_writer_output_with_granules() {
        let bytes = ogg_tone(440.0, 10, None);
        let (head, packets) = parse_ogg_opus(&bytes).unwrap();
        assert_eq!(head.channels, 1);
        assert_eq!(head.granule_rate, 48_000);
        assert_eq!(packets.len(), 10);
        assert_eq!(packets[9].page_granule, Some(9600));
        assert_eq!(TrackDecoder::duration_samples(&bytes).unwrap(), 9600);
    }

    #[test]
    fn rejects_corrupt_pages() {
        let mut bytes = ogg_tone(440.0, 3, None);
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF;
        assert!(parse_ogg_opus(&bytes).is_err());
        assert!(parse_ogg_opus(b"not ogg at all").is_err());
    }

    #[test]
    fn decoder_restores_gaps_as_silence_on_the_timeline() {
        // 5 frames, then a 3-frame hole after frame 1, then the rest.
        let bytes = ogg_tone(440.0, 5, Some((2, 3)));
        let mut dec = TrackDecoder::new(&bytes, 1).unwrap();
        let mut blocks = Vec::new();
        let mut b = vec![0i16; BLOCK_SAMPLES];
        while dec.next_block(&mut b) {
            blocks.push(rms(&b));
        }
        assert_eq!(blocks.len(), 8, "5 audio + 3 silent blocks");
        assert!(blocks[1] > 0.2, "audio before the gap: {blocks:?}");
        assert!(
            blocks[2] < 0.001 && blocks[3] < 0.001 && blocks[4] < 0.001,
            "gap is silent: {blocks:?}"
        );
        assert!(
            blocks[5] > 0.2 && blocks[7] > 0.2,
            "audio resumes after the gap"
        );
    }

    #[test]
    fn mixes_two_offset_tracks_into_wav_and_opus() {
        let a = ogg_tone(440.0, 10, None);
        let b = ogg_tone(660.0, 10, None);
        let tracks = |ch: u8| {
            vec![
                MixTrack {
                    decoder: TrackDecoder::new(&a, ch).unwrap(),
                    offset_samples: 0,
                    gain: 1.0,
                },
                MixTrack {
                    decoder: TrackDecoder::new(&b, ch).unwrap(),
                    offset_samples: 5 * BLOCK_SAMPLES as u64,
                    gain: 1.0,
                },
            ]
        };
        let mut wav = Cursor::new(Vec::new());
        let mut sink = WavSink::new(&mut wav, 1).unwrap();
        let total = mix_tracks(tracks(1), 1, &mut sink).unwrap();
        assert_eq!(total, 15 * BLOCK_SAMPLES as u64, "B starts 100 ms late");
        let parsed = aurix_common::tts_stt::parse_wav(wav.get_ref()).unwrap();
        assert_eq!(parsed.sample_rate, 48_000);
        assert_eq!(parsed.channels, 1);
        assert_eq!(parsed.samples.len() as u64, total);
        let first = rms(&parsed.samples[..5 * BLOCK_SAMPLES]);
        let overlap = rms(&parsed.samples[5 * BLOCK_SAMPLES..10 * BLOCK_SAMPLES]);
        let tail = rms(&parsed.samples[10 * BLOCK_SAMPLES..]);
        assert!((first - 0.35).abs() < 0.05, "A alone ≈ 0.5/√2: {first}");
        assert!(
            overlap > first * 1.2,
            "both speakers louder than one: {overlap}"
        );
        assert!((tail - 0.35).abs() < 0.05, "B alone: {tail}");

        let mut ogg = Vec::new();
        let mut sink = OpusSink::new(&mut ogg, 2, 96_000, false).unwrap();
        assert!(mix_tracks(tracks(1), 2, &mut sink).is_err());
        let total = mix_tracks(tracks(2), 2, &mut sink).unwrap();
        drop(sink);
        assert_eq!(total, 15 * BLOCK_SAMPLES as u64);
        let (head, packets) = parse_ogg_opus(&ogg).unwrap();
        assert_eq!(head.channels, 2);
        assert_eq!(packets.len(), 15);
        let mut dec = TrackDecoder::new(&ogg, 2).unwrap();
        let mut block = vec![0i16; BLOCK_SAMPLES * 2];
        let mut n = 0;
        while dec.next_block(&mut block) {
            n += 1;
        }
        assert_eq!(n, 15, "the mix decodes back to the same length");
    }

    #[test]
    fn stt_segmentation_cuts_on_silence_and_reports_offsets() {
        // 1.0 s tone, 1.0 s hole, 1.0 s tone → two segments, the second starting at 2 s.
        let bytes = ogg_tone(440.0, 100, Some((50, 50)));
        let mut segments = Vec::new();
        let total_ms = segment_for_stt(&bytes, 16_000, 16_000 * 30, 16_000 / 2, |pcm, at| {
            segments.push((pcm.len(), at, rms(&pcm)));
            Ok(())
        })
        .unwrap();
        assert_eq!(total_ms, 3000);
        assert_eq!(segments.len(), 2, "{segments:?}");
        assert!(segments[0].1 <= 20, "{segments:?}");
        assert!(
            (1900..=2100).contains(&segments[1].1),
            "second segment starts after the hole: {segments:?}"
        );
        assert!(segments.iter().all(|s| s.2 > 0.1));

        let mut chunks = 0;
        segment_for_stt(&bytes, 16_000, 16_000 / 4, 1, |_, _| {
            chunks += 1;
            Ok(())
        })
        .unwrap();
        assert!(chunks >= 8, "max_samples caps the chunk length: {chunks}");
    }
}
