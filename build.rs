fn main() {
    reject_tray_assets_outside_tray_slint();
    reject_tray_app_icon_byte_twins();
    #[cfg(windows)]
    {
        embed_manifest_for_all_targets();
        embed_icon_and_version();
    }
    slint_build::compile("ui/app.slint").expect("failed to compile ui/app.slint");
}

/// `assets/tray-icon/` is tray-only, and the rule cannot be left to a comment:
/// drawing one of those images in a window makes the renderer take over the
/// shared image cache entry, after which `Image::to_rgba8()` returns None and
/// the tray's `set_icon` fails silently -- no error, no log (see ui/tray.slint).
/// A whole directory rather than a list of files, so the check stays one string
/// match and never needs a roster update when an asset is added.
///
/// Not `#[cfg(windows)]`: the convention does not depend on the host.
fn reject_tray_assets_outside_tray_slint() {
    println!("cargo:rerun-if-changed=ui");
    for entry in std::fs::read_dir("ui").expect("failed to read ui/") {
        let path = entry.expect("failed to read a ui/ entry").path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("slint")
            || path.file_name().and_then(|name| name.to_str()) == Some("tray.slint")
        {
            continue;
        }
        let source = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
        assert!(
            !source.contains("assets/tray-icon/"),
            "{} references assets/tray-icon/. Those assets are tray-only: drawing one \
             in a window makes the tray's set_icon fail silently, because slint's \
             Image::to_rgba8() returns None once a renderer has uploaded the image to a \
             GPU texture. Put a copy under assets/app-icon/ and draw that instead.",
            path.display()
        );
    }
}

/// The other half of the tray-asset rule, and a different hole from
/// `reject_tray_assets_outside_tray_slint()`: that one bans *referencing*
/// `assets/tray-icon/` from another `.slint`, this one bans a tray PNG being
/// *byte-identical* to an `assets/app-icon/` PNG. The tray copies must stay the
/// same picture but different bytes -- equal bytes get folded into one static by
/// constant merging, slint keys its image cache on the pointer, so the tray ends
/// up sharing the window's cache entry; once the window draws it the image lives
/// in a GPU texture, `Image::to_rgba8()` returns None and `Shell_NotifyIcon`
/// fails with no error and no log (task2470/2490: the recording badge never went
/// away on stop).
///
/// Not `#[cfg(windows)]`: the convention does not depend on the host.
fn reject_tray_app_icon_byte_twins() {
    println!("cargo:rerun-if-changed=assets/tray-icon");
    println!("cargo:rerun-if-changed=assets/app-icon");
    let pngs = |dir: &str| {
        let mut files: Vec<(std::path::PathBuf, Vec<u8>)> = std::fs::read_dir(dir)
            .unwrap_or_else(|error| panic!("failed to read {dir}/: {error}"))
            .map(|entry| entry.expect("failed to read a directory entry").path())
            .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("png"))
            .map(|path| {
                let bytes = std::fs::read(&path)
                    .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
                (path, bytes)
            })
            .collect();
        files.sort_by(|a, b| a.0.cmp(&b.0));
        files
    };
    let app_icons = pngs("assets/app-icon");
    for (tray_path, tray_bytes) in pngs("assets/tray-icon") {
        for (app_path, app_bytes) in &app_icons {
            assert!(
                &tray_bytes != app_bytes,
                "{} and {} are byte-identical. Same pixels is fine -- same bytes is not: \
                 identical arrays get folded together by constant merging, and slint keys \
                 its image cache on the pointer, so the tray shares the window's entry. \
                 Once the window draws it, Image::to_rgba8() returns None and the tray's \
                 set_icon fails silently -- no error, no log, the recording badge just \
                 never goes away. Fix it by re-encoding the tray side to different bytes \
                 with the same pixels (change the PNG row filters or the zlib level).",
                tray_path.display(),
                app_path.display()
            );
        }
    }
}

/// Common-Controls v6. Without the manifest dependency, comctl32 resolves to its
/// manifest-less v5 stub, which is missing symbols the tray/menu code needs (e.g.
/// `TaskDialogIndirect`), and the binary fails to launch with
/// STATUS_ENTRYPOINT_NOT_FOUND. This used to ride along with tauri-build (which
/// only linked it into the main `[[bin]]`, never into `cargo test` harnesses --
/// hence `rustc-link-arg`, which covers both).
/// https://github.com/tauri-apps/tauri/issues/13419
#[cfg(windows)]
fn embed_manifest_for_all_targets() {
    let manifest = std::env::current_dir()
        .unwrap()
        .join("windows-app-manifest.xml");
    println!("cargo:rerun-if-changed={}", manifest.display());
    println!("cargo:rustc-link-arg=/MANIFEST:EMBED");
    println!(
        "cargo:rustc-link-arg=/MANIFESTINPUT:{}",
        manifest.to_str().unwrap()
    );
}

/// The icon Explorer, the taskbar and the Start Menu shortcut show, plus the
/// version block. `tauri-build` emitted both through tauri-winres until task131.
///
/// Two icon groups ride along: ID 1 is the app icon (G-P1), which Windows shows
/// for the exe itself -- Explorer, the Start Menu, the pinned taskbar button;
/// ID 2 is the `.lvb` file icon, which the installer's `DefaultIcon` points at
/// by resource ID (`,-2`, see installer/liveback.nsi).
///
/// The exe resource is *not* picked up as the window's HICON: the frameless
/// winit window only gets one if `Window.icon` is set, so the window icon is
/// registered separately by `icon:` in `ui/app.slint` (task2480).
#[cfg(windows)]
fn embed_icon_and_version() {
    println!("cargo:rerun-if-changed=assets/app-icon/liveback-dark.ico");
    println!("cargo:rerun-if-changed=assets/lvb-icon/liveback-lvb.ico");
    let mut resource = winresource::WindowsResource::new();
    resource.set_icon("assets/app-icon/liveback-dark.ico");
    resource.set_icon_with_id("assets/lvb-icon/liveback-lvb.ico", "2");
    resource.set("ProductName", "Liveback");
    resource.set("FileDescription", "Liveback");
    if let Err(error) = resource.compile() {
        // A missing rc.exe must not make the crate unbuildable; the exe just
        // ends up without an icon.
        println!("cargo:warning=failed to embed the icon resource: {error}");
    }
}
