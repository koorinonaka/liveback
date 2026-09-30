use crate::ui_state::export_failure as msg;
use crate::ui_state::locale::active as ui_locale;
use std::path::Path;

use windows::core::Interface;
use windows::Win32::Foundation::E_OUTOFMEMORY;
use windows::Win32::Media::MediaFoundation::{
    IMFAttributes, IMFMediaType, MFAudioFormat_AAC, MFCreateMFByteStreamOnStream,
    MFCreateSourceReaderFromByteStream, MFMediaType_Audio, MFVideoFormat_AV1, MFVideoFormat_H264,
    MF_BYTESTREAM_CONTENT_TYPE, MF_E_INVALIDSTREAMNUMBER, MF_MT_AUDIO_AVG_BYTES_PER_SECOND,
    MF_MT_AUDIO_NUM_CHANNELS, MF_MT_AUDIO_SAMPLES_PER_SECOND, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE,
    MF_MT_MAJOR_TYPE, MF_MT_MPEG_SEQUENCE_HEADER, MF_MT_SUBTYPE,
    MF_SOURCE_READER_FIRST_VIDEO_STREAM,
};
use windows::Win32::UI::Shell::SHCreateMemStream;

use crate::{encoder, encoder::VideoCodec, ring_buffer};

use super::ExportFailure;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct VideoFingerprint {
    /// **Which codec this stream is, carried in the fingerprint itself**
    /// (task1770), and for H.264 the `profile_idc` its SPS declares.
    ///
    /// The two travel together on purpose. `part_writer`'s boundary re-encoder
    /// feeds this straight to `create_with_codec`, so an AV1 source cannot be
    /// handed to an H.264 encoder. A bare `profile: u32` could: Media
    /// Foundation reuses `MF_MT_MPEG2_PROFILE` across codec families -- the AV1
    /// MFT reports its own `seq_profile` (0/1/2) there -- so a number alone
    /// says nothing about whose profile it is, and the mismatch would be
    /// silent. That is the shape of the accident task1310 fixed.
    pub(super) codec: VideoCodec,
    pub(super) sequence_header: Vec<u8>,
    pub(super) level: u32,
    pub(super) frame_size: u64,
    pub(super) frame_rate: u8,
}

impl VideoFingerprint {
    /// Task080: whether two video streams are safe to splice/compare for export
    /// purposes, deliberately ignoring `frame_rate`. `frame_rate` is
    /// `classify_frame_rate`'s bucketed guess at the *nominal* encode rate,
    /// derived from the source container's *observed* average sample timing --
    /// which WGC's irregular frame delivery can put far from the rate the
    /// hardware encoder was actually configured at (confirmed on real data: a
    /// true 60fps encode, level=40, with a container-computed average of only
    /// ~34-37fps). `sequence_header`/`profile`/`level`/`frame_size` are read
    /// directly from the real H.264 SPS bytes and need no such inference, so
    /// they alone are the trustworthy signal of codec compatibility.
    ///
    /// For AV1 the sequence-header bytes are the AV1 sequence header OBU, whose
    /// `seq_profile`/`seq_level_idx` are inside those very bytes -- so the byte
    /// comparison already covers everything `profile`/`level` say for H.264,
    /// and `level` stays 0 there rather than being parsed back out (task1770).
    pub(super) fn codec_compatible(&self, other: &VideoFingerprint) -> bool {
        self.codec == other.codec
            && self.sequence_header == other.sequence_header
            && self.level == other.level
            && self.frame_size == other.frame_size
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct AudioFingerprint {
    sample_rate: u32,
    channels: u32,
    avg_bytes_per_second: u32,
}

pub(super) struct SourceTypes {
    pub(super) video: IMFMediaType,
    /// One entry per audio track, in track order (task1280). Empty only when
    /// *no* segment in the part has audio at all: the export then writes a
    /// video-only MP4 instead of failing (task124).
    pub(super) audio: Vec<IMFMediaType>,
    pub(super) video_fingerprint: VideoFingerprint,
    audio_fingerprint: Vec<AudioFingerprint>,
}

pub(super) fn probe_source_types(
    segments: &[ring_buffer::SegmentRecord],
    _runtime: &encoder::MfRuntime,
) -> Result<SourceTypes, ExportFailure> {
    let mut baseline: Option<SourceTypes> = None;
    for segment in segments {
        unsafe {
            let reader = open_reader(&segment.path)?;
            let video = reader
                .GetCurrentMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32)
                .map_err(|error| {
                    ExportFailure::new(msg::video_info_unreadable(ui_locale(), error))
                })?;
            let audio = audio_media_types(&reader)?;
            let video_fingerprint = video_fingerprint(&video)?;
            let audio_fingerprint = audio
                .iter()
                .map(audio_fingerprint)
                .collect::<Result<Vec<_>, _>>()?;
            if let Some(existing) = &mut baseline {
                // `codec_compatible` (Task080), not `!=`: `video_fingerprint.frame_rate`
                // is `classify_frame_rate`'s bucketed guess at the *observed*
                // average sample timing, which WGC's irregular frame delivery
                // can flip between adjacent segments of the very same
                // recording even though the encoder's actual configuration
                // never changed (confirmed on real data, see Task086).
                // `sequence_header`/`profile`/`level`/`frame_size` are read
                // directly from the real H.264 SPS bytes and need no such
                // tolerance. `AudioFingerprint`'s fields are all declared AAC
                // media-type attributes (MF_MT_AUDIO_*), not derived from
                // observed timing, so they have no equivalent noisy field and
                // keep the strict comparison.
                // Compared track for track, and only where both sides have
                // one: a video-only segment (or one recorded before an extra
                // track existed) contributes nothing to compare rather than a
                // mismatch.
                let audio_mismatch = !existing.audio_fingerprint.is_empty()
                    && !audio_fingerprint.is_empty()
                    && existing.audio_fingerprint != audio_fingerprint;
                if !existing
                    .video_fingerprint
                    .codec_compatible(&video_fingerprint)
                    || audio_mismatch
                {
                    return Err(ExportFailure::new(msg::segments_config_mismatch(
                        ui_locale(),
                    )));
                }
                // A video-only segment can appear anywhere in a part, including
                // first (capture task085). Adopting the first audio type found
                // -- rather than only the first segment's -- keeps the output's
                // audio track for every segment that does have one, instead of
                // silently dropping a part's entire audio because its opening
                // segment happened to be degenerate.
                if existing.audio.len() < audio.len() {
                    existing.audio = audio;
                    existing.audio_fingerprint = audio_fingerprint;
                }
            } else {
                baseline = Some(SourceTypes {
                    video,
                    audio,
                    video_fingerprint,
                    audio_fingerprint,
                });
            }
        }
    }
    baseline.ok_or_else(|| ExportFailure::new(msg::no_segment_to_export(ui_locale())))
}

/// `Ok(None)` when the segment genuinely has no audio track.
///
/// A recorded segment shorter than one AAC frame (~21.33ms) finalizes
/// video-only -- `capture.rs` task085 covers producing exactly that shape --
/// and Media Foundation answers any request for its (nonexistent) first audio
/// stream with `MF_E_INVALIDSTREAMNUMBER`. That single HRESULT is the "no
/// audio here" answer and must not fail the export; every other error still
/// does, so a present-but-broken audio track is still reported instead of
/// being silently dropped.
pub(super) unsafe fn audio_media_types(
    reader: &windows::Win32::Media::MediaFoundation::IMFSourceReader,
) -> Result<Vec<IMFMediaType>, ExportFailure> {
    unsafe {
        let mut out = Vec::new();
        for stream in audio_stream_indices(reader) {
            match reader.GetCurrentMediaType(stream) {
                Ok(media_type) => out.push(media_type),
                Err(error) if error.code() == MF_E_INVALIDSTREAMNUMBER => {}
                Err(error) => {
                    return Err(ExportFailure::new(msg::aac_info_unreadable(
                        ui_locale(),
                        error,
                    )))
                }
            }
        }
        Ok(out)
    }
}

/// A file with more streams than this is not something this app wrote.
const MAX_READER_STREAMS: u32 = 32;

/// The reader stream index of each audio track, **in track order**.
///
/// Reversed, because the reader is: the mp4 numbers its traks in the order the
/// recording added them (track 0, the capture target, lowest) and the Source
/// Reader hands the streams back the other way round. Measured in task1260 and
/// again in task1270; `playback::segment_reader` carries the same note and the
/// same one-line fix. Order *is* the track identity in an MP4 the SinkWriter
/// writes -- there is no way to name a track -- so getting this backwards
/// would silently reorder the tracks of every export.
pub(super) unsafe fn audio_stream_indices(
    reader: &windows::Win32::Media::MediaFoundation::IMFSourceReader,
) -> Vec<u32> {
    unsafe {
        let mut audio = Vec::new();
        for index in 0..MAX_READER_STREAMS {
            let Ok(native) = reader.GetNativeMediaType(index, 0) else {
                break;
            };
            if native.GetGUID(&MF_MT_MAJOR_TYPE).ok() == Some(MFMediaType_Audio) {
                // The reader selects only the *first* audio stream by default,
                // and reading an unselected one comes back MF_E_INVALIDREQUEST.
                let _ = reader.SetStreamSelection(index, true);
                audio.push(index);
            }
        }
        audio.reverse();
        audio
    }
}

/// A segment's bytes as Media Foundation must be handed them: repaired when a
/// `moof` straddles one of the MP4 source's 64KiB read boundaries (task1900),
/// and verbatim otherwise.
///
/// Split out of [`open_reader`] so the repair can be checked at the byte level
/// without Media Foundation.
fn repaired_segment_bytes(path: &Path) -> Result<Vec<u8>, ExportFailure> {
    let bytes = std::fs::read(path).map_err(|error| {
        ExportFailure::new(msg::segment_open_failed(ui_locale(), path.display(), error))
    })?;
    match crate::encoder::mp4_boxes::unstraddle_fragments(&bytes) {
        Some(padded) => {
            tracing::info!(
                target: "task1920_export_repack",
                path = %path.display(),
                was = bytes.len(),
                now = padded.len(),
                "padded a fragment off the MP4 source's 64KiB read boundary"
            );
            Ok(padded)
        }
        None => Ok(bytes),
    }
}

/// Opens a segment for the export pipeline.
///
/// **Through a byte stream rather than the URL** (task1920): Windows' MP4
/// source silently ends a presentation at a `moof` lying across one of its
/// 64KiB read boundaries, dropping every later sample on both tracks and
/// reporting `ENDOFSTREAM` -- so an export whose range covered such a segment
/// truncated with nothing to catch. Task1900 measured that the defect is the
/// MP4 source's own and not the byte stream's (the same segment stopped at the
/// same place through `MFCreateSourceReaderFromURL`), and fixed the playback
/// reader; this is the same repair on the read path every export segment goes
/// through -- container sessions via their staged copies, loose-file sessions
/// via the session directory, both of them here.
pub(super) unsafe fn open_reader(
    path: &Path,
) -> Result<windows::Win32::Media::MediaFoundation::IMFSourceReader, ExportFailure> {
    let bytes = repaired_segment_bytes(path)?;
    let open = || -> windows::core::Result<_> {
        unsafe {
            let stream = SHCreateMemStream(Some(&bytes))
                .ok_or_else(|| windows::core::Error::from(E_OUTOFMEMORY))?;
            let byte_stream = MFCreateMFByteStreamOnStream(&stream)?;
            // The resolver sniffs fMP4 fine; naming the type costs nothing and
            // spares a blind debugging session if it ever stops.
            if let Ok(attributes) = byte_stream.cast::<IMFAttributes>() {
                let _ = attributes.SetString(
                    &MF_BYTESTREAM_CONTENT_TYPE,
                    &windows::core::HSTRING::from("video/mp4"),
                );
            }
            MFCreateSourceReaderFromByteStream(&byte_stream, None)
        }
    };
    open().map_err(|error| {
        ExportFailure::new(msg::segment_open_failed(ui_locale(), path.display(), error))
    })
}

fn media_blob(media_type: &IMFMediaType, key: &windows::core::GUID) -> Vec<u8> {
    unsafe {
        let size = media_type.GetBlobSize(key).unwrap_or_default();
        if size == 0 {
            return Vec::new();
        }
        let mut bytes = vec![0; size as usize];
        let mut written = 0;
        if media_type
            .GetBlob(key, &mut bytes, Some(&mut written))
            .is_err()
        {
            return Vec::new();
        }
        bytes.truncate(written as usize);
        bytes
    }
}

pub(super) fn video_fingerprint(
    media_type: &IMFMediaType,
) -> Result<VideoFingerprint, ExportFailure> {
    unsafe {
        let subtype = media_type.GetGUID(&MF_MT_SUBTYPE).ok();
        let raw = media_blob(media_type, &MF_MT_MPEG_SEQUENCE_HEADER);
        let (codec, sequence_header, level) = if subtype == Some(MFVideoFormat_H264) {
            let sequence_header = normalize_annex_b(&raw);
            if sequence_header.is_empty() {
                return Err(ExportFailure::new(msg::h264_sequence_header_missing(
                    ui_locale(),
                )));
            }
            let (profile, level) = sps_profile_level(&sequence_header)
                .ok_or_else(|| ExportFailure::new(msg::h264_sps_unparsable(ui_locale())))?;
            (VideoCodec::H264 { profile }, sequence_header, level)
        } else if subtype == Some(MFVideoFormat_AV1) {
            // Measured 2026-08-25 (`av1_export_probe_reports_sequence_header_and_
            // sink_writer_support`): Media Foundation publishes
            // `MF_MT_MPEG_SEQUENCE_HEADER` for AV1 too -- 17 bytes, the AV1
            // sequence header OBU, against H.264's 39-byte SPS+PPS.
            //
            // Taken raw, deliberately. `normalize_annex_b` rewrites 4-byte
            // start codes into 3-byte ones, which is an Annex-B concept AV1 has
            // no equivalent of: run it here and any `00 00 00 01` that happens
            // to fall inside the OBU is silently corrupted, and the byte
            // comparison that is the whole matching mechanism compares the
            // wrong bytes.
            if raw.is_empty() {
                return Err(ExportFailure::new(msg::av1_sequence_header_missing(
                    ui_locale(),
                )));
            }
            (VideoCodec::Av1, raw, 0)
        } else {
            return Err(ExportFailure::new(msg::video_codec_unsupported(
                ui_locale(),
                format!("{subtype:?}"),
            )));
        };
        Ok(VideoFingerprint {
            codec,
            sequence_header,
            level,
            frame_size: media_type
                .GetUINT64(&MF_MT_FRAME_SIZE)
                .map_err(|_| ExportFailure::new(msg::video_frame_size_missing(ui_locale())))?,
            frame_rate: classify_frame_rate(
                media_type
                    .GetUINT64(&MF_MT_FRAME_RATE)
                    .map_err(|_| ExportFailure::new(msg::video_frame_rate_missing(ui_locale())))?,
            )?,
        })
    }
}

fn normalize_annex_b(bytes: &[u8]) -> Vec<u8> {
    let mut normalized = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index..].starts_with(&[0, 0, 0, 1]) {
            normalized.extend_from_slice(&[0, 0, 1]);
            index += 4;
        } else {
            normalized.push(bytes[index]);
            index += 1;
        }
    }
    normalized
}

fn sps_profile_level(sequence_header: &[u8]) -> Option<(u32, u32)> {
    sequence_header
        .windows(7)
        .find(|window| window[..4] == [0, 0, 1, 0x67])
        .map(|window| (u32::from(window[4]), u32::from(window[6])))
}

/// Every nominal rate `encoder::validate_config` will build an encoder at, in
/// the order the boundary re-encoder should try them (task370). This is a
/// *candidate list*, not a classification: `classify_frame_rate` only picks
/// which one to try first, because the observed average it buckets on is not
/// trustworthy enough to decide alone -- see `codec_compatible`.
pub(super) const BOUNDARY_FRAME_RATE_CANDIDATES: [u8; 3] = [30, 60, 120];

pub(super) fn classify_frame_rate(packed: u64) -> Result<u8, ExportFailure> {
    const MIN_PLAUSIBLE_FPS: f64 = 1.0;
    // The rate is an observed average, so a steady 120fps recording lands a
    // hair above 120 (120.385 seen) -- the ceiling only rejects garbage.
    const MAX_PLAUSIBLE_FPS: f64 = 240.0;
    const BUCKET_MIDPOINT_FPS: f64 = 45.0;
    // Task370: only ever a first guess. A 120fps recording whose delivery was
    // steady lands here and saves the boundary re-encoder two attempts; one
    // whose observed average sagged into the 30 bucket (which is what really
    // happens -- see `part_writer`) is still recovered by walking the rest of
    // `BOUNDARY_FRAME_RATE_CANDIDATES`. Moving this boundary alone would fix
    // nothing, which is why it is not the fix.
    const HIGH_BUCKET_MIDPOINT_FPS: f64 = 90.0;
    let numerator = (packed >> 32) as u32;
    let denominator = packed as u32;
    if numerator == 0 || denominator == 0 {
        return Err(ExportFailure::new(msg::video_frame_rate_invalid(
            ui_locale(),
        )));
    }
    let rate = numerator as f64 / denominator as f64;
    if !(MIN_PLAUSIBLE_FPS..=MAX_PLAUSIBLE_FPS).contains(&rate) {
        return Err(ExportFailure::new(msg::video_frame_rate_unsupported(
            ui_locale(),
            rate,
        )));
    }
    if rate < BUCKET_MIDPOINT_FPS {
        Ok(30)
    } else if rate < HIGH_BUCKET_MIDPOINT_FPS {
        Ok(60)
    } else {
        Ok(120)
    }
}

pub(super) fn audio_fingerprint(
    media_type: &IMFMediaType,
) -> Result<AudioFingerprint, ExportFailure> {
    unsafe {
        if media_type.GetGUID(&MF_MT_SUBTYPE).ok() != Some(MFAudioFormat_AAC) {
            return Err(ExportFailure::new(msg::audio_codec_not_aac(ui_locale())));
        }
        Ok(AudioFingerprint {
            sample_rate: media_type
                .GetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND)
                .map_err(|_| ExportFailure::new(msg::aac_sample_rate_missing(ui_locale())))?,
            channels: media_type
                .GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS)
                .map_err(|_| ExportFailure::new(msg::aac_channels_missing(ui_locale())))?,
            avg_bytes_per_second: media_type
                .GetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND)
                .unwrap_or_default(),
        })
    }
}

#[cfg(test)]
mod repair_tests {
    use super::repaired_segment_bytes;
    use crate::ui_state::export_failure as msg;
    use crate::ui_state::locale::active as ui_locale;

    const GRID: usize = 64 * 1024;

    fn boxed(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(body);
        out
    }

    /// A `moof` whose single `traf` is `default-base-is-moof`, followed by its
    /// `mdat`, placed so the `moof` crosses a 64KiB boundary.
    fn segment(straddles: bool) -> Vec<u8> {
        let mut tfhd = 0x0002_0000u32.to_be_bytes().to_vec();
        tfhd.extend_from_slice(&1u32.to_be_bytes());
        let moof = boxed(b"moof", &boxed(b"traf", &boxed(b"tfhd", &tfhd)));
        let start = if straddles { GRID - 8 } else { GRID + 8 };
        let mut out = boxed(b"moov", &vec![0u8; 1000]);
        out.extend_from_slice(&boxed(b"free", &vec![0u8; start - out.len() - 8]));
        out.extend_from_slice(&moof);
        out.extend_from_slice(&boxed(b"mdat", &vec![7u8; 2_000]));
        out
    }

    fn moof_offsets(bytes: &[u8]) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        let mut at = 0usize;
        while at + 8 <= bytes.len() {
            let size = u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
            if &bytes[at + 4..at + 8] == b"moof" {
                out.push((at, size));
            }
            at += size;
        }
        out
    }

    fn straddles(bytes: &[u8]) -> bool {
        moof_offsets(bytes)
            .into_iter()
            .any(|(at, size)| at / GRID != (at + size - 1) / GRID)
    }

    fn written(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("livia-{name}-{}.mp4", std::process::id()));
        std::fs::write(&path, bytes).unwrap();
        path
    }

    /// Task1920: what the export hands Media Foundation is the *repaired*
    /// bytes, not the file on disk. Checked here rather than through
    /// `open_reader` so it needs no Media Foundation.
    #[test]
    fn the_export_read_path_repairs_a_straddling_segment() {
        let bytes = segment(true);
        assert!(straddles(&bytes), "fixture is not the case");
        let path = written("task1920-straddling", &bytes);

        let opened = repaired_segment_bytes(&path).expect("the segment reads");

        assert!(
            !straddles(&opened),
            "the export still hands MF a straddling fragment"
        );
        assert!(opened.len() > bytes.len(), "the padding is missing");
        let _ = std::fs::remove_file(&path);
    }

    /// The other half of the same contract: a segment that needs nothing is
    /// handed over exactly as it lies on disk.
    #[test]
    fn a_segment_that_needs_no_repair_reaches_media_foundation_verbatim() {
        let bytes = segment(false);
        assert!(!straddles(&bytes), "fixture is not the case");
        let path = written("task1920-healthy", &bytes);

        assert_eq!(
            repaired_segment_bytes(&path).expect("the segment reads"),
            bytes,
            "an untouched segment must reach MF byte for byte"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// A missing file still reports the segment's path, the way the URL-based
    /// reader's failure did -- in the log since t260926-97dd, where the path
    /// moved off the screen (Goal 1: 「`{path}` の生値は本文に出さず」).
    #[test]
    fn a_read_failure_still_names_the_segment() {
        let path = std::env::temp_dir().join("livia-task1920-does-not-exist.mp4");
        let error = repaired_segment_bytes(&path).expect_err("a missing segment fails");
        let detail = crate::ui_state::export_failure::last_detail();
        // Against the wording module rather than a copy of the string.
        let expected = msg::segment_open_failed(ui_locale(), path.display(), "");
        assert_eq!(error.message, expected);
        assert!(!error.message.contains("livia-task1920-does-not-exist"));
        assert!(
            detail.contains("livia-task1920-does-not-exist"),
            "unexpected detail: {detail}"
        );
    }
}
