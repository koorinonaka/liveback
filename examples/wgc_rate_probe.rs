//! Measures what WGC's `MinUpdateInterval` actually costs a window capture.
//!
//! Throwaway probe, not a test: it opens a frame pool on a window this process
//! draws into as fast as it can, counts `FrameArrived` for a few seconds under
//! the system default, then sets the interval the recording asks for and counts
//! again. Nothing here touches the encoder, the ring buffer or any recording.
//!
//! `cargo run --example wgc_rate_probe`

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use windows::core::{factory, IInspectable, Interface};
use windows::Foundation::{TimeSpan, TypedEventHandler};
use windows::Graphics::Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::Foundation::{COLORREF, HMODULE, HWND, RECT};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11RenderTargetView, ID3D11Texture2D,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIDevice, IDXGIFactory2, IDXGISwapChain1, DXGI_PRESENT,
    DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL, DXGI_USAGE_RENDER_TARGET_OUTPUT,
};
use windows::Win32::Graphics::Gdi::{CreateSolidBrush, DeleteObject, FillRect, GetDC, ReleaseDC};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
use windows::Win32::System::WinRT::Direct3D11::CreateDirect3D11DeviceFromDXGIDevice;
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, DispatchMessageW, PeekMessageW, ShowWindow, TranslateMessage,
    MSG, PM_REMOVE, SW_SHOW, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
};

const MEASURE: Duration = Duration::from_secs(6);
const WIDTH: i32 = 640;
const HEIGHT: i32 = 360;

fn main() -> windows::core::Result<()> {
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok()? };

    // The window lives on the painting thread, which also pumps its messages:
    // a window whose owner never pumps is marked unresponsive and DWM starts
    // compositing a ghost of it, which is not what this is trying to measure.
    let painting = Arc::new(AtomicU64::new(1));
    let (hwnd_tx, hwnd_rx) = std::sync::mpsc::channel();
    let painter = {
        let painting = painting.clone();
        std::thread::spawn(move || paint_until_stopped(&hwnd_tx, &painting))
    };
    let hwnd = HWND(hwnd_rx.recv().expect("the painter reports its window") as *mut _);

    let device = direct3d_device()?;
    let item: GraphicsCaptureItem = unsafe {
        factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?.CreateForWindow(hwnd)?
    };
    let size = item.Size()?;
    let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
        &device,
        DirectXPixelFormat::B8G8R8A8UIntNormalized,
        2,
        size,
    )?;
    let session = pool.CreateCaptureSession(&item)?;

    let arrived = Arc::new(AtomicU64::new(0));
    let counter = arrived.clone();
    let token = pool.FrameArrived(&TypedEventHandler::new(
        move |pool: windows::core::Ref<'_, Direct3D11CaptureFramePool>,
              _: windows::core::Ref<'_, IInspectable>| {
            if let Some(pool) = pool.as_ref() {
                // Taking the frame is what returns the surface to the pool; a
                // handler that never does stalls delivery after two frames.
                let _ = pool.TryGetNextFrame();
            }
            counter.fetch_add(1, Ordering::Relaxed);
            Ok(())
        },
    ))?;

    println!(
        "window {WIDTH}x{HEIGHT}, pool depth 2, measuring {}s per leg",
        MEASURE.as_secs()
    );
    match session.MinUpdateInterval() {
        Ok(interval) => println!(
            "MinUpdateInterval default = {} (100ns) = {:.3} ms = {:.2}/s",
            interval.Duration,
            interval.Duration as f64 / 10_000.0,
            10_000_000.0 / interval.Duration.max(1) as f64
        ),
        Err(error) => println!("MinUpdateInterval default unreadable: {error}"),
    }

    session.StartCapture()?;
    println!("leg 1 (system default): {:.2}/s", measure(&arrived));

    // Half the 120fps period: DWM rounds the interval up to a whole number of
    // refreshes, so asking for exactly 1/120s lands on the boundary *below* 120.
    let wanted = TimeSpan {
        Duration: 10_000_000 / 240,
    };
    match session.SetMinUpdateInterval(wanted) {
        Ok(()) => println!(
            "set MinUpdateInterval = {} (100ns) = {:.3} ms; reads back {:?}",
            wanted.Duration,
            wanted.Duration as f64 / 10_000.0,
            session.MinUpdateInterval().map(|i| i.Duration)
        ),
        Err(error) => println!("SetMinUpdateInterval failed: {error}"),
    }
    println!(
        "leg 2 (half of the 120fps period): {:.2}/s",
        measure(&arrived)
    );

    // A third leg at the smallest interval the API will take, to separate "the
    // knob works" from "120 is itself a ceiling".
    match session.SetMinUpdateInterval(TimeSpan { Duration: 1 }) {
        Ok(()) => println!("set MinUpdateInterval = 1 (100ns)"),
        Err(error) => println!("SetMinUpdateInterval(1) failed: {error}"),
    }
    println!("leg 3 (uncapped): {:.2}/s", measure(&arrived));

    session.Close()?;
    pool.RemoveFrameArrived(token)?;
    pool.Close()?;
    painting.store(0, Ordering::Relaxed);
    let paints = painter.join().unwrap_or(0);
    println!("painter drew {paints} times");
    Ok(())
}

fn measure(arrived: &AtomicU64) -> f64 {
    arrived.store(0, Ordering::Relaxed);
    let started = Instant::now();
    std::thread::sleep(MEASURE);
    let elapsed = started.elapsed().as_secs_f64();
    arrived.load(Ordering::Relaxed) as f64 / elapsed
}

/// Keeps the window's pixels changing so DWM has a reason to recompose it.
///
/// Two painters, chosen by `PROBE_PAINTER`: `gdi` fills the window DC, `d3d`
/// presents a flip-model swap chain with no vsync. The distinction is the whole
/// point of the second run -- a game is the second kind, and DWM does not
/// necessarily recompose the two at the same rate.
fn paint_until_stopped(hwnd_tx: &std::sync::mpsc::Sender<usize>, painting: &AtomicU64) -> u64 {
    let hwnd = unsafe {
        CreateWindowExW(
            Default::default(),
            windows::core::w!("STATIC"),
            windows::core::w!("wgc rate probe"),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            100,
            100,
            WIDTH,
            HEIGHT,
            None,
            None,
            None,
            None,
        )
        .expect("probe window")
    };
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOW);
    }
    hwnd_tx
        .send(hwnd.0 as usize)
        .expect("the prober is waiting");

    let rect = RECT {
        left: 0,
        top: 0,
        right: WIDTH,
        bottom: HEIGHT,
    };
    let d3d = (std::env::var("PROBE_PAINTER").as_deref() == Ok("d3d"))
        .then(|| SwapChainPainter::create(hwnd).expect("probe swap chain"));

    let mut painted = 0u64;
    let mut message = MSG::default();
    while painting.load(Ordering::Relaxed) == 1 {
        unsafe {
            while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        match &d3d {
            Some(painter) => painter.present(painted),
            None => unsafe {
                let dc = GetDC(Some(hwnd));
                let brush =
                    CreateSolidBrush(COLORREF((painted as u32).wrapping_mul(2_654_435_761)));
                FillRect(dc, &rect, brush);
                let _ = DeleteObject(brush.into());
                ReleaseDC(Some(hwnd), dc);
            },
        }
        painted += 1;
        // ~1000/s ceiling: far above any refresh rate, cheap enough to leave the
        // GPU and DWM the room the measurement is about.
        std::thread::sleep(Duration::from_millis(1));
    }
    unsafe {
        let _ = DestroyWindow(hwnd);
    }
    painted
}

/// A flip-model swap chain on the probe window, presented without waiting for
/// vsync -- the shape a game's output has, as opposed to GDI's.
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
                    Width: WIDTH as u32,
                    Height: HEIGHT as u32,
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
        let phase = (tick % 60) as f32 / 60.0;
        unsafe {
            self.context
                .ClearRenderTargetView(&self.target, &[phase, 1.0 - phase, phase, 1.0]);
            // SyncInterval 0: present as soon as the GPU is done, which is what
            // an uncapped game does.
            let _ = self.swap_chain.Present(0, DXGI_PRESENT(0));
        }
    }
}

fn direct3d_device() -> windows::core::Result<IDirect3DDevice> {
    let mut device: Option<ID3D11Device> = None;
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
            None,
        )?;
    }
    let dxgi: IDXGIDevice = device
        .expect("D3D11CreateDevice returned no device")
        .cast()?;
    let inspectable = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi)? };
    inspectable.cast()
}
