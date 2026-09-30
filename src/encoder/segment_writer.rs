use std::{
    cell::{Cell, RefCell},
    fs,
    path::PathBuf,
    sync::mpsc,
    time::{Duration, Instant},
};

use windows::{
    core::{implement, IUnknown, Interface},
    Win32::Media::MediaFoundation::{
        IMFAsyncCallback, IMFAsyncCallback_Impl, IMFAsyncResult, IMFClockStateSink,
        IMFFinalizableMediaSink, IMFMediaSink, IMFSample, IMFStreamSink, MFCreateFMPEG4MediaSink,
        MFCreateFile, MFCreateMFByteStreamOnStream, MFCreateMediaType, MFCreateSample,
        MF_ACCESSMODE_WRITE, MF_E_NOTACCEPTING, MF_FILEFLAGS_NONE, MF_OPENMODE_FAIL_IF_EXIST,
    },
    Win32::System::Com::IStream,
};

use crate::ring_buffer::store;

/// How long the video arm may wait out a sink answering `MF_E_NOTACCEPTING`
/// the **first** time it offers a sample.
///
/// Only the video arm sleeps (task610). Its refusals really are the transient
/// kind task088 described -- the segment's last sample is written from
/// `finish_sink` immediately before `BeginFinalize`, and waiting a moment is
/// cheaper than failing the segment. A deadline rather than the old
/// `attempt < 50`, because a count never meant a duration: 50 nominal
/// milliseconds of 1ms sleeps measured 80.
///
/// A sample that has *already* been refused for this whole budget is re-offered
/// with no wait at all (`release_held_video`): the budget buys out a transient
/// refusal, and once it has failed to, sleeping again only keeps the capture
/// thread away from the audio the sink is waiting for (t260911-e7f3).
const VIDEO_BACKPRESSURE_BUDGET: Duration = Duration::from_millis(250);

/// Offers a sample to a stream sink. `Ok(false)` means the sink is refusing
/// input right now and the sample has to be offered again later -- it is not
/// an error, and nothing has been consumed.
///
/// **Do not raise `budget` for either arm.** The obvious fix for task480's
/// death -- "the budget is 50ms and the sink refused for 82ms, so widen it" --
/// was measured and is wrong: with a 2000ms budget the audio sink refused for
/// the entire 2000ms, six runs out of six. Sleeping here happens *on the
/// capture thread*, which is the only thread that drains the H.264 encoder into
/// the muxer, and it is that drain the sink is waiting for. Waiting longer only
/// makes the deadlock last longer. The caller has to go back to its loop and
/// come back with the same sample -- which, since t260911-e7f3, is what the
/// video arm does too.
///
/// Silent unless the wait actually engaged, so a healthy recording pays nothing
/// and a log line means something happened.
fn offer_to_sink(
    stream: &IMFStreamSink,
    sample: &IMFSample,
    what: &'static str,
    budget: Duration,
) -> windows::core::Result<bool> {
    let started = Instant::now();
    let mut waited = false;
    loop {
        match unsafe { stream.ProcessSample(sample) } {
            Ok(()) => {
                if waited {
                    tracing::warn!(
                        target: "task610_backpressure",
                        stream = what,
                        waited_ms = started.elapsed().as_millis() as u64,
                        "the sink refused samples before it took one"
                    );
                }
                return Ok(true);
            }
            Err(error) if error.code() == MF_E_NOTACCEPTING => {
                if started.elapsed() >= budget {
                    return Ok(false);
                }
                waited = true;
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(error) => return Err(error),
        }
    }
}

/// A copy of an MFT output sample that outlives the MFT's ownership of it.
///
/// An async MFT releases the output samples it handed over when its next event
/// arrives, so anything that keeps one past the current `poll` -- the muxer's
/// `pending_video` queue, while the sink is refusing (t260911-e7f3) -- has to
/// hold its own sample around the compressed buffer. The buffer itself is
/// reference-counted and safe to keep; that is the same trade
/// `write_video_sample_at` has always made one sample later.
///
/// Only ever called on the refusal path, so a healthy recording pays nothing
/// for it.
pub(super) fn retain_video_sample(sample: &IMFSample) -> Result<IMFSample, EncoderStartError> {
    unsafe {
        let buffer = sample
            .ConvertToContiguousBuffer()
            .map_err(|error| device_error("retained video buffer", error))?;
        let retained =
            MFCreateSample().map_err(|error| device_error("retained video sample", error))?;
        retained
            .AddBuffer(&buffer)
            .map_err(|error| device_error("retained video AddBuffer", error))?;
        if let Ok(time) = sample.GetSampleTime() {
            retained
                .SetSampleTime(time)
                .map_err(|error| device_error("retained video timestamp", error))?;
        }
        if let Ok(duration) = sample.GetSampleDuration() {
            retained
                .SetSampleDuration(duration)
                .map_err(|error| device_error("retained video duration", error))?;
        }
        // The clean point is what `push_video_sample` rotates on; losing it
        // would turn a boundary into an ordinary frame.
        if sample
            .GetUINT32(&windows::Win32::Media::MediaFoundation::MFSampleExtension_CleanPoint)
            .unwrap_or_default()
            != 0
        {
            retained
                .SetUINT32(
                    &windows::Win32::Media::MediaFoundation::MFSampleExtension_CleanPoint,
                    1,
                )
                .map_err(|error| device_error("retained video clean point", error))?;
        }
        Ok(retained)
    }
}

use super::mp4_boxes::{finalize_fragmented_mp4, finalize_fragmented_mp4_bytes};
use super::{
    device_error, validate_config, EncoderConfig, EncoderStartError, EncoderStartErrorKind,
    MfRuntime,
};

/// Where a segment's bytes are being written (task420).
///
/// The file arm is what recording has always done: a `.partial.mp4` that gets
/// renamed into place once it is whole. The memory arm writes into an
/// `IStream` instead and hands the bytes back, which is what appending to a
/// `.lvb` container needs -- there is no file to rename there, a segment is a
/// range inside one.
///
/// **Nothing in the app picks the memory arm yet.** Recording still takes the
/// file arm; task450 is where that flips.
enum SegmentDestination {
    File {
        partial: PathBuf,
        final_path: PathBuf,
    },
    Memory(IStream),
}

pub struct Mp4SegmentWriter {
    _runtime: MfRuntime,
    sink: IMFMediaSink,
    clock: IMFClockStateSink,
    stream: IMFStreamSink,
    /// One stream sink per audio track, in track order: index 0 is the capture
    /// target, the rest are the extra applications (task1260). Empty when the
    /// recording has no audio at all.
    audio_streams: Vec<IMFStreamSink>,
    destination: SegmentDestination,
    /// Segment-relative (100ns) timestamp of the first sample written to each
    /// track, recorded via `Cell` since `write_*_sample_at` take `&self`. Feeds
    /// `finalize_fragmented_mp4` so a track's true first-sample offset survives
    /// into its tfdt instead of being discarded as an implicit 0 (Task062).
    video_first_timestamp_100ns: Cell<Option<i64>>,
    audio_first_timestamp_100ns: Vec<Cell<Option<i64>>>,
    /// The most recent video sample and its segment-relative time, withheld
    /// from the sink until the next one reveals how long it actually lasted
    /// (task201). `RefCell` for the same reason as the `Cell`s above: the
    /// write path takes `&self`.
    held_video: RefCell<Option<(IMFSample, i64)>>,
    /// Whether what is in `held_video` has already been offered to the sink for
    /// a whole `VIDEO_BACKPRESSURE_BUDGET` and refused (t260911-e7f3). It
    /// decides two things: the re-offer costs no wait, and the warning is
    /// logged once per stall rather than once per loop turn.
    held_video_refused: Cell<bool>,
    /// How many samples each stream has actually taken (task610). A sink that
    /// has taken none and is refusing is a different animal from one that is
    /// merely full.
    video_accepted: Cell<u64>,
    audio_accepted: Vec<Cell<u64>>,
}

#[implement(IMFAsyncCallback)]
pub(super) struct Mp4FinalizeCallback {
    pub(super) sink: IMFFinalizableMediaSink,
    pub(super) completion: mpsc::Sender<Result<(), windows::core::Error>>,
}

unsafe impl Send for Mp4FinalizeCallback {}
unsafe impl Sync for Mp4FinalizeCallback {}

impl IMFAsyncCallback_Impl for Mp4FinalizeCallback_Impl {
    fn GetParameters(&self, _flags: *mut u32, _queue: *mut u32) -> windows::core::Result<()> {
        Err(windows::core::Error::from_win32())
    }

    fn Invoke(&self, result: windows::core::Ref<'_, IMFAsyncResult>) -> windows::core::Result<()> {
        let completion = result
            .ok()
            .and_then(|result| unsafe { self.sink.EndFinalize(result) });
        let _ = self.completion.send(completion);
        Ok(())
    }
}

impl Mp4SegmentWriter {
    pub fn create_with_audio(
        config: &EncoderConfig,
        video_media_type: &windows::Win32::Media::MediaFoundation::IMFMediaType,
        aac_media_type: Option<&windows::Win32::Media::MediaFoundation::IMFMediaType>,
        audio_tracks: usize,
        index: u64,
    ) -> Result<Self, EncoderStartError> {
        validate_config(config)?;
        fs::create_dir_all(&config.output_dir).map_err(|error| EncoderStartError {
            kind: EncoderStartErrorKind::Io,
            diagnostics: format!("create segment directory failed: {error}"),
        })?;
        let (partial, final_path) = store::segment_paths(&config.output_dir, index);
        Self::create_at_paths(
            config,
            video_media_type,
            aac_media_type,
            audio_tracks,
            partial,
            final_path,
        )
    }

    pub fn create_at_paths(
        config: &EncoderConfig,
        video_media_type: &windows::Win32::Media::MediaFoundation::IMFMediaType,
        aac_media_type: Option<&windows::Win32::Media::MediaFoundation::IMFMediaType>,
        audio_tracks: usize,
        partial: PathBuf,
        final_path: PathBuf,
    ) -> Result<Self, EncoderStartError> {
        validate_config(config)?;
        if let Some(parent) = partial.parent() {
            fs::create_dir_all(parent).map_err(|error| EncoderStartError {
                kind: EncoderStartErrorKind::Io,
                diagnostics: format!("create MP4 directory failed: {error}"),
            })?;
        }
        let runtime = MfRuntime::start()?;
        let byte_stream = unsafe {
            let path = windows::core::HSTRING::from(partial.to_string_lossy().as_ref());
            MFCreateFile(
                MF_ACCESSMODE_WRITE,
                MF_OPENMODE_FAIL_IF_EXIST,
                MF_FILEFLAGS_NONE,
                &path,
            )
            .map_err(|error| EncoderStartError {
                kind: EncoderStartErrorKind::Io,
                diagnostics: format!("create partial MP4 file failed: {error}"),
            })?
        };
        Self::create_on_byte_stream(
            runtime,
            &byte_stream,
            video_media_type,
            aac_media_type,
            audio_tracks,
            SegmentDestination::File {
                partial,
                final_path,
            },
        )
    }

    /// A segment written into memory rather than onto disk (task420).
    ///
    /// Same sink, same media types, same clock and the same finalize
    /// handshake as the file constructor -- only the byte stream differs, so
    /// everything `Mp4SegmentWriter` knows about segments (the held-frame
    /// release of task201, the first-sample timestamps that feed tfdt, the
    /// `MF_E_NOTACCEPTING` retry) applies unchanged. `finalize_into_bytes`
    /// returns what would have been the file's contents.
    ///
    /// `SHCreateMemStream(NULL)` specifically: task402 measured it producing
    /// byte-identical output to the file sink, and measured
    /// `CreateStreamOnHGlobal` failing sink creation outright with
    /// E_INVALIDARG. Do not swap it.
    pub fn create_in_memory(
        config: &EncoderConfig,
        video_media_type: &windows::Win32::Media::MediaFoundation::IMFMediaType,
        aac_media_type: Option<&windows::Win32::Media::MediaFoundation::IMFMediaType>,
        audio_tracks: usize,
    ) -> Result<Self, EncoderStartError> {
        validate_config(config)?;
        let runtime = MfRuntime::start()?;
        let stream = super::memory_sink::shell_memory_stream()?;
        let byte_stream = unsafe {
            MFCreateMFByteStreamOnStream(&stream).map_err(|error| EncoderStartError {
                kind: EncoderStartErrorKind::Io,
                diagnostics: format!("MFCreateMFByteStreamOnStream failed: {error}"),
            })?
        };
        Self::create_on_byte_stream(
            runtime,
            &byte_stream,
            video_media_type,
            aac_media_type,
            audio_tracks,
            SegmentDestination::Memory(stream),
        )
    }

    /// The half of construction that does not care where the bytes land.
    fn create_on_byte_stream(
        runtime: MfRuntime,
        byte_stream: &windows::Win32::Media::MediaFoundation::IMFByteStream,
        video_media_type: &windows::Win32::Media::MediaFoundation::IMFMediaType,
        aac_media_type: Option<&windows::Win32::Media::MediaFoundation::IMFMediaType>,
        audio_tracks: usize,
        destination: SegmentDestination,
    ) -> Result<Self, EncoderStartError> {
        unsafe {
            let sink = MFCreateFMPEG4MediaSink(byte_stream, video_media_type, aac_media_type)
                .map_err(|error| EncoderStartError {
                    kind: EncoderStartErrorKind::Device,
                    diagnostics: format!("MFCreateFMPEG4MediaSink failed: {error}"),
                })?;
            let sink_stream = sink
                .GetStreamSinkByIndex(0)
                .map_err(|error| EncoderStartError {
                    kind: EncoderStartErrorKind::Device,
                    diagnostics: format!("MP4 video stream setup failed: {error}"),
                })?;
            let mut audio_streams = Vec::new();
            if let Some(aac_media_type) = aac_media_type {
                audio_streams.push(sink.GetStreamSinkByIndex(1).map_err(|error| {
                    EncoderStartError {
                        kind: EncoderStartErrorKind::Device,
                        diagnostics: format!("MP4 audio stream setup failed: {error}"),
                    }
                })?);
                // Extra tracks are added to the sink the factory already built
                // (task1240 proved it is not a fixed-stream sink). The first
                // argument is an *identifier*, not an index, and the factory
                // has already handed out 1 and 2 -- asking for 2 comes back
                // MF_E_STREAMSINK_EXISTS. Query what exists and carry on past
                // the highest rather than assuming either numbering.
                let mut next_identifier = 0;
                for index in 0..sink.GetStreamSinkCount().unwrap_or(0) {
                    if let Ok(existing) = sink.GetStreamSinkByIndex(index) {
                        next_identifier =
                            next_identifier.max(existing.GetIdentifier().unwrap_or(0) + 1);
                    }
                }
                for track in 1..audio_tracks {
                    let media_type = MFCreateMediaType()
                        .map_err(|error| device_error("MP4 extra AAC media type", error))?;
                    aac_media_type
                        .CopyAllItems(&media_type)
                        .map_err(|error| device_error("MP4 extra AAC media type copy", error))?;
                    let stream =
                        sink.AddStreamSink(next_identifier, &media_type)
                            .map_err(|error| EncoderStartError {
                                kind: EncoderStartErrorKind::Device,
                                diagnostics: format!(
                                    "MP4 audio track {track} setup failed: {error}"
                                ),
                            })?;
                    next_identifier += 1;
                    audio_streams.push(stream);
                }
            }
            let clock = sink
                .cast::<IMFClockStateSink>()
                .map_err(|error| device_error("MP4 clock state sink", error))?;
            clock
                .OnClockStart(0, 0)
                .map_err(|error| device_error("MP4 clock start", error))?;
            Ok(Self {
                _runtime: runtime,
                sink,
                clock,
                stream: sink_stream,
                destination,
                video_first_timestamp_100ns: Cell::new(None),
                audio_first_timestamp_100ns: (0..audio_streams.len())
                    .map(|_| Cell::new(None))
                    .collect(),
                held_video: RefCell::new(None),
                held_video_refused: Cell::new(false),
                video_accepted: Cell::new(0),
                audio_accepted: (0..audio_streams.len()).map(|_| Cell::new(0)).collect(),
                audio_streams,
            })
        }
    }

    /// `Ok(false)` means the sink refused the sample; nothing was written and
    /// the caller has to offer it again after letting its loop run (task610).
    /// Track 0 -- the capture target -- which is every caller that predates
    /// multi-track audio.
    pub fn write_aac_sample_at(
        &self,
        sample: &IMFSample,
        segment_timestamp_100ns: i64,
    ) -> Result<bool, EncoderStartError> {
        self.write_aac_sample_on(0, sample, segment_timestamp_100ns)
    }

    pub fn write_aac_sample_on(
        &self,
        track: usize,
        sample: &IMFSample,
        segment_timestamp_100ns: i64,
    ) -> Result<bool, EncoderStartError> {
        crate::insight_scope!("mux_write_aac");
        let Some(stream) = self.audio_streams.get(track) else {
            return Err(EncoderStartError {
                kind: EncoderStartErrorKind::Device,
                diagnostics: format!("MP4 writer has no AAC stream {track}"),
            });
        };
        let first = &self.audio_first_timestamp_100ns[track];
        if first.get().is_none() {
            first.set(Some(segment_timestamp_100ns));
        }
        unsafe {
            let buffer = sample
                .ConvertToContiguousBuffer()
                .map_err(|error| device_error("AAC output buffer", error))?;
            let mux_sample =
                MFCreateSample().map_err(|error| device_error("MP4 AAC sample", error))?;
            mux_sample
                .AddBuffer(&buffer)
                .map_err(|error| device_error("MP4 AAC buffer", error))?;
            mux_sample
                .SetSampleTime(segment_timestamp_100ns)
                .map_err(|error| device_error("MP4 AAC timestamp", error))?;
            mux_sample
                .SetSampleDuration(
                    sample
                        .GetSampleDuration()
                        .map_err(|error| device_error("AAC duration", error))?,
                )
                .map_err(|error| device_error("MP4 AAC duration", error))?;
            // Task088: event-driven audio capture delivers samples in ~10ms
            // bursts instead of the old once-per-video-frame cadence, and this
            // sink stream has no MEStreamSinkRequestSample credit gating, so a
            // burst can outrun the sink's queue and get MF_E_NOTACCEPTING.
            //
            // Zero budget (task610): the caller queues a refused sample and
            // offers it again next time round its loop. Sleeping here would
            // block the only thread that can clear the refusal -- see
            // `offer_to_sink`.
            let accepted = offer_to_sink(stream, &mux_sample, "aac", Duration::ZERO)
                .map_err(|error| device_error("MP4 AAC ProcessSample", error))?;
            if accepted {
                let counter = &self.audio_accepted[track];
                counter.set(counter.get() + 1);
            }
            Ok(accepted)
        }
    }

    /// Queues a video sample, holding it back one place so its duration can be
    /// the real interval to the next one (task201).
    ///
    /// The encoder stamps every input with a nominal `1/frame_rate`, but WGC
    /// only delivers a frame when the content *changes*, so the true intervals
    /// are whatever the target was doing. The fMP4 sink derives each sample's
    /// duration from the next sample's time and falls back to the stamped
    /// value only for the last one in a fragment -- which made every fragment
    /// end ~one frame short and compressed a 1.97s segment's internal
    /// timeline to 1.83s. Holding one back means every sample but the last
    /// carries a measured duration, and `flush_video` gives the last one the
    /// only figure that can't be measured from inside this segment.
    ///
    /// `Ok(false)` means the sink would not take the sample held from last time,
    /// so `sample` has **not** been consumed and the caller has to come back
    /// with it (t260911-e7f3). There is one hold slot, so nothing may be taken
    /// while it is occupied.
    pub fn write_video_sample_at(
        &self,
        sample: &IMFSample,
        segment_timestamp_100ns: i64,
    ) -> Result<bool, EncoderStartError> {
        crate::insight_scope!("mux_write_h264");
        // The one held from last time is now measurable: this sample's time is
        // where it ends. Done before anything is copied, so a refusal costs no
        // buffer work at all.
        if !self.release_held_video(
            Some(segment_timestamp_100ns),
            if self.held_video_refused.get() {
                Duration::ZERO
            } else {
                VIDEO_BACKPRESSURE_BUDGET
            },
        )? {
            return Ok(false);
        }
        if self.video_first_timestamp_100ns.get().is_none() {
            self.video_first_timestamp_100ns
                .set(Some(segment_timestamp_100ns));
        }
        let mux_sample = unsafe {
            // MFT-owned output samples are released when next async event arrives. Keep MP4
            // muxing independent by retaining a contiguous compressed buffer, never re-encoding.
            let buffer = sample
                .ConvertToContiguousBuffer()
                .map_err(|error| device_error("video output buffer", error))?;
            let mux_sample =
                MFCreateSample().map_err(|error| device_error("MP4 video sample", error))?;
            mux_sample
                .AddBuffer(&buffer)
                .map_err(|error| device_error("MP4 video buffer", error))?;
            mux_sample
                .SetSampleTime(segment_timestamp_100ns)
                .map_err(|error| device_error("MP4 timestamp", error))?;
            // Kept as the fallback `flush_video(None)` uses when the interval
            // to the next sample never becomes known (end of recording).
            mux_sample
                .SetSampleDuration(
                    sample
                        .GetSampleDuration()
                        .map_err(|error| device_error("video duration", error))?,
                )
                .map_err(|error| device_error("MP4 duration", error))?;
            if sample
                .GetUINT32(&windows::Win32::Media::MediaFoundation::MFSampleExtension_CleanPoint)
                .unwrap_or_default()
                != 0
            {
                mux_sample
                    .SetUINT32(
                        &windows::Win32::Media::MediaFoundation::MFSampleExtension_CleanPoint,
                        1,
                    )
                    .map_err(|error| device_error("MP4 clean point", error))?;
            }
            mux_sample
        };
        *self.held_video.borrow_mut() = Some((mux_sample, segment_timestamp_100ns));
        Ok(true)
    }

    /// Offers the withheld sample to the sink. `Ok(false)` means it is still
    /// withheld: `budget` ran out, and it stays in the slot to be offered again.
    fn release_held_video(
        &self,
        until_100ns: Option<i64>,
        budget: Duration,
    ) -> Result<bool, EncoderStartError> {
        let Some((held, held_timestamp)) = self.held_video.borrow_mut().take() else {
            return Ok(true);
        };
        unsafe {
            // A non-monotonic `until` would mean a negative duration; the
            // caller's floor (`last_video_pts_100ns + 1`) makes that
            // impossible, and clamping keeps a violation from reaching the
            // sink as a wildly wrong unsigned value.
            if let Some(duration) = until_100ns.map(|until| (until - held_timestamp).max(0)) {
                held.SetSampleDuration(duration)
                    .map_err(|error| device_error("MP4 measured duration", error))?;
            }
            // Unlike the audio arm this one waits -- this stream is the one the
            // sink is usually waiting *for*, so a refusal here is normally the
            // transient kind. It used to *fail* when the wait ran out, on the
            // argument that there is nowhere to queue a video sample. That was
            // true of this struct and false of the pipeline: a sample the sink
            // will not take can go back in the hold slot and be offered again
            // next turn, which is the turn that gets the queued AAC to the sink
            // and clears the refusal (t260911-e7f3). The one thing the slot
            // cannot do is take a *second* sample, so the caller is told the
            // sample it brought was not consumed. The last sample of a segment
            // still has nowhere to go -- `finish_sink` has `BeginFinalize`
            // next -- and that arm alone still fails.
            let accepted = offer_to_sink(&self.stream, &held, "h264", budget).map_err(|error| {
                EncoderStartError {
                    kind: EncoderStartErrorKind::Device,
                    diagnostics: format!("MP4 stream ProcessSample failed: {error}"),
                }
            })?;
            if accepted {
                self.video_accepted.set(self.video_accepted.get() + 1);
                self.held_video_refused.set(false);
                Ok(true)
            } else {
                // Once per stall, not once per re-offer: the retry runs every
                // loop turn and a line each would bury the one that matters.
                if !self.held_video_refused.replace(true) {
                    tracing::warn!(
                        target: "task610_backpressure",
                        held_timestamp,
                        until_100ns = ?until_100ns,
                        video_accepted = self.video_accepted.get(),
                        audio_accepted = ?self
                            .audio_accepted
                            .iter()
                            .map(Cell::get)
                            .collect::<Vec<_>>(),
                        first_video = ?self.video_first_timestamp_100ns.get(),
                        first_audio = ?self
                            .audio_first_timestamp_100ns
                            .iter()
                            .map(Cell::get)
                            .collect::<Vec<_>>(),
                        "the video sink refused for the whole budget; holding the sample for the next pass"
                    );
                }
                *self.held_video.borrow_mut() = Some((held, held_timestamp));
                Ok(false)
            }
        }
    }

    fn finish_sink(&self) -> Result<(), EncoderStartError> {
        // The single flush point: whatever is still held has to reach the sink
        // before finalize or the segment loses its last frame entirely
        // (task201). It keeps the encoder's nominal duration, which is the
        // right convention rather than a fallback -- nothing follows it in this
        // file, so it feeds no tfdt and distorts no timestamp, and the catalog
        // declares this segment's end the same way (`exclusive_segment_end` =
        // last sample + 1/fps). Measuring it against the *next* segment's
        // opening clean point instead would make the file claim a span the
        // manifest does not, and would bake a capture-side stall (task202) into
        // the recording as a phantom multi-hundred-ms final frame.
        // The five stages carry their own scopes (task208): the whole thing runs
        // on the capture thread, and "which of the five" decides whether this is
        // a local optimization or a structural move off that thread.
        //
        // This is the one arm that still dies on a refusal it cannot wait out
        // (t260911-e7f3): every other caller can come back next turn, and this
        // one has `BeginFinalize` next. The full budget, not the zero-wait
        // re-offer, because failing the segment is the alternative.
        {
            crate::insight_scope!("mux_final_1_release_held");
            if !self.release_held_video(None, VIDEO_BACKPRESSURE_BUDGET)? {
                tracing::error!(
                    target: "task610_backpressure",
                    video_accepted = self.video_accepted.get(),
                    audio_accepted = ?self
                        .audio_accepted
                        .iter()
                        .map(Cell::get)
                        .collect::<Vec<_>>(),
                    first_video = ?self.video_first_timestamp_100ns.get(),
                    first_audio = ?self
                        .audio_first_timestamp_100ns
                        .iter()
                        .map(Cell::get)
                        .collect::<Vec<_>>(),
                    "the video sink refused the segment's last sample for the whole budget"
                );
                return Err(EncoderStartError {
                    kind: EncoderStartErrorKind::Device,
                    diagnostics: format!(
                        "MP4 stream ProcessSample refused input for {}ms",
                        VIDEO_BACKPRESSURE_BUDGET.as_millis()
                    ),
                });
            }
        }
        let finalizable = self
            .sink
            .cast::<IMFFinalizableMediaSink>()
            .map_err(|error| device_error("MP4 finalizable media sink", error))?;
        {
            crate::insight_scope!("mux_final_2_begin_finalize");
            let (sender, receiver) = mpsc::channel();
            let callback: IMFAsyncCallback = Mp4FinalizeCallback {
                sink: finalizable.clone(),
                completion: sender,
            }
            .into();
            unsafe {
                finalizable
                    .BeginFinalize(&callback, None::<&IUnknown>)
                    .map_err(|error| device_error("MP4 BeginFinalize", error))?;
            }
            receiver
                .recv_timeout(Duration::from_secs(10))
                .map_err(|error| EncoderStartError {
                    kind: EncoderStartErrorKind::Io,
                    diagnostics: format!("MP4 finalize callback timeout: {error}"),
                })?
                .map_err(|error| device_error("MP4 EndFinalize", error))?;
        }
        {
            crate::insight_scope!("mux_final_3_shutdown");
            unsafe { self.clock.OnClockStop(0) }.map_err(|error| EncoderStartError {
                kind: EncoderStartErrorKind::Io,
                diagnostics: format!("MP4 clock stop failed: {error}"),
            })?;
            unsafe { self.sink.Shutdown() }.map_err(|error| EncoderStartError {
                kind: EncoderStartErrorKind::Io,
                diagnostics: format!("MP4 media sink finalization failed: {error}"),
            })?;
        }
        Ok(())
    }

    /// The tfdt fix-up, which both destinations need and neither can skip:
    /// without it a segment's first-sample offset is lost and the file will
    /// not open (task062/task085).
    ///
    /// Stage 4 of the five `finish_sink` used to own end to end. It moved out
    /// when the memory destination arrived (task420) because the two apply it
    /// to different things -- a path, or a byte array -- but the
    /// `mux_final_4_tfdt` scope stays on both arms so task208's breakdown
    /// still measures the same work.
    fn first_timestamps(&self) -> (i64, Vec<i64>) {
        (
            self.video_first_timestamp_100ns.get().unwrap_or(0),
            self.audio_first_timestamp_100ns
                .iter()
                .map(|first| first.get().unwrap_or(0))
                .collect(),
        )
    }

    /// Closes a **file** segment: flush, tfdt, rename into place.
    ///
    /// Panics nothing and corrupts nothing if called on a memory writer -- it
    /// returns an error instead, because the destination is fixed at
    /// construction and mixing them up is a programming mistake, not a
    /// runtime condition.
    pub fn finalize(self) -> Result<PathBuf, EncoderStartError> {
        // Closing a segment is a sink flush plus a rename, both synchronous on
        // the capture thread -- a prime suspect for a seam-sized stall.
        crate::insight_scope!("mux_finalize_segment");
        let SegmentDestination::File {
            partial,
            final_path,
        } = &self.destination
        else {
            return Err(EncoderStartError {
                kind: EncoderStartErrorKind::Io,
                diagnostics: "finalize() on an in-memory segment writer; use \
                              finalize_into_bytes()"
                    .into(),
            });
        };
        self.finish_sink()?;
        {
            crate::insight_scope!("mux_final_4_tfdt");
            let (video, audio) = self.first_timestamps();
            finalize_fragmented_mp4(partial, video, &audio).map_err(|error| EncoderStartError {
                kind: EncoderStartErrorKind::Io,
                diagnostics: format!("fMP4 tfdt injection failed: {error}"),
            })?;
        }
        crate::insight_scope!("mux_final_5_rename");
        // `store::publish`, not a bare rename: one module owns the moment a
        // partial becomes a real file (task400). Not the index-taking
        // `publish_segment`, because the paths arrived through
        // `create_at_paths` -- the export writer opens one of these on a path
        // of its own choosing, outside any session directory.
        store::publish(partial, final_path).map_err(|error| EncoderStartError {
            kind: EncoderStartErrorKind::Io,
            diagnostics: format!("segment atomic rename failed: {error}"),
        })?;
        Ok(final_path.clone())
    }

    /// Closes an **in-memory** segment and hands back the bytes a file
    /// destination would have contained (task420).
    ///
    /// No stage 5: there is nothing to rename. The container's own append is
    /// what publishes these bytes, and that is the caller's business.
    pub fn finalize_into_bytes(self) -> Result<Vec<u8>, EncoderStartError> {
        crate::insight_scope!("mux_finalize_segment");
        let SegmentDestination::Memory(stream) = &self.destination else {
            return Err(EncoderStartError {
                kind: EncoderStartErrorKind::Io,
                diagnostics: "finalize_into_bytes() on a file segment writer; use finalize()"
                    .into(),
            });
        };
        self.finish_sink()?;
        let bytes = super::memory_sink::read_stream(stream)?;
        crate::insight_scope!("mux_final_4_tfdt");
        let (video, audio) = self.first_timestamps();
        let rewritten = finalize_fragmented_mp4_bytes(&bytes, video, &audio).map_err(|error| {
            EncoderStartError {
                kind: EncoderStartErrorKind::Io,
                diagnostics: format!("fMP4 tfdt injection failed: {error}"),
            }
        })?;
        // `None` means the sink already produced a compliant file that needed
        // no padding either, and the rewrite would have been a copy of its
        // input.
        Ok(rewritten.unwrap_or(bytes))
    }
}
