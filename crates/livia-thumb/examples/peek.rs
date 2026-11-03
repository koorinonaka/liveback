//! What the shell would show, without the shell: `cargo run -p livia-thumb
//! --example peek -- <file.lvb>` prints how long the lookup took and what came
//! out of it. The point of the timing is that it stays flat as the container
//! grows -- the handler reads three records, never the file.

fn main() {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: peek <file.lvb>");
        std::process::exit(2);
    };
    let mut file = std::fs::File::open(&path).expect("open the container");
    let started = std::time::Instant::now();
    let jpeg = livia_thumb::first_thumbnail_jpeg(&mut file).expect("read a thumbnail");
    let elapsed = started.elapsed();
    let decoded = image::load_from_memory_with_format(&jpeg, image::ImageFormat::Jpeg)
        .expect("decode the thumbnail");
    println!(
        "{path}: {} byte jpeg, {}x{}, in {:.1}ms",
        jpeg.len(),
        decoded.width(),
        decoded.height(),
        elapsed.as_secs_f64() * 1000.0
    );
}
