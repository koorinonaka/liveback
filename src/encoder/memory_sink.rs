//! Spike (task402, plan R1): can the fMP4 media sink write into memory?
//!
//! The container plan needs a finished segment as a byte array so it can be
//! appended to `session.lvb` -- today the sink writes a `.partial.mp4` and the
//! bytes are read back off disk. The open question, and the plan's top risk,
//! is whether `MFCreateFMPEG4MediaSink` will accept a byte stream that is not
//! a file: fragmented MP4 seeks backwards and calls `SetLength` while writing,
//! to fix up `moov` and `mfra` after the fact, and a memory stream has to
//! answer both.
//!
//! No COM implementation of `IMFByteStream` here, and none needed: Windows
//! already has memory `IStream`s, and `MFCreateMFByteStreamOnStream` adapts
//! one. This module is that adapter plus enough of `Mp4SegmentWriter`'s sink
//! setup to answer the question. **The recording path is untouched** -- the
//! only caller is the test beside it.

use std::sync::mpsc;
use std::time::Duration;

use windows::core::{IUnknown, Interface};
use windows::Win32::Media::MediaFoundation::{
    IMFAsyncCallback, IMFByteStream, IMFClockStateSink, IMFFinalizableMediaSink, IMFMediaSink,
    IMFMediaType, IMFStreamSink, MFCreateFMPEG4MediaSink, MFCreateMFByteStreamOnStream,
};
use windows::Win32::System::Com::{IStream, STREAM_SEEK_SET};
use windows::Win32::UI::Shell::SHCreateMemStream;

use super::{device_error, EncoderStartError, EncoderStartErrorKind, MfRuntime};

// The `windows` crate does not generate this one (it generates the rest of
// ole32 that this repo uses), and the spike is supposed to try both memory
// streams rather than assume the first is enough -- so it is declared here.
// Nothing outside this module calls it.
#[link(name = "ole32")]
extern "system" {
    fn CreateStreamOnHGlobal(
        hglobal: *mut std::ffi::c_void,
        delete_on_release: i32,
        stream: *mut *mut std::ffi::c_void,
    ) -> i32;
}

/// Which memory `IStream` to build on.
///
/// Both are tried because they differ in the one way that matters: whether a
/// write past the current end grows the stream. `SHCreateMemStream` is handed
/// a fixed buffer, so it may not; `CreateStreamOnHGlobal` with `fDeleteOnRelease`
/// owns a movable HGLOBAL and does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemoryStreamKind {
    /// `SHCreateMemStream(NULL, 0)` -- an empty shell memory stream.
    ShellMemStream,
    /// `CreateStreamOnHGlobal(NULL, TRUE)` -- auto-growing.
    HGlobal,
}

/// The memory `IStream` the recording path uses, on its own so
/// `Mp4SegmentWriter` does not repeat the call (task420). Shell-backed
/// specifically -- see `MemoryStreamKind` for why the HGlobal one is not an
/// option.
pub fn shell_memory_stream() -> Result<IStream, EncoderStartError> {
    MemoryStreamKind::ShellMemStream.create()
}

impl MemoryStreamKind {
    pub const ALL: [MemoryStreamKind; 2] =
        [MemoryStreamKind::ShellMemStream, MemoryStreamKind::HGlobal];

    fn create(self) -> Result<IStream, EncoderStartError> {
        match self {
            MemoryStreamKind::ShellMemStream => unsafe {
                SHCreateMemStream(None).ok_or_else(|| EncoderStartError {
                    kind: EncoderStartErrorKind::Io,
                    diagnostics: "SHCreateMemStream returned null".into(),
                })
            },
            MemoryStreamKind::HGlobal => unsafe {
                let mut raw: *mut std::ffi::c_void = std::ptr::null_mut();
                let hresult = CreateStreamOnHGlobal(std::ptr::null_mut(), 1, &mut raw);
                if hresult < 0 || raw.is_null() {
                    return Err(EncoderStartError {
                        kind: EncoderStartErrorKind::Io,
                        diagnostics: format!("CreateStreamOnHGlobal failed: 0x{hresult:08x}"),
                    });
                }
                Ok(IStream::from_raw(raw))
            },
        }
    }
}

/// Where a spike sink should put its bytes.
///
/// The file arm exists so the test can build two sinks that differ in
/// *nothing but this* -- same media types, same clock, same finalize -- and
/// compare what comes out. Without it, "the memory one produced a different
/// sample count" could just as easily be two hardware encoder runs disagreeing.
#[derive(Clone, Debug)]
pub enum SinkTarget {
    Memory(MemoryStreamKind),
    File(std::path::PathBuf),
}

/// An fMP4 media sink with the same media types and the same clock handling
/// `Mp4SegmentWriter::create_at_paths` uses, pointed wherever `SinkTarget`
/// says.
pub struct MemoryFmp4Sink {
    _runtime: MfRuntime,
    sink: IMFMediaSink,
    clock: IMFClockStateSink,
    pub video: IMFStreamSink,
    pub audio: Option<IMFStreamSink>,
    stream: Option<IStream>,
    path: Option<std::path::PathBuf>,
    _byte_stream: IMFByteStream,
}

impl MemoryFmp4Sink {
    pub fn create(
        target: SinkTarget,
        video_media_type: &IMFMediaType,
        aac_media_type: Option<&IMFMediaType>,
    ) -> Result<Self, EncoderStartError> {
        let runtime = MfRuntime::start()?;
        let (stream, path) = match &target {
            SinkTarget::Memory(kind) => (Some(kind.create()?), None),
            SinkTarget::File(path) => (None, Some(path.clone())),
        };
        unsafe {
            let byte_stream = match (&stream, &path) {
                (Some(stream), _) => {
                    MFCreateMFByteStreamOnStream(stream).map_err(|error| EncoderStartError {
                        kind: EncoderStartErrorKind::Io,
                        diagnostics: format!("MFCreateMFByteStreamOnStream failed: {error}"),
                    })?
                }
                (None, Some(path)) => {
                    let wide = windows::core::HSTRING::from(path.to_string_lossy().as_ref());
                    windows::Win32::Media::MediaFoundation::MFCreateFile(
                        windows::Win32::Media::MediaFoundation::MF_ACCESSMODE_WRITE,
                        windows::Win32::Media::MediaFoundation::MF_OPENMODE_DELETE_IF_EXIST,
                        windows::Win32::Media::MediaFoundation::MF_FILEFLAGS_NONE,
                        &wide,
                    )
                    .map_err(|error| EncoderStartError {
                        kind: EncoderStartErrorKind::Io,
                        diagnostics: format!("MFCreateFile failed: {error}"),
                    })?
                }
                (None, None) => unreachable!("a target is always one or the other"),
            };
            let sink = MFCreateFMPEG4MediaSink(&byte_stream, video_media_type, aac_media_type)
                .map_err(|error| EncoderStartError {
                    kind: EncoderStartErrorKind::Device,
                    diagnostics: format!("MFCreateFMPEG4MediaSink on memory failed: {error}"),
                })?;
            let video = sink
                .GetStreamSinkByIndex(0)
                .map_err(|error| device_error("memory MP4 video stream", error))?;
            let audio = if aac_media_type.is_some() {
                Some(
                    sink.GetStreamSinkByIndex(1)
                        .map_err(|error| device_error("memory MP4 audio stream", error))?,
                )
            } else {
                None
            };
            let clock = sink
                .cast::<IMFClockStateSink>()
                .map_err(|error| device_error("memory MP4 clock state sink", error))?;
            clock
                .OnClockStart(0, 0)
                .map_err(|error| device_error("memory MP4 clock start", error))?;
            Ok(Self {
                _runtime: runtime,
                sink,
                clock,
                video,
                audio,
                stream,
                path,
                _byte_stream: byte_stream,
            })
        }
    }

    /// Finalizes exactly as `Mp4SegmentWriter::finish_sink` does -- the same
    /// `BeginFinalize`/`EndFinalize` handshake, clock stop and `Shutdown` --
    /// and then reads the stream back instead of leaving a file behind.
    pub fn finish(self) -> Result<Vec<u8>, EncoderStartError> {
        let finalizable = self
            .sink
            .cast::<IMFFinalizableMediaSink>()
            .map_err(|error| device_error("memory MP4 finalizable sink", error))?;
        let (sender, receiver) = mpsc::channel();
        let callback: IMFAsyncCallback = super::segment_writer::Mp4FinalizeCallback {
            sink: finalizable.clone(),
            completion: sender,
        }
        .into();
        unsafe {
            finalizable
                .BeginFinalize(&callback, None::<&IUnknown>)
                .map_err(|error| device_error("memory MP4 BeginFinalize", error))?;
        }
        receiver
            .recv_timeout(Duration::from_secs(10))
            .map_err(|error| EncoderStartError {
                kind: EncoderStartErrorKind::Io,
                diagnostics: format!("memory MP4 finalize callback timeout: {error}"),
            })?
            .map_err(|error| device_error("memory MP4 EndFinalize", error))?;
        unsafe { self.clock.OnClockStop(0) }.map_err(|error| EncoderStartError {
            kind: EncoderStartErrorKind::Io,
            diagnostics: format!("memory MP4 clock stop failed: {error}"),
        })?;
        unsafe { self.sink.Shutdown() }.map_err(|error| EncoderStartError {
            kind: EncoderStartErrorKind::Io,
            diagnostics: format!("memory MP4 sink shutdown failed: {error}"),
        })?;
        match (&self.stream, &self.path) {
            (Some(stream), _) => read_stream(stream),
            (None, Some(path)) => std::fs::read(path).map_err(|error| EncoderStartError {
                kind: EncoderStartErrorKind::Io,
                diagnostics: format!("reading the file sink back failed: {error}"),
            }),
            (None, None) => unreachable!("a target is always one or the other"),
        }
    }
}

/// The whole stream, from the top. `Stat` for the length, seek to 0, read.
pub fn read_stream(stream: &IStream) -> Result<Vec<u8>, EncoderStartError> {
    unsafe {
        let mut stat = Default::default();
        stream
            .Stat(&mut stat, windows::Win32::System::Com::STATFLAG_NONAME)
            .map_err(|error| EncoderStartError {
                kind: EncoderStartErrorKind::Io,
                diagnostics: format!("IStream::Stat failed: {error}"),
            })?;
        let len = stat.cbSize as usize;
        stream
            .Seek(0, STREAM_SEEK_SET, None)
            .map_err(|error| EncoderStartError {
                kind: EncoderStartErrorKind::Io,
                diagnostics: format!("IStream::Seek failed: {error}"),
            })?;
        let mut out = vec![0u8; len];
        let mut read = 0u32;
        stream
            .Read(
                out.as_mut_ptr().cast(),
                len as u32,
                Some(std::ptr::addr_of_mut!(read)),
            )
            .ok()
            .map_err(|error| EncoderStartError {
                kind: EncoderStartErrorKind::Io,
                diagnostics: format!("IStream::Read failed: {error}"),
            })?;
        out.truncate(read as usize);
        Ok(out)
    }
}
