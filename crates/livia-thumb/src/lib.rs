//! Explorer thumbnail handler for `.lvb` session containers (task710).
//!
//! Explorer only has one way to show something other than a generic icon for a
//! file type: an in-proc COM `IThumbnailProvider`. So this crate is a DLL, and
//! the installer points the `Liveback.Session` ProgID's ShellEx at it.
//!
//! **It never decodes video.** Every segment already carries a 480x270 JPEG
//! (`SEGMENT_THUMBNAIL_WIDTH` in `capture/worker/setup.rs`), written into the
//! container next to the segment it belongs to. Producing a thumbnail is three
//! reads: the 4 KiB header, the checkpoint record a header slot names, and the
//! thumbnail record the checkpoint's first thumbnail-bearing segment names.
//! That matters -- a 15 minute session is gigabytes, and Explorer asks for
//! thumbnails by the folderful.
//!
//! The container format is **included from `livia`'s source**, not depended on:
//! a `livia` dependency would drag slint+skia into a cdylib, which crashes
//! rustc on this toolchain (see the root `Cargo.toml`, task120). Including the
//! one file keeps the format in one place, so the handler follows it.
//!
//! Initialization is `IInitializeWithStream` rather than `IInitializeWithFile`
//! on purpose: it is what lets the shell run the handler in an isolated host
//! (`dllhost.exe`). A container being recorded into always has a half-written
//! tail, so odd input is the normal case here, and a crash must not take
//! Explorer with it.

// Every build of this crate prints LNK4104 twice: link.exe insists the COM
// entry points below (`DllGetClassObject`, `DllCanUnloadNow`) be exported
// PRIVATE so they stay out of the import library, and rustc's generated
// `lib.def` cannot say PRIVATE. Nothing imports them -- the shell resolves
// them with `GetProcAddress` -- so the warning is noise. `/IGNORE:4104` alone
// does not help: link.exe still prints "creating library ...", which trips the
// same lint. Silencing the lint here is the only knob that ends it.
#![allow(linker_messages)]

// A `#[path]` module resolves *its* children against the directory holding the
// file, so `container.rs`'s own `#[cfg(test)] mod tests;` would land on
// `src/ring_buffer/tests.rs` -- the ring buffer's test tree, not the
// container's. That is why the declaration over there carries an explicit
// `#[path = "container/tests.rs"]`.
#[allow(dead_code)]
#[path = "../../../src/ring_buffer/container.rs"]
mod container;

use std::cell::RefCell;
use std::cmp::Reverse;
use std::ffi::c_void;
use std::io::{self, Read, Seek, SeekFrom};
use std::panic::{catch_unwind, AssertUnwindSafe};

use container::{
    crc32, decode_header, read_record_at, Located, RecordKind, Slot, Snapshot, HEADER_LEN,
    RECORD_HEADER_LEN,
};
use windows::core::BOOL;
use windows::core::{implement, Interface, Ref, GUID, HRESULT};
use windows::Win32::Foundation::{CLASS_E_CLASSNOTAVAILABLE, CLASS_E_NOAGGREGATION, E_FAIL};
use windows::Win32::Graphics::Gdi::{
    CreateDIBSection, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HBITMAP,
};
use windows::Win32::System::Com::{
    IClassFactory, IClassFactory_Impl, IStream, STREAM_SEEK_CUR, STREAM_SEEK_END, STREAM_SEEK_SET,
};
use windows::Win32::UI::Shell::PropertiesSystem::{
    IInitializeWithStream, IInitializeWithStream_Impl,
};
use windows::Win32::UI::Shell::{
    IThumbnailProvider, IThumbnailProvider_Impl, WTSAT_RGB, WTS_ALPHATYPE,
};
use windows_core::IUnknown;

/// The handler's class id. The installer writes it under
/// `HKCU\Software\Classes\CLSID` and points the ProgID's ShellEx at it, so this
/// constant and `installer/liveback.nsi` have to agree.
const CLSID_LIVIA_THUMBNAIL: GUID = GUID::from_u128(0x0d9ec9d9_f746_4c28_8662_3b546bb83cb9);

// ---------- reading the container ----------

/// The JPEG of the first segment that has one, or an error.
///
/// Mirrors `ContainerReader::open_at_checkpoint` + `read_located`, over a
/// `Read + Seek` instead of a `File` so an `IStream` (and a `Cursor`, in tests)
/// can drive it. Deliberately **no fallback to the full scan**: a container
/// with no usable checkpoint was recorded into for less than a checkpoint
/// interval and has no thumbnail to find anyway. Erroring out lets Explorer
/// fall back to the ProgID's icon, which is the right answer.
pub fn first_thumbnail_jpeg<R: Read + Seek>(src: &mut R) -> io::Result<Vec<u8>> {
    let len = src.seek(SeekFrom::End(0))?;
    src.seek(SeekFrom::Start(0))?;
    let mut header_bytes = vec![0u8; HEADER_LEN as usize];
    src.read_exact(&mut header_bytes)?;
    let (_header, slot_a, slot_b) = decode_header(&header_bytes)?;

    // Newest generation first, then the other -- the same order and the same
    // reason as the reader in `livia`: the newest slot routinely points past
    // the end of a container that crashed, while the older one still resolves.
    let mut candidates: Vec<Slot> = [slot_a, slot_b].into_iter().flatten().collect();
    candidates.sort_by_key(|slot| Reverse(slot.generation));
    for slot in candidates {
        let Some(snapshot) = read_checkpoint(src, slot, len) else {
            continue;
        };
        let Some(located) = snapshot
            .segments
            .iter()
            .find_map(|segment| segment.thumbnail)
        else {
            continue;
        };
        return read_located(src, located, len);
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "no thumbnail in this container",
    ))
}

fn read_checkpoint<R: Read + Seek>(src: &mut R, slot: Slot, len: u64) -> Option<Snapshot> {
    let record_offset = slot.offset.saturating_sub(RECORD_HEADER_LEN as u64);
    let record_len = (RECORD_HEADER_LEN as u64).checked_add(slot.len)?;
    if record_offset.checked_add(record_len)? > len {
        return None;
    }
    let mut bytes = vec![0u8; usize::try_from(record_len).ok()?];
    src.seek(SeekFrom::Start(record_offset)).ok()?;
    src.read_exact(&mut bytes).ok()?;
    // Offset 0: `bytes` starts at the record, so the location's spans are
    // slice-relative. Only the payload is used from it.
    let (location, payload) = read_record_at(&bytes, 0)?;
    if location.kind != RecordKind::Checkpoint || location.payload.len != slot.len {
        return None;
    }
    serde_json::from_slice(payload).ok()
}

fn read_located<R: Read + Seek>(src: &mut R, located: Located, len: u64) -> io::Result<Vec<u8>> {
    let bad = |what: &str| io::Error::new(io::ErrorKind::InvalidData, what.to_owned());
    let end = located
        .record
        .offset
        .checked_add(located.record.len)
        .ok_or_else(|| bad("record span overflows"))?;
    if end > len {
        return Err(bad("record lies outside the container"));
    }
    let mut bytes =
        vec![0u8; usize::try_from(located.record.len).map_err(|_| bad("record too big"))?];
    src.seek(SeekFrom::Start(located.record.offset))?;
    src.read_exact(&mut bytes)?;
    if crc32(&bytes) != located.crc {
        return Err(bad("record failed its checksum"));
    }
    let start = usize::try_from(located.body.offset.saturating_sub(located.record.offset))
        .map_err(|_| bad("body offset out of range"))?;
    let body_len = usize::try_from(located.body.len).map_err(|_| bad("body too big"))?;
    bytes
        .get(start..start.saturating_add(body_len))
        .map(<[u8]>::to_vec)
        .ok_or_else(|| bad("body lies outside its record"))
}

// ---------- JPEG -> HBITMAP ----------

/// Decodes the JPEG and hands back a 32bpp top-down DIB section, shrunk to fit
/// `cx` on its long edge. Never enlarges: the shell scales a smaller bitmap
/// itself, and blowing up a 480x270 JPEG to a 1024px request would only cost
/// memory.
fn bitmap_from_jpeg(jpeg: &[u8], cx: u32) -> windows::core::Result<HBITMAP> {
    let decoded = image::load_from_memory_with_format(jpeg, image::ImageFormat::Jpeg)
        .map_err(|_| windows::core::Error::from(E_FAIL))?;
    let fitted = if cx > 0 && (decoded.width() > cx || decoded.height() > cx) {
        decoded.thumbnail(cx, cx)
    } else {
        decoded
    };
    let rgba = fitted.to_rgba8();
    let (width, height) = (rgba.width(), rgba.height());
    if width == 0 || height == 0 {
        return Err(E_FAIL.into());
    }

    let mut info = BITMAPINFO::default();
    info.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
    info.bmiHeader.biWidth = width as i32;
    // Negative height means top-down. A positive one here is the classic
    // upside-down thumbnail.
    info.bmiHeader.biHeight = -(height as i32);
    info.bmiHeader.biPlanes = 1;
    info.bmiHeader.biBitCount = 32;
    info.bmiHeader.biCompression = BI_RGB.0;

    let mut bits: *mut c_void = std::ptr::null_mut();
    let bitmap = unsafe { CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0)? };
    if bits.is_null() {
        return Err(E_FAIL.into());
    }
    let pixels =
        unsafe { std::slice::from_raw_parts_mut(bits.cast::<u8>(), (width * height * 4) as usize) };
    for (out, pixel) in pixels.chunks_exact_mut(4).zip(rgba.pixels()) {
        // BGRA, and opaque: `WTSAT_RGB` tells the shell to ignore alpha, but
        // 255 keeps it honest for anything that looks anyway.
        out[0] = pixel.0[2];
        out[1] = pixel.0[1];
        out[2] = pixel.0[0];
        out[3] = 255;
    }
    Ok(bitmap)
}

// ---------- the COM object ----------

/// `IStream` as `Read + Seek`, so the reader above needs to know nothing about
/// COM (and can be tested over a `Cursor`).
struct StreamSource(IStream);

impl Read for StreamSource {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut read = 0u32;
        let len = u32::try_from(buf.len()).unwrap_or(u32::MAX);
        let hr = unsafe { self.0.Read(buf.as_mut_ptr().cast(), len, Some(&mut read)) };
        if hr.is_err() {
            return Err(io::Error::other(hr.message()));
        }
        Ok(read as usize)
    }
}

impl Seek for StreamSource {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let (origin, by) = match pos {
            SeekFrom::Start(offset) => (STREAM_SEEK_SET, offset as i64),
            SeekFrom::End(offset) => (STREAM_SEEK_END, offset),
            SeekFrom::Current(offset) => (STREAM_SEEK_CUR, offset),
        };
        let mut at = 0u64;
        unsafe { self.0.Seek(by, origin, Some(&mut at)) }
            .map_err(|error| io::Error::other(error.message()))?;
        Ok(at)
    }
}

#[implement(IThumbnailProvider, IInitializeWithStream)]
#[derive(Default)]
struct LvbThumbnail {
    stream: RefCell<Option<IStream>>,
}

impl IInitializeWithStream_Impl for LvbThumbnail_Impl {
    fn Initialize(&self, pstream: Ref<'_, IStream>, _grfmode: u32) -> windows::core::Result<()> {
        let mut slot = self.stream.borrow_mut();
        if slot.is_some() {
            return Err(E_FAIL.into());
        }
        *slot = pstream.cloned();
        Ok(())
    }
}

impl IThumbnailProvider_Impl for LvbThumbnail_Impl {
    fn GetThumbnail(
        &self,
        cx: u32,
        phbmp: *mut HBITMAP,
        pdwalpha: *mut WTS_ALPHATYPE,
    ) -> windows::core::Result<()> {
        // Unwinding out of a COM vtable aborts the host process, and the host
        // here is the shell's. Anything unexpected becomes E_FAIL, which
        // Explorer reads as "no thumbnail, use the icon".
        catch_unwind(AssertUnwindSafe(|| {
            let stream = self
                .stream
                .borrow()
                .clone()
                .ok_or(windows::core::Error::from(E_FAIL))?;
            let jpeg = first_thumbnail_jpeg(&mut StreamSource(stream))
                .map_err(|_| windows::core::Error::from(E_FAIL))?;
            let bitmap = bitmap_from_jpeg(&jpeg, cx)?;
            unsafe {
                *phbmp = bitmap;
                *pdwalpha = WTSAT_RGB;
            }
            Ok(())
        }))
        .unwrap_or_else(|_| Err(E_FAIL.into()))
    }
}

#[implement(IClassFactory)]
struct Factory;

impl IClassFactory_Impl for Factory_Impl {
    fn CreateInstance(
        &self,
        punkouter: Ref<'_, IUnknown>,
        riid: *const GUID,
        ppvobject: *mut *mut c_void,
    ) -> windows::core::Result<()> {
        if !punkouter.is_null() {
            return Err(CLASS_E_NOAGGREGATION.into());
        }
        let instance: IUnknown = LvbThumbnail::default().into();
        unsafe { instance.query(riid, ppvobject).ok() }
    }

    fn LockServer(&self, _flock: BOOL) -> windows::core::Result<()> {
        Ok(())
    }
}

/// # Safety
/// Called by COM with pointers it owns.
#[no_mangle]
pub unsafe extern "system" fn DllGetClassObject(
    rclsid: *const GUID,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT {
    if rclsid.is_null() || riid.is_null() || ppv.is_null() {
        return E_FAIL;
    }
    if *rclsid != CLSID_LIVIA_THUMBNAIL {
        return CLASS_E_CLASSNOTAVAILABLE;
    }
    let factory: IClassFactory = Factory.into();
    factory.query(riid, ppv)
}

/// Always S_FALSE: the handler holds nothing worth unloading for, and staying
/// loaded is what makes the second thumbnail in a folder cheap.
#[no_mangle]
pub extern "system" fn DllCanUnloadNow() -> HRESULT {
    windows::Win32::Foundation::S_FALSE
}

#[cfg(test)]
mod tests {
    use super::*;
    use container::ContainerBuilder;
    use std::io::Cursor;

    fn jpeg(width: u32, height: u32) -> Vec<u8> {
        let image = image::RgbImage::from_fn(width, height, |x, y| {
            image::Rgb([(x * 9) as u8, (y * 9) as u8, 128])
        });
        let mut out = Vec::new();
        image::codecs::jpeg::JpegEncoder::new(&mut Cursor::new(&mut out))
            .encode_image(&image)
            .unwrap();
        out
    }

    fn container_bytes(build: impl FnOnce(ContainerBuilder) -> ContainerBuilder) -> Vec<u8> {
        let dir = std::env::temp_dir().join(format!(
            "livia-thumb-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("fixture.lvb");
        build(ContainerBuilder::new("capture-thumb"))
            .build(&path)
            .unwrap();
        let bytes = std::fs::read(&path).unwrap();
        std::fs::remove_file(&path).ok();
        // The `ThreadId` in `dir`'s name already makes this collision-free
        // (not a flake source); without this the directory itself survived
        // every run, four per full-suite run measured (follow-up of tasks
        // 3230/3240/3290).
        std::fs::remove_dir_all(&dir).ok();
        bytes
    }

    #[test]
    fn the_first_segment_with_a_thumbnail_is_the_one_explorer_gets() {
        let wanted = jpeg(16, 9);
        let other = jpeg(8, 8);
        let bytes = container_bytes(|builder| {
            builder
                .segment(0, 0, 2_000_000, b"first segment")
                // Segment 0 has none, so the pick has to walk past it.
                .segment(1, 2_000_000, 4_000_000, b"second segment")
                .thumbnail(1, &wanted)
                .segment(2, 4_000_000, 6_000_000, b"third segment")
                .thumbnail(2, &other)
                .checkpoint()
        });

        let found = first_thumbnail_jpeg(&mut Cursor::new(bytes)).unwrap();
        assert_eq!(
            found, wanted,
            "the earliest thumbnail is the session's face"
        );
        let decoded = image::load_from_memory_with_format(&found, image::ImageFormat::Jpeg)
            .expect("what comes out is a decodable JPEG");
        assert_eq!((decoded.width(), decoded.height()), (16, 9));
    }

    #[test]
    fn a_container_with_no_thumbnail_yet_errors_instead_of_panicking() {
        let bytes = container_bytes(|builder| {
            builder
                .segment(0, 0, 2_000_000, b"only segment")
                .checkpoint()
        });
        assert!(first_thumbnail_jpeg(&mut Cursor::new(bytes)).is_err());
    }

    #[test]
    fn rubbish_input_errors_instead_of_panicking() {
        // Empty, short, wrong magic, and a header with no checkpoint behind it:
        // every one of these is something Explorer will hand this handler
        // sooner or later, and none may panic -- a panic here would be an
        // abort inside the shell's host process.
        assert!(first_thumbnail_jpeg(&mut Cursor::new(Vec::new())).is_err());
        assert!(first_thumbnail_jpeg(&mut Cursor::new(vec![0u8; 512])).is_err());
        assert!(first_thumbnail_jpeg(&mut Cursor::new(vec![0u8; 8192])).is_err());

        let mut truncated = container_bytes(|builder| {
            builder
                .segment(0, 0, 2_000_000, b"segment")
                .thumbnail(0, &jpeg(8, 8))
                .checkpoint()
        });
        truncated.truncate(HEADER_LEN as usize + 16);
        assert!(first_thumbnail_jpeg(&mut Cursor::new(truncated)).is_err());
    }

    #[test]
    fn a_rotted_thumbnail_record_fails_its_checksum() {
        let picture = jpeg(8, 8);
        let mut bytes = container_bytes(|builder| {
            builder
                .segment(0, 0, 2_000_000, b"segment")
                .thumbnail(0, &picture)
                .checkpoint()
        });
        // Flip a byte inside the stored JPEG itself: the record's CRC has to
        // catch it, rather than the handler feeding rot to the decoder.
        let at = bytes
            .windows(picture.len())
            .position(|window| window == picture)
            .expect("the fixture stores the jpeg verbatim");
        bytes[at + picture.len() / 2] ^= 0xff;
        assert!(first_thumbnail_jpeg(&mut Cursor::new(bytes)).is_err());
    }
}
