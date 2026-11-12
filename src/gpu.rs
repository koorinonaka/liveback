//! The D3D11 pieces capture, playback, encode and export all build on: the
//! device and the shader compile (t261025-1b1a). The passes that use them
//! stay with their owners (`capture::gpu`, `playback::gpu`).

use windows::{
    core::{Interface, PCSTR},
    Graphics::DirectX::Direct3D11::IDirect3DDevice,
    Win32::{
        Foundation::{HANDLE, HMODULE},
        Graphics::{
            Direct3D::{Fxc::D3DCompile, ID3DBlob, D3D_DRIVER_TYPE_HARDWARE},
            Direct3D11::{
                D3D11CreateDevice, ID3D11ClassLinkage, ID3D11Device, ID3D11DeviceContext,
                ID3D11PixelShader, ID3D11VertexShader, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                D3D11_CREATE_DEVICE_DEBUG, D3D11_CREATE_DEVICE_FLAG,
                D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION,
            },
            Dxgi::IDXGIDevice,
        },
        System::WinRT::Direct3D11::CreateDirect3D11DeviceFromDXGIDevice,
    },
};

/// What every device here is made with: BGRA for the capture surfaces and the
/// UI's textures, `VIDEO_SUPPORT` for the video processor, the encoder MFT and
/// the DXVA decoder, which refuse a device created without it.
pub(crate) const DEVICE_FLAGS: D3D11_CREATE_DEVICE_FLAG = D3D11_CREATE_DEVICE_FLAG(
    D3D11_CREATE_DEVICE_BGRA_SUPPORT.0 | D3D11_CREATE_DEVICE_VIDEO_SUPPORT.0,
);

/// A hardware device and its immediate context, created with `flags`.
pub(crate) fn create_device(
    flags: D3D11_CREATE_DEVICE_FLAG,
) -> windows::core::Result<(ID3D11Device, ID3D11DeviceContext)> {
    let mut device = None;
    let mut context = None;
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            flags,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )?;
    }
    let device = device.ok_or_else(windows::core::Error::from_win32)?;
    raise_gpu_priority(&device);
    Ok((
        device,
        context.ok_or_else(windows::core::Error::from_win32)?,
    ))
}

#[link(name = "gdi32")]
extern "system" {
    fn D3DKMTSetProcessSchedulingPriorityClass(process: HANDLE, class: i32) -> i32;
}

/// `D3DKMT_SCHEDULINGPRIORITYCLASS_HIGH`. `REALTIME` (5) needs admin.
const SCHEDULING_PRIORITY_CLASS_HIGH: i32 = 4;

/// `LIVEBACK_GPU_PRIORITY=1` raises this process's GPU scheduling class to
/// HIGH (once) and every device's GPU thread priority to +7, for the
/// t261108-0681 A/B: a foreground game's GPU work can keep ours waiting, and
/// a capture copy that waits keeps the WGC frame pool's surface busy, so WGC
/// stops handing over frames. Anything else, unset included, changes nothing.
/// A refusal is one log line; the device is used as it is.
// ponytail: env switch for the A/B only; make it a fixed default or delete it once 0681 rules.
fn raise_gpu_priority(device: &ID3D11Device) {
    if std::env::var("LIVEBACK_GPU_PRIORITY").ok().as_deref() != Some("1") {
        return;
    }
    static CLASS: std::sync::Once = std::sync::Once::new();
    CLASS.call_once(|| {
        // SAFETY: the pseudo handle of this process and a plain enum value.
        let status = unsafe {
            D3DKMTSetProcessSchedulingPriorityClass(
                windows::Win32::System::Threading::GetCurrentProcess(),
                SCHEDULING_PRIORITY_CLASS_HIGH,
            )
        };
        tracing::info!(
            event = "gpu_priority_class",
            status,
            "gpu scheduling class high"
        );
    });
    let thread = device
        .cast::<IDXGIDevice>()
        .and_then(|dxgi| unsafe { dxgi.SetGPUThreadPriority(7) });
    tracing::info!(event = "gpu_thread_priority", applied = thread.is_ok(), error = ?thread.err(), "gpu thread priority +7");
}

/// The capture and encode device: `create_device` with `DEVICE_FLAGS`, plus
/// the WinRT wrapper WGC's frame pool is created on.
///
/// task4280: the debug layer is what lets `leak_probe` list the live objects,
/// so it is asked for when the probe is on. Missing SDK layers is not a reason
/// to fail the recording.
pub(crate) fn create_d3d_device(
) -> windows::core::Result<(ID3D11Device, ID3D11DeviceContext, IDirect3DDevice)> {
    let (device, context) = if crate::capture::leak_probe::enabled() {
        create_device(DEVICE_FLAGS | D3D11_CREATE_DEVICE_DEBUG).or_else(|error| {
            tracing::warn!(event = "d3d_debug_device_unavailable", %error, "debug layer refused");
            create_device(DEVICE_FLAGS)
        })?
    } else {
        create_device(DEVICE_FLAGS)?
    };
    let dxgi: IDXGIDevice = device.cast()?;
    let direct3d = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi)? }.cast()?;
    Ok((device, context, direct3d))
}

/// The full-screen triangle both the capture transform and playback's resample
/// pass draw with (t260913-2527).
pub(crate) const FULLSCREEN_VERTEX_SHADER: &[u8] = b"
struct Output { float4 position : SV_POSITION; float2 uv : TEXCOORD; };
Output main(uint id : SV_VertexID) {
  float2 p[3] = { float2(-1,-1), float2(-1,3), float2(3,-1) };
  float2 u[3] = { float2(0,1), float2(0,-1), float2(2,1) };
  Output o; o.position=float4(p[id],0,1); o.uv=u[id]; return o;
}\0";

unsafe fn compile_shader(source: &[u8], target: PCSTR) -> windows::core::Result<ID3DBlob> {
    let mut code = None;
    let mut errors = None;
    let result = D3DCompile(
        source.as_ptr().cast(),
        source.len().saturating_sub(1),
        PCSTR::null(),
        None,
        None::<&windows::Win32::Graphics::Direct3D::ID3DInclude>,
        PCSTR(c"main".as_ptr().cast()),
        target,
        0,
        0,
        &mut code,
        Some(&mut errors),
    );
    if let Err(error) = result {
        let message = errors.map_or_else(
            || "D3DCompile failed".to_owned(),
            |blob| {
                let bytes = std::slice::from_raw_parts(
                    blob.GetBufferPointer().cast::<u8>(),
                    blob.GetBufferSize(),
                );
                String::from_utf8_lossy(bytes)
                    .trim_end_matches('\0')
                    .to_owned()
            },
        );
        return Err(windows::core::Error::new(error.code(), message));
    }
    let code = code.ok_or_else(windows::core::Error::from_win32)?;
    if code.GetBufferSize() == 0 {
        return Err(windows::core::Error::new(
            windows::core::HRESULT(0x8000_4005u32 as i32),
            "D3DCompile returned an empty shader blob",
        ));
    }
    Ok(code)
}

pub(crate) unsafe fn create_vertex_shader(
    device: &ID3D11Device,
    source: &[u8],
) -> windows::core::Result<ID3D11VertexShader> {
    let code = compile_shader(source, PCSTR(c"vs_5_0".as_ptr().cast()))?;
    let bytes = std::slice::from_raw_parts(code.GetBufferPointer().cast(), code.GetBufferSize());
    let mut shader = None;
    device.CreateVertexShader(bytes, None::<&ID3D11ClassLinkage>, Some(&mut shader))?;
    shader.ok_or_else(windows::core::Error::from_win32)
}

/// The compile error blob comes back inside the `Error`'s message, which is the
/// whole diagnosis for an HLSL typo.
pub(crate) unsafe fn create_pixel_shader(
    device: &ID3D11Device,
    source: &[u8],
) -> windows::core::Result<ID3D11PixelShader> {
    let code = compile_shader(source, PCSTR(c"ps_5_0".as_ptr().cast()))?;
    let bytes = std::slice::from_raw_parts(code.GetBufferPointer().cast(), code.GetBufferSize());
    let mut shader = None;
    device.CreatePixelShader(bytes, None::<&ID3D11ClassLinkage>, Some(&mut shader))?;
    shader.ok_or_else(windows::core::Error::from_win32)
}
