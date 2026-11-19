//! The screen video as the phone sends it: HEVC in RTP (RFC 7798), payload
//! type 100, with the parameter sets in band and a single keyframe at the
//! start of each stream.

use idevice::core_device::RtpPacket;
use shiguredo_mp4::{
    bitstream::h265::{H265ConstantFrameRate, H265SampleEntryConfig, LengthSize, build_hvc1_box},
    boxes::Hvc1Box,
};

pub const PAYLOAD_TYPE: u8 = 100;

/// The RTP timestamps count 24 000 per second. Measured against the sender
/// reports' wall clock on an iPhone (iOS 27.2.1); not the 90 kHz of most
/// video RTP.
pub const RTP_CLOCK: u32 = 24_000;

const NAL_AP: u8 = 48;
const NAL_FU: u8 = 49;
pub const NAL_VPS: u8 = 32;
pub const NAL_SPS: u8 = 33;
pub const NAL_PPS: u8 = 34;
const NAL_AUD: u8 = 35;

pub fn nal_type(first: u8) -> u8 {
    (first >> 1) & 0x3f
}

/// One frame: the NAL units sent under one RTP timestamp.
#[derive(Debug, Default)]
pub struct AccessUnit {
    pub ts: u32,
    /// Microseconds since the Unix epoch when its first packet came.
    pub arrival_us: u64,
    pub nals: Vec<Vec<u8>>,
    /// Packets lost before its first packet ([`Assembler::lost`] then).
    pub lost_before: u64,
}

impl AccessUnit {
    /// Random access point: decodable without the frames before it.
    pub fn is_keyframe(&self) -> bool {
        self.nals.iter().any(|n| (16..=23).contains(&nal_type(n[0])))
    }

    /// Whether a slice of the picture came: types below 32 are VCL NALs.
    pub fn has_picture(&self) -> bool {
        self.nals.iter().any(|n| nal_type(n[0]) < 32)
    }

    pub fn parameter_set(&self, kind: u8) -> Option<&[u8]> {
        self.nals.iter().find(|n| nal_type(n[0]) == kind).map(|n| n.as_slice())
    }

    /// The frame as an MP4 sample: each NAL with a 4-byte length, without
    /// the parameter sets and delimiters (those live in the sample entry).
    pub fn sample(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for n in self.nals.iter().filter(|n| !(NAL_VPS..=NAL_AUD).contains(&nal_type(n[0]))) {
            out.extend_from_slice(&(n.len() as u32).to_be_bytes());
            out.extend_from_slice(n);
        }
        out
    }
}

/// Puts frames back together from datagrams in arrival order. A gap in the
/// sequence numbers is counted as lost packets; the fragment it cut is
/// dropped and the frame goes on without it.
#[derive(Debug, Default)]
pub struct Assembler {
    current: Option<AccessUnit>,
    fragment: Option<Vec<u8>>,
    last_seq: Option<u16>,
    pub lost: u64,
    /// Timestamp of the first frame a lost packet damaged: the frame being
    /// received at the gap, or the one after it when the gap fell between
    /// frames.
    pub first_loss_ts: Option<u32>,
}

impl Assembler {
    /// Feed one datagram. Returns the frames it completed, in order: a
    /// packet with a new timestamp ends the frame before it, and the RTP
    /// marker ends its own.
    pub fn push(&mut self, datagram: &[u8], arrival_us: u64) -> Vec<AccessUnit> {
        let mut done = Vec::new();
        let Some(packet) = RtpPacket::parse(datagram) else { return done };
        if packet.payload_type != PAYLOAD_TYPE || packet.payload.len() < 2 {
            return done;
        }
        let seq = packet.sequence_number;
        if let Some(prev) = self.last_seq {
            let delta = seq.wrapping_sub(prev);
            if delta == 0 || delta > 0x8000 {
                // A duplicate, or one overtaken by later packets.
                return done;
            }
            if delta > 1 {
                self.lost += u64::from(delta - 1);
                self.fragment = None;
                let damaged = self.current.as_ref().map_or(packet.timestamp, |au| au.ts);
                self.first_loss_ts.get_or_insert(damaged);
            }
        }
        self.last_seq = Some(seq);

        if self.current.as_ref().is_some_and(|au| au.ts != packet.timestamp) {
            done.extend(self.current.take());
            self.fragment = None;
        }
        let lost_before = self.lost;
        let au = self.current.get_or_insert_with(|| AccessUnit {
            ts: packet.timestamp,
            arrival_us,
            nals: Vec::new(),
            lost_before,
        });
        let p = packet.payload;
        match nal_type(p[0]) {
            NAL_AP => {
                let mut off = 2;
                while let Some(len) = p.get(off..off + 2).map(|b| u16::from_be_bytes([b[0], b[1]]) as usize) {
                    off += 2;
                    let Some(nal) = p.get(off..off + len).filter(|n| n.len() >= 2) else { break };
                    au.nals.push(nal.to_vec());
                    off += len;
                }
            }
            NAL_FU if p.len() >= 3 => {
                let header = p[2];
                if header & 0x80 != 0 {
                    let mut nal = vec![(p[0] & 0x81) | ((header & 0x3f) << 1), p[1]];
                    nal.extend_from_slice(&p[3..]);
                    self.fragment = Some(nal);
                } else if let Some(nal) = self.fragment.as_mut() {
                    nal.extend_from_slice(&p[3..]);
                }
                if header & 0x40 != 0
                    && let Some(nal) = self.fragment.take()
                {
                    au.nals.push(nal);
                }
            }
            NAL_FU => {}
            _ => au.nals.push(p.to_vec()),
        }
        if packet.marker {
            done.extend(self.current.take());
        }
        done
    }

    /// Forget the packets lost before the first packet of `au`, the frame
    /// a file starts at: the frames they damaged are not in it. Any lost
    /// since then hit `au` itself.
    pub fn start_at(&mut self, au: &AccessUnit) {
        self.lost = self.lost.saturating_sub(au.lost_before);
        self.first_loss_ts = (self.lost > 0).then_some(au.ts);
    }
}

/// The parameter sets of one stream, as the sample entry carries them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParameterSets {
    pub vps: Vec<u8>,
    pub sps: Vec<u8>,
    pub pps: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// The `hvc1` entry whose `hvcC` carries the three sets.
    pub entry: Hvc1Box,
}

impl ParameterSets {
    /// From the stream's first keyframe, which carries all three.
    pub fn from_keyframe(au: &AccessUnit) -> Option<Self> {
        let (vps, sps, pps) = (au.parameter_set(NAL_VPS)?, au.parameter_set(NAL_SPS)?, au.parameter_set(NAL_PPS)?);
        let config = H265SampleEntryConfig {
            length_size: LengthSize::FourBytes,
            avg_frame_rate: H265SampleEntryConfig::AVG_FRAME_RATE_UNSPECIFIED,
            constant_frame_rate: H265ConstantFrameRate::Unknown,
        };
        let entry = build_hvc1_box(&[vps.to_vec()], &[sps.to_vec()], &[pps.to_vec()], &config).ok()?;
        Some(Self {
            vps: vps.to_vec(),
            sps: sps.to_vec(),
            pps: pps.to_vec(),
            width: entry.visual.width.into(),
            height: entry.visual.height.into(),
            entry,
        })
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// Writes a bit string, for building parameter sets in tests.
    #[derive(Default)]
    pub struct BitWriter {
        bytes: Vec<u8>,
        n: usize,
    }

    impl BitWriter {
        pub fn bits(&mut self, value: u32, count: u32) -> &mut Self {
            for i in (0..count).rev() {
                if self.n.is_multiple_of(8) {
                    self.bytes.push(0);
                }
                if (value >> i) & 1 == 1 {
                    *self.bytes.last_mut().unwrap() |= 0x80 >> (self.n % 8);
                }
                self.n += 1;
            }
            self
        }

        pub fn ue(&mut self, v: u32) -> &mut Self {
            let x = v + 1;
            let len = 32 - x.leading_zeros();
            self.bits(0, len - 1).bits(x, len)
        }

        pub fn finish(&mut self) -> Vec<u8> {
            self.bits(1, 1);
            while !self.n.is_multiple_of(8) {
                self.bits(0, 1);
            }
            self.bytes.clone()
        }
    }

    /// A main-profile 4:2:0 8-bit SPS NAL for a picture of this size.
    pub fn sps_nal(width: u32, height: u32) -> Vec<u8> {
        let mut w = BitWriter::default();
        w.bits(0, 4).bits(0, 3).bits(1, 1);
        // general profile: space 0, tier 0, idc 1; compatibility; flags; level 120.
        w.bits(1, 8).bits(0x6000_0000, 32).bits(0, 32).bits(0, 16).bits(120, 8);
        w.ue(0).ue(1).ue(width).ue(height).bits(0, 1).ue(0).ue(0);
        let mut nal = vec![(NAL_SPS << 1), 1];
        nal.extend(w.finish());
        nal
    }

    fn rtp(seq: u16, ts: u32, marker: bool, payload: &[u8]) -> Vec<u8> {
        let mut d = vec![0x80, PAYLOAD_TYPE | if marker { 0x80 } else { 0 }];
        d.extend_from_slice(&seq.to_be_bytes());
        d.extend_from_slice(&ts.to_be_bytes());
        d.extend_from_slice(&0x1234u32.to_be_bytes());
        d.extend_from_slice(payload);
        d
    }

    pub fn keyframe_with(sps: Vec<u8>) -> AccessUnit {
        AccessUnit { nals: vec![vec![NAL_VPS << 1, 1, 0xaa], sps, vec![NAL_PPS << 1, 1, 0xbb], vec![19 << 1, 1, 5]], ..Default::default() }
    }

    #[test]
    fn the_parameter_sets_give_the_picture_size() {
        let p = ParameterSets::from_keyframe(&keyframe_with(sps_nal(1408, 2016))).unwrap();
        assert_eq!((p.width, p.height), (1408, 2016));
        assert_eq!(ParameterSets::from_keyframe(&keyframe_with(vec![0x42, 0x01, 0x01])), None, "a cut SPS is refused, not read past");
    }

    #[test]
    fn frames_come_together_from_single_aggregated_and_fragmented_packets() {
        let mut a = Assembler::default();
        let vps = [(NAL_VPS << 1), 1, 0xaa];
        let pps = [(NAL_PPS << 1), 1, 0xbb];
        let mut ap = vec![(NAL_AP << 1), 1];
        for nal in [&vps[..], &pps[..]] {
            ap.extend_from_slice(&(nal.len() as u16).to_be_bytes());
            ap.extend_from_slice(nal);
        }
        assert!(a.push(&rtp(1, 100, false, &ap), 1).is_empty());
        // An IDR slice (type 19) in two fragments.
        let fu = |start: bool, end: bool, data: &[u8]| {
            let mut p = vec![(NAL_FU << 1), 1, 19 | if start { 0x80 } else { 0 } | if end { 0x40 } else { 0 }];
            p.extend_from_slice(data);
            p
        };
        assert!(a.push(&rtp(2, 100, false, &fu(true, false, &[1, 2])), 2).is_empty());
        let au = a.push(&rtp(3, 100, true, &fu(false, true, &[3])), 3).pop().unwrap();
        assert_eq!(au.nals.len(), 3);
        assert_eq!(au.nals[2], vec![19 << 1, 1, 1, 2, 3]);
        assert!(au.is_keyframe());
        assert_eq!(au.sample().len(), 4 + 5, "parameter sets stay out of the sample");
        assert_eq!(a.lost, 0);
    }

    #[test]
    fn a_gap_counts_lost_packets_and_drops_the_cut_fragment() {
        let mut a = Assembler::default();
        let start = [(NAL_FU << 1), 1, 0x80 | 1, 9];
        let end = [(NAL_FU << 1), 1, 0x40 | 1, 9];
        a.push(&rtp(10, 500, false, &start), 0);
        let au = a.push(&rtp(13, 500, true, &end), 0).pop().unwrap();
        assert_eq!(a.lost, 2);
        assert!(au.nals.is_empty());
        assert_eq!(a.first_loss_ts, Some(500), "the gap cut the frame being received");
        // A late packet from before is ignored, not counted again.
        assert!(a.push(&rtp(11, 500, true, &[2, 1, 7]), 0).is_empty());
        assert_eq!(a.lost, 2);
        // A new timestamp without a marker closes the previous frame too.
        a.push(&rtp(14, 900, false, &[2, 1, 7]), 0);
        let done = a.push(&rtp(15, 1300, false, &[2, 1, 8]), 0);
        assert_eq!(done.iter().map(|au| au.ts).collect::<Vec<_>>(), vec![900]);
        assert!(a.push(&[0x80, 0x48, 0, 1], 0).is_empty(), "short datagrams are skipped");
        assert_eq!(a.first_loss_ts, Some(500), "a later loss does not move the first");
    }

    #[test]
    fn a_whole_frame_after_an_unfinished_one_completes_both() {
        let mut a = Assembler::default();
        // The frame at 100 loses its last packet, the one with the marker.
        assert!(a.push(&rtp(1, 100, false, &[2, 1, 7]), 0).is_empty());
        let done = a.push(&rtp(3, 500, true, &[2, 1, 8]), 10);
        assert_eq!(done.iter().map(|au| (au.ts, au.arrival_us)).collect::<Vec<_>>(), vec![(100, 0), (500, 10)]);
        assert!(a.current.is_none(), "nothing held back for the end of the stream to lose");
    }

    #[test]
    fn a_gap_between_frames_damages_the_frame_after_it() {
        let mut a = Assembler::default();
        a.push(&rtp(20, 100, true, &[2, 1, 7]), 0);
        assert_eq!(a.first_loss_ts, None);
        a.push(&rtp(23, 300, true, &[2, 1, 8]), 0);
        assert_eq!((a.lost, a.first_loss_ts), (2, Some(300)));
    }
}
