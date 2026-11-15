//! The host: command line, the winit window + wgpu surface, translation of
//! window events into `Input`, frame pacing, native dialogs, the
//! single-instance socket, and the headless `--snapshot` test harness.
#![allow(dead_code)]

use crate::app::App;
use crate::gfx::{self, BgFrame, Gpu, Renderer, SceneRaw};
use crate::platform::{AppOptions, DialogResult, Effect, FrameOut};
use crate::ui::{CursorKind, Input, Key, Mods};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalSize};
use winit::event::{ElementState, Ime, KeyEvent, MouseButton, MouseScrollDelta, StartCause, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key as WinitKey, NamedKey};
use winit::window::{CursorIcon, Fullscreen, Window, WindowId};

const HELP: &str = "\
Glint — a small, pretty text editor

Usage:
  glint [FILES...]            open files (in the running instance if there is one)
  glint --snapshot OUT.png [--size WxH] [--scale F] [--do SCRIPT] [--no-session] [FILES...]
                              render one frame off-screen (testing)
  glint --help | --version

Snapshot script steps (separated by ';', use '\\;' for a literal semicolon):
  key:ctrl+shift+p   type:text   move:x,y   click:x,y[,mods]   dclick:x,y   rclick:x,y
  mclick:x,y   drag:x1,y1,x2,y2   wheel:x,y,dy[,mods]   wait:secs   resize:WxH
  focus:0|1   dialog:path   open:path   shot:path.png
";

#[derive(Debug)]
enum UserEvent {
    Open(Vec<PathBuf>),
    Dialog(DialogResult),
}

struct Args {
    paths: Vec<PathBuf>,
    snapshot: Option<PathBuf>,
    size: (u32, u32),
    scale: f32,
    script: String,
    no_session: bool,
}

fn absolute(p: &str) -> PathBuf {
    let p = Path::new(p);
    if p.is_absolute() {
        return p.to_path_buf();
    }
    std::env::current_dir().map(|d| d.join(p)).unwrap_or_else(|_| p.to_path_buf())
}

fn parse_size(s: &str) -> Option<(u32, u32)> {
    let (w, h) = s.split_once(['x', 'X'])?;
    let (w, h) = (w.trim().parse::<u32>().ok()?, h.trim().parse::<u32>().ok()?);
    if w == 0 || h == 0 { None } else { Some((w, h)) }
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args { paths: Vec::new(), snapshot: None, size: (1280, 800), scale: 1.0, script: String::new(), no_session: false };
    let mut it = std::env::args().skip(1);
    let mut only_files = false;
    while let Some(arg) = it.next() {
        if only_files || !arg.starts_with("--") {
            a.paths.push(absolute(&arg));
            continue;
        }
        let mut value = |name: &str| it.next().ok_or_else(|| format!("{name} needs a value"));
        match arg.as_str() {
            "--" => only_files = true,
            "--help" => {
                print!("{HELP}");
                std::process::exit(0);
            }
            "--version" => {
                println!("glint {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "--snapshot" => a.snapshot = Some(absolute(&value("--snapshot")?)),
            "--size" => {
                let v = value("--size")?;
                a.size = parse_size(&v).ok_or_else(|| format!("bad --size '{v}', expected WxH"))?;
            }
            "--scale" => {
                let v = value("--scale")?;
                a.scale = v.parse::<f32>().ok().filter(|s| *s > 0.1 && *s < 8.0).ok_or_else(|| format!("bad --scale '{v}'"))?;
            }
            "--do" => a.script = value("--do")?,
            "--no-session" => a.no_session = true,
            other => return Err(format!("unknown option '{other}' (see --help)")),
        }
    }
    Ok(a)
}

pub fn main() {
    let _ = env_logger::Builder::from_default_env().try_init();
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("glint: {e}");
            std::process::exit(2);
        }
    };
    let code = if args.snapshot.is_some() { run_snapshot(args) } else { run_window(args) };
    std::process::exit(code);
}

// ====================================================================
// windowed mode
// ====================================================================

fn socket_path() -> PathBuf {
    crate::config::runtime_dir().join("glint.sock")
}

/// Hand our file list to an already running instance. Returns true if one
/// took it.
fn send_to_running(sock: &Path, paths: &[PathBuf]) -> bool {
    let Ok(mut stream) = UnixStream::connect(sock) else { return false };
    let mut msg = String::new();
    for p in paths {
        msg.push_str(&p.to_string_lossy());
        msg.push('\n');
    }
    stream.write_all(msg.as_bytes()).is_ok() && stream.flush().is_ok()
}

fn listen(sock: &Path, proxy: EventLoopProxy<UserEvent>) -> bool {
    let _ = std::fs::remove_file(sock);
    if let Some(dir) = sock.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let listener = match UnixListener::bind(sock) {
        Ok(l) => l,
        Err(e) => {
            log::warn!("single-instance socket unavailable: {e}");
            return false;
        }
    };
    std::thread::Builder::new()
        .name("glint-ipc".into())
        .spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                let paths: Vec<PathBuf> = BufReader::new(stream)
                    .lines()
                    .map_while(Result::ok)
                    .filter(|l| !l.trim().is_empty())
                    .map(PathBuf::from)
                    .collect();
                // an empty list still raises the window
                if proxy.send_event(UserEvent::Open(paths)).is_err() {
                    break;
                }
            }
        })
        .is_ok()
}

struct Gfx {
    gpu: Gpu,
    instance: wgpu::Instance,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    /// Non-sRGB format the frame views (and the renderer) use.
    view_format: wgpu::TextureFormat,
    renderer: Renderer,
    // Keep the window last so it is dropped after the surface.
    window: Arc<Window>,
}

struct Host {
    app: App,
    gfx: Option<Gfx>,
    proxy: EventLoopProxy<UserEvent>,
    start: Instant,
    last: f64,
    next_bg: f64,
    /// Pending timed wake-up (seconds on the `start` clock).
    deadline: Option<f64>,
    occluded: bool,
    force_dirty: bool,
    mods: Mods,
    /// Background parameters (time excluded) of the last rendered background.
    bg_drawn: Option<BgKey>,
    failed: bool,
}

/// Everything about a background frame except its clock, so a paused
/// background still re-renders when the theme or a knob changes.
#[derive(Clone, Copy, PartialEq)]
struct BgKey {
    a: SceneRaw,
    b: SceneRaw,
    fade: f32,
    scale: f32,
}

impl BgKey {
    fn of(bg: &BgFrame) -> BgKey {
        let (mut a, mut b) = (bg.a, bg.b);
        a.a[1] = 0.0;
        b.a[1] = 0.0;
        BgKey { a, b, fade: bg.fade, scale: bg.scale }
    }
}

fn pick_format(caps: &wgpu::SurfaceCapabilities) -> Option<(wgpu::TextureFormat, wgpu::TextureFormat)> {
    use wgpu::TextureFormat as F;
    // (surface format, view format): colour math is done in sRGB space, so
    // the view must not apply an sRGB transfer function.
    for want in [F::Bgra8Unorm, F::Rgba8Unorm] {
        if caps.formats.contains(&want) {
            return Some((want, want));
        }
    }
    for want in [F::Bgra8UnormSrgb, F::Rgba8UnormSrgb] {
        if caps.formats.contains(&want) {
            return Some((want, want.remove_srgb_suffix()));
        }
    }
    caps.formats.first().map(|f| (*f, f.remove_srgb_suffix()))
}

impl Gfx {
    fn new(event_loop: &ActiveEventLoop) -> Result<Gfx, String> {
        let attrs = Window::default_attributes()
            .with_title("Glint")
            .with_inner_size(LogicalSize::new(1280.0, 800.0))
            .with_min_inner_size(LogicalSize::new(480.0, 320.0));
        // The Wayland and X11 extension traits both set the same application
        // name (app id / WM_CLASS), so one call covers either backend.
        #[cfg(target_os = "linux")]
        let attrs = winit::platform::wayland::WindowAttributesExtWayland::with_name(attrs, "glint", "glint");
        let window = Arc::new(event_loop.create_window(attrs).map_err(|e| format!("could not create a window: {e}"))?);

        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_with_display_handle(Box::new(
            event_loop.owned_display_handle(),
        )));
        let surface = instance.create_surface(window.clone()).map_err(|e| format!("could not create a surface: {e}"))?;
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            ..Default::default()
        }))
        .map_err(|e| format!("no suitable GPU: {e}"))?;
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
            .map_err(|e| format!("could not open the GPU: {e}"))?;

        let caps = surface.get_capabilities(&adapter);
        let (format, view_format) = pick_format(&caps).ok_or("the surface reports no usable formats")?;
        let alpha_mode = if caps.alpha_modes.contains(&wgpu::CompositeAlphaMode::Opaque) {
            wgpu::CompositeAlphaMode::Opaque
        } else {
            caps.alpha_modes.first().copied().unwrap_or(wgpu::CompositeAlphaMode::Auto)
        };
        let size = window.inner_size();
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::AutoVsync,
            alpha_mode,
            view_formats: if view_format != format { vec![view_format] } else { vec![] },
            desired_maximum_frame_latency: 2,
            color_space: wgpu::SurfaceColorSpace::Auto,
        };
        surface.configure(&device, &config);
        let gpu = Gpu { device, queue };
        let renderer = Renderer::new(&gpu, view_format, config.width, config.height);
        window.set_ime_allowed(true);
        Ok(Gfx { gpu, instance, surface, config, view_format, renderer, window })
    }

    fn reconfigure(&mut self) {
        self.surface.configure(&self.gpu.device, &self.config);
    }
}

fn translate_key(ev: &KeyEvent) -> Option<Key> {
    Some(match &ev.logical_key {
        WinitKey::Named(n) => match n {
            NamedKey::Enter => Key::Enter,
            NamedKey::Tab => Key::Tab,
            NamedKey::Backspace => Key::Backspace,
            NamedKey::Delete => Key::Delete,
            NamedKey::Escape => Key::Escape,
            NamedKey::ArrowLeft => Key::Left,
            NamedKey::ArrowRight => Key::Right,
            NamedKey::ArrowUp => Key::Up,
            NamedKey::ArrowDown => Key::Down,
            NamedKey::Home => Key::Home,
            NamedKey::End => Key::End,
            NamedKey::PageUp => Key::PageUp,
            NamedKey::PageDown => Key::PageDown,
            NamedKey::Space => Key::Char(' '),
            NamedKey::F1 => Key::F(1),
            NamedKey::F2 => Key::F(2),
            NamedKey::F3 => Key::F(3),
            NamedKey::F4 => Key::F(4),
            NamedKey::F5 => Key::F(5),
            NamedKey::F6 => Key::F(6),
            NamedKey::F7 => Key::F(7),
            NamedKey::F8 => Key::F(8),
            NamedKey::F9 => Key::F(9),
            NamedKey::F10 => Key::F(10),
            NamedKey::F11 => Key::F(11),
            NamedKey::F12 => Key::F(12),
            _ => return None,
        },
        WinitKey::Character(s) => Key::Char(s.chars().next()?),
        _ => return None,
    })
}

fn cursor_icon(c: CursorKind) -> CursorIcon {
    match c {
        CursorKind::Default => CursorIcon::Default,
        CursorKind::Text => CursorIcon::Text,
        CursorKind::Pointer => CursorIcon::Pointer,
        CursorKind::ResizeH => CursorIcon::EwResize,
        CursorKind::ResizeV => CursorIcon::NsResize,
    }
}

impl Host {
    fn now(&self) -> f64 {
        self.start.elapsed().as_secs_f64()
    }

    fn request_redraw(&self) {
        if let Some(g) = &self.gfx {
            g.window.request_redraw();
        }
    }

    fn quit(&mut self, event_loop: &ActiveEventLoop) {
        self.app.shutdown();
        event_loop.exit();
    }

    fn resized(&mut self, size: PhysicalSize<u32>) {
        let Some(g) = &mut self.gfx else { return };
        if size.width == 0 || size.height == 0 {
            return;
        }
        g.config.width = size.width;
        g.config.height = size.height;
        g.reconfigure();
        g.renderer.resize(&g.gpu, size.width, size.height);
        self.app.resize(size.width as f32, size.height as f32, g.window.scale_factor() as f32);
        self.force_dirty = true;
        g.window.request_redraw();
    }

    fn redraw(&mut self, event_loop: &ActiveEventLoop) {
        if self.gfx.is_none() {
            return;
        }
        let now = self.now();
        let dt = (now - self.last).clamp(0.0, 0.1);
        self.last = now;
        let out: FrameOut = self.app.frame(now, dt as f32);

        // background pacing: capped fps while it animates; when it is paused
        // or static, re-render only if something other than its clock changed
        let mut bg = out.bg;
        let key = BgKey::of(&bg);
        let live = out.bg_animating && !self.occluded;
        bg.render = if live { now >= self.next_bg } else { self.bg_drawn != Some(key) };

        let g = self.gfx.as_mut().unwrap();
        let mut presented = false;
        match g.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame) => {
                let view = frame
                    .texture
                    .create_view(&wgpu::TextureViewDescriptor { format: Some(g.view_format), ..Default::default() });
                g.renderer.render(&g.gpu, &view, &self.app.cx.scene, out.scene_dirty || self.force_dirty, &bg, &mut self.app.cx.text);
                g.window.pre_present_notify();
                g.gpu.queue.present(frame);
                presented = true;
            }
            wgpu::CurrentSurfaceTexture::Timeout => {
                g.window.request_redraw();
            }
            // hidden: wait for the next event instead of spinning
            wgpu::CurrentSurfaceTexture::Occluded => {}
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Suboptimal(_) => {
                g.reconfigure();
                g.window.request_redraw();
            }
            wgpu::CurrentSurfaceTexture::Lost => match g.instance.create_surface(g.window.clone()) {
                Ok(s) => {
                    g.surface = s;
                    g.reconfigure();
                    g.window.request_redraw();
                }
                Err(e) => {
                    log::error!("lost the surface and could not recreate it: {e}");
                    self.failed = true;
                    self.quit(event_loop);
                    return;
                }
            },
            wgpu::CurrentSurfaceTexture::Validation => {
                log::error!("surface validation error");
                self.failed = true;
                self.quit(event_loop);
                return;
            }
        }
        if presented {
            self.force_dirty = false;
            if bg.render {
                self.bg_drawn = Some(key);
                if live {
                    self.next_bg = now + 1.0 / out.bg_fps.max(1) as f64;
                }
            }
        } else {
            // the scene we were told about never reached the screen
            self.force_dirty = true;
        }

        if self.run_effects(event_loop) {
            return;
        }

        // ---- when to draw next
        let g = self.gfx.as_ref().unwrap();
        if out.animating || self.app.needs_redraw() {
            self.deadline = None;
            event_loop.set_control_flow(ControlFlow::Wait);
            g.window.request_redraw();
            return;
        }
        let mut deadline = out.wake;
        if live {
            deadline = Some(deadline.map_or(self.next_bg, |d| d.min(self.next_bg)));
        }
        self.deadline = deadline;
        match deadline {
            Some(d) => {
                let at = self.start + Duration::from_secs_f64(d.max(0.0));
                event_loop.set_control_flow(ControlFlow::WaitUntil(at));
            }
            None => event_loop.set_control_flow(ControlFlow::Wait),
        }
    }

    /// Carry out what the app asked for. Returns true if the app is quitting.
    fn run_effects(&mut self, event_loop: &ActiveEventLoop) -> bool {
        for fx in self.app.take_effects() {
            let Some(g) = &self.gfx else { break };
            match fx {
                Effect::Title(t) => g.window.set_title(&t),
                Effect::Cursor(c) => g.window.set_cursor(cursor_icon(c)),
                Effect::OpenDialog { dir } => {
                    let proxy = self.proxy.clone();
                    spawn_dialog(move || {
                        let mut d = rfd::FileDialog::new().set_title("Open");
                        if let Some(dir) = dir.filter(|d| d.is_dir()) {
                            d = d.set_directory(dir);
                        }
                        if let Some(files) = d.pick_files().filter(|f| !f.is_empty()) {
                            let _ = proxy.send_event(UserEvent::Dialog(DialogResult::Open(files)));
                        }
                    });
                }
                Effect::SaveDialog { doc, dir, name } => {
                    let proxy = self.proxy.clone();
                    spawn_dialog(move || {
                        let mut d = rfd::FileDialog::new().set_title("Save As");
                        if let Some(dir) = dir.filter(|d| d.is_dir()) {
                            d = d.set_directory(dir);
                        }
                        if !name.is_empty() {
                            d = d.set_file_name(name);
                        }
                        let path = d.save_file();
                        let _ = proxy.send_event(UserEvent::Dialog(DialogResult::Save { doc, path }));
                    });
                }
                Effect::ToggleFullscreen => {
                    let on = g.window.fullscreen().is_some();
                    g.window.set_fullscreen(if on { None } else { Some(Fullscreen::Borderless(None)) });
                }
                Effect::Reveal(path) => {
                    if let Err(e) = std::process::Command::new("xdg-open")
                        .arg(&path)
                        .stdin(std::process::Stdio::null())
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .spawn()
                    {
                        log::warn!("xdg-open {}: {e}", path.display());
                    }
                }
                Effect::Quit => {
                    self.quit(event_loop);
                    return true;
                }
            }
        }
        false
    }
}

fn spawn_dialog(f: impl FnOnce() + Send + 'static) {
    if let Err(e) = std::thread::Builder::new().name("glint-dialog".into()).spawn(f) {
        log::warn!("could not start a dialog thread: {e}");
    }
}

impl ApplicationHandler<UserEvent> for Host {
    fn new_events(&mut self, event_loop: &ActiveEventLoop, cause: StartCause) {
        if let StartCause::ResumeTimeReached { .. } = cause {
            // the timed wake-up fired; the redraw sets the next one
            self.deadline = None;
            event_loop.set_control_flow(ControlFlow::Wait);
            self.request_redraw();
        }
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.gfx.is_some() {
            return;
        }
        match Gfx::new(event_loop) {
            Ok(g) => {
                let size = g.window.inner_size();
                self.app.resize(size.width.max(1) as f32, size.height.max(1) as f32, g.window.scale_factor() as f32);
                self.force_dirty = true;
                g.window.request_redraw();
                self.gfx = Some(g);
            }
            Err(e) => {
                eprintln!("glint: {e}");
                self.failed = true;
                self.app.shutdown();
                event_loop.exit();
            }
        }
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: UserEvent) {
        self.app.set_time(self.now());
        match event {
            UserEvent::Open(paths) => {
                if !paths.is_empty() {
                    self.app.open_paths(paths);
                }
                if let Some(g) = &self.gfx {
                    g.window.focus_window();
                }
            }
            UserEvent::Dialog(r) => self.app.dialog_result(r),
        }
        self.request_redraw();
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        if self.gfx.is_none() {
            return;
        }
        // input handlers run between frames; give them the real time
        self.app.set_time(self.now());
        match event {
            WindowEvent::RedrawRequested => {
                self.redraw(event_loop);
                return;
            }
            WindowEvent::CloseRequested => {
                self.quit(event_loop);
                return;
            }
            WindowEvent::Resized(size) => self.resized(size),
            WindowEvent::ScaleFactorChanged { .. } => {
                let size = self.gfx.as_ref().unwrap().window.inner_size();
                self.resized(size);
            }
            WindowEvent::Occluded(o) => {
                self.occluded = o;
                if !o {
                    self.request_redraw();
                }
            }
            WindowEvent::Focused(f) => self.app.input(Input::Focus(f)),
            WindowEvent::ModifiersChanged(m) => {
                let s = m.state();
                self.mods = Mods { ctrl: s.control_key(), shift: s.shift_key(), alt: s.alt_key() };
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if event.state == ElementState::Pressed {
                    if let Some(key) = translate_key(&event) {
                        let text = event.text.as_ref().map(|t| t.to_string());
                        self.app.input(Input::Key { key, mods: self.mods, text, repeat: event.repeat });
                    }
                }
            }
            WindowEvent::Ime(Ime::Commit(s)) => {
                if !s.is_empty() {
                    self.app.input(Input::Text(s));
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.app.input(Input::MouseMove(position.x as f32, position.y as f32));
            }
            WindowEvent::CursorLeft { .. } => self.app.input(Input::MouseLeft),
            WindowEvent::MouseInput { state, button, .. } => {
                let b = match button {
                    MouseButton::Left => Some(0u8),
                    MouseButton::Middle => Some(1),
                    MouseButton::Right => Some(2),
                    _ => None,
                };
                if let Some(button) = b {
                    self.app.input(match state {
                        ElementState::Pressed => Input::MouseDown { button, mods: self.mods },
                        ElementState::Released => Input::MouseUp { button },
                    });
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let (dx, dy, pixels) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => (x, y, false),
                    MouseScrollDelta::PixelDelta(p) => (p.x as f32, p.y as f32, true),
                };
                self.app.input(Input::Wheel { dx, dy, pixels, mods: self.mods });
            }
            WindowEvent::DroppedFile(p) => self.app.open_paths(vec![p]),
            _ => {}
        }
        if self.app.needs_redraw() {
            self.request_redraw();
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // A timed wake-up can be overtaken by other events; make sure it
        // still fires once it is due.
        if let Some(d) = self.deadline {
            if self.now() >= d {
                self.deadline = None;
                event_loop.set_control_flow(ControlFlow::Wait);
                self.request_redraw();
            }
        }
    }
}

fn run_window(args: Args) -> i32 {
    let sock = socket_path();
    if send_to_running(&sock, &args.paths) {
        return 0;
    }

    let event_loop = match EventLoop::<UserEvent>::with_user_event().build() {
        Ok(l) => l,
        Err(e) => {
            eprintln!("glint: could not start the event loop: {e}");
            return 1;
        }
    };
    let proxy = event_loop.create_proxy();
    let listening = listen(&sock, proxy.clone());

    // The app comes up before the window so that a broken session or config
    // can never leave an empty window behind.
    let app = App::new(AppOptions { system_clipboard: true, restore_session: !args.no_session, write_config: true, paths: args.paths });
    let mut host = Host {
        app,
        gfx: None,
        proxy,
        start: Instant::now(),
        last: 0.0,
        next_bg: 0.0,
        deadline: None,
        occluded: false,
        force_dirty: true,
        mods: Mods::default(),
        bg_drawn: None,
        failed: false,
    };
    let result = event_loop.run_app(&mut host);
    if result.is_err() {
        // the normal quit path already saved; this one did not get the chance
        host.app.shutdown();
    }
    if listening {
        let _ = std::fs::remove_file(&sock);
    }
    // drop GPU objects before the process exits
    host.gfx = None;
    match result {
        Ok(()) if !host.failed => 0,
        Ok(()) => 1,
        Err(e) => {
            eprintln!("glint: {e}");
            1
        }
    }
}

// ====================================================================
// snapshot mode
// ====================================================================

const DT: f64 = 1.0 / 60.0;

struct Snap {
    gpu: Gpu,
    renderer: Renderer,
    app: App,
    w: u32,
    h: u32,
    scale: f32,
    t: f64,
    bg: BgFrame,
    /// Answers for upcoming file dialogs.
    answers: VecDeque<PathBuf>,
    quit: bool,
}

impl Snap {
    fn step(&mut self, n: usize) {
        for _ in 0..n {
            if self.quit {
                return;
            }
            let out = self.app.frame(self.t, DT as f32);
            self.t += DT;
            self.bg = out.bg;
            for fx in self.app.take_effects() {
                match fx {
                    Effect::OpenDialog { .. } => {
                        let paths: Vec<PathBuf> = self.answers.drain(..).collect();
                        if !paths.is_empty() {
                            self.app.dialog_result(DialogResult::Open(paths));
                        }
                    }
                    Effect::SaveDialog { doc, .. } => {
                        let path = self.answers.pop_front();
                        self.app.dialog_result(DialogResult::Save { doc, path });
                    }
                    Effect::Quit => self.quit = true,
                    Effect::Title(_) | Effect::Cursor(_) | Effect::ToggleFullscreen | Effect::Reveal(_) => {}
                }
            }
        }
    }

    fn shot(&mut self, path: &Path) -> Result<(), String> {
        let mut bg = self.bg;
        bg.render = true;
        let px = gfx::render_to_pixels(&self.gpu, &mut self.renderer, self.w, self.h, &self.app.cx.scene, &bg, &mut self.app.cx.text);
        if let Some(dir) = path.parent() {
            if !dir.as_os_str().is_empty() {
                let _ = std::fs::create_dir_all(dir);
            }
        }
        gfx::save_png(path, self.w, self.h, &px).map_err(|e| format!("{}: {e}", path.display()))
    }

    fn send(&mut self, ev: Input) {
        self.app.input(ev);
    }

    fn click(&mut self, x: f32, y: f32, button: u8, mods: Mods, times: usize) {
        self.send(Input::MouseMove(x, y));
        self.step(1);
        for i in 0..times {
            if i > 0 {
                self.step(1);
            }
            self.send(Input::MouseDown { button, mods });
            self.send(Input::MouseUp { button });
        }
    }

    fn run_step(&mut self, step: &str) -> Result<(), String> {
        let (name, arg) = step.split_once(':').unwrap_or((step, ""));
        let name = name.trim().to_ascii_lowercase();
        match name.as_str() {
            "key" => {
                let (key, mods) = parse_combo(arg.trim())?;
                let text = key_text(key, mods);
                self.send(Input::Key { key, mods, text, repeat: false });
            }
            "type" => {
                for c in unescape(arg).chars() {
                    let (key, text) = match c {
                        '\n' => (Key::Enter, None),
                        '\t' => (Key::Tab, None),
                        c => (Key::Char(c), Some(c.to_string())),
                    };
                    self.send(Input::Key { key, mods: Mods::default(), text, repeat: false });
                    self.step(1);
                }
            }
            "move" => {
                let v = nums(arg, 2)?;
                self.send(Input::MouseMove(v[0], v[1]));
            }
            "click" | "dclick" | "rclick" | "mclick" => {
                let (v, mods) = nums_mods(arg, 2)?;
                let (button, times) = match name.as_str() {
                    "click" => (0, 1),
                    "dclick" => (0, 2),
                    "rclick" => (2, 1),
                    _ => (1, 1),
                };
                self.click(v[0], v[1], button, mods, times);
            }
            "drag" => {
                let (v, mods) = nums_mods(arg, 4)?;
                self.send(Input::MouseMove(v[0], v[1]));
                self.step(1);
                self.send(Input::MouseDown { button: 0, mods });
                self.step(1);
                let n = 8;
                for i in 1..=n {
                    let f = i as f32 / n as f32;
                    self.send(Input::MouseMove(v[0] + (v[2] - v[0]) * f, v[1] + (v[3] - v[1]) * f));
                    self.step(1);
                }
                self.send(Input::MouseUp { button: 0 });
            }
            "wheel" => {
                let (v, mods) = nums_mods(arg, 3)?;
                self.send(Input::MouseMove(v[0], v[1]));
                self.step(1);
                self.send(Input::Wheel { dx: 0.0, dy: v[2], pixels: false, mods });
            }
            "wait" => {
                let secs: f64 = arg.trim().parse().map_err(|_| format!("bad wait '{arg}'"))?;
                self.step((secs.max(0.0) / DT).ceil() as usize);
            }
            "resize" => {
                let (w, h) = parse_size(arg).ok_or_else(|| format!("bad resize '{arg}'"))?;
                self.w = w;
                self.h = h;
                self.app.resize(w as f32, h as f32, self.scale);
            }
            "focus" => {
                let on = !matches!(arg.trim(), "0" | "false" | "off" | "no");
                self.send(Input::Focus(on));
            }
            "dialog" => self.answers.push_back(absolute(arg.trim())),
            "open" => self.app.open_paths(vec![absolute(arg.trim())]),
            "shot" => {
                self.step(1);
                self.shot(&absolute(arg.trim()))?;
            }
            _ => return Err(format!("unknown step '{step}'")),
        }
        Ok(())
    }
}

/// Split a script on ';', honouring '\;'.
fn split_script(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '\\' if it.peek() == Some(&';') => {
                cur.push(';');
                it.next();
            }
            ';' => out.push(std::mem::take(&mut cur)),
            c => cur.push(c),
        }
    }
    out.push(cur);
    out.into_iter().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('\\') => out.push('\\'),
            Some(o) => {
                out.push('\\');
                out.push(o);
            }
            None => out.push('\\'),
        }
    }
    out
}

fn parse_mods(s: &str, mods: &mut Mods) -> bool {
    match s.trim().to_ascii_lowercase().as_str() {
        "ctrl" | "control" => mods.ctrl = true,
        "shift" => mods.shift = true,
        "alt" => mods.alt = true,
        _ => return false,
    }
    true
}

fn parse_combo(combo: &str) -> Result<(Key, Mods), String> {
    let mut mods = Mods::default();
    // a trailing '+' is the plus key itself ("ctrl++")
    let (head, key_name) = if combo == "+" {
        ("", "+")
    } else if let Some(h) = combo.strip_suffix("++") {
        (h, "+")
    } else {
        match combo.rsplit_once('+') {
            Some((h, k)) => (h, k),
            None => ("", combo),
        }
    };
    for m in head.split('+').filter(|m| !m.trim().is_empty()) {
        if !parse_mods(m, &mut mods) {
            return Err(format!("unknown modifier '{m}' in '{combo}'"));
        }
    }
    let k = key_name.trim();
    let lower = k.to_ascii_lowercase();
    let key = match lower.as_str() {
        "enter" | "return" => Key::Enter,
        "tab" => Key::Tab,
        "backspace" => Key::Backspace,
        "delete" | "del" => Key::Delete,
        "esc" | "escape" => Key::Escape,
        "left" => Key::Left,
        "right" => Key::Right,
        "up" => Key::Up,
        "down" => Key::Down,
        "home" => Key::Home,
        "end" => Key::End,
        "pageup" | "pgup" => Key::PageUp,
        "pagedown" | "pgdn" => Key::PageDown,
        "space" => Key::Char(' '),
        "semicolon" => Key::Char(';'),
        "plus" => Key::Char('+'),
        "backslash" => Key::Char('\\'),
        "comma" => Key::Char(','),
        "" => return Err(format!("no key in '{combo}'")),
        _ => {
            if let Some(n) = lower.strip_prefix('f').and_then(|n| n.parse::<u8>().ok()).filter(|n| (1..=12).contains(n)) {
                Key::F(n)
            } else {
                let mut chars = k.chars();
                let c = chars.next().unwrap();
                if chars.next().is_some() {
                    return Err(format!("unknown key '{k}' in '{combo}'"));
                }
                // like a real keyboard: shift produces the upper-case character
                Key::Char(if mods.shift { c.to_uppercase().next().unwrap_or(c) } else { c })
            }
        }
    };
    Ok((key, mods))
}

fn key_text(key: Key, mods: Mods) -> Option<String> {
    if mods.ctrl || mods.alt {
        return None;
    }
    match key {
        Key::Char(c) if !c.is_control() => Some(c.to_string()),
        _ => None,
    }
}

fn nums(arg: &str, n: usize) -> Result<Vec<f32>, String> {
    let (v, _) = nums_mods(arg, n)?;
    Ok(v)
}

/// `n` comma-separated numbers, optionally followed by '+'-joined modifiers.
fn nums_mods(arg: &str, n: usize) -> Result<(Vec<f32>, Mods), String> {
    let parts: Vec<&str> = arg.split(',').map(str::trim).collect();
    if parts.len() < n || parts.len() > n + 1 {
        return Err(format!("expected {n} numbers in '{arg}'"));
    }
    let mut v = Vec::with_capacity(n);
    for p in &parts[..n] {
        v.push(p.parse::<f32>().map_err(|_| format!("bad number '{p}' in '{arg}'"))?);
    }
    let mut mods = Mods::default();
    if let Some(m) = parts.get(n) {
        for m in m.split('+').filter(|m| !m.is_empty()) {
            if !parse_mods(m, &mut mods) {
                return Err(format!("unknown modifier '{m}' in '{arg}'"));
            }
        }
    }
    Ok((v, mods))
}

fn run_snapshot(args: Args) -> i32 {
    let out_path = args.snapshot.clone().unwrap();
    let gpu = match gfx::headless_gpu() {
        Ok(g) => g,
        Err(e) => {
            eprintln!("glint: {e}");
            return 1;
        }
    };
    let (w, h) = args.size;
    let renderer = Renderer::new(&gpu, wgpu::TextureFormat::Rgba8Unorm, w, h);
    // A snapshot run must never read or write the user's real session or
    // config: persistence is only on inside a GLINT_HOME sandbox.
    let sandboxed = std::env::var_os("GLINT_HOME").is_some();
    let mut app = App::new(AppOptions {
        system_clipboard: false,
        restore_session: sandboxed && !args.no_session,
        write_config: sandboxed,
        paths: args.paths,
    });
    app.resize(w as f32, h as f32, args.scale);
    app.input(Input::Focus(true));
    let mut snap =
        Snap { gpu, renderer, app, w, h, scale: args.scale, t: 0.0, bg: BgFrame::default(), answers: VecDeque::new(), quit: false };

    let mut code = 0;
    snap.step(3);
    for step in split_script(&args.script) {
        if snap.quit {
            break;
        }
        if let Err(e) = snap.run_step(&step) {
            eprintln!("glint: snapshot script: {e}");
        }
        snap.step(2);
    }
    snap.step(120);
    // a script that quit the app still gets its final frame
    if let Err(e) = snap.shot(&out_path) {
        eprintln!("glint: {e}");
        code = 1;
    } else {
        println!("wrote {}", out_path.display());
    }
    snap.app.shutdown();
    code
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn combos() {
        assert_eq!(parse_combo("ctrl+shift+p").unwrap(), (Key::Char('P'), Mods { ctrl: true, shift: true, alt: false }));
        assert_eq!(parse_combo("Enter").unwrap(), (Key::Enter, Mods::default()));
        assert_eq!(parse_combo("ctrl++").unwrap(), (Key::Char('+'), Mods { ctrl: true, shift: false, alt: false }));
        assert_eq!(parse_combo("+").unwrap(), (Key::Char('+'), Mods::default()));
        assert_eq!(parse_combo("alt+f4").unwrap(), (Key::F(4), Mods { ctrl: false, shift: false, alt: true }));
        assert_eq!(parse_combo("ctrl+\\").unwrap().0, Key::Char('\\'));
        assert_eq!(parse_combo("shift+f3").unwrap(), (Key::F(3), Mods { ctrl: false, shift: true, alt: false }));
        assert!(parse_combo("hyper+x").is_err());
        assert!(parse_combo("ctrl+nope").is_err());
    }

    #[test]
    fn key_text_rules() {
        assert_eq!(key_text(Key::Char('a'), Mods::default()), Some("a".into()));
        assert_eq!(key_text(Key::Char(' '), Mods::default()), Some(" ".into()));
        assert_eq!(key_text(Key::Char('A'), Mods { shift: true, ..Default::default() }), Some("A".into()));
        assert_eq!(key_text(Key::Char('s'), Mods { ctrl: true, ..Default::default() }), None);
        assert_eq!(key_text(Key::Enter, Mods::default()), None);
    }

    #[test]
    fn script_splitting() {
        assert_eq!(split_script("key:ctrl+n; type:a\\;b ;; wait:0.5"), vec!["key:ctrl+n", "type:a;b", "wait:0.5"]);
        assert_eq!(unescape("a\\nb\\tc\\\\d"), "a\nb\tc\\d");
    }

    /// End-to-end run of the snapshot harness; needs a GPU, so it is opt-in:
    /// `GLINT_SMOKE_OUT=/some/dir cargo test runner::tests::snapshot_smoke -- --ignored`
    #[test]
    #[ignore]
    fn snapshot_smoke() {
        let dir = PathBuf::from(std::env::var("GLINT_SMOKE_OUT").expect("set GLINT_SMOKE_OUT"));
        let args = Args {
            paths: vec![],
            snapshot: Some(dir.join("final.png")),
            size: (640, 400),
            scale: 1.0,
            script: format!(
                "key:ctrl+n; type:hello\\nworld; click:100,100; dclick:120,120,shift; drag:10,10,200,200; \
                 wheel:100,100,-3; wait:0.1; focus:0; focus:1; resize:800x500; shot:{}; bogus:1; key:ctrl+q",
                dir.join("mid.png").display()
            ),
            no_session: true,
        };
        assert_eq!(run_snapshot(args), 0);
        assert!(dir.join("final.png").exists() && dir.join("mid.png").exists());
    }

    #[test]
    fn numbers_and_mods() {
        assert_eq!(nums("10, 20.5", 2).unwrap(), vec![10.0, 20.5]);
        let (v, m) = nums_mods("1,2,shift+ctrl", 2).unwrap();
        assert_eq!(v, vec![1.0, 2.0]);
        assert!(m.shift && m.ctrl && !m.alt);
        assert!(nums("1", 2).is_err());
        assert_eq!(parse_size("800x600"), Some((800, 600)));
        assert_eq!(parse_size("0x600"), None);
    }
}
