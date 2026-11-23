//! A one-track HEVC MP4, written as the frames come: the samples go into
//! `<file>.part` straight away and only the sample table stays in memory.
//! `finish` appends the index (`moov`) and renames the file into place, so
//! a file without `.part` is always complete.

use std::{
    fs::File,
    io::{BufWriter, Seek, SeekFrom, Write},
    num::NonZeroU32,
    path::{Path, PathBuf},
};

use shiguredo_mp4::{
    Encode, TrackKind,
    boxes::SampleEntry,
    mux::{Mp4FileMuxer, Mp4FileMuxerOptions, Sample as MuxSample},
};

use super::hevc::{ParameterSets, RTP_CLOCK};
use crate::device::display::Rotation;

/// One frame's entry in the sample table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sample {
    pub size: u32,
    /// In RTP clock ticks.
    pub duration: u32,
    pub key: bool,
}

/// Ticks from one RTP timestamp to a later one, across the 32-bit wrap. A
/// timestamp that went backwards gives one tick, so time never runs back.
pub fn ticks_between(earlier: u32, later: u32) -> u32 {
    match later.wrapping_sub(earlier) {
        0 => 1,
        d if d > u32::MAX / 2 => 1,
        d => d,
    }
}

/// Microseconds of wall clock as RTP ticks, at least one.
pub fn ticks_from_micros(us: u64) -> u32 {
    (us.saturating_mul(u64::from(RTP_CLOCK)) / 1_000_000).clamp(1, u64::from(u32::MAX)) as u32
}

pub fn seconds(ticks: u64) -> f64 {
    ticks as f64 / f64::from(RTP_CLOCK)
}

const TIMESCALE: NonZeroU32 = NonZeroU32::new(RTP_CLOCK).unwrap();

/// The track matrix that plays a `width` x `height` frame of a panel the
/// way the screen looks with the display turned by `turn`. A point (x, y)
/// of the frame plays at (a x + c y + tx, b x + d y + ty), stored as
/// [a, b, 0, c, d, 0, tx, ty, 1] in 16.16 (the last in 2.30) fixed point.
fn matrix(turn: Rotation, width: u32, height: u32) -> [i32; 9] {
    const ONE: i32 = 0x0001_0000;
    let (w, h) = ((width as i32).saturating_mul(ONE), (height as i32).saturating_mul(ONE));
    let [a, b, c, d, tx, ty] = match turn {
        Rotation::R0 => [ONE, 0, 0, ONE, 0, 0],
        Rotation::R90 => [0, ONE, -ONE, 0, h, 0],
        Rotation::R180 => [-ONE, 0, 0, -ONE, w, h],
        Rotation::R270 => [0, -ONE, ONE, 0, 0, w],
    };
    [a, b, 0, c, d, 0, tx, ty, 0x4000_0000]
}

pub struct Writer {
    path: PathBuf,
    part: PathBuf,
    file: BufWriter<File>,
    /// Only used for its box layout: it never touches the file.
    muxer: Mp4FileMuxer,
    samples: Vec<Sample>,
    total_ticks: u64,
}

impl Writer {
    pub fn part_path(path: &Path) -> PathBuf {
        let mut name = path.as_os_str().to_owned();
        name.push(".part");
        PathBuf::from(name)
    }

    pub fn create(path: &Path) -> std::io::Result<Self> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        let part = Self::part_path(path);
        let mut file = BufWriter::new(File::create(&part)?);
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
        let muxer = Mp4FileMuxer::with_options(Mp4FileMuxerOptions { creation_timestamp: now, ..Default::default() }).map_err(std::io::Error::other)?;
        file.write_all(muxer.initial_boxes_bytes())?;
        Ok(Self { path: path.to_path_buf(), part, file, muxer, samples: Vec::new(), total_ticks: 0 })
    }

    /// Append a frame's data. Its duration is set when the next one comes.
    pub fn append(&mut self, data: &[u8], key: bool) -> std::io::Result<()> {
        self.file.write_all(data)?;
        self.samples.push(Sample { size: data.len() as u32, duration: 0, key });
        Ok(())
    }

    /// Set the duration of the last frame appended.
    pub fn set_last_duration(&mut self, ticks: u32) {
        if let Some(s) = self.samples.last_mut() {
            self.total_ticks -= u64::from(s.duration);
            s.duration = ticks.max(1);
            self.total_ticks += u64::from(s.duration);
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn samples(&self) -> &[Sample] {
        &self.samples
    }

    pub fn total_ticks(&self) -> u64 {
        self.total_ticks
    }

    /// Write the index and move the file into place, the frames to play
    /// turned by `turn`. Returns its size. On a failure the partial file is
    /// removed: nothing else would.
    pub fn finish(self, params: &ParameterSets, turn: Rotation) -> std::io::Result<u64> {
        let part = self.part.clone();
        let finished = self.write_index(params, turn);
        if finished.is_err() {
            let _ = std::fs::remove_file(part);
        }
        finished
    }

    fn write_index(mut self, params: &ParameterSets, turn: Rotation) -> std::io::Result<u64> {
        // The samples sit back to back right after the initial boxes, which
        // is where the muxer expects the first one to start.
        let mut data_offset = self.muxer.initial_boxes_bytes().len() as u64;
        let entry = SampleEntry::Hvc1(params.entry.clone());
        for (i, s) in self.samples.iter().enumerate() {
            let sample = MuxSample {
                track_kind: TrackKind::Video,
                sample_entry: (i == 0).then(|| entry.clone()),
                keyframe: s.key,
                timescale: TIMESCALE,
                duration: s.duration,
                composition_time_offset: None,
                data_offset,
                data_size: s.size as usize,
            };
            self.muxer.append_sample(&sample).map_err(std::io::Error::other)?;
            data_offset += u64::from(s.size);
        }
        let finalized = self.muxer.finalize().map_err(std::io::Error::other)?;
        // The muxer always writes the identity matrix. Another matrix encodes
        // to a moov of the same length, written in place of the muxer's.
        let turned = match turn {
            Rotation::R0 => None,
            _ => {
                let mut moov = finalized.moov_box().clone();
                for trak in &mut moov.trak_boxes {
                    trak.tkhd_box.matrix = matrix(turn, params.width, params.height);
                }
                let encode = |m: &shiguredo_mp4::boxes::MoovBox| m.encode_to_vec().map_err(std::io::Error::other);
                Some((encode(finalized.moov_box())?, encode(&moov)?))
            }
        };
        let mut file = self.file.into_inner().map_err(|e| e.into_error())?;
        // Without faststart the moov goes where the samples end. A sample whose
        // write failed half way can lie past that, so the file is cut after it.
        let mut end = 0;
        for (offset, bytes) in finalized.offset_and_bytes_pairs() {
            let bytes = match &turned {
                Some((plain, turned)) if bytes == plain.as_slice() => turned.as_slice(),
                _ => bytes,
            };
            file.seek(SeekFrom::Start(offset))?;
            file.write_all(bytes)?;
            end = end.max(offset + bytes.len() as u64);
        }
        file.set_len(end)?;
        file.sync_all()?;
        let size = file.metadata()?.len();
        drop(file);
        std::fs::rename(&self.part, &self.path)?;
        Ok(size)
    }

    /// Give up: remove the partial file.
    pub fn discard(self) {
        let part = self.part.clone();
        drop(self);
        let _ = std::fs::remove_file(part);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::hevc::tests::{keyframe_with, sps_nal};
    use shiguredo_mp4::{
        Decode,
        boxes::{MoovBox, TkhdBox},
        demux::{Input, Mp4FileDemuxer},
    };

    fn params(w: u32, h: u32) -> ParameterSets {
        ParameterSets::from_keyframe(&keyframe_with(sps_nal(w, h))).unwrap()
    }

    /// The matrix of the file's video track, from its index at the end.
    fn matrix_of(file: &[u8]) -> [i32; 9] {
        let at = file.windows(4).rposition(|w| w == b"moov").unwrap() - 4;
        MoovBox::decode(&file[at..]).unwrap().0.trak_boxes[0].tkhd_box.matrix
    }

    /// Where a matrix plays a point of the frame, in pixels.
    fn play(m: [i32; 9], (x, y): (f64, f64)) -> (f64, f64) {
        let f = |i: usize| f64::from(m[i]) / 65536.0;
        (f(0) * x + f(3) * y + f(6), f(1) * x + f(4) * y + f(7))
    }

    #[test]
    fn a_turned_panel_plays_each_point_where_the_screen_shows_it() {
        // The frame is of the panel; a touch at a share of the screen lands
        // at `turn` of it on the panel (measured), and must play back there.
        let (w, h) = (1408.0, 2016.0);
        for turn in [Rotation::R0, Rotation::R90, Rotation::R180, Rotation::R270] {
            let played = if matches!(turn, Rotation::R90 | Rotation::R270) { (h, w) } else { (w, h) };
            for share in [(0.0, 0.0), (1.0, 0.0), (0.2, 0.6), (1.0, 1.0)] {
                let (pu, pv) = turn.turn(share);
                let at = play(matrix(turn, 1408, 2016), (pu * w, pv * h));
                let expected = (share.0 * played.0, share.1 * played.1);
                assert!((at.0 - expected.0).abs() < 1e-6 && (at.1 - expected.1).abs() < 1e-6, "{turn:?} {share:?}: {at:?}");
            }
        }
        assert_eq!(matrix(Rotation::R0, 1408, 2016), TkhdBox::DEFAULT_MATRIX);
    }

    #[test]
    fn a_turned_recording_keeps_its_frames_and_carries_the_turn() {
        let dir = std::env::temp_dir().join(format!("iph-mp4-turned-{}", std::process::id()));
        let read = |turn: Rotation| {
            let path = dir.join(format!("{turn:?}.mp4"));
            let mut w = Writer::create(&path).unwrap();
            for (data, key) in [(&[1u8; 10][..], true), (&[2; 3][..], false)] {
                w.append(data, key).unwrap();
                w.set_last_duration(400);
            }
            let size = w.finish(&params(1408, 2016), turn).unwrap();
            let file = std::fs::read(&path).unwrap();
            assert_eq!(size, file.len() as u64);
            let mut demuxer = Mp4FileDemuxer::new();
            demuxer.handle_input(Input { position: 0, data: &file });
            let mut samples = Vec::new();
            while let Some(s) = demuxer.next_sample().unwrap() {
                samples.push((file[s.data_offset as usize..s.data_offset as usize + s.data_size].to_vec(), s.keyframe, s.duration));
            }
            (file.len(), samples, matrix_of(&file))
        };
        let (plain_len, plain_samples, plain) = read(Rotation::R0);
        let (turned_len, turned_samples, turned) = read(Rotation::R90);
        assert_eq!(plain, TkhdBox::DEFAULT_MATRIX);
        assert_eq!(turned, matrix(Rotation::R90, 1408, 2016));
        assert_eq!((turned_len, turned_samples), (plain_len, plain_samples));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rtp_time_becomes_media_time_at_24_khz() {
        assert_eq!(ticks_between(1000, 1400), 400);
        assert_eq!(ticks_between(u32::MAX - 99, 300), 400, "across the wrap");
        assert_eq!(ticks_between(1400, 1000), 1, "never backwards");
        assert_eq!(ticks_between(7, 7), 1);
        assert_eq!(ticks_from_micros(500_000), 12_000);
        assert_eq!(ticks_from_micros(0), 1);
        assert!((seconds(240_000) - 10.0).abs() < 1e-9);
    }

    #[test]
    fn the_file_parses_back_with_its_samples_durations_and_decoder_setup() {
        let dir = std::env::temp_dir().join(format!("iph-mp4-{}", std::process::id()));
        let path = dir.join("clip.mp4");
        let p = params(1408, 2016);
        let mut w = Writer::create(&path).unwrap();
        assert!(Writer::part_path(&path).exists() && !path.exists());
        let frames: [(&[u8], bool, u32); 4] = [(&[1; 10], true, 400), (&[2; 3], false, 400), (&[3; 7], false, 800), (&[4; 5], false, 400)];
        for (data, key, duration) in frames {
            w.append(data, key).unwrap();
            w.set_last_duration(duration);
        }
        assert_eq!(w.total_ticks(), 2000);
        let size = w.finish(&p, Rotation::R0).unwrap();
        let file = std::fs::read(&path).unwrap();
        assert_eq!(size, file.len() as u64);
        assert!(!Writer::part_path(&path).exists());

        let mut demuxer = Mp4FileDemuxer::new();
        demuxer.handle_input(Input { position: 0, data: &file });
        let track = demuxer.tracks().unwrap()[0].clone();
        assert_eq!((track.timescale.get(), track.duration), (RTP_CLOCK, 2000));
        let mut read = Vec::new();
        while let Some(s) = demuxer.next_sample().unwrap() {
            let data = file[s.data_offset as usize..s.data_offset as usize + s.data_size].to_vec();
            let entry = s.sample_entry.cloned();
            read.push((data, s.keyframe, s.timestamp, s.duration, entry));
        }
        assert_eq!(read.len(), 4);
        let times: Vec<_> = read.iter().map(|(d, k, t, dur, _)| (d.len(), *k, *t, *dur)).collect();
        assert_eq!(times, [(10, true, 0, 400), (3, false, 400, 400), (7, false, 800, 800), (5, false, 1600, 400)]);
        assert_eq!(read[0].0, vec![1; 10]);
        assert_eq!(read[3].0, vec![4; 5]);
        assert_eq!(read[0].4, Some(shiguredo_mp4::boxes::SampleEntry::Hvc1(p.entry.clone())));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_discarded_recording_leaves_no_file() {
        let dir = std::env::temp_dir().join(format!("iph-mp4-discard-{}", std::process::id()));
        let path = dir.join("clip.mp4");
        let w = Writer::create(&path).unwrap();
        w.discard();
        assert!(!Writer::part_path(&path).exists() && !path.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
