#[cfg(windows)]
fn main() {
    println!("cargo:rerun-if-changed=assets/log_viewer.html");
    generate_tray_icon().expect("failed to generate tray icon");

    let mut res = winresource::WindowsResource::new();
    res.set_icon("assets/icon.ico");
    res.set("ProductName", "SMS Notifier");
    res.set("FileDescription", "SMS Notifier");
    res.compile().unwrap();
}

#[cfg(not(windows))]
fn main() {
    println!("cargo:rerun-if-changed=assets/log_viewer.html");
    generate_tray_icon().expect("failed to generate tray icon");
}

/// Decode the 32px tray PNG to RGBA at build time so the executable embeds only
/// the small RGBA payload; `assets/icon.ico` is still embedded once as the
/// Windows application icon via winresource.
fn generate_tray_icon() -> Result<(), String> {
    println!("cargo:rerun-if-changed=assets/tray.png");
    let data = std::fs::read("assets/tray.png").map_err(|e| e.to_string())?;
    let mut reader = png::Decoder::new(std::io::Cursor::new(data.as_slice()))
        .read_info()
        .map_err(|e| e.to_string())?;
    // tray-icon's Icon::from_rgba needs 8-bit RGBA. Reject anything else at
    // build time instead of panicking at runtime, and keep the payload small.
    {
        let info = reader.info();
        if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
            return Err(format!(
                "assets/tray.png must be 8-bit RGBA (got {:?}/{:?}); re-export with an alpha channel",
                info.color_type, info.bit_depth
            ));
        }
        if info.width > 64 || info.height > 64 {
            return Err(format!(
                "assets/tray.png is {}x{}; use a small (e.g. 32x32) tray icon to keep the binary small",
                info.width, info.height
            ));
        }
    }
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let frame = reader.next_frame(&mut buf).map_err(|e| e.to_string())?;
    let out_dir = std::env::var("OUT_DIR").map_err(|e| e.to_string())?;
    let out_dir = std::path::Path::new(&out_dir);
    std::fs::write(out_dir.join("tray_icon.rgba"), &buf[..frame.buffer_size()])
        .map_err(|e| e.to_string())?;
    std::fs::write(
        out_dir.join("tray_icon.rs"),
        format!(
            "const TRAY_ICON_WIDTH: u32 = {};\nconst TRAY_ICON_HEIGHT: u32 = {};\n",
            frame.width, frame.height
        ),
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}
