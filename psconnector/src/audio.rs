//! Audio: Opus packets from the console → 16-bit PCM.
//!
//! The PS5 sends one packet every 10 ms. Each packet holds `src` Opus frames (the new audio) plus `fec`
//! copies of the frames just before it, all `unit_size` bytes long. A lost packet is therefore usually
//! covered by the copies in the next one; anything still missing is filled by Opus packet loss concealment.
//! Decoding runs on its own thread, like video.

use std::sync::mpsc::{Receiver, Sender, SyncSender, TrySendError};

use log::{debug, warn};
use opus_decoder::OpusDecoder;

/// Stream audio format, from the STREAMINFO audio header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioConfig {
    pub channels: u16,
    pub rate: u32,
    /// Samples per channel in one frame (480 = 10 ms at 48 kHz)
    pub frame_size: u32,
}

impl AudioConfig {
    /// `channels(1) bits(1) rate(4, BE) frame_size(4, BE) ...`
    pub(crate) fn parse(h: &[u8]) -> Option<Self> {
        if h.len() < 10 {
            return None;
        }
        let rate = u32::from_be_bytes(h[2..6].try_into().ok()?);
        let frame_size = u32::from_be_bytes(h[6..10].try_into().ok()?);
        (h[0] > 0 && rate > 0 && frame_size > 0).then_some(Self { channels: h[0].into(), rate, frame_size })
    }
}

/// A chunk of decoded audio: interleaved 16-bit samples.
#[derive(Clone, Debug)]
pub struct Audio {
    pub rate: u32,
    pub channels: u16,
    pub samples: Vec<i16>,
}

pub(crate) struct AudioPacket<'a> {
    pub frame: u16,
    pub src: u8,
    pub fec: u8,
    pub unit_size: usize,
    pub key_pos: u64,
    /// Encrypted units, `unit_size * (src + fec)` bytes
    pub data: &'a [u8],
}

/// Parses a PS5 audio packet (Takion type 3).
pub(crate) fn parse(buf: &[u8]) -> Option<AudioPacket<'_>> {
    if buf.len() < 20 || buf[0] & 0x0F != 0x03 {
        return None;
    }
    let has_nalu = (buf[0] >> 4) & 1 != 0;
    let frame = u16::from_be_bytes([buf[3], buf[4]]);
    let dword2 = u32::from_be_bytes(buf[5..9].try_into().ok()?);
    let key_pos = u32::from_be_bytes(buf[14..18].try_into().ok()?) as u64;
    // 18-byte header + 1 + (3 when it carries a NALU) + 1 extra byte on PS5
    let offset = 18 + 1 + if has_nalu { 3 } else { 0 } + 1;
    let src = (dword2 & 0x0F) as u8;
    let fec = ((dword2 >> 4) & 0x0F) as u8;
    let unit_size = ((dword2 >> 8) & 0xFF) as usize;
    let data = buf.get(offset..)?;
    let need = unit_size * (usize::from(src) + usize::from(fec));
    (src > 0 && unit_size > 0 && data.len() >= need).then(|| AudioPacket {
        frame,
        src,
        fec,
        unit_size,
        key_pos,
        data: &data[..need],
    })
}

/// Longest gap filled with concealment; a bigger jump just restarts playback.
const MAX_CONCEAL: u16 = 10;

/// Puts Opus frames back in order, using the copies to fill gaps.
#[derive(Default)]
pub(crate) struct Reorder {
    last: Option<u16>,
}

impl Reorder {
    /// Takes one decrypted packet and returns the frames to decode next, oldest first.
    /// `None` means a frame that was lost (decode it with concealment).
    pub fn push(&mut self, frame: u16, src: u8, fec: u8, unit_size: usize, data: &[u8]) -> Vec<Option<Vec<u8>>> {
        let unit = |i: usize| data[i * unit_size..(i + 1) * unit_size].to_vec();
        // copies of the frames before this one come after the new frames in the packet
        let copies =
            (0..fec).map(|j| (frame.wrapping_sub(u16::from(fec)).wrapping_add(u16::from(j)), usize::from(src + j)));
        let new = (0..src).map(|i| (frame.wrapping_add(u16::from(i)), usize::from(i)));
        let mut out = Vec::new();
        for (index, at) in copies.chain(new) {
            let Some(last) = self.last else {
                // starting: copies of frames from before the stream began are not real audio
                if at < usize::from(src) {
                    self.last = Some(index);
                    out.push(Some(unit(at)));
                }
                continue;
            };
            let ahead = index.wrapping_sub(last);
            if ahead == 0 || ahead > u16::MAX / 2 {
                continue; // already played (or older)
            }
            if ahead > 1 && ahead - 1 <= MAX_CONCEAL {
                out.extend(std::iter::repeat_n(None, usize::from(ahead - 1)));
            }
            self.last = Some(index);
            out.push(Some(unit(at)));
        }
        out
    }
}

pub(crate) enum AudioMsg {
    Config(AudioConfig),
    Frame(Option<Vec<u8>>),
}

pub(crate) type AudioTx = Sender<AudioMsg>;

/// Starts the decoder thread. Decoded chunks go to `out`; if nobody reads them they are dropped.
pub(crate) fn spawn_decoder(rx: Receiver<AudioMsg>, out: SyncSender<Audio>) {
    let spawned = std::thread::Builder::new().name("psconnector-audio".into()).spawn(move || {
        let mut dec: Option<(AudioConfig, OpusDecoder)> = None;
        let mut pcm = Vec::new();
        while let Ok(msg) = rx.recv() {
            match msg {
                AudioMsg::Config(cfg) => {
                    if dec.as_ref().is_some_and(|(c, _)| *c == cfg) {
                        continue;
                    }
                    match OpusDecoder::new(cfg.rate, cfg.channels.into()) {
                        Ok(d) => {
                            debug!(
                                "audio: {} Hz, {} channels, {} samples per frame",
                                cfg.rate, cfg.channels, cfg.frame_size
                            );
                            dec = Some((cfg, d));
                        }
                        Err(e) => warn!("cannot create the Opus decoder: {e:?}"),
                    }
                }
                AudioMsg::Frame(frame) => {
                    let Some((cfg, d)) = dec.as_mut() else { continue };
                    // room for the longest Opus frame (120 ms)
                    pcm.resize(cfg.rate as usize / 1000 * 120 * usize::from(cfg.channels), 0);
                    let n = match d.decode(frame.as_deref().unwrap_or(&[]), &mut pcm, false) {
                        Ok(n) => n,
                        Err(e) => {
                            debug!("failed to decode an audio frame: {e:?}");
                            continue;
                        }
                    };
                    let chunk = Audio {
                        rate: cfg.rate,
                        channels: cfg.channels,
                        samples: pcm[..n * usize::from(cfg.channels)].to_vec(),
                    };
                    if let Err(TrySendError::Disconnected(_)) = out.try_send(chunk) {
                        return;
                    }
                }
            }
        }
    });
    if let Err(e) = spawned {
        warn!("cannot start the audio thread: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_and_packet_layout() {
        let cfg = AudioConfig::parse(&[2, 16, 0, 0, 0xBB, 0x80, 0, 0, 1, 0xE0, 0, 0, 0, 1]).unwrap();
        assert_eq!(cfg, AudioConfig { channels: 2, rate: 48_000, frame_size: 480 });

        let mut buf = vec![0x03, 0, 7, 0, 8, 0x00, 0x02, 0x50, 0x21, 5, 0, 0, 0, 0, 0, 0, 0x01, 0x30, 0, 0];
        buf.extend(std::iter::repeat_n(0xAA, 240));
        let p = parse(&buf).unwrap();
        assert_eq!((p.frame, p.src, p.fec, p.unit_size, p.key_pos, p.data.len()), (8, 1, 2, 80, 0x130, 240));
    }

    #[test]
    fn reorder_fills_a_lost_packet_from_the_next_ones_copies() {
        let mut r = Reorder::default();
        // packet layout: [new frame, copy of frame-2, copy of frame-1]
        let p = |f: u8| vec![f, f.wrapping_sub(2), f.wrapping_sub(1)];
        assert_eq!(r.push(1, 1, 2, 1, &p(1)), vec![Some(vec![1])], "copies before the start are skipped");
        assert_eq!(r.push(2, 1, 2, 1, &p(2)), vec![Some(vec![2])]);
        // packet 3 is lost; packet 4 brings a copy of 3
        assert_eq!(r.push(4, 1, 2, 1, &p(4)), vec![Some(vec![3]), Some(vec![4])]);
        // packets 5..=7 lost; 8 only has copies of 6 and 7, so 5 is concealed
        assert_eq!(r.push(8, 1, 2, 1, &p(8)), vec![None, Some(vec![6]), Some(vec![7]), Some(vec![8])]);
        // a late duplicate is ignored
        assert_eq!(r.push(8, 1, 2, 1, &p(8)), Vec::<Option<Vec<u8>>>::new());
    }
}
