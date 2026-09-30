//! Transform-pipeline pixel and HDR tone-map verification tests.
use super::*;

#[test]
fn aspect_fit_adds_black_bars_without_distortion() {
    assert_eq!(
        aspect_fit(
            CaptureSize {
                width: 16,
                height: 9
            },
            CaptureSize {
                width: 100,
                height: 100
            }
        ),
        FitRect {
            x: 0,
            y: 22,
            width: 100,
            height: 56
        }
    );
}

/// The `& !1` rounding in `worker.rs` leaves an odd-sized window one pixel
/// wider/taller than the output it encodes into. Fitting that would resample
/// every pixel; `crop_or_fit` draws 1:1 instead and the rasterizer clips the
/// overhang (task980).
#[test]
fn crop_or_fit_draws_one_to_one_when_the_source_overhangs_by_at_most_a_pixel() {
    let output = CaptureSize {
        width: 1236,
        height: 694,
    };
    for (width, height) in [(1236, 694), (1237, 694), (1236, 695), (1237, 695)] {
        let input = CaptureSize { width, height };
        assert_eq!(
            crop_or_fit(input, output),
            FitRect {
                x: 0,
                y: 0,
                width,
                height
            },
            "a {width}x{height} source into {}x{} should not be scaled",
            output.width,
            output.height
        );
    }
}

/// Anything further off than a pixel is a real resize (the window changed size
/// mid-session), and that still letterboxes through `aspect_fit`.
#[test]
fn crop_or_fit_falls_back_to_the_fit_path_beyond_a_pixel() {
    let output = CaptureSize {
        width: 1236,
        height: 694,
    };
    for (width, height) in [(1238, 694), (1236, 696), (1235, 694), (2560, 1440)] {
        let input = CaptureSize { width, height };
        assert_eq!(crop_or_fit(input, output), aspect_fit(input, output));
    }
    // A degenerate output (nothing to draw into) keeps `aspect_fit`'s empty rect
    // rather than handing the rasterizer a viewport for a render target that has
    // no pixels.
    let (input, output) = (
        CaptureSize {
            width: 1,
            height: 1,
        },
        CaptureSize {
            width: 0,
            height: 0,
        },
    );
    assert_eq!(crop_or_fit(input, output), aspect_fit(input, output));
}

pub(super) fn verify_gpu_transform_pipeline_compiles_on_the_active_adapter() {
    let (device, context, _) = create_d3d_device().expect("D3D11 device should be available");
    TransformPipeline::new(
        device,
        context,
        CaptureSize {
            width: 64,
            height: 64,
        },
        gpu::HDR_WHITE_POINT_FLOOR,
    )
    .expect("fullscreen SDR/HDR transform shaders should compile");
}

pub(super) fn verify_d3d11_debug_layer_is_available_for_capture_diagnostics() {
    let _ = create_d3d_device_with_flags(
        D3D11_CREATE_DEVICE_BGRA_SUPPORT
            | D3D11_CREATE_DEVICE_VIDEO_SUPPORT
            | windows::Win32::Graphics::Direct3D11::D3D11_CREATE_DEVICE_DEBUG,
    )
    .expect("Graphics Tools D3D11 debug layer should create a capture device");
}

pub(super) fn verify_sdr_transform_preserves_source_pixel_values() {
    let (device, context, _) = create_d3d_device().expect("D3D11 device should be available");
    let pipeline = TransformPipeline::new(
        device.clone(),
        context.clone(),
        CaptureSize {
            width: 64,
            height: 64,
        },
        gpu::HDR_WHITE_POINT_FLOOR,
    )
    .expect("transform pipeline should initialize");

    // Input and output sizes are identical so aspect_fit does not scale, ruling
    // out sampler interpolation as a source of channel drift.
    for value in [0x00u8, 0x80u8, 0xFFu8] {
        let source = create_test_source_with_pixels(
            &device,
            CaptureSize {
                width: 64,
                height: 64,
            },
            &[value; 64 * 64 * 4],
        )
        .expect("solid-color source texture should initialize");
        let output = pipeline
            .transform(
                &source,
                CaptureSize {
                    width: 64,
                    height: 64,
                },
                Inset::default(),
                false,
            )
            .expect("SDR transform should draw");
        let pixel = read_bgra8_pixel(&device, &context, &output, 32, 32)
            .expect("center pixel readback should succeed");
        for (channel_name, channel) in [("B", pixel[0]), ("G", pixel[1]), ("R", pixel[2])] {
            let delta = i32::from(channel) - i32::from(value);
            assert!(
                delta.abs() <= 1,
                "SDR pass-through must preserve pixel values: input {value:#04x}, \
                 output {channel_name}={channel:#04x} (delta {delta})"
            );
        }
    }
}

/// The pixel-level proof that a one-pixel overhang is cropped rather than
/// scaled (task980). A 65x65 source into a 64x64 output is the odd-window case
/// after `worker.rs` rounds down for NV12; the pattern is a per-pixel
/// checkerboard, so anything that resamples lands the sample points between
/// texels and reads back mid-grey instead of the black and white that went in.
pub(super) fn verify_a_one_pixel_overhang_is_cropped_not_resampled() {
    const OUTPUT: i32 = 64;
    const SOURCE: i32 = OUTPUT + 1;
    let checker = |x: i32, y: i32| if (x + y) % 2 == 0 { 0xFFu8 } else { 0x00u8 };
    let (device, context, _) = create_d3d_device().expect("D3D11 device should be available");
    let pipeline = TransformPipeline::new(
        device.clone(),
        context.clone(),
        CaptureSize {
            width: OUTPUT,
            height: OUTPUT,
        },
        gpu::HDR_WHITE_POINT_FLOOR,
    )
    .expect("transform pipeline should initialize");

    let mut pixels = vec![0u8; (SOURCE * SOURCE * 4) as usize];
    for y in 0..SOURCE {
        for x in 0..SOURCE {
            let offset = ((y * SOURCE + x) * 4) as usize;
            pixels[offset..offset + 4].fill(checker(x, y));
        }
    }
    let source = create_test_source_with_pixels(
        &device,
        CaptureSize {
            width: SOURCE,
            height: SOURCE,
        },
        &pixels,
    )
    .expect("checkerboard source texture should initialize");
    let output = pipeline
        .transform(
            &source,
            CaptureSize {
                width: SOURCE,
                height: SOURCE,
            },
            Inset::default(),
            false,
        )
        .expect("SDR transform should draw");

    // Corners and centre: a scaled draw drifts furthest from the source the
    // further along each axis it gets, so sampling both ends catches it even if
    // one corner happened to land on a texel centre.
    for (x, y) in [(0, 0), (1, 0), (0, 1), (31, 31), (62, 63), (63, 63)] {
        let pixel = read_bgra8_pixel(&device, &context, &output, x as u32, y as u32)
            .expect("pixel readback should succeed");
        let expected = checker(x, y);
        for (channel_name, channel) in [("B", pixel[0]), ("G", pixel[1]), ("R", pixel[2])] {
            let delta = i32::from(channel) - i32::from(expected);
            assert!(
                delta.abs() <= 1,
                "a {SOURCE}x{SOURCE} source into {OUTPUT}x{OUTPUT} must be drawn 1:1: \
                 at ({x},{y}) expected {expected:#04x}, got {channel_name}={channel:#04x} \
                 (delta {delta})"
            );
        }
    }
}

pub(super) fn create_test_source_with_pixels(
    device: &ID3D11Device,
    size: CaptureSize,
    bgra_pixels: &[u8],
) -> windows::core::Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: size.width as u32,
        Height: size.height as u32,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        ..Default::default()
    };
    let initial_data = D3D11_SUBRESOURCE_DATA {
        pSysMem: bgra_pixels.as_ptr().cast(),
        SysMemPitch: size.width as u32 * 4,
        SysMemSlicePitch: 0,
    };
    let mut texture = None;
    unsafe {
        device.CreateTexture2D(&desc, Some(&initial_data), Some(&mut texture))?;
    }
    texture.ok_or_else(windows::core::Error::from_win32)
}

/// Copies `texture` to a CPU-readable staging texture and reads back the
/// BGRA8 bytes of a single pixel at (`x`, `y`).
fn read_bgra8_pixel(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    texture: &ID3D11Texture2D,
    x: u32,
    y: u32,
) -> windows::core::Result<[u8; 4]> {
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { texture.GetDesc(&mut desc) };
    let staging_desc = D3D11_TEXTURE2D_DESC {
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
        ..desc
    };
    let mut staging = None;
    unsafe {
        device.CreateTexture2D(&staging_desc, None, Some(&mut staging))?;
    }
    let staging = staging.ok_or_else(windows::core::Error::from_win32)?;
    unsafe {
        context.CopyResource(&staging, texture);
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
        let row = mapped
            .pData
            .cast::<u8>()
            .add((y * mapped.RowPitch) as usize);
        let pixel = std::slice::from_raw_parts(row.add((x * 4) as usize), 4);
        let result = [pixel[0], pixel[1], pixel[2], pixel[3]];
        context.Unmap(&staging, 0);
        Ok(result)
    }
}

pub(super) fn verify_gpu_transform_writes_a_fixed_bgra8_texture() {
    let (device, context, _) = create_d3d_device().expect("D3D11 device should be available");
    let pipeline = TransformPipeline::new(
        device.clone(),
        context,
        CaptureSize {
            width: 64,
            height: 64,
        },
        gpu::HDR_WHITE_POINT_FLOOR,
    )
    .expect("transform pipeline should initialize");
    let source = create_test_source(
        &device,
        CaptureSize {
            width: 16,
            height: 9,
        },
    )
    .expect("source texture should initialize");
    let output = pipeline
        .transform(
            &source,
            CaptureSize {
                width: 16,
                height: 9,
            },
            Inset::default(),
            false,
        )
        .expect("SDR transform should draw");
    let mut desc = Default::default();
    unsafe { output.GetDesc(&mut desc) };
    assert_eq!(desc.Width, 64);
    assert_eq!(desc.Height, 64);
    assert_eq!(desc.Format, DXGI_FORMAT_B8G8R8A8_UNORM);
}

/// Encodes an f32 into IEEE-754 binary16 bits. Only needs to be correct for the
/// finite, non-subnormal magnitudes used by the HDR tone-map tests (0.0 and small
/// positive linear values); see the known-pattern self-test below.
fn f32_to_f16_bits(value: f32) -> u16 {
    let bits = value.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    if value == 0.0 {
        return sign;
    }
    let exp = ((bits >> 23) & 0xff) as i32 - 127 + 15;
    let mantissa = bits & 0x7f_ffff;
    if exp <= 0 {
        return sign;
    }
    if exp >= 0x1f {
        return sign | 0x7c00;
    }
    sign | ((exp as u16) << 10) | (mantissa >> 13) as u16
}

/// Creates an `R16G16B16A16_FLOAT` source texture with every pixel set to the same
/// scRGB linear `(r, g, b, 1.0)` value, for exercising the HDR tone-map shader with
/// known input luminance.
fn create_test_source_with_float_pixels(
    device: &ID3D11Device,
    size: CaptureSize,
    rgb: f32,
) -> windows::core::Result<ID3D11Texture2D> {
    let texel = [
        f32_to_f16_bits(rgb),
        f32_to_f16_bits(rgb),
        f32_to_f16_bits(rgb),
        f32_to_f16_bits(1.0),
    ];
    let pixel_count = (size.width * size.height) as usize;
    let mut pixels = Vec::with_capacity(pixel_count * 4);
    for _ in 0..pixel_count {
        pixels.extend_from_slice(&texel);
    }
    let desc = D3D11_TEXTURE2D_DESC {
        Width: size.width as u32,
        Height: size.height as u32,
        MipLevels: 1,
        ArraySize: 1,
        Format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R16G16B16A16_FLOAT,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        ..Default::default()
    };
    let initial_data = D3D11_SUBRESOURCE_DATA {
        pSysMem: pixels.as_ptr().cast(),
        SysMemPitch: size.width as u32 * 4 * 2,
        SysMemSlicePitch: 0,
    };
    let mut texture = None;
    unsafe {
        device.CreateTexture2D(&desc, Some(&initial_data), Some(&mut texture))?;
    }
    texture.ok_or_else(windows::core::Error::from_win32)
}

/// An SDR pixel composited into an HDR desktop must come back out of the capture
/// path as the sRGB value it went in as.
///
/// This is an *identity* assertion, and that is the whole point. Task1820
/// measured that the scRGB composition scale WGC hands over is frozen at session
/// start, so what arrives is exactly `linear_sRGB * W_c` and the correct inverse
/// is a division by the same `W_c`. Anything else is a tone curve applied to
/// content that was never HDR to begin with.
///
/// Task1790's version of this test asserted only that bright steps stayed
/// *separated*, which is why it passed on an extended-Reinhard curve that lifted
/// sRGB 120 to 178 at this machine's 4.1 white point -- the recording looked
/// washed out while the suite stayed green (task2750). Separation is a strictly
/// weaker property than identity, so it is gone: do not reintroduce it as the
/// load-bearing assert.
///
/// Three white points: the floor (an SDR-reference monitor, the regression guard
/// for the fallback path), 2.5 (a 200-nit SDR content brightness, the common
/// case), and 4.1 (this development machine's measured level, the reported bug).
pub(super) fn verify_hdr_tone_map_returns_sdr_sources_to_their_original_srgb_values() {
    for white_point in [gpu::HDR_WHITE_POINT_FLOOR, 2.5, 4.1] {
        verify_hdr_tone_map_at_white_point(white_point);
    }
}

/// The sRGB EOTF: a display-referred code back to the linear light it stands for.
/// The piecewise form, matching the shader's encode, so the round trip has no
/// systematic error of its own to hide behind.
fn srgb_to_linear(code: u8) -> f32 {
    let c = f32::from(code) / 255.0;
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

fn verify_hdr_tone_map_at_white_point(white_point: f32) {
    let (device, context, _) = create_d3d_device().expect("D3D11 device should be available");
    let pipeline = TransformPipeline::new(
        device.clone(),
        context.clone(),
        CaptureSize {
            width: 64,
            height: 64,
        },
        white_point,
    )
    .expect("transform pipeline should initialize");

    let sample = |value: f32| -> u8 {
        let source = create_test_source_with_float_pixels(
            &device,
            CaptureSize {
                width: 64,
                height: 64,
            },
            value,
        )
        .expect("float source texture should initialize");
        let output = pipeline
            .transform(
                &source,
                CaptureSize {
                    width: 64,
                    height: 64,
                },
                Inset::default(),
                true,
            )
            .expect("HDR transform should draw");
        let pixel = read_bgra8_pixel(&device, &context, &output, 32, 32)
            .expect("center pixel readback should succeed");
        pixel[0] // B == G == R for a gray input; any channel works.
    };

    // What WGC actually hands over for an SDR pixel on an HDR desktop: the sRGB
    // code, linearized, scaled by the composition white level (task1820).
    let round_trip = |code: u8| -> u8 { sample(srgb_to_linear(code) * white_point) };

    // The load-bearing assert. The ramp spans the range the old Reinhard curve
    // got most wrong: at white point 4.1 it put 60 at 110, 120 at 178 and 200 at
    // 231, all of which are further than 1 from where they started.
    let mut measured = Vec::new();
    for code in [0u8, 36, 60, 120, 200, 235, 245, 255] {
        let out = round_trip(code);
        measured.push((code, out));
        assert!(
            (i32::from(out) - i32::from(code)).abs() <= 1,
            "at white point {white_point}, sRGB {code} composited to scRGB {} must come back as {code} +-1, got {out} (full ramp: {measured:?})",
            srgb_to_linear(code) * white_point
        );
    }

    // Above the white point there is no rolloff by design (task2750): genuine HDR
    // highlights hard-clip so that SDR content below the white point stays exact.
    let just_over_white = sample(white_point * 1.01);
    let superwhite = sample(white_point * 4.0);
    assert_eq!(
        just_over_white,
        255,
        "at white point {white_point}, linear {} sits above white and must clip to 255",
        white_point * 1.01
    );
    assert_eq!(
        superwhite,
        255,
        "at white point {white_point}, linear {} sits far above white and must clip to 255",
        white_point * 4.0
    );
}

pub(super) fn verify_hdr_float_source_tone_maps_to_bgra8() {
    let (device, context, _) = create_d3d_device().expect("D3D11 device should be available");
    let pipeline = TransformPipeline::new(
        device.clone(),
        context,
        CaptureSize {
            width: 64,
            height: 64,
        },
        gpu::HDR_WHITE_POINT_FLOOR,
    )
    .expect("pipeline should initialize");
    let source = create_test_source_with_format(
        &device,
        CaptureSize {
            width: 16,
            height: 9,
        },
        windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R16G16B16A16_FLOAT,
    )
    .expect("float source texture should initialize");
    let output = pipeline
        .transform(
            &source,
            CaptureSize {
                width: 16,
                height: 9,
            },
            Inset::default(),
            true,
        )
        .expect("HDR transform should draw");
    let mut desc = Default::default();
    unsafe { output.GetDesc(&mut desc) };
    assert_eq!(desc.Format, DXGI_FORMAT_B8G8R8A8_UNORM);
}

fn create_test_source(
    device: &ID3D11Device,
    size: CaptureSize,
) -> windows::core::Result<ID3D11Texture2D> {
    create_test_source_with_format(device, size, DXGI_FORMAT_B8G8R8A8_UNORM)
}

fn create_test_source_with_format(
    device: &ID3D11Device,
    size: CaptureSize,
    format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT,
) -> windows::core::Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: size.width as u32,
        Height: size.height as u32,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        ..Default::default()
    };
    let mut texture = None;
    unsafe {
        device.CreateTexture2D(&desc, None, Some(&mut texture))?;
    }
    texture.ok_or_else(windows::core::Error::from_win32)
}

/// A zero inset must leave the viewport exactly what `crop_or_fit` chose
/// (task2770): every recording of a non-maximized window goes through here.
#[test]
fn source_viewport_with_a_zero_inset_is_crop_or_fit() {
    for (input, output) in [
        ((1236, 694), (1236, 694)),
        ((1237, 695), (1236, 694)),
        ((1920, 1080), (1280, 720)),
        ((16, 9), (100, 100)),
        ((886, 693), (886, 692)),
    ] {
        let input = CaptureSize {
            width: input.0,
            height: input.1,
        };
        let output = CaptureSize {
            width: output.0,
            height: output.1,
        };
        assert_eq!(
            source_viewport(input, Inset::default(), output),
            crop_or_fit(input, output)
        );
    }
}

/// Drawn 1:1, the whole source is shifted up and left by the inset so the
/// visible part lands at the origin and the overhang falls off the target.
#[test]
fn source_viewport_shifts_a_one_to_one_source_by_the_inset() {
    let inset = |n| Inset {
        left: n,
        top: n,
        right: n,
        bottom: n,
    };
    // A custom-frame window maximized on a 1920x1080 monitor (measured).
    assert_eq!(
        source_viewport(
            CaptureSize {
                width: 1920,
                height: 1040
            },
            Inset {
                bottom: 8,
                ..Inset::default()
            },
            CaptureSize {
                width: 1920,
                height: 1032
            }
        ),
        FitRect {
            x: 0,
            y: 0,
            width: 1920,
            height: 1040
        }
    );
    // The UE5 editor recording the task was filed for: 11 px on every side.
    assert_eq!(
        source_viewport(
            CaptureSize {
                width: 3862,
                height: 2110
            },
            inset(11),
            CaptureSize {
                width: 3840,
                height: 2088
            }
        ),
        FitRect {
            x: -11,
            y: -11,
            width: 3862,
            height: 2110
        }
    );
    // Odd visible size: the `& !1` output still takes the 1:1 path.
    assert_eq!(
        source_viewport(
            CaptureSize {
                width: 1937,
                height: 1049
            },
            inset(8),
            CaptureSize {
                width: 1920,
                height: 1032
            }
        ),
        FitRect {
            x: -8,
            y: -8,
            width: 1937,
            height: 1049
        }
    );
}

/// When the visible part has to be scaled to fit, the inset scales with it.
#[test]
fn source_viewport_scales_the_inset_with_the_fit() {
    let input = CaptureSize {
        width: 1936,
        height: 1048,
    };
    let inset = Inset {
        left: 8,
        top: 8,
        right: 8,
        bottom: 8,
    };
    let output = CaptureSize {
        width: 960,
        height: 516,
    };
    assert_eq!(
        aspect_fit(
            CaptureSize {
                width: 1920,
                height: 1032
            },
            output
        ),
        FitRect {
            x: 0,
            y: 0,
            width: 960,
            height: 516
        }
    );
    assert_eq!(
        source_viewport(input, inset, output),
        FitRect {
            x: -4,
            y: -4,
            width: 968,
            height: 524
        }
    );
}

/// An inset that leaves no visible pixels is ignored rather than producing an
/// empty or inverted viewport.
#[test]
fn source_viewport_ignores_an_inset_that_leaves_nothing() {
    let input = CaptureSize {
        width: 16,
        height: 16,
    };
    let output = CaptureSize {
        width: 16,
        height: 16,
    };
    let inset = Inset {
        left: 8,
        top: 0,
        right: 8,
        bottom: 0,
    };
    assert_eq!(
        source_viewport(input, inset, output),
        crop_or_fit(input, output)
    );
}

mod overhang {
    use super::*;
    use crate::capture::worker::overhang;
    use windows::Win32::Foundation::RECT;

    fn rect(left: i32, top: i32, right: i32, bottom: i32) -> RECT {
        RECT {
            left,
            top,
            right,
            bottom,
        }
    }

    fn size(width: i32, height: i32) -> CaptureSize {
        CaptureSize { width, height }
    }

    const WORK_1080P: RECT = RECT {
        left: 0,
        top: 0,
        right: 1920,
        bottom: 1032,
    };

    /// The custom-frame window (measured): only the strip under the taskbar
    /// is outside the work area.
    #[test]
    fn an_asymmetric_overhang_is_reported_per_side() {
        assert_eq!(
            overhang(
                size(1920, 1040),
                [rect(-8, -8, 1928, 1040), rect(0, 0, 1920, 1040)],
                WORK_1080P
            ),
            Inset {
                bottom: 8,
                ..Inset::default()
            }
        );
        // Taskbar on the left: the left overhang includes the taskbar width.
        assert_eq!(
            overhang(
                size(1936, 1096),
                [rect(-8, -8, 1928, 1088), rect(-8, -8, 1928, 1088)],
                rect(64, 0, 1920, 1080)
            ),
            Inset {
                left: 72,
                top: 8,
                right: 8,
                bottom: 8
            }
        );
        // mspaint maximized (measured 2026-09-03): WGC sized the frame by the
        // extended frame bounds, which already sit inside the work area.
        assert_eq!(
            overhang(
                size(1920, 1032),
                [rect(-8, -8, 1928, 1040), rect(0, 0, 1920, 1032)],
                WORK_1080P
            ),
            Inset::default()
        );
        // The UE5 editor at 4K/150% the task was filed for: the frame is the
        // whole window rect, 11 px past the work area on every side.
        assert_eq!(
            overhang(
                size(3862, 2110),
                [rect(-11, -11, 3851, 2099), rect(0, 0, 3840, 2088)],
                rect(0, 0, 3840, 2088)
            ),
            Inset {
                left: 11,
                top: 11,
                right: 11,
                bottom: 11
            }
        );
        // A rect entirely inside the work area never yields a negative side,
        // and a frame matching neither candidate is left alone.
        assert_eq!(
            overhang(
                size(800, 600),
                [rect(100, 100, 900, 700), rect(107, 100, 893, 693)],
                WORK_1080P
            ),
            Inset::default()
        );
        assert_eq!(
            overhang(
                size(1000, 1000),
                [rect(-8, -8, 1928, 1040), rect(0, 0, 1920, 1032)],
                WORK_1080P
            ),
            Inset::default()
        );
    }
}

/// The GPU thumbnail downscale (task2820): right size, right format, and a
/// box filter that does not tint what it averages.
///
/// A solid colour is the strongest cheap check on the 4x4 tap shader --
/// averaging sixteen samples of one value has to give that value back, so a
/// wrong tap weight, a wrong divisor or a stray alpha would all show up as
/// drift here. The size assertion is what pins the readback to
/// `gpu::THUMBNAIL_SIZE` rather than the frame size it used to return.
pub(super) fn verify_the_thumbnail_readback_is_shrunk_on_the_gpu() {
    const SOURCE: i32 = 1024;
    let (device, context, _) = create_d3d_device().expect("D3D11 device should be available");
    let pipeline = TransformPipeline::new(
        device.clone(),
        context.clone(),
        CaptureSize {
            width: SOURCE,
            height: SOURCE,
        },
        gpu::HDR_WHITE_POINT_FLOOR,
    )
    .expect("transform pipeline should initialize");

    for value in [0x00u8, 0x80u8, 0xFFu8] {
        // On the heap: 1024x1024 BGRA is 4MB, and a stack array that size
        // overflows the test thread before the first assertion runs.
        let pixels = vec![value; (SOURCE * SOURCE * 4) as usize];
        let source = create_test_source_with_pixels(
            &device,
            CaptureSize {
                width: SOURCE,
                height: SOURCE,
            },
            &pixels,
        )
        .expect("solid-color source texture should initialize");
        let output = pipeline
            .transform(
                &source,
                CaptureSize {
                    width: SOURCE,
                    height: SOURCE,
                },
                Inset::default(),
                false,
            )
            .expect("SDR transform should draw");
        let (width, height, bgra) = pipeline
            .thumbnail_bgra(&output)
            .expect("thumbnail readback should succeed");

        assert_eq!(
            (width as i32, height as i32),
            (gpu::THUMBNAIL_SIZE.width, gpu::THUMBNAIL_SIZE.height),
            "the readback must come back at the thumbnail size, not the frame size"
        );
        assert_eq!(
            bgra.len(),
            (width as usize) * (height as usize) * 4,
            "BGRA8 must come back tightly packed"
        );
        // The centre pixel: a corner could legitimately sample past the edge
        // of the source, where the clamp sampler repeats the edge texel -- the
        // same value here, but not a property worth asserting.
        let centre = ((height / 2) as usize * width as usize + (width / 2) as usize) * 4;
        for (channel_name, channel) in [
            ("B", bgra[centre]),
            ("G", bgra[centre + 1]),
            ("R", bgra[centre + 2]),
        ] {
            let delta = i32::from(channel) - i32::from(value);
            assert!(
                delta.abs() <= 1,
                "averaging one colour must return it: input {value:#04x}, \
                 output {channel_name}={channel:#04x} (delta {delta})"
            );
        }
    }
}
