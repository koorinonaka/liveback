//! Where a recording's GPU memory goes after it stops (task4280). Diagnostics
//! only, and off unless `LIVEBACK_D3D_DEBUG=1` is set in the environment the
//! app was launched from -- a shipping run never reaches anything below.
//!
//! Two instruments, because each is blind where the other sees:
//!
//! - [`report_live_objects`] creates the capture device with the D3D11 debug
//!   layer and asks DXGI for every live D3D/DXGI object in the process, with
//!   its public `Refcount` *and* `IntRef`. A D3D11 device is kept alive by its
//!   children through the internal count, which the public refcount below
//!   cannot see.
//! - [`com_refcount`] reads the public COM refcount of any interface. It is the
//!   only one of the two that sees WinRT objects (the WGC pool, session, item
//!   and the interop `IDirect3DDevice`), which DXGI's report does not list.
//!
//! The report goes to the log file as `info!`: the file layer drops `debug!`.

use std::sync::OnceLock;

use windows::{
    core::Interface,
    Win32::Graphics::Dxgi::{
        DXGIGetDebugInterface1, IDXGIDebug, IDXGIInfoQueue, DXGI_DEBUG_ALL, DXGI_DEBUG_RLO_DETAIL,
        DXGI_INFO_QUEUE_MESSAGE,
    },
};

/// Whether this process was launched with `LIVEBACK_D3D_DEBUG=1`.
pub(crate) fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("LIVEBACK_D3D_DEBUG").is_some_and(|v| v == "1"))
}

/// The public COM refcount of `interface`, counting the caller's own
/// reference: a value nobody else holds reads 1.
///
/// Interfaces cast from one object (`ID3D11Device` / `IDXGIDevice` /
/// `ID3D11VideoDevice`) share one count, so the expectation is "how many
/// references *this code* holds to that object right now", not 1.
///
/// # Safety
/// `interface` must be a live COM pointer, which a borrowed windows-rs
/// interface always is.
pub(crate) unsafe fn com_refcount<I: Interface>(interface: &I) -> u32 {
    let raw = interface.as_raw();
    let vtable = unsafe { &**(raw as *const *const windows::core::IUnknown_Vtbl) };
    let after_add = unsafe { (vtable.AddRef)(raw) };
    unsafe { (vtable.Release)(raw) };
    after_add - 1
}

/// Logs every live D3D/DXGI object in the process under `label`, one
/// `d3d_live_object` line each, bracketed by `d3d_live_objects` with the count.
///
/// Only objects created on a debug-layer device are listed, which with
/// `LIVEBACK_D3D_DEBUG=1` is every device `create_d3d_device` makes.
pub(crate) fn report_live_objects(label: &'static str) {
    if !enabled() {
        return;
    }
    unsafe {
        let queue: IDXGIInfoQueue = match DXGIGetDebugInterface1(0) {
            Ok(queue) => queue,
            Err(error) => {
                tracing::warn!(event = "d3d_live_objects_unavailable", label, %error, "no DXGI info queue");
                return;
            }
        };
        let debug: IDXGIDebug = match DXGIGetDebugInterface1(0) {
            Ok(debug) => debug,
            Err(error) => {
                tracing::warn!(event = "d3d_live_objects_unavailable", label, %error, "no DXGI debug");
                return;
            }
        };
        // Whatever the debug layer said while recording is not this report.
        queue.ClearStoredMessages(DXGI_DEBUG_ALL);
        if let Err(error) = debug.ReportLiveObjects(DXGI_DEBUG_ALL, DXGI_DEBUG_RLO_DETAIL) {
            tracing::warn!(event = "d3d_live_objects_unavailable", label, %error, "ReportLiveObjects failed");
            return;
        }
        let count = queue.GetNumStoredMessages(DXGI_DEBUG_ALL);
        tracing::info!(event = "d3d_live_objects", label, count, "live D3D objects");
        for index in 0..count {
            let mut length = 0usize;
            if queue
                .GetMessage(DXGI_DEBUG_ALL, index, None, &mut length)
                .is_err()
                || length == 0
            {
                continue;
            }
            // u64 storage: the message struct is 8-aligned, a byte buffer is not.
            let mut storage = vec![0u64; length.div_ceil(8)];
            let message = storage.as_mut_ptr().cast::<DXGI_INFO_QUEUE_MESSAGE>();
            if queue
                .GetMessage(DXGI_DEBUG_ALL, index, Some(message), &mut length)
                .is_err()
            {
                continue;
            }
            let message = &*message;
            let text = if message.pDescription.is_null() {
                String::new()
            } else {
                let bytes =
                    std::slice::from_raw_parts(message.pDescription, message.DescriptionByteLength);
                String::from_utf8_lossy(bytes)
                    .trim_end_matches('\0')
                    .to_owned()
            };
            tracing::info!(
                event = "d3d_live_object",
                label,
                index,
                text,
                "live D3D object"
            );
        }
        queue.ClearStoredMessages(DXGI_DEBUG_ALL);
    }
}

/// Whether `name` is listed in `LIVEBACK_4280_TEARDOWN` (comma separated).
/// Candidate teardown steps are tried from one build by relaunching with a
/// different value, instead of one build per guess.
pub(crate) fn teardown_experiment(name: &str) -> bool {
    std::env::var("LIVEBACK_4280_TEARDOWN")
        .is_ok_and(|value| value.split(',').any(|item| item.trim() == name))
}
