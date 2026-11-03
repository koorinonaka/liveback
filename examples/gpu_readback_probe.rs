//! What a playback frame would cost to read back from the GPU, for task
//! t260913-2527.
//!
//! slint 1.17's only GPU-texture inputs are OpenGL (`BorrowedOpenGLTextureBuilder`)
//! and WGPU (`unstable-wgpu-28/29`); this app runs skia on a D3D11 surface, so a
//! frame converted and scaled on the GPU has to come back to the CPU before slint
//! can take it. That readback is the whole question: if it costs more than the
//! CPU convert + downscale it would replace, the GPU route does not pay.
//!
//! Times both directions at the sizes playback actually deals in: the readback
//! (`CopyResource` into a STAGING texture plus `Map`/copy/`Unmap`) and the upload
//! (`Map(WRITE_DISCARD)` plus copy), because the playback decoder hands over
//! system memory today -- there is no `IMFDXGIDeviceManager` on that reader -- so
//! a GPU pass would have to put the frame *on* the GPU first.
//!
//! `cargo run --example gpu_readback_probe`

use std::time::Instant;

use windows::core::Interface;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Resource, ID3D11Texture2D,
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CPU_ACCESS_READ,
    D3D11_CPU_ACCESS_WRITE, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ,
    D3D11_MAP_WRITE_DISCARD, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    D3D11_USAGE_DYNAMIC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC,
};

/// Sizes playback deals in: the user's recording, a 1080p one, the 720p bench
/// source, and the ~0.63x stage a windowed player scales those down to.
const SIZES: [(u32, u32, &str); 4] = [
    (1922, 1112, "user's recording"),
    (1920, 1080, "1080p recording"),
    (1280, 720, "bench recording"),
    (1213, 682, "windowed stage"),
];

const ROUNDS: u32 = 60;

fn main() -> windows::core::Result<()> {
    let (device, context) = device()?;
    println!("rounds per size: {ROUNDS}");
    for (width, height, what) in SIZES {
        let source = texture(&device, width, height, false)?;
        let staging = texture(&device, width, height, true)?;
        let bytes = (width as usize) * (height as usize) * 4;
        let mut destination = vec![0u8; bytes];

        // One untimed pass: the first copy pays for allocations this is not
        // trying to measure.
        readback(&context, &source, &staging, &mut destination)?;

        let mut total = 0f64;
        let mut worst = 0f64;
        for _ in 0..ROUNDS {
            let started = Instant::now();
            readback(&context, &source, &staging, &mut destination)?;
            let ms = started.elapsed().as_secs_f64() * 1000.0;
            total += ms;
            worst = worst.max(ms);
        }
        println!(
            "{width}x{height} ({what}) readback: {:.2} MB  mean {:.3} ms  max {:.3} ms",
            bytes as f64 / (1024.0 * 1024.0),
            total / f64::from(ROUNDS),
            worst,
        );

        // The other direction, in NV12 -- what the decoder actually hands over,
        // and three eighths the bytes of RGBA. NV12 only exists at even
        // dimensions (`E_INVALIDARG` otherwise), which is the same rule
        // `encoder::validate_config` enforces on the recording side.
        let (width, height) = (width & !1, height & !1);
        let nv12 = dynamic_nv12(&device, width, height)?;
        let nv12_bytes = (width as usize) * (height as usize) * 3 / 2;
        let source_rows = vec![0u8; nv12_bytes];
        upload(&context, &nv12, &source_rows, width, height)?;
        let mut total = 0f64;
        let mut worst = 0f64;
        for _ in 0..ROUNDS {
            let started = Instant::now();
            upload(&context, &nv12, &source_rows, width, height)?;
            let ms = started.elapsed().as_secs_f64() * 1000.0;
            total += ms;
            worst = worst.max(ms);
        }
        println!(
            "{width}x{height} ({what}) upload NV12: {:.2} MB  mean {:.3} ms  max {:.3} ms",
            nv12_bytes as f64 / (1024.0 * 1024.0),
            total / f64::from(ROUNDS),
            worst,
        );
    }
    Ok(())
}

/// `Map(WRITE_DISCARD)` plus a row-by-row copy: putting a decoded NV12 frame on
/// the GPU without a staging round trip.
fn upload(
    context: &ID3D11DeviceContext,
    texture: &ID3D11Texture2D,
    source: &[u8],
    width: u32,
    height: u32,
) -> windows::core::Result<()> {
    unsafe {
        let resource: ID3D11Resource = texture.cast()?;
        let mut mapped = Default::default();
        context.Map(&resource, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut mapped))?;
        // NV12 is a luma plane of `height` rows followed by a half-height chroma
        // plane, both `width` bytes wide.
        let rows = height as usize + height as usize / 2;
        for row in 0..rows {
            let from = &source[row * width as usize..][..width as usize];
            let into = (mapped.pData as *mut u8).add(row * mapped.RowPitch as usize);
            std::ptr::copy_nonoverlapping(from.as_ptr(), into, width as usize);
        }
        context.Unmap(&resource, 0);
    }
    Ok(())
}

fn dynamic_nv12(
    device: &ID3D11Device,
    width: u32,
    height: u32,
) -> windows::core::Result<ID3D11Texture2D> {
    let description = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_NV12,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DYNAMIC,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
        MiscFlags: 0,
    };
    let mut texture = None;
    unsafe { device.CreateTexture2D(&description, None, Some(&mut texture))? };
    Ok(texture.expect("CreateTexture2D returned nothing"))
}

/// `CopyResource` into the staging texture, then map it and copy the rows out --
/// what any "do it on the GPU, hand the pixels to slint" design has to pay.
fn readback(
    context: &ID3D11DeviceContext,
    source: &ID3D11Texture2D,
    staging: &ID3D11Texture2D,
    destination: &mut [u8],
) -> windows::core::Result<()> {
    unsafe {
        let source_resource: ID3D11Resource = source.cast()?;
        let staging_resource: ID3D11Resource = staging.cast()?;
        context.CopyResource(&staging_resource, &source_resource);
        let mut mapped = Default::default();
        context.Map(&staging_resource, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
        let mut description = D3D11_TEXTURE2D_DESC::default();
        staging.GetDesc(&mut description);
        let row_bytes = (description.Width as usize) * 4;
        for row in 0..description.Height as usize {
            let from = (mapped.pData as *const u8).add(row * mapped.RowPitch as usize);
            let into = &mut destination[row * row_bytes..][..row_bytes];
            std::ptr::copy_nonoverlapping(from, into.as_mut_ptr(), row_bytes);
        }
        context.Unmap(&staging_resource, 0);
    }
    Ok(())
}

fn texture(
    device: &ID3D11Device,
    width: u32,
    height: u32,
    staging: bool,
) -> windows::core::Result<ID3D11Texture2D> {
    let description = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: if staging {
            D3D11_USAGE_STAGING
        } else {
            D3D11_USAGE_DEFAULT
        },
        BindFlags: if staging {
            0
        } else {
            D3D11_BIND_RENDER_TARGET.0 as u32
        },
        CPUAccessFlags: if staging {
            D3D11_CPU_ACCESS_READ.0 as u32
        } else {
            0
        },
        MiscFlags: 0,
    };
    let mut texture = None;
    unsafe { device.CreateTexture2D(&description, None, Some(&mut texture))? };
    Ok(texture.expect("CreateTexture2D returned nothing"))
}

fn device() -> windows::core::Result<(ID3D11Device, ID3D11DeviceContext)> {
    let mut device = None;
    let mut context = None;
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )?;
    }
    Ok((
        device.expect("no device"),
        context.expect("no device context"),
    ))
}
