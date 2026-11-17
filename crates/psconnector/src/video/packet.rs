//! Video packets: parsing and putting units back together into frames.

#[allow(unused_imports)]
use super::*;

pub struct VideoPacket<'a> {
    pub frame: u16,
    pub unit: u16,
    pub total: u16,
    pub fec: u16,
    pub key_pos: u64,
    pub data: &'a [u8],
}

impl VideoPacket<'_> {
    pub fn src_units(&self) -> u16 {
        self.total - self.fec
    }
}

pub fn parse(buf: &[u8]) -> Option<VideoPacket<'_>> {
    if buf.len() < 21 || buf[0] & 0x0F != 0x02 {
        return None;
    }
    let has_nalu = (buf[0] >> 4) & 1 != 0;
    let frame = u16::from_be_bytes([buf[3], buf[4]]);
    let dword2 = u32::from_be_bytes(buf[5..9].try_into().unwrap());
    let key_pos = u32::from_be_bytes(buf[14..18].try_into().unwrap()) as u64;
    let offset = 18 + 3 + if has_nalu { 3 } else { 0 };
    let total = (((dword2 >> 10) & 0x7FF) + 1) as u16;
    let fec = (dword2 & 0x3FF) as u16;
    (buf.len() >= offset && fec < total).then(|| VideoPacket {
        frame,
        unit: ((dword2 >> 21) & 0x7FF) as u16,
        total,
        fec,
        key_pos,
        data: &buf[offset..],
    })
}

struct Pending {
    src: u16,
    /// source units first, then FEC units
    units: Vec<Option<Vec<u8>>>,
    have_src: u16,
    have_all: u16,
}

/// FEC use, logged every 10 s.
#[derive(Default)]
struct FecStats {
    since: Option<std::time::Instant>,
    recovered: u32,
    units: u32,
}

/// Collects units into frames. Units may arrive out of order.
pub struct Assembler {
    header: Vec<u8>,
    pending: BTreeMap<u16, Pending>,
    last_done: Option<u16>,
    /// Units in a row that looked older than the last finished frame
    stale_run: u32,
    fec: FecStats,
}

/// That many "old" units in a row means the console restarted its frame numbering (it does so when it
/// changes the stream, e.g. after adapting to the network): start following the new numbers.
const RESYNC_AFTER: u32 = 120;

/// Diagnostics: PSCONNECTOR_NO_FEC=1 ignores FEC units (to compare with recovery on).
fn fec_disabled() -> bool {
    static OFF: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *OFF.get_or_init(|| std::env::var_os("PSCONNECTOR_NO_FEC").is_some())
}

/// Units this many frames behind the last finished one are just late, not a sign of a restart.
const RECENT_FRAMES: u16 = 64;

pub enum Assembled {
    /// A complete H.264 frame
    Frame(u16, Vec<u8>),
    /// Frames in start..=end were lost. A key frame should be requested.
    Lost(u16, u16),
}

impl Assembler {
    fn report_fec(&mut self) {
        let since = *self.fec.since.get_or_insert_with(std::time::Instant::now);
        if since.elapsed() >= std::time::Duration::from_secs(10) {
            if self.fec.recovered > 0 {
                log::info!(
                    "video: FEC rebuilt {} lost units in {} frames in 10 s (no key frame needed)",
                    self.fec.units,
                    self.fec.recovered
                );
            }
            self.fec = FecStats { since: Some(std::time::Instant::now()), ..FecStats::default() };
        }
    }

    pub fn new(header: Vec<u8>) -> Self {
        Self { header, pending: BTreeMap::new(), last_done: None, stale_run: 0, fec: FecStats::default() }
    }

    /// Adds one decrypted unit (`unit` counts source units first, then FEC units, up to `total`).
    pub fn push(&mut self, frame: u16, unit: u16, src: u16, total: u16, data: Vec<u8>, out: &mut Vec<Assembled>) {
        if unit >= total || src == 0 || src > total {
            return;
        }
        if let Some(done) = self.last_done {
            // already finished or older (u16 wrap-around aware)
            let behind = done.wrapping_sub(frame);
            if behind < 0x8000 {
                // late units of recent frames (FEC units after a frame is complete, say) are normal
                if behind <= RECENT_FRAMES {
                    return;
                }
                self.stale_run += 1;
                if self.stale_run < RESYNC_AFTER {
                    return;
                }
                log::info!("video frame numbers restarted ({done} → {frame}); following them");
                self.last_done = None;
                self.pending.clear();
            }
        }
        self.stale_run = 0;
        let p = self.pending.entry(frame).or_insert_with(|| Pending {
            src,
            units: vec![None; total as usize],
            have_src: 0,
            have_all: 0,
        });
        if p.units.len() != total as usize || p.src != src {
            return; // inconsistent unit info
        }
        let slot = &mut p.units[unit as usize];
        if slot.is_none() {
            *slot = Some(data);
            p.have_all += 1;
            if unit < src {
                p.have_src += 1;
            }
        }
        // complete, or enough units to rebuild the missing source units from the FEC units
        let fec_ok = !fec_disabled() && p.have_all >= p.src;
        if p.have_src < p.src && !fec_ok {
            return;
        }
        let p = self.pending.remove(&frame).unwrap();
        let k = p.src as usize;
        let sources: Vec<Vec<u8>> = if p.have_src == p.src {
            p.units.into_iter().take(k).flatten().collect()
        } else {
            let len = p.units.iter().flatten().map(Vec::len).max().unwrap_or(0);
            let padded: Vec<Option<Vec<u8>>> = p
                .units
                .iter()
                .map(|u| {
                    u.as_ref().map(|u| {
                        let mut v = u.clone();
                        v.resize(len, 0);
                        v
                    })
                })
                .collect();
            let missing: Vec<usize> = (0..k).filter(|&i| p.units[i].is_none()).collect();
            let Some(mut rebuilt) = super::fec::recover(&padded, k, len) else { return };
            // rebuilt units are full length; their own padding field says how much of the end is filler
            for &i in &missing {
                let u = &mut rebuilt[i];
                let padding = u.get(..2).map_or(0, |b| u16::from_be_bytes([b[0], b[1]]) as usize);
                if padding > 0 && padding + 2 <= u.len() {
                    u.truncate(u.len() - padding);
                }
            }
            self.fec.recovered += 1;
            self.fec.units += missing.len() as u32;
            rebuilt
        };
        self.report_fec();
        // unfinished frames older than this one are treated as lost
        let older: Vec<u16> = self.pending.keys().copied().filter(|&f| frame.wrapping_sub(f) < 0x8000).collect();
        for f in &older {
            self.pending.remove(f);
        }
        let expected = self.last_done.map(|d| d.wrapping_add(1)).unwrap_or(frame);
        if expected != frame {
            out.push(Assembled::Lost(expected, frame.wrapping_sub(1)));
        }
        self.last_done = Some(frame);
        let mut h264 = self.header.clone();
        for unit in sources {
            // the first 2 bytes of a unit are the padding size (unit size minus data size)
            h264.extend_from_slice(unit.get(2..).unwrap_or_default());
        }
        out.push(Assembled::Frame(frame, h264));
    }
}
