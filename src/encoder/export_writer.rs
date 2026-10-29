use std::{path::PathBuf, sync::mpsc};

use windows::{
    core::{implement, Interface, PCWSTR},
    Win32::Media::MediaFoundation::{
        IMFSample, IMFSinkWriter, IMFSinkWriterCallback, IMFSinkWriterCallback_Impl,
        IMFSinkWriterEx, MFCreateAttributes, MFCreateFile, MFCreateSample,
        MFCreateSinkWriterFromURL, MFSampleExtension_CleanPoint, MFTranscodeContainerType_MPEG4,
        MF_ACCESSMODE_WRITE, MF_E_INVALIDINDEX, MF_FILEFLAGS_NONE, MF_OPENMODE_FAIL_IF_EXIST,
        MF_READWRITE_DISABLE_CONVERTERS, MF_SINK_WRITER_ASYNC_CALLBACK,
        MF_SINK_WRITER_DISABLE_THROTTLING, MF_TRANSCODE_CONTAINERTYPE,
    },
};

use super::{device_error, EncoderStartError, EncoderStartErrorKind, MfRuntime};

pub struct Mp4ExportWriter {
    _runtime: MfRuntime,
    writer: IMFSinkWriter,
    _finalize_callback: IMFSinkWriterCallback,
    finalize_completion: mpsc::Receiver<windows::core::HRESULT>,
    video_stream: u32,
    /// One sink stream per audio track, in track order (task1280). Empty when
    /// the source has no audio at all -- a real recorded segment can be
    /// video-only (see `capture` task085), and such an export degrades to a
    /// video-only MP4 instead of failing.
    audio_streams: Vec<u32>,
    partial: PathBuf,
}

#[implement(IMFSinkWriterCallback)]
struct SinkWriterFinalizeCallback {
    completion: mpsc::Sender<windows::core::HRESULT>,
}

unsafe impl Send for SinkWriterFinalizeCallback {}
unsafe impl Sync for SinkWriterFinalizeCallback {}

impl IMFSinkWriterCallback_Impl for SinkWriterFinalizeCallback_Impl {
    fn OnFinalize(&self, status: windows::core::HRESULT) -> windows::core::Result<()> {
        let _ = self.completion.send(status);
        Ok(())
    }

    fn OnMarker(
        &self,
        _stream: u32,
        _context: *const core::ffi::c_void,
    ) -> windows::core::Result<()> {
        Ok(())
    }
}

impl Mp4ExportWriter {
    /// `video` is the source's own compressed media type, whatever codec it
    /// names: H.264 or AV1 (task1770). Nothing here inspects it -- the sink
    /// writer is handed the type and writes the matching sample entry, the same
    /// way the recording muxer does.
    pub fn create(
        video: &windows::Win32::Media::MediaFoundation::IMFMediaType,
        aac: &[windows::Win32::Media::MediaFoundation::IMFMediaType],
        partial: PathBuf,
    ) -> Result<Self, EncoderStartError> {
        let runtime = MfRuntime::start()?;
        unsafe {
            let (finalize_sender, finalize_completion) = mpsc::channel();
            let finalize_callback: IMFSinkWriterCallback = SinkWriterFinalizeCallback {
                completion: finalize_sender,
            }
            .into();
            let stream = MFCreateFile(
                MF_ACCESSMODE_WRITE,
                MF_OPENMODE_FAIL_IF_EXIST,
                MF_FILEFLAGS_NONE,
                &windows::core::HSTRING::from(partial.to_string_lossy().as_ref()),
            )
            .map_err(|error| device_error("create export partial", error))?;
            let mut attrs = None;
            MFCreateAttributes(&mut attrs, 1)
                .map_err(|error| device_error("export sink attributes", error))?;
            let attrs = attrs.ok_or_else(|| EncoderStartError {
                kind: EncoderStartErrorKind::Device,
                diagnostics: "export sink attributes unavailable".into(),
            })?;
            attrs
                .SetGUID(&MF_TRANSCODE_CONTAINERTYPE, &MFTranscodeContainerType_MPEG4)
                .map_err(|error| device_error("export MPEG4 container", error))?;
            attrs
                .SetUINT32(&MF_SINK_WRITER_DISABLE_THROTTLING, 1)
                .map_err(|error| device_error("disable export throttling", error))?;
            attrs
                .SetUINT32(&MF_READWRITE_DISABLE_CONVERTERS, 1)
                .map_err(|error| device_error("disable export converters", error))?;
            attrs
                .SetUnknown(&MF_SINK_WRITER_ASYNC_CALLBACK, &finalize_callback)
                .map_err(|error| device_error("set export finalize callback", error))?;
            if attrs
                .GetUINT32(&MF_SINK_WRITER_DISABLE_THROTTLING)
                .map_err(|error| device_error("read export throttling setting", error))?
                != 1
                || attrs
                    .GetUINT32(&MF_READWRITE_DISABLE_CONVERTERS)
                    .map_err(|error| device_error("read export converter setting", error))?
                    != 1
            {
                return Err(EncoderStartError {
                    kind: EncoderStartErrorKind::Device,
                    diagnostics: "export SinkWriter安全属性のread-backに失敗しました".into(),
                });
            }
            let writer = MFCreateSinkWriterFromURL(PCWSTR::null(), &stream, &attrs)
                .map_err(|error| device_error("create export sink writer", error))?;
            let video_stream = writer
                .AddStream(video)
                .map_err(|error| device_error("add export video", error))?;
            // In track order, and that order is the only identification an
            // MP4 written by the SinkWriter has: it cannot name a track
            // (task1280).
            let mut audio_streams = Vec::with_capacity(aac.len());
            for media_type in aac {
                audio_streams.push(
                    writer
                        .AddStream(media_type)
                        .map_err(|error| device_error("add export AAC", error))?,
                );
            }
            writer
                .SetInputMediaType(
                    video_stream,
                    video,
                    None::<&windows::Win32::Media::MediaFoundation::IMFAttributes>,
                )
                .map_err(|error| device_error("set export video input", error))?;
            for (stream, media_type) in audio_streams.iter().zip(aac.iter()) {
                writer
                    .SetInputMediaType(
                        *stream,
                        media_type,
                        None::<&windows::Win32::Media::MediaFoundation::IMFAttributes>,
                    )
                    .map_err(|error| device_error("set export AAC input", error))?;
            }
            verify_sink_writer_has_no_transforms(&writer, video_stream, "video")?;
            for stream in &audio_streams {
                verify_sink_writer_has_no_transforms(&writer, *stream, "audio")?;
            }
            writer
                .BeginWriting()
                .map_err(|error| device_error("begin export writer", error))?;
            Ok(Self {
                _runtime: runtime,
                writer,
                _finalize_callback: finalize_callback,
                finalize_completion,
                video_stream,
                audio_streams,
                partial,
            })
        }
    }
    pub fn write_video(&self, sample: &IMFSample, timestamp: i64) -> Result<(), EncoderStartError> {
        self.write(sample, timestamp, self.video_stream, "映像")
    }
    pub fn write_aac(
        &self,
        track: usize,
        sample: &IMFSample,
        timestamp: i64,
    ) -> Result<(), EncoderStartError> {
        let Some(stream) = self.audio_streams.get(track).copied() else {
            return Err(EncoderStartError {
                kind: EncoderStartErrorKind::Device,
                diagnostics: format!("export writer has no AAC stream {track}"),
            });
        };
        self.write(sample, timestamp, stream, "AAC")
    }
    fn write(
        &self,
        sample: &IMFSample,
        timestamp: i64,
        stream: u32,
        label: &str,
    ) -> Result<(), EncoderStartError> {
        unsafe {
            let buffer = sample
                .ConvertToContiguousBuffer()
                .map_err(|error| device_error(&format!("export {label} buffer"), error))?;
            let mux = MFCreateSample()
                .map_err(|error| device_error(&format!("export {label} sample"), error))?;
            mux.AddBuffer(&buffer)
                .map_err(|error| device_error(&format!("export {label} add buffer"), error))?;
            mux.SetSampleTime(timestamp)
                .map_err(|error| device_error(&format!("export {label} timestamp"), error))?;
            mux.SetSampleDuration(
                sample
                    .GetSampleDuration()
                    .map_err(|error| device_error(&format!("export {label} duration"), error))?,
            )
            .map_err(|error| device_error(&format!("export {label} set duration"), error))?;
            // The muxer learns which samples are key frames from this
            // attribute and from nothing else. Without it the MPEG4 sink
            // writes `stss` with `entry_count = 0`, and an index that says
            // "no sync samples" is not a missing index -- MF's MP4 source
            // reads it and drops every seek to sample 0: `SetCurrentPosition`
            // returns `S_OK`, `GetCharacteristics` still advertises
            // `CAN_SEEK`, and playback restarts from 0.000 s every time
            // (task3480 measured 908.9 ms average on a 30 s clip and 10.1 s
            // worst case on a 3-minute one, against 50.6 ms for the same
            // bitstream remuxed by ffmpeg -- the only difference between the
            // two files was `stss`).
            //
            // Copied only when the source sample carries it, exactly as
            // `segment_writer::write_video_sample_at` does on the recording
            // side. Never stamped unconditionally: that would list delta
            // frames as sync samples, and a seek landing on a non-key frame
            // decodes garbage -- an index that lies is worse than none. The
            // conditional form is also what makes this safe for `write_aac`,
            // which shares this method: every AAC frame really is a clean
            // point, so copying the flag it already carries states a fact.
            if sample
                .GetUINT32(&MFSampleExtension_CleanPoint)
                .unwrap_or_default()
                != 0
            {
                mux.SetUINT32(&MFSampleExtension_CleanPoint, 1)
                    .map_err(|error| device_error(&format!("export {label} clean point"), error))?;
            }
            self.writer
                .WriteSample(stream, &mux)
                .map_err(|error| device_error(&format!("export {label} WriteSample"), error))
        }
    }
    pub fn finalize(self) -> Result<PathBuf, EncoderStartError> {
        unsafe { self.writer.Finalize() }
            .map_err(|error| device_error("export SinkWriter Finalize", error))?;
        // Bounded like `Mp4SegmentWriter::finish_sink`, only more generous
        // (an export part can be minutes of media): the helper's heartbeat
        // keeps ticking whether or not MF ever delivers OnFinalize, so an
        // unbounded `recv()` here hung the export past every watchdog.
        let status = self
            .finalize_completion
            .recv_timeout(std::time::Duration::from_secs(120))
            .map_err(|error| EncoderStartError {
                kind: EncoderStartErrorKind::Device,
                diagnostics: format!("export SinkWriter OnFinalizeを受信できません: {error}"),
            })?;
        if status.is_err() {
            return Err(device_error(
                "export SinkWriter OnFinalize",
                windows::core::Error::from(status),
            ));
        }
        Ok(self.partial)
    }
}

fn verify_sink_writer_has_no_transforms(
    writer: &IMFSinkWriter,
    stream: u32,
    label: &str,
) -> Result<(), EncoderStartError> {
    unsafe {
        let writer = writer.cast::<IMFSinkWriterEx>().map_err(|error| {
            device_error(
                &format!("export {label} transform inspection unavailable"),
                error,
            )
        })?;
        let mut category = windows::core::GUID::zeroed();
        let mut transform = None;
        match writer.GetTransformForStream(stream, 0, Some(&mut category), &mut transform) {
            Ok(()) => Err(EncoderStartError {
                kind: EncoderStartErrorKind::Device,
                diagnostics: format!("export {label}に隠れたMedia Foundation transformがあります"),
            }),
            Err(error) if error.code() == MF_E_INVALIDINDEX => Ok(()),
            Err(error) => Err(device_error(
                &format!("export {label} transform inspection"),
                error,
            )),
        }
    }
}
