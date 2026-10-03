//! Records a high-rate window through the real capture pipeline and reports what
//! actually landed in the container, for task t260913-578a.
//!
//! Drives `CaptureSession` directly -- the same worker, encoder and muxer the app
//! uses -- against a window this process presents to as fast as the GPU allows.
//! No UI, no settings file, no ring buffer of the user's: segments go to a
//! throwaway directory under `%TEMP%` that it names on stdout and leaves in
//! place -- the whole point is to count frames in them afterwards.
//!
//! `BENCH_SOURCE_ONLY=1` skips the recording entirely and just holds the window
//! open as a capture target for something else (the app, for instance).
//!
//! `cargo run --features insight --example capture_rate_bench -- --insight 30 60 120`
//! (no frame rates given: 120 only). `BENCH_SECONDS` sets the per-rate length and
//! `BENCH_WIDTH` / `BENCH_HEIGHT` the window size.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use livia::capture::targets::CaptureTargetKind;
use livia::capture::{CaptureConfig, CaptureSession, CaptureSize};
use livia::settings::RecordingCodec;
use windows::Win32::Foundation::{HMODULE, HWND};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11RenderTargetView, ID3D11Texture2D,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION,
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

/// `BENCH_WIDTH` / `BENCH_HEIGHT` override these: the cost of a frame is mostly
/// pixels, so a headroom number is only meaningful at the size being asked about.
fn bench_size() -> (i32, i32) {
    fn from_env(name: &str, fallback: i32) -> i32 {
        std::env::var(name)
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(fallback)
    }
    (from_env("BENCH_WIDTH", 1280), from_env("BENCH_HEIGHT", 720))
}

fn main() {
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok() }.expect("MTA");
    livia::logging::configure_logging();

    let rates: Vec<u8> = {
        // `--insight` and friends are read by the library from `args_os`; skip
        // them here so they are not mistaken for a frame rate.
        let args: Vec<String> = std::env::args()
            .skip(1)
            .filter(|arg| !arg.starts_with('-'))
            .collect();
        if args.is_empty() {
            vec![120]
        } else {
            args.iter()
                .map(|arg| arg.parse().expect("frame rate must be 30, 60 or 120"))
                .collect()
        }
    };
    let seconds: u64 = std::env::var("BENCH_SECONDS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(30);

    let (width, height) = bench_size();
    let painting = Arc::new(AtomicU64::new(1));
    let (hwnd_tx, hwnd_rx) = std::sync::mpsc::channel();
    let painter = {
        let painting = painting.clone();
        std::thread::spawn(move || present_until_stopped(&hwnd_tx, &painting))
    };
    let hwnd = HWND(hwnd_rx.recv().expect("the painter reports its window") as *mut _);
    let handle = format!("0x{:X}", hwnd.0 as usize);

    println!("bench window {width}x{height}, {seconds}s per rate");
    if std::env::var("BENCH_SOURCE_ONLY").is_ok() {
        // Just be a high-rate capture target for something else -- the app
        // recording it, say -- and hold the window until Ctrl+C.
        println!("source-only: hwnd {handle}; presenting until killed");
        loop {
            std::thread::sleep(Duration::from_secs(3600));
        }
    }
    for rate in rates {
        record_one(&handle, rate, seconds);
    }

    painting.store(0, Ordering::Relaxed);
    let presents = painter.join().unwrap_or(0);
    println!("source presented {presents} frames total");
}

fn record_one(handle: &str, frame_rate: u8, seconds: u64) {
    let output_dir = std::env::temp_dir().join(format!(
        "livia-rate-bench-{}-{frame_rate}",
        std::process::id()
    ));
    let _ = std::fs::create_dir_all(&output_dir);
    let (stopped, _events) = crossbeam_channel::bounded(1);
    let (finalized, segments) = crossbeam_channel::unbounded();

    let started = Instant::now();
    let mut session = CaptureSession::start(
        CaptureConfig {
            window_handle: handle.to_string(),
            kind: CaptureTargetKind::Window,
            process_id: std::process::id(),
            frame_rate,
            include_cursor: false,
            output_size: CaptureSize {
                width: bench_size().0,
                height: bench_size().1,
            },
            retention_minutes: livia::ring_buffer::DEFAULT_RETENTION_MINUTES,
            encoder_output_dir: output_dir.clone(),
            container_path: None,
            capture_all_audio: false,
            codec: RecordingCodec::H264,
        },
        stopped,
        finalized,
    )
    .expect("capture should start on the bench window");

    std::thread::sleep(Duration::from_secs(seconds));
    session.stop();
    let elapsed = started.elapsed().as_secs_f64();
    let finalized: Vec<_> = segments.try_iter().collect();

    // `diagnostics()` is crate-private, so the counters come from the log instead:
    // `session_end arrived=… throttled=… no_credit=… queue_full=…` in
    // `%LOCALAPPDATA%\com.liveback.desktop\logs\liveback.log*`.
    println!(
        "frame_rate={frame_rate} elapsed={elapsed:.1}s segments={}",
        finalized.len(),
    );
    println!("  segments in {}", output_dir.display());
}

/// The bench's source: a flip-model swap chain presented with no vsync, which is
/// the shape a game's output has. GDI drawing is composited differently and caps
/// around 60/s regardless of the capture session's settings (task t260913-69dd).
fn present_until_stopped(hwnd_tx: &std::sync::mpsc::Sender<usize>, painting: &AtomicU64) -> u64 {
    let hwnd = unsafe {
        CreateWindowExW(
            Default::default(),
            windows::core::w!("STATIC"),
            windows::core::w!("capture rate bench"),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            100,
            100,
            bench_size().0,
            bench_size().1,
            None,
            None,
            None,
            None,
        )
        .expect("bench window")
    };
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOW);
    }
    hwnd_tx.send(hwnd.0 as usize).expect("the bench is waiting");

    let painter = SwapChainPainter::create(hwnd).expect("bench swap chain");
    let mut presented = 0u64;
    let mut message = MSG::default();
    while painting.load(Ordering::Relaxed) == 1 {
        unsafe {
            while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        painter.present(presented);
        presented += 1;
        // ~1000/s ceiling: well above any refresh rate, and it leaves the GPU the
        // room the measurement is about.
        std::thread::sleep(Duration::from_millis(1));
    }
    unsafe {
        let _ = DestroyWindow(hwnd);
    }
    presented
}

struct SwapChainPainter {
    context: ID3D11DeviceContext,
    target: ID3D11RenderTargetView,
    swap_chain: IDXGISwapChain1,
}

impl SwapChainPainter {
    fn create(hwnd: HWND) -> windows::core::Result<Self> {
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
                    Width: bench_size().0 as u32,
                    Height: bench_size().1 as u32,
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
        Ok(Self {
            context,
            target: target.expect("CreateRenderTargetView returned nothing"),
            swap_chain,
        })
    }

    fn present(&self, tick: u64) {
        // Moving detail, not a flat fill: a constant-quality encoder spends almost
        // nothing on a still picture, which would make the encode side of this
        // measurement meaningless.
        let phase = (tick % 120) as f32 / 120.0;
        unsafe {
            self.context
                .ClearRenderTargetView(&self.target, &[phase, 1.0 - phase, 0.5, 1.0]);
            let _ = self.swap_chain.Present(0, DXGI_PRESENT(0));
        }
    }
}
