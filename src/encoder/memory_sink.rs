//! The memory `IStream` `Mp4SegmentWriter::create_in_memory` hands the fMP4
//! sink, and the read-back of what the sink wrote into it.
//!
//! No COM implementation of `IMFByteStream` here, and none needed: Windows
//! already has memory `IStream`s, and `MFCreateMFByteStreamOnStream` adapts
//! one. Task402's spike measured that `MFCreateFMPEG4MediaSink` accepts one
//! (fragmented MP4 seeks backwards and calls `SetLength` while writing) and
//! that `SHCreateMemStream` is the one that works -- `CreateStreamOnHGlobal`
//! failed sink creation with E_INVALIDARG.

use windows::Win32::System::Com::{IStream, STREAM_SEEK_SET};
use windows::Win32::UI::Shell::SHCreateMemStream;

use super::{EncoderStartError, EncoderStartErrorKind};

/// The memory `IStream` the recording path uses, on its own so
/// `Mp4SegmentWriter` does not repeat the call (task420). Shell-backed
/// specifically: `SHCreateMemStream(NULL, 0)`, an empty shell memory stream.
pub fn shell_memory_stream() -> Result<IStream, EncoderStartError> {
    unsafe {
        SHCreateMemStream(None).ok_or_else(|| EncoderStartError {
            kind: EncoderStartErrorKind::Io,
            diagnostics: "SHCreateMemStream returned null".into(),
        })
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
