//! Headless rendering: a custom [`slint::platform::Platform`] backed by a
//! thread-local [`MinimalSoftwareWindow`], used by the unit tests (real
//! component interaction without a display) and the `screenshot`
//! subcommand (pixel-true evidence renders for the phase report).
//!
//! The app binary renders the *same* `.slint` code this way — the only
//! difference from the desktop path is the renderer (software vs femtovg),
//! so screenshots are honest evidence, not mockups.

use std::fs::File;
use std::io::BufWriter;
use std::rc::Rc;

use slint::platform::software_renderer::{
    MinimalSoftwareWindow, RepaintBufferType, SoftwareRenderer,
};
use slint::platform::{Platform, PlatformError, WindowAdapter};
use slint::{ComponentHandle, PhysicalSize, SharedPixelBuffer, WindowSize};

thread_local! {
    static WINDOW: Rc<MinimalSoftwareWindow> =
        MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    static INSTALLED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

struct HeadlessPlatform;

impl Platform for HeadlessPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        Ok(current_window())
    }
}

/// The headless window for the current thread.
#[must_use]
pub fn current_window() -> Rc<MinimalSoftwareWindow> {
    WINDOW.with(|w| w.clone())
}

/// Install the headless platform on the current thread. Slint's context
/// (and therefore the platform) is thread-local, so this runs once per
/// thread — the test harness spawns a fresh thread per test. Safe to call
/// repeatedly on the same thread (no-op after the first). Must run before
/// any component is created on that thread.
pub fn install() {
    // If a real backend is forced via the environment (desktop runs),
    // respect it and do not override.
    if std::env::var_os("SLINT_BACKEND").is_some() {
        return;
    }
    WINDOW.with(|_| {
        INSTALLED.with(|done| {
            if done.get() {
                return;
            }
            done.set(true);
            // The platform slot is thread-local; a prior install on this
            // thread (or a leftover context) is fine to ignore.
            let _ = slint::platform::set_platform(Box::new(HeadlessPlatform));
        });
    });
}

/// Render the component's window once at `size` and return the RGB buffer.
///
/// # Panics
/// If the window needs a bigger buffer than `size` (never the case — the
/// renderer clips to the buffer).
#[must_use]
pub fn render_to_buffer(
    app: &impl ComponentHandle,
    size: PhysicalSize,
) -> SharedPixelBuffer<slint::Rgb8Pixel> {
    let window = current_window();
    window.set_size(WindowSize::Physical(size));
    app.window().request_redraw();
    let mut buffer = SharedPixelBuffer::<slint::Rgb8Pixel>::new(size.width, size.height);
    let drew = window.draw_if_needed(|renderer: &SoftwareRenderer| {
        renderer.render(buffer.make_mut_slice(), size.width as usize);
    });
    if !drew {
        // force one more pass — layout changes may not have flagged dirty
        app.window().request_redraw();
        window.draw_if_needed(|renderer: &SoftwareRenderer| {
            renderer.render(buffer.make_mut_slice(), size.width as usize);
        });
    }
    buffer
}

/// Render the app to a PNG file (the `screenshot` subcommand and test
/// evidence artifacts).
///
/// # Errors
/// I/O or PNG encoding failures.
pub fn render_to_png(
    app: &impl ComponentHandle,
    size: PhysicalSize,
    path: &std::path::Path,
) -> std::io::Result<()> {
    let buffer = render_to_buffer(app, size);
    write_png(path, buffer.width(), buffer.height(), buffer.as_bytes())
}

/// Encode RGB8 bytes as a PNG.
///
/// # Errors
/// I/O or PNG encoding failures.
pub fn write_png(
    path: &std::path::Path,
    width: u32,
    height: u32,
    rgb: &[u8],
) -> std::io::Result<()> {
    let file = File::create(path)?;
    let mut enc = png::Encoder::new(BufWriter::new(file), width, height);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc
        .write_header()
        .map_err(|e| std::io::Error::other(format!("png header: {e}")))?;
    writer
        .write_image_data(rgb)
        .map_err(|e| std::io::Error::other(format!("png data: {e}")))?;
    writer
        .finish()
        .map_err(|e| std::io::Error::other(format!("png finish: {e}")))?;
    Ok(())
}
