//! `--live`: the reel in a window, in real time, on a loop.
//!
//! The frames are the offline reel's, drawn by the same three passes into the
//! same image buffer; what changes is where they go. Each one is copied from
//! the image buffer into the swapchain's image on the GPU and presented, so
//! nothing comes back to the CPU at all, and the clock is the wall clock: a
//! frame that takes longer than the display's interval is a frame skipped,
//! never a reel played slower.

use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use punchcutter_fonts::Merged;
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Window, WindowId};

use crate::{Args, Gpu, Renderer, instance};

pub fn run(args: Args, fonts: Merged, readme: String) -> Result<()> {
    let event_loop = EventLoop::new().context("no window system to open a window on")?;
    let mut app = Live {
        args,
        assets: Some((fonts, readme)),
        showing: None,
        error: None,
    };
    event_loop.run_app(&mut app)?;
    match app.error {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

struct Live {
    args: Args,
    /// The font and the README, until the window opens and takes them.
    assets: Option<(Merged, String)>,
    showing: Option<Showing>,
    /// What stopped the loop, reported once it has.
    error: Option<anyhow::Error>,
}

/// The window and everything drawing into it.
struct Showing {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    renderer: Renderer,
    /// `--samples`, when given, which a resize then leaves alone.
    samples: Option<u32>,
    /// When the first frame reached the screen: the reel's clock, and the
    /// frame rate's, start there rather than when the window was made.
    started: Option<Instant>,
    /// Frames presented since `since`, for the rate printed every few seconds.
    frames: u32,
    since: Instant,
}

impl Live {
    fn open(&mut self, event_loop: &ActiveEventLoop) -> Result<Showing> {
        let attributes = Window::default_attributes()
            .with_title("punchcutter")
            .with_inner_size(PhysicalSize::new(self.args.width, self.args.height));
        let window = Arc::new(event_loop.create_window(attributes)?);
        let instance = instance();
        let surface = instance
            .create_surface(window.clone())
            .context("the window would not give a Vulkan surface")?;
        let gpu = Gpu::new(
            &instance,
            self.args.discrete,
            self.args.profile,
            Some(&surface),
        )?;

        // The frame reaches the swapchain by a copy, so the swapchain has to
        // take one, in a format whose bytes are the image buffer's.
        let caps = surface.get_capabilities(&gpu.adapter);
        if !caps.usages.contains(wgpu::TextureUsages::COPY_DST) {
            bail!("this swapchain cannot be copied into");
        }
        use wgpu::TextureFormat as F;
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|format| {
                matches!(
                    format,
                    F::Rgba8Unorm | F::Rgba8UnormSrgb | F::Bgra8Unorm | F::Bgra8UnormSrgb
                )
            })
            .context("the swapchain offers no eight-bit RGBA or BGRA format")?;
        let size = window.inner_size();
        let (width, height) = (size.width.max(1), size.height.max(1));
        let mut config = surface
            .get_default_config(&gpu.adapter, width, height)
            .context("the surface and the adapter do not agree")?;
        config.format = format;
        config.usage = wgpu::TextureUsages::COPY_DST;
        config.present_mode = wgpu::PresentMode::Fifo;
        config.view_formats = Vec::new();
        surface.configure(&gpu.device, &config);

        let (fonts, readme) = self.assets.take().context("the window opened twice")?;
        let bgra = matches!(format, F::Bgra8Unorm | F::Bgra8UnormSrgb);
        let mut renderer = Renderer::new(gpu, fonts, readme, width, height, bgra)?;
        renderer.samples = self.args.samples.unwrap_or(samples_for(width, height));
        if self.args.profile {
            renderer.profiler = Some(crate::Profiler::new(&renderer.gpu));
        }
        // The first frame shapes every string and sets every block, a fifth
        // of a second; it is done here, before the clock starts, so the reel
        // opens on its first frame rather than a fifth of a second in.
        renderer.draw(self.args.from)?;
        renderer
            .gpu
            .device
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|err| anyhow::anyhow!("the first frame never finished: {err}"))?;
        eprintln!(
            "live on {}: {width}x{height}, {format:?}, {} sample(s) a pixel; Escape quits",
            renderer.gpu.name, renderer.samples
        );
        Ok(Showing {
            window,
            surface,
            config,
            renderer,
            samples: self.args.samples,
            started: None,
            frames: 0,
            since: Instant::now(),
        })
    }
}

/// One coverage sample a pixel for a window with more pixels than a
/// 1920x1200 screen: at that density, or on a scaled display, four samples
/// are four times the work for no difference anyone can see.
fn samples_for(width: u32, height: u32) -> u32 {
    if u64::from(width) * u64::from(height) > 1920 * 1200 {
        1
    } else {
        4
    }
}

impl Showing {
    fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.config.width = width;
        self.config.height = height;
        self.surface
            .configure(&self.renderer.gpu.device, &self.config);
        self.renderer.resize(width, height);
        if self.samples.is_none() {
            self.renderer.samples = samples_for(width, height);
        }
    }

    fn frame(&mut self, from: f32, to: f32) -> Result<()> {
        let elapsed = self.started.map_or(0.0, |at| at.elapsed().as_secs_f32());
        let t = from + elapsed % (to - from);
        self.renderer.draw(t)?;

        let texture = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(texture) => texture,
            wgpu::CurrentSurfaceTexture::Suboptimal(texture) => {
                // Shown this once, then the swapchain is made again.
                self.renderer.present(&texture.texture);
                self.renderer.gpu.queue.present(texture);
                self.surface
                    .configure(&self.renderer.gpu.device, &self.config);
                return Ok(());
            }
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface
                    .configure(&self.renderer.gpu.device, &self.config);
                return Ok(());
            }
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                return Ok(());
            }
            wgpu::CurrentSurfaceTexture::Validation => {
                bail!("the swapchain image failed validation")
            }
        };
        self.renderer.present(&texture.texture);
        self.window.pre_present_notify();
        self.renderer.gpu.queue.present(texture);

        if self.started.is_none() {
            self.started = Some(Instant::now());
            self.since = Instant::now();
            return Ok(());
        }
        self.frames += 1;
        let elapsed = self.since.elapsed().as_secs_f32();
        if elapsed >= 3.0 {
            eprintln!("{:.1} frames a second", self.frames as f32 / elapsed);
            self.frames = 0;
            self.since = Instant::now();
        }
        Ok(())
    }
}

impl ApplicationHandler for Live {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.showing.is_some() {
            return;
        }
        match self.open(event_loop) {
            Ok(showing) => self.showing = Some(showing),
            Err(err) => {
                self.error = Some(err);
                event_loop.exit();
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        let Some(showing) = self.showing.as_mut() else {
            return;
        };
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::KeyboardInput { event, .. }
                if event.state == ElementState::Pressed
                    && event.logical_key == Key::Named(NamedKey::Escape) =>
            {
                event_loop.exit()
            }
            WindowEvent::Resized(size) => showing.resize(size.width, size.height),
            WindowEvent::RedrawRequested => {
                if let Err(err) = showing.frame(self.args.from, self.args.to) {
                    self.error = Some(err);
                    event_loop.exit();
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, _: &ActiveEventLoop) {
        if let Some(showing) = &self.showing {
            showing.window.request_redraw();
        }
    }
}
