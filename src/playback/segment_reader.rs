use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::Media::MediaFoundation::{
    IMF2DBuffer2, IMFAttributes, IMFByteStream, IMFDXGIBuffer, IMFDXGIDeviceManager, IMFMediaType,
    IMFSample, IMFSourceReader, MF2DBuffer_LockFlags_Read, MFAudioFormat_Float, MFCreateAttributes,
    MFCreateMediaType, MFCreateSourceReaderFromByteStream, MFMediaType_Audio, MFMediaType_Video,
    MFVideoArea, MFVideoFormat_NV12, MFVideoFormat_RGB32, MF_BYTESTREAM_CONTENT_TYPE,
    MF_MT_AUDIO_BITS_PER_SAMPLE, MF_MT_AUDIO_NUM_CHANNELS, MF_MT_AUDIO_SAMPLES_PER_SECOND,
    MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE, MF_MT_MINIMUM_DISPLAY_APERTURE,
    MF_MT_SUBTYPE, MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, MF_SOURCE_READERF_ENDOFSTREAM,
    MF_SOURCE_READER_D3D_MANAGER, MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING,
    MF_SOURCE_READER_FIRST_VIDEO_STREAM,
};

#[cfg(test)]
use super::source::memory_byte_stream;
use crate::encoder::aac::{AUDIO_CHANNELS, AUDIO_SAMPLE_RATE};

// ---------- per-segment source reader ----------

/// What the Source Reader is handing over, and therefore which conversion the
/// frame needs on the way to RGBA.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum VideoFormat {
    /// The decoder's own output, taken with no Video Processor in the way.
    /// Colour conversion is ours (`nv12_to_rgba`), which is the whole point:
    /// `ReadSample` drops from ~8ms a frame to under 2ms (task206).
    Nv12,
    /// The pre-task206 path: the basic Video Processor converts to RGB32 and
    /// `swizzle_bgra` reorders it. Kept as the fallback for a decoder that
    /// will not offer NV12.
    Bgra,
}

pub(super) struct SegmentReader {
    pub(super) reader: IMFSourceReader,
    pub(super) playlist_index: usize,
    pub(super) width: u32,
    pub(super) height: u32,
    /// Row pitch in bytes. For [`VideoFormat::Bgra`] a negative value means
    /// bottom-up rows -- the advanced Video Processor used to hand those out
    /// (task120), the basic one returns top-down (+7680 for 1920 wide), and
    /// `swizzle_bgra` reads the sign rather than assuming (task200). For
    /// [`VideoFormat::Nv12`] this is the Y plane's pitch and the interleaved
    /// UV plane follows at the same pitch.
    pub(super) stride: i32,
    /// What `MF_MT_DEFAULT_STRIDE` actually said, or `None` when the attribute
    /// was absent and [`Self::stride`] above is the width-derived guess
    /// (task500). Kept apart from `stride` only so the one-shot diagnostic can
    /// report which of the two the media type really was.
    declared_stride: Option<i32>,
    format: VideoFormat,
    /// Reader stream indices of the audio tracks, **in track order**: index 0
    /// is the capture target, the rest are the extra applications (task1270).
    ///
    /// Track order is not reader order. The mp4 numbers its traks in the order
    /// the sink was given them, but the Source Reader hands the streams back in
    /// the reverse of that (measured in task1260), so this list is sorted by
    /// the stream descriptor identifier -- which for MP4 is the track id.
    pub(super) audio_streams: Vec<u32>,
    video_done: bool,
    audio_done: Vec<bool>,
    /// Local timestamp of the last video sample handed out since the last
    /// source seek, so a precise seek can tell whether decoding forward from
    /// here reaches its target (2026-09-15, the coarse/precise scrub pair).
    last_video_local: Option<i64>,
    /// The earliest segment-local time this reader can serve (t261017-3f94):
    /// 0 for a whole segment, just past the GOP's start for one a seek read
    /// from that GOP on. A source seek before it finds no sample at all, so a seek there
    /// has to open the segment again.
    pub(super) readable_from: i64,
}

/// The reader stream index of every audio track the file has, **in track
/// order** (task1270), each one selected -- the reader selects only the first
/// audio stream by default, and reading an unselected one comes back
/// `MF_E_INVALIDREQUEST`. The one copy for playback and export
/// (t261025-00ad).
///
/// `accept` sees each audio stream before it is selected and may refuse it:
/// a refused stream contributes no track rather than failing the whole reader
/// (playback refuses one that will not decode to float PCM -- a video-only
/// segment is an ordinary thing, task124).
pub(crate) fn audio_streams_in_track_order(
    reader: &IMFSourceReader,
    mut accept: impl FnMut(u32) -> bool,
) -> Vec<u32> {
    unsafe {
        let mut audio = Vec::new();
        for index in 0..MAX_READER_STREAMS {
            let Ok(native) = reader.GetNativeMediaType(index, 0) else {
                break;
            };
            if native.GetGUID(&MF_MT_MAJOR_TYPE).ok() != Some(MFMediaType_Audio) || !accept(index) {
                continue;
            }
            let _ = reader.SetStreamSelection(index, true);
            audio.push(index);
        }
        // **Reversed, because the reader is.** The mp4 numbers its traks in the
        // order the sink was given them -- track 0 (the capture target) is the
        // lowest id -- and the Source Reader hands the streams back in the
        // reverse of that. Measured twice: a segment written with tracks
        // carrying 43/22/15 samples reads back 15/22/43 (task1260), and a
        // video+2 audio file reads back as audio, audio, video while its traks
        // are video, audio, audio.
        //
        // The presentation descriptor is no help: its stream identifiers are
        // 1..n in *reader* order, not the mp4 track ids, so it agrees with
        // whichever order it is asked about (measured while writing this).
        //
        // Order *is* the track identity in an MP4 the SinkWriter writes, so
        // getting this backwards would swap which application a mixer row
        // controls and silently reorder the tracks of every export. It cannot
        // affect a single-track recording, which is every recording made
        // before task1260.
        audio.reverse();
        audio
    }
}

/// Sets `index` to 48kHz stereo f32, or says it would not take it.
fn decodes_to_float_pcm(reader: &IMFSourceReader, index: u32) -> bool {
    unsafe {
        let Ok(pcm) = MFCreateMediaType() else {
            return false;
        };
        pcm.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio).is_ok()
            && pcm.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_Float).is_ok()
            && pcm
                .SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, AUDIO_SAMPLE_RATE)
                .is_ok()
            && pcm
                .SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, u32::from(AUDIO_CHANNELS))
                .is_ok()
            && pcm.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 32).is_ok()
            && reader.SetCurrentMediaType(index, None, &pcm).is_ok()
    }
}

/// A file with more streams than this is not something this app wrote.
const MAX_READER_STREAMS: u32 = 32;

fn hr<T>(context: &str, result: windows::core::Result<T>) -> Result<T, String> {
    result.map_err(|error| format!("{context}: {error}"))
}

/// The picture's own size when the media type carries one apart from the
/// surface's, with the origin at 0,0 (the only shape `CopySubresourceRegion`'s
/// box and the CPU packers read).
fn display_aperture(media_type: &IMFMediaType) -> Option<(u32, u32)> {
    let mut area = MFVideoArea::default();
    unsafe {
        media_type
            .GetBlob(
                &MF_MT_MINIMUM_DISPLAY_APERTURE,
                std::slice::from_raw_parts_mut(
                    &mut area as *mut MFVideoArea as *mut u8,
                    std::mem::size_of::<MFVideoArea>(),
                ),
                None,
            )
            .ok()?;
    }
    let origin = (area.OffsetX.value, area.OffsetY.value);
    (origin == (0, 0) && area.Area.cx > 0 && area.Area.cy > 0)
        .then_some((area.Area.cx as u32, area.Area.cy as u32))
}

/// Opens a segment, preferring the decoder's native NV12 over a converted
/// RGB32 (task206). The fallback is not decoration: everything downstream --
/// the timeline, the audio clock, the whole review screen -- depends on a
/// segment opening, so a decoder that refuses NV12 must still play, just at
/// the old cost.
///
/// Playback itself goes through [`open_segment_with`] (task3500); this is the
/// `&[u8]` shape the measurement tests read a segment file with.
#[cfg(test)]
pub(super) fn open_segment(bytes: &[u8], playlist_index: usize) -> Result<SegmentReader, String> {
    open_segment_with(playlist_index, None, || {
        memory_byte_stream(bytes, playlist_index)
    })
}

/// [`open_segment`] over whatever produced the bytes (task3500).
///
/// The closure rather than one stream: the RGB32 fallback needs a stream that
/// has not been read from, and asking the source for a second one is how that
/// stayed true when the seam moved up from `&[u8]`.
pub(super) fn open_segment_with(
    playlist_index: usize,
    manager: Option<&IMFDXGIDeviceManager>,
    mut byte_stream: impl FnMut() -> Result<IMFByteStream, String>,
) -> Result<SegmentReader, String> {
    match open_stream_as(byte_stream()?, playlist_index, VideoFormat::Nv12, manager) {
        Ok(reader) => Ok(reader),
        Err(error) => {
            tracing::warn!(
                event = "playback_nv12_unavailable",
                %error,
                "NV12 output refused; falling back to the RGB32 video processor"
            );
            // `None` on the retry on purpose: this arm exists for a decoder
            // that would not hand over NV12 at all, and everything downstream
            // depends on a segment opening. Asking that same decoder for a
            // hardware transform as well would only add a second way for the
            // one path that must not fail to fail.
            open_stream_as(byte_stream()?, playlist_index, VideoFormat::Bgra, None)
        }
    }
}

/// [`open_segment`] with the format forced, for the NV12-vs-RGB32 measurement
/// and -- with `manager` -- the software-vs-hardware one (t260915-e6c8).
#[cfg(test)]
pub(super) fn open_as(
    bytes: &[u8],
    playlist_index: usize,
    format: VideoFormat,
    manager: Option<&IMFDXGIDeviceManager>,
) -> Result<SegmentReader, String> {
    open_stream_as(
        memory_byte_stream(bytes, playlist_index)?,
        playlist_index,
        format,
        manager,
    )
}

/// `manager` is the D3D11 device the decoder should decode *onto*
/// (t260915-e6c8). `None` -- what production still passes -- leaves the reader
/// exactly as it was: a software decoder writing NV12 into system memory. With
/// `Some`, the reader is allowed a hardware transform and hands back
/// `IMFDXGIBuffer` samples instead, which is a different shape downstream, so
/// the two are not interchangeable by the caller.
pub(super) fn open_stream_as(
    byte_stream: IMFByteStream,
    playlist_index: usize,
    format: VideoFormat,
    manager: Option<&IMFDXGIDeviceManager>,
) -> Result<SegmentReader, String> {
    unsafe {
        // The resolver sniffs fMP4 fine, but naming the type costs nothing
        // and spares a blind debugging session if it ever stops.
        if let Ok(attributes) = byte_stream.cast::<IMFAttributes>() {
            let _ = attributes.SetString(
                &MF_BYTESTREAM_CONTENT_TYPE,
                &windows::core::HSTRING::from("video/mp4"),
            );
        }
        // The Video Processor is asked for only on the fallback path. On the
        // NV12 path there is nothing for it to convert, and leaving it out is
        // what removes the ~8ms a frame it charged. When it *is* asked for it
        // must be basic, not ADVANCED (task200): the advanced one also does
        // frame-rate conversion and takes its rate from the first fragment --
        // a segment written at 20fps of content came back as 12 of its 36
        // frames, uniformly re-timed.
        let attributes = if format == VideoFormat::Bgra || manager.is_some() {
            let mut attributes: Option<IMFAttributes> = None;
            hr("attributes", MFCreateAttributes(&mut attributes, 3))?;
            let attributes = attributes.ok_or("attributes missing")?;
            if format == VideoFormat::Bgra {
                hr(
                    "video processing",
                    attributes.SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1),
                )?;
            }
            if let Some(manager) = manager {
                // Both, or neither: the manager alone is a hint the reader is
                // free to ignore, and the flag alone has no device to decode
                // onto.
                hr(
                    "d3d manager",
                    attributes.SetUnknown(&MF_SOURCE_READER_D3D_MANAGER, manager),
                )?;
                hr(
                    "hardware transforms",
                    attributes.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1),
                )?;
            }
            Some(attributes)
        } else {
            None
        };
        let reader = hr(
            "source reader",
            MFCreateSourceReaderFromByteStream(&byte_stream, attributes.as_ref()),
        )?;

        let video = hr("video type", MFCreateMediaType())?;
        hr(
            "video major",
            video.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video),
        )?;
        hr(
            "video subtype",
            video.SetGUID(
                &MF_MT_SUBTYPE,
                match format {
                    VideoFormat::Nv12 => &MFVideoFormat_NV12,
                    VideoFormat::Bgra => &MFVideoFormat_RGB32,
                },
            ),
        )?;
        hr(
            "video output",
            reader.SetCurrentMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32, None, &video),
        )?;
        let current = hr(
            "video current type",
            reader.GetCurrentMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32),
        )?;
        let packed = current.GetUINT64(&MF_MT_FRAME_SIZE).unwrap_or(0);
        // The hardware AV1 decoder reports its padded surface here (1936x1120
        // for a 1922x1112 window recording) and the picture only in the
        // aperture; the padding renders as a green stripe on the right.
        let (width, height) = display_aperture(&current)
            .unwrap_or(((packed >> 32) as u32, (packed & 0xFFFF_FFFF) as u32));
        // NV12's default stride is the Y plane's, so the fallback width when
        // the attribute is absent differs by format: one byte a pixel of luma
        // against four of BGRA.
        let declared_stride = current
            .GetUINT32(&MF_MT_DEFAULT_STRIDE)
            .map(|value| value as i32)
            .ok();
        // The surface's width, not the aperture's: the rows are laid out at it.
        let surface_width = ((packed >> 32) as u32).max(width);
        let stride = declared_stride.unwrap_or(match format {
            VideoFormat::Nv12 => surface_width as i32,
            VideoFormat::Bgra => (surface_width * 4) as i32,
        });

        let audio_streams =
            audio_streams_in_track_order(&reader, |index| decodes_to_float_pcm(&reader, index));

        Ok(SegmentReader {
            reader,
            playlist_index,
            width,
            height,
            stride,
            declared_stride,
            format,
            video_done: false,
            audio_done: vec![false; audio_streams.len()],
            audio_streams,
            last_video_local: None,
            readable_from: 0,
        })
    }
}

impl SegmentReader {
    /// Whether the source accepted the seek. A refused one (a position that
    /// rounds onto the track's duration in its own timescale, t260913-b4a6)
    /// moves nothing: the reader carries on from wherever it stood.
    pub(super) fn seek_local(&mut self, local_100ns: i64) -> bool {
        use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
        use windows::Win32::System::Variant::VT_I8;
        let mut position = PROPVARIANT::default();
        unsafe {
            (*position.Anonymous.Anonymous).vt = VT_I8;
            (*position.Anonymous.Anonymous).Anonymous.hVal = local_100ns.max(0);
        }
        let accepted = unsafe {
            self.reader
                .SetCurrentPosition(&windows::core::GUID::zeroed(), &position)
                .is_ok()
        };
        self.video_done = false;
        self.audio_done.fill(false);
        self.last_video_local = None;
        accepted
    }

    /// See `last_video_local`: `None` until a sample follows a source seek.
    pub(super) fn last_video_local(&self) -> Option<i64> {
        self.last_video_local
    }

    pub(super) fn video_done(&self) -> bool {
        self.video_done
    }

    /// Next decoded video sample as (local timestamp, sample), with no colour
    /// conversion done yet. Split out from [`Self::next_video`] so a precise
    /// seek can decode its way to the target and drop the frames it passes
    /// without paying the ~4.7ms conversion on each (task207). `ReadSample`
    /// itself cannot be skipped -- the Source Reader has no decode-less
    /// forward step -- so what is saved here is the conversion, not the decode.
    pub(super) fn next_video_sample(&mut self) -> Option<(i64, IMFSample)> {
        if self.video_done {
            return None;
        }
        let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
        match read_sample(&self.reader, stream, &mut self.video_done) {
            Ok(Some((timestamp, sample))) => {
                self.last_video_local = Some(timestamp);
                Some((timestamp, sample))
            }
            Ok(None) => None,
            Err(error) => {
                // The twin of the audio trace 0937f8e added: a failed read
                // ends the segment with no ENDOFSTREAM and, being `None`,
                // is indistinguishable from one to the caller -- which
                // makes the worker cross a segment early. Task1900 measured
                // the crossing-early case and found ENDOFSTREAM rather than
                // this, so the path stays unproven; say so instead of going
                // quiet if it ever is taken.
                tracing::warn!(
                    event = "playback_video_read_failed",
                    %error,
                    "video ReadSample failed; no more video from this reader"
                );
                None
            }
        }
    }

    /// A decoded sample as RGBA rows. Sits above the format branch so NV12 and
    /// the Bgra fallback both go through the one entry point, and keeps the
    /// buffer's `Lock`/`Unlock` pair inside a single function.
    ///
    /// `spare` is a buffer an earlier frame was written into and the UI has
    /// since finished with (task1640); it is written again in place when it is
    /// the right size, and a new one is allocated when it is `None` or is not.
    pub(super) fn convert(
        &self,
        sample: &IMFSample,
        mut spare: Option<Vec<u8>>,
    ) -> Option<Vec<u8>> {
        // The buffer is the authority on its own row pitch; the media type only
        // ever *claims* one (task500). They agree at 1920 wide, where the pitch
        // needs no padding, and that is the only width this had ever been run
        // at -- so a window recording, which WGC hands over at whatever odd
        // size the frame happens to be, sheared diagonally. Same principle
        // `src/capture/gpu.rs::read_bgra_texture` already follows on the
        // capture side with D3D11's `RowPitch`.
        if let Some(rgba) = self.convert_from_2d_buffer(sample, &mut spare) {
            return Some(rgba);
        }
        unsafe {
            let buffer = sample.ConvertToContiguousBuffer().ok()?;
            let mut bytes = std::ptr::null_mut();
            let mut length = 0;
            buffer.Lock(&mut bytes, None, Some(&mut length)).ok()?;
            let source = std::slice::from_raw_parts(bytes, length as usize);
            let rgba = self.convert_rows(source, self.stride, spare.take());
            let _ = buffer.Unlock();
            Some(rgba)
        }
    }

    /// The decoder's own D3D11 texture behind `sample` and the slice of it
    /// this frame is, for a reader opened with a device manager (t260915-e6c8).
    ///
    /// `None` for a system-memory buffer, so this is also *the* test for which
    /// of the two shapes a frame arrived in: a reader opened with
    /// `manager: None` can never answer `Some`, and one opened with a manager
    /// can still answer `None` if Media Foundation declined the hardware
    /// transform.
    pub(super) fn dxgi_texture(&self, sample: &IMFSample) -> Option<(ID3D11Texture2D, u32)> {
        if !matches!(self.format, VideoFormat::Nv12) {
            return None;
        }
        unsafe {
            let dxgi: IMFDXGIBuffer = sample.GetBufferByIndex(0).ok()?.cast().ok()?;
            let mut texture: Option<ID3D11Texture2D> = None;
            dxgi.GetResource(
                &ID3D11Texture2D::IID,
                &mut texture as *mut _ as *mut *mut core::ffi::c_void,
            )
            .ok()?;
            Some((texture?, dxgi.GetSubresourceIndex().ok()?))
        }
    }

    /// The sample's NV12 planes copied out tightly packed -- `width` bytes a
    /// row, the luma plane then the interleaved chroma one -- for the GPU path
    /// (t260913-2527), which wants the decoder's own planes rather than the
    /// RGBA `convert` makes of them.
    ///
    /// The copy is what makes the pitch a local problem: what comes back is
    /// packed, so the upload and `nv12_to_rgba` both read it at `width` and
    /// neither has to know what the buffer's pitch was (task500's lesson, paid
    /// once here instead of twice downstream).
    ///
    /// `None` -- meaning "use the CPU pair" -- when the segment decoded to
    /// BGRA rather than NV12, when the frame is an odd size (an NV12 texture
    /// cannot be one), or when the buffer will not report its own pitch.
    pub(super) fn nv12_planes(
        &self,
        sample: &IMFSample,
        spare: Option<Vec<u8>>,
    ) -> Option<Vec<u8>> {
        if !matches!(self.format, VideoFormat::Nv12) {
            return None;
        }
        let (width, height) = (self.width as usize, self.height as usize);
        if width == 0 || height == 0 || width % 2 == 1 || height % 2 == 1 {
            return None;
        }
        let bytes = width * height * 3 / 2;
        let mut planes = super::exact_or_new(spare, bytes);
        with_2d_rows(sample, |source, pitch, _| {
            let pitch = pitch as usize;
            (pitch >= width && pack_nv12_planes(&mut planes, source, pitch, width, height))
                .then_some(())
        })?;
        Some(planes)
    }

    /// The same conversion the fallback does, given the rows and the pitch they
    /// are actually laid out at. Split out so both paths convert identically
    /// and only the source of `pitch` differs.
    fn convert_rows(&self, source: &[u8], pitch: i32, spare: Option<Vec<u8>>) -> Vec<u8> {
        // The size check is the local half of task1640's contract: a spare that
        // is not exactly this frame is dropped here rather than half-written.
        let bytes = super::rgba_bytes(self.width, self.height);
        let mut rgba = super::exact_or_new(spare, bytes);
        match self.format {
            VideoFormat::Nv12 => nv12_to_rgba_into(
                &mut rgba,
                source,
                self.width,
                self.height,
                pitch.unsigned_abs(),
            ),
            VideoFormat::Bgra => {
                swizzle_bgra_into(&mut rgba, source, self.width, self.height, pitch);
            }
        }
        rgba
    }

    /// Rows read at the pitch `IMF2DBuffer2` reports, or `None` to fall back.
    ///
    /// Falls back when the buffer does not implement the interface, and
    /// deliberately also when the pitch comes back negative: that is a
    /// bottom-up buffer, and flipping is out of scope for task500 -- the
    /// contiguous path below copes with the sign already.
    fn convert_from_2d_buffer(
        &self,
        sample: &IMFSample,
        spare: &mut Option<Vec<u8>>,
    ) -> Option<Vec<u8>> {
        with_2d_rows(sample, |source, pitch, length| {
            self.log_pitch_once(pitch, length);
            Some(self.convert_rows(source, pitch, spare.take()))
        })
    }

    /// Task500 Steps 2: says once per process what the media type claimed and
    /// what the buffer actually uses, so "the attribute was absent" and "the
    /// attribute was present but wrong" stop being indistinguishable.
    fn log_pitch_once(&self, pitch: i32, length: u32) {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            tracing::info!(
                target: "task500_pitch",
                width = self.width,
                height = self.height,
                format = ?self.format,
                declared_stride = ?self.declared_stride,
                effective_stride_before = self.stride,
                buffer_pitch = pitch,
                buffer_length = length,
                "row pitch as the decoded buffer reports it"
            );
        });
    }

    /// Next decoded video frame as (local timestamp, swizzled RGBA rows).
    ///
    /// Test-only since t260914-8d69: the prefetch was its last caller in the
    /// shipped engine, and it now stops at [`Self::next_video_sample`] so the
    /// crossing's first frame converts on the worker's one scaling path. What
    /// is left is the decode-and-convert-every-frame control the seek and
    /// crossing comparisons in `tests.rs` measure the split against.
    #[cfg(test)]
    pub(super) fn next_video(&mut self) -> Option<(i64, Vec<u8>)> {
        // Decode plus the BGRA swizzle task200 measured at 5.87ms a 1080p
        // frame unoptimized -- the single biggest per-frame cost in review.
        crate::insight_scope!("playback_decode_video");
        let (timestamp, sample) = self.next_video_sample()?;
        // No spare here: nothing on this path is the engine thread's steady
        // per-frame work, and the recycled buffer belongs to that (task1640).
        Some((timestamp, self.convert(&sample, None)?))
    }

    /// Next decoded audio block as (local timestamp, interleaved f32).
    /// Track 0 -- the capture target -- which is every caller that predates
    /// multi-track audio.
    pub(super) fn next_audio(&mut self) -> Option<(i64, Vec<f32>)> {
        self.next_audio_on(0)
    }

    pub(super) fn next_audio_on(&mut self, track: usize) -> Option<(i64, Vec<f32>)> {
        crate::insight_scope!("playback_decode_audio");
        let stream = *self.audio_streams.get(track)?;
        if self.audio_done.get(track).copied().unwrap_or(true) {
            return None;
        }
        let (timestamp, sample) =
            match read_sample(&self.reader, stream, &mut self.audio_done[track]) {
                Ok(read) => read?,
                Err(error) => {
                    // Task1890 cleared this path of causing the 789ms holes, but
                    // it still ends the track without ENDOFSTREAM and without
                    // setting `audio_done` -- say so instead of going quiet.
                    tracing::warn!(
                        event = "playback_audio_read_failed",
                        track,
                        %error,
                        "audio ReadSample failed; no more audio from this reader"
                    );
                    return None;
                }
            };
        unsafe {
            let buffer = sample.ConvertToContiguousBuffer().ok()?;
            let mut bytes = std::ptr::null_mut();
            let mut length = 0;
            buffer.Lock(&mut bytes, None, Some(&mut length)).ok()?;
            let samples = std::slice::from_raw_parts(
                bytes.cast::<f32>(),
                length as usize / std::mem::size_of::<f32>(),
            )
            .to_vec();
            let _ = buffer.Unlock();
            Some((timestamp, samples))
        }
    }
}

/// The next sample on `stream` and its local timestamp, past the empty reads
/// the Source Reader hands back between samples. `Ok(None)` is the end of the
/// stream, and sets `done`; a failed read is the caller's to report, and
/// leaves `done` alone.
fn read_sample(
    reader: &IMFSourceReader,
    stream: u32,
    done: &mut bool,
) -> windows::core::Result<Option<(i64, IMFSample)>> {
    loop {
        let mut flags = 0;
        let mut timestamp = 0;
        let mut sample = None;
        unsafe {
            reader.ReadSample(
                stream,
                0,
                None,
                Some(&mut flags),
                Some(&mut timestamp),
                Some(&mut sample),
            )?;
        }
        if let Some(sample) = sample {
            return Ok(Some((timestamp, sample)));
        }
        if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
            *done = true;
            return Ok(None);
        }
    }
}

/// The sample's rows as `IMF2DBuffer2` reports them -- `f` gets the bytes from
/// the first row, the pitch and the length -- under one `Lock2DSize` /
/// `Unlock2D` pair. `None` without calling `f` when the buffer is not 2D, the
/// lock fails, or the pitch is not one [`usable_buffer_pitch`] takes.
fn with_2d_rows<T>(sample: &IMFSample, f: impl FnOnce(&[u8], i32, u32) -> Option<T>) -> Option<T> {
    unsafe {
        let two_d: IMF2DBuffer2 = sample.GetBufferByIndex(0).ok()?.cast().ok()?;
        let mut scanline0 = std::ptr::null_mut();
        let mut pitch = 0i32;
        let mut start = std::ptr::null_mut();
        let mut length = 0u32;
        two_d
            .Lock2DSize(
                MF2DBuffer_LockFlags_Read,
                &mut scanline0,
                &mut pitch,
                &mut start,
                &mut length,
            )
            .ok()?;
        let result = usable_buffer_pitch(pitch)
            .filter(|_| !start.is_null())
            .and_then(|pitch| {
                f(
                    std::slice::from_raw_parts(start, length as usize),
                    pitch,
                    length,
                )
            });
        let _ = two_d.Unlock2D();
        result
    }
}

/// The copy loop [`SegmentReader::nv12_planes`] runs, with no Media Foundation
/// in it so it can be tested directly.
///
/// `false` -- meaning "use the CPU pair" -- when `source` is shorter than the
/// layout it claims, rather than reading past it or half-filling `planes`.
///
/// The chroma plane is taken from [`livia_pixels::nv12_plane_rows`], not from
/// `height`: the decoder pads its Y plane to a multiple of 16 rows, and reading
/// chroma at `height` picks up that zeroed padding as U=V=0 -- a bright green
/// band across the top of the picture and every colour below it pulled from a
/// few rows too high. `nv12_to_rgba_into` has always derived it this way; this
/// path (t260913-2527) did not, which is what put the green band back in the
/// live preview while the recorded mp4 -- encoded from BGRA on the capture
/// side, never through here -- stayed correct.
fn pack_nv12_planes(
    planes: &mut [u8],
    source: &[u8],
    pitch: usize,
    width: usize,
    height: usize,
) -> bool {
    let plane_rows = livia_pixels::nv12_plane_rows(source.len(), pitch, height);
    let chroma_rows = height / 2;
    if source.len() < (plane_rows + chroma_rows - 1) * pitch + width {
        return false;
    }
    for row in 0..height {
        planes[row * width..][..width].copy_from_slice(&source[row * pitch..][..width]);
    }
    for row in 0..chroma_rows {
        planes[(height + row) * width..][..width]
            .copy_from_slice(&source[(plane_rows + row) * pitch..][..width]);
    }
    true
}

/// Whether a pitch reported by `IMF2DBuffer2` is one this code will read rows
/// at (task500).
///
/// Only the sign is in question. A positive pitch is the row stride, padding
/// included, and is exactly what the conversion wants. A negative one means the
/// buffer is bottom-up; flipping is out of scope, so it reads as "not usable"
/// and the caller takes the contiguous path, which already handles the sign.
pub(super) fn usable_buffer_pitch(pitch: i32) -> Option<i32> {
    (pitch > 0).then_some(pitch)
}

// Lives in its own package so a dev build can optimize it without optimizing
// this one -- see the workspace root's Cargo.toml (task200).
/// The allocating pair (task1640 left playback on the `_into` half): what the
/// conversion tests compare against, on both sides of the crate edge.
#[cfg(test)]
pub(super) use livia_pixels::{nv12_to_rgba, swizzle_bgra};
pub(super) use livia_pixels::{nv12_to_rgba_into, swizzle_bgra_into};

#[cfg(test)]
mod tests {
    use super::*;

    /// Task500: only a positive pitch is read from the 2D buffer; a bottom-up
    /// one falls back rather than being flipped here.
    #[test]
    fn only_a_top_down_buffer_pitch_is_used() {
        assert_eq!(usable_buffer_pitch(704), Some(704));
        assert_eq!(usable_buffer_pitch(1920), Some(1920));
        assert_eq!(usable_buffer_pitch(-704), None, "bottom-up falls back");
        assert_eq!(usable_buffer_pitch(0), None);
    }

    /// The task500 bug itself, with no Media Foundation involved: an NV12
    /// buffer whose rows are padded out to a wider pitch than the frame.
    ///
    /// 686 wide is one of the sizes that sheared. Reading it back at the
    /// *width* -- what the media type's stride amounted to -- walks 18 bytes
    /// short of a row every row, which is the diagonal seam. Reading it at the
    /// buffer's own pitch reproduces the picture exactly.
    #[test]
    fn nv12_rows_are_read_at_the_buffers_pitch_not_the_frame_width() {
        const WIDTH: u32 = 686;
        const HEIGHT: u32 = 432;
        const PITCH: u32 = 704;
        // Each row is filled with its own row number, so a row read at the
        // wrong offset shows up as the wrong value rather than as noise.
        let rows = HEIGHT as usize;
        let mut buffer = vec![0u8; PITCH as usize * rows * 3 / 2];
        for row in 0..rows {
            let at = row * PITCH as usize;
            buffer[at..at + WIDTH as usize].fill((row % 251) as u8);
        }
        // Flat mid-grey chroma everywhere, so the luma is what the assertions
        // below are reading.
        let uv_base = PITCH as usize * rows;
        buffer[uv_base..].fill(128);

        let correct = nv12_to_rgba(&buffer, WIDTH, HEIGHT, PITCH);
        let sheared = nv12_to_rgba(&buffer, WIDTH, HEIGHT, WIDTH);
        let row_bytes = WIDTH as usize * 4;

        // Every row of the correctly-read frame is one flat colour: the row
        // never straddles two source rows.
        for row in 0..rows {
            let out = &correct[row * row_bytes..(row + 1) * row_bytes];
            let first = out[0];
            assert!(
                out.iter().step_by(4).all(|red| *red == first),
                "row {row} is not a single flat value; the pitch was misread"
            );
        }
        // And the shear is real, so the assertion above is actually load
        // bearing rather than passing for both readings.
        assert_ne!(
            correct, sheared,
            "reading at the frame width must not agree with reading at the pitch"
        );
    }

    /// The green band: the decoder pads its Y plane to a multiple of 16 rows,
    /// so 1080 lines come back in a buffer built for 1088 (measured on this
    /// machine: pitch 1920, length 3,133,440) and the chroma plane does *not*
    /// start at row `height`. Packing from row `height` copies the zeroed luma
    /// padding as chroma -- U=V=0, which is bright green -- and slides every
    /// real chroma row 8 rows late.
    ///
    /// Scaled down here so the assertion is on the values, not the size.
    #[test]
    fn nv12_packing_finds_the_uv_plane_past_the_y_planes_padding() {
        const WIDTH: usize = 8;
        const HEIGHT: usize = 6;
        const PITCH: usize = 16;
        const PLANE_ROWS: usize = 8; // two rows of luma padding
        let mut source = vec![0u8; PITCH * PLANE_ROWS * 3 / 2];
        source[..PITCH * HEIGHT].fill(235);
        source[PITCH * PLANE_ROWS..].fill(200);

        let mut planes = vec![0u8; WIDTH * HEIGHT * 3 / 2];
        assert!(pack_nv12_planes(&mut planes, &source, PITCH, WIDTH, HEIGHT));
        assert!(
            planes[..WIDTH * HEIGHT].iter().all(|luma| *luma == 235),
            "the luma plane is the visible rows"
        );
        assert!(
            planes[WIDTH * HEIGHT..].iter().all(|chroma| *chroma == 200),
            "chroma came from the padding (U=V=0, the green band) instead of the UV plane"
        );
    }

    /// A buffer shorter than the layout it claims is refused outright -- the
    /// caller reads that as "use the CPU pair" -- rather than handing back a
    /// half-filled frame.
    #[test]
    fn nv12_packing_refuses_a_short_buffer() {
        let source = vec![235u8; 16 * 4];
        let mut planes = vec![0u8; 8 * 6 * 3 / 2];
        assert!(!pack_nv12_planes(&mut planes, &source, 16, 8, 6));
    }
}
