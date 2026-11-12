//! The Media Foundation sample chores every encoder and export site repeats:
//! an empty sample with a buffer behind it, bytes in and out of a sample, a
//! compressed buffer rewrapped in a sample of its own, and the `ReadSample`
//! loop.

use windows::Win32::Media::MediaFoundation::{
    IMFSample, IMFSourceReader, MFCreateMemoryBuffer, MFCreateSample, MFSampleExtension_CleanPoint,
    MF_SOURCE_READERF_ENDOFSTREAM,
};

/// A new sample holding one empty memory buffer of `capacity` bytes (at least
/// one: MF refuses a zero-sized buffer).
pub(crate) fn empty(capacity: u32) -> windows::core::Result<IMFSample> {
    unsafe {
        let sample = MFCreateSample()?;
        let buffer = MFCreateMemoryBuffer(capacity.max(1))?;
        sample.AddBuffer(&buffer)?;
        Ok(sample)
    }
}

/// `bytes` copied into a new sample, stamped with `time` and `duration` where
/// given.
pub(crate) fn from_bytes(
    bytes: &[u8],
    time: Option<i64>,
    duration: Option<i64>,
) -> windows::core::Result<IMFSample> {
    unsafe {
        let sample = empty(bytes.len() as u32)?;
        let buffer = sample.GetBufferByIndex(0)?;
        let mut dest = std::ptr::null_mut();
        buffer.Lock(&mut dest, None, None)?;
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), dest, bytes.len());
        buffer.Unlock()?;
        buffer.SetCurrentLength(bytes.len() as u32)?;
        stamp(&sample, time, duration, false)?;
        Ok(sample)
    }
}

/// `source`'s buffer, made contiguous, in a new sample of its own, stamped with
/// `time` and `duration` where given and marked a clean point when `clean`.
///
/// What a sample that has to outlive its producer needs: an async MFT
/// reclaims its output samples on its next event, but the buffer is
/// reference-counted and safe to keep. Which samples are clean points is each
/// caller's rule, so it decides.
pub(crate) fn rewrap(
    source: &IMFSample,
    time: Option<i64>,
    duration: Option<i64>,
    clean: bool,
) -> windows::core::Result<IMFSample> {
    unsafe {
        let buffer = source.ConvertToContiguousBuffer()?;
        let sample = MFCreateSample()?;
        sample.AddBuffer(&buffer)?;
        stamp(&sample, time, duration, clean)?;
        Ok(sample)
    }
}

/// Whether `sample` carries `MFSampleExtension_CleanPoint`.
pub(crate) fn is_clean_point(sample: &IMFSample) -> bool {
    unsafe { sample.GetUINT32(&MFSampleExtension_CleanPoint) }.unwrap_or_default() != 0
}

/// Runs `read` over `sample`'s bytes, made contiguous and locked for the call.
pub(crate) fn with_bytes<T>(
    sample: &IMFSample,
    read: impl FnOnce(&[u8]) -> T,
) -> windows::core::Result<T> {
    unsafe {
        let buffer = sample.ConvertToContiguousBuffer()?;
        let mut data = std::ptr::null_mut();
        let mut length = 0;
        buffer.Lock(&mut data, None, Some(&mut length))?;
        let out = read(std::slice::from_raw_parts(data, length as usize));
        let _ = buffer.Unlock();
        Ok(out)
    }
}

fn stamp(
    sample: &IMFSample,
    time: Option<i64>,
    duration: Option<i64>,
    clean: bool,
) -> windows::core::Result<()> {
    unsafe {
        if let Some(time) = time {
            sample.SetSampleTime(time)?;
        }
        if let Some(duration) = duration {
            sample.SetSampleDuration(duration)?;
        }
        if clean {
            sample.SetUINT32(&MFSampleExtension_CleanPoint, 1)?;
        }
    }
    Ok(())
}

/// Reads `stream` to its end, handing each sample and its timestamp to `each`.
///
/// A read failure goes through `on_err`; an `Err` from `each` stops the walk
/// and comes back as is, which is also how a caller stops early. Iterations
/// that deliver no sample (a gap, a format change) do not reach `each`.
///
/// # Safety
///
/// Media Foundation must be running on this thread, and `reader` opened on it.
pub(crate) unsafe fn for_each_sample<E>(
    reader: &IMFSourceReader,
    stream: u32,
    on_err: impl Fn(windows::core::Error) -> E,
    mut each: impl FnMut(i64, IMFSample) -> Result<(), E>,
) -> Result<(), E> {
    loop {
        let mut flags = 0;
        let mut timestamp = 0;
        let mut sample = None;
        reader
            .ReadSample(
                stream,
                0,
                None,
                Some(&mut flags),
                Some(&mut timestamp),
                Some(&mut sample),
            )
            .map_err(&on_err)?;
        if let Some(sample) = sample {
            each(timestamp, sample)?;
        }
        if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
            return Ok(());
        }
    }
}
