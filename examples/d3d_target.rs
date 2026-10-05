//! A recording target whose pixels change at the display's refresh rate.
//!
//! `.agents/tools/anim-target.rs` and `av-target.rs` paint with GDI on a
//! `SetTimer`, so their content changes about 60 times a second however fast
//! the monitor runs (`KNOWLEDGE.md`, "WGC の供給レートは `MinUpdateInterval` の
//! 既定 16ms で決まる" -- the GDI ceiling paragraph). That is enough material
//! for a 60Hz desk and not enough to record at 120fps on a fast one: WGC only
//! hands over a frame when the window's content actually changed, so the
//! recorded rate lands on the painter's rate, not on the rate
//! `configure_min_update_interval` asked DWM for.
//!
//! This target presents a flip-model swap chain with `SyncInterval 0` instead --
//! the shape a game's output has -- which recomposes as fast as DWM will take
//! it. On ALICE's 280Hz DISPLAY1 that lets a `frameRate: 120` recording reach
//! its setting for the first time (task t260913-3842 / order 4510, whose AC1
//! names 120fps material and whose `Blockers` item 2 has never been testable:
//! `should_skip_frame`'s `step * 1.5 < period` means material at or below 90fps
//! never skips a frame at 1x).
//!
//! The painter is lifted from `examples/wgc_rate_probe.rs`, which measured
//! 279.82/s with it against about 60/s for its GDI half. This file keeps only
//! the window and drops all the capture machinery -- liveback does the
//! recording here.
//!
//!   cargo run --release --example d3d_target -- <width> <height> <seconds>
//!
//! Verification apparatus, not product code: nothing under `src/` refers to it.

use std::time::{Duration, Instant};

use windows::core::Interface;
use windows::Win32::Foundation::{HMODULE, HWND, RECT};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11DeviceContext1,
    ID3D11RenderTargetView, ID3D11Texture2D, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIFactory2, IDXGISwapChain1, DXGI_PRESENT, DXGI_SWAP_CHAIN_DESC1,
    DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL, DXGI_USAGE_RENDER_TARGET_OUTPUT,
};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, DispatchMessageW, PeekMessageW, ShowWindow, TranslateMessage,
    MSG, PM_REMOVE, SW_SHOW, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
};

fn main() -> windows::core::Result<()> {
    let mut args = std::env::args().skip(1);
    let width: i32 = args.next().and_then(|a| a.parse().ok()).unwrap_or(1280);
    let height: i32 = args.next().and_then(|a| a.parse().ok()).unwrap_or(720);
    let seconds: u64 = args.next().and_then(|a| a.parse().ok()).unwrap_or(60);

    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok()? };

    // The window is created on the thread that pumps it: a window whose owner
    // never pumps is marked unresponsive and DWM composites a ghost of it.
    let hwnd = unsafe {
        CreateWindowExW(
            Default::default(),
            windows::core::w!("STATIC"),
            windows::core::w!("liveback d3d target"),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            100,
            100,
            width,
            height,
            None,
            None,
            None,
            None,
        )?
    };
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOW);
    }

    let painter = SwapChainPainter::create(hwnd, width, height)?;
    println!(
        "d3d_target: {width}x{height} for {seconds}s, hwnd {:?}",
        hwnd.0
    );

    let until = Instant::now() + Duration::from_secs(seconds);
    let mut message = MSG::default();
    let mut painted = 0u64;
    while Instant::now() < until {
        unsafe {
            while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        painter.present(painted);
        painted += 1;
        // Same 1ms breather the probe takes: a ~1000/s ceiling is far above any
        // refresh rate and leaves DWM the room the recording is about.
        std::thread::sleep(Duration::from_millis(1));
    }
    unsafe {
        let _ = DestroyWindow(hwnd);
    }
    let rate = painted as f64 / seconds as f64;
    println!("d3d_target: presented {painted} frames ({rate:.1}/s)");
    Ok(())
}

/// A flip-model swap chain presented without waiting for vsync.
struct SwapChainPainter {
    context: ID3D11DeviceContext,
    context1: ID3D11DeviceContext1,
    target: ID3D11RenderTargetView,
    swap_chain: IDXGISwapChain1,
    width: i32,
    height: i32,
}

impl SwapChainPainter {
    fn create(hwnd: HWND, width: i32, height: i32) -> windows::core::Result<Self> {
        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
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
        let device = device.expect("D3D11CreateDevice returned no device");
        let context = context.expect("D3D11CreateDevice returned no context");
        let factory: IDXGIFactory2 = unsafe { CreateDXGIFactory1()? };
        let swap_chain = unsafe {
            factory.CreateSwapChainForHwnd(
                &device,
                hwnd,
                &DXGI_SWAP_CHAIN_DESC1 {
                    Width: width as u32,
                    Height: height as u32,
                    Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                    BufferCount: 2,
                    SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
                    ..Default::default()
                },
                None,
                None,
            )?
        };
        let back_buffer: ID3D11Texture2D = unsafe { swap_chain.GetBuffer(0)? };
        let mut target = None;
        unsafe { device.CreateRenderTargetView(&back_buffer, None, Some(&mut target))? };
        let context1: ID3D11DeviceContext1 = context.cast()?;
        Ok(Self {
            context,
            context1,
            target: target.expect("CreateRenderTargetView returned nothing"),
            swap_chain,
            width,
            height,
        })
    }

    /// A still grid with two bars sweeping across it.
    ///
    /// The first version cleared the whole surface to a colour that changed
    /// every frame. It recorded fine and it was useless to look at: with the
    /// entire image changing at once there is no fixed reference, so a dropped
    /// frame and a segment crossing look exactly like the material itself, and
    /// a person asked to watch it for a minute reports 「目に痛い」 (2026-09-22,
    /// task t260913-3842's AC5 could not be judged on it).
    ///
    /// So: a dark ground and a static grid that stay put, and motion carried by
    /// two bars at different speeds. Stutter shows as a bar that stops against
    /// the grid; a crossing that loses a frame shows as a bar that jumps a gap.
    /// Only the bars change between frames, which is still a content change on
    /// every present -- WGC hands over a frame whenever anything moved.
    ///
    /// `ClearView` takes rectangles, so all of this is clears: no shaders, no
    /// vertex buffers, nothing that would make this file something to maintain.
    fn present(&self, tick: u64) {
        const GRID: i32 = 80;
        let w = self.width;
        let h = self.height;
        unsafe {
            self.context
                .ClearRenderTargetView(&self.target, &[0.05, 0.06, 0.08, 1.0]);

            let mut lines: Vec<RECT> = Vec::new();
            let mut x = GRID;
            while x < w {
                lines.push(RECT {
                    left: x,
                    top: 0,
                    right: x + 2,
                    bottom: h,
                });
                x += GRID;
            }
            let mut y = GRID;
            while y < h {
                lines.push(RECT {
                    left: 0,
                    top: y,
                    right: w,
                    bottom: y + 2,
                });
                y += GRID;
            }
            self.context1
                .ClearView(&self.target, &[0.22, 0.25, 0.30, 1.0], Some(&lines));

            // Two speeds: the fast bar makes a single missing frame visible as a
            // wider-than-usual gap, the slow one keeps a reference that is easy
            // to follow by eye for a whole minute.
            let fast = ((tick * 7) % (w as u64)) as i32;
            let slow = ((tick * 2) % (w as u64)) as i32;
            let bar = |left: i32, width: i32| RECT {
                left,
                top: 0,
                right: (left + width).min(w),
                bottom: h,
            };
            self.context1.ClearView(
                &self.target,
                &[0.85, 0.74, 0.35, 1.0],
                Some(&[bar(fast, 18)]),
            );
            self.context1.ClearView(
                &self.target,
                &[0.35, 0.62, 0.85, 1.0],
                Some(&[bar(slow, 34)]),
            );

            let _ = self.swap_chain.Present(0, DXGI_PRESENT(0));
        }
    }
}
