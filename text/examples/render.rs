//! The curve renderer's harness: Slug's coverage on the GPU, as PNGs and as
//! numbers.
//!
//! The gate is **a person's look** at the PNGs plus the per-pixel errors it
//! prints, and it exists to catch a port that is wrong about one glyph. So this
//! runs headless (no surface, no window), renders a line of text at three
//! sizes and four Adwaita icons at two (when the theme is installed), and for
//! every glyph compares the GPU's coverage against swash's
//! raster of the same glyph at the same size, pixel for pixel. The PNGs land in
//! `text/test-runs/`.
//!
//! Run it with:
//!
//! ```text
//! cargo run -p punchcutter-text --example render
//! ```
//!
//! If `VK_DRIVER_FILES` is absent on a machine with more than one GPU, it is
//! set below to the integrated GPU's driver ([`integrated_icd`]), because the
//! one thing this must never do on a hybrid laptop is wake the discrete GPU to
//! draw a letter. The adapter is then chosen by `device_type ==
//! IntegratedGpu` (or a software rasteriser such as lavapipe, which is what a
//! headless CI runner has), and anything NVIDIA is refused outright rather than
//! merely deprioritised.

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use punchcutter_core::plain::bytes_of_slice;
use punchcutter_text::bands::{Builder, CurveSet};
use punchcutter_text::demo::{Params, Placement};
use punchcutter_text::font::{self, Font};
use punchcutter_text::icon;

/// The integrated GPU's Vulkan driver manifest, on a machine with more than
/// one GPU: the driver of the first render node whose kernel driver is not
/// NVIDIA's, when it is one Mesa ships a known manifest for. `None` on a
/// single-GPU machine (nothing to avoid) or when the driver is not known, and
/// then the adapter filter below is the only guard.
fn integrated_icd() -> Option<String> {
    let mut nodes: Vec<String> = std::fs::read_dir("/sys/class/drm")
        .ok()?
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter(|name| name.starts_with("renderD"))
        .collect();
    if nodes.len() < 2 {
        return None;
    }
    nodes.sort();
    let driver = nodes.iter().find_map(|node| {
        let link = std::fs::read_link(format!("/sys/class/drm/{node}/device/driver")).ok()?;
        let name = link.file_name()?.to_str()?.to_owned();
        (name != "nvidia").then_some(name)
    })?;
    let manifest = match driver.as_str() {
        "amdgpu" | "radeon" => "radeon_icd",
        "i915" | "xe" => "intel_icd",
        _ => return None,
    };
    let path = format!(
        "/usr/share/vulkan/icd.d/{manifest}.{}.json",
        std::env::consts::ARCH
    );
    Path::new(&path).is_file().then_some(path)
}

const STATUS_ICONS: &str = "/usr/share/icons/Adwaita/symbolic/status";

/// Four Adwaita status icons: two volume levels, a Wi-Fi signal and Bluetooth.
const ICONS: [&str; 4] = [
    "audio-volume-high-symbolic",
    "audio-volume-muted-symbolic",
    "network-wireless-signal-good-symbolic",
    "bluetooth-active-symbolic",
];

/// Round letters, a descender, straight stems, digits, and three punctuation
/// marks with curves and dots in them.
const LINE: &str = "Hamburgefonstiv 0123456789 :%—…";

/// 16 is a UI text size, 28 a large size, 64 the size at which a bad curve is
/// unmissable.
const SIZES: [f32; 3] = [16.0, 28.0, 64.0];

const ICON_SIZES: [f32; 2] = [16.0, 32.0];

/// Padding around a glyph's cell, pixels. Slug's dilation is one pixel; this is
/// room to see that the coverage outside the ink really is zero.
const PAD: i32 = 6;

fn main() -> Result<()> {
    if std::env::var_os("VK_DRIVER_FILES").is_none() {
        if let Some(icd) = integrated_icd() {
            // SAFETY: called on the one thread that exists, before wgpu's
            // instance and therefore before the loader reads the variable.
            // Nothing else in this process touches the environment.
            unsafe { std::env::set_var("VK_DRIVER_FILES", &icd) };
            println!("VK_DRIVER_FILES was unset; pinned to {icd}");
        }
    }

    let out = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("test-runs");
    std::fs::create_dir_all(&out).with_context(|| format!("could not make {}", out.display()))?;

    let gpu = Gpu::new()?;

    // --- the font ----------------------------------------------------------

    let font_path = font::default_sans()
        .context("fontconfig named no sans-serif; pass one by editing the example")?;
    println!("\nfont: {}", font_path.display());
    let font_bytes = std::fs::read(&font_path)
        .with_context(|| format!("could not read {}", font_path.display()))?;

    let started = Instant::now();
    let font = Font::parse(&font_bytes, 0).context("the font would not become curves")?;
    let preprocess = started.elapsed();

    let [points_b, bands_b, curves_b, glyphs_b] = font.curves.buffer_bytes();
    println!(
        "  unitsPerEm {}, {} glyphs ({} blank, {} refused), axes {:?} (default instance)",
        font.units_per_em, font.glyph_count, font.blank_glyphs, font.refused_glyphs, font.axes
    );
    println!(
        "  preprocessed in {:.1} ms — {} points, {} bands, {} band curves, worst band {} curves",
        preprocess.as_secs_f64() * 1000.0,
        font.curves.points.len(),
        font.curves.bands.len(),
        font.curves.band_curves.len(),
        font.curves.worst_band(),
    );
    println!(
        "  buffers {} B total: points {} B, bands {} B, band curves {} B, glyphs {} B",
        font.curves.bytes(),
        points_b,
        bands_b,
        curves_b,
        glyphs_b
    );

    let font_set = gpu.upload(&font.curves, "font")?;

    // The glyph ids for the sample line, through `cmap` — the *only* font-table
    // lookup the CPU does here, and only because the shaper is a separate
    // crate. No kerning and no ligatures: glyphs are placed by `hmtx` advance.
    let face = ttf_parser::Face::parse(&font_bytes, 0).context("the face parsed once already")?;
    let line: Vec<(char, u32)> = LINE
        .chars()
        .map(|ch| {
            let gid = face.glyph_index(ch).map(|id| u32::from(id.0)).unwrap_or(0);
            (ch, gid)
        })
        .collect();
    let missing: Vec<char> = line
        .iter()
        .filter(|(_, gid)| *gid == 0)
        .map(|(ch, _)| *ch)
        .collect();
    if !missing.is_empty() {
        println!("  note: the font has no glyph for {missing:?}; drawn as .notdef");
    }

    for size in SIZES {
        let path = out.join(format!("glyphs-{}.png", size as u32));
        draw_line(&gpu, &font_set, &font.curves, &line, size, &path)?;
        println!("  wrote {}", path.display());
    }

    // --- the comparison ----------------------------------------------------

    println!("\nper-pixel against swash (same outlines, same size, unhinted):");
    let swash_font =
        swash::FontRef::from_index(&font_bytes, 0).context("swash would not read the font")?;
    let mut scale_context = swash::scale::ScaleContext::new();

    for size in SIZES {
        let path = out.join(format!("compare-{}.png", size as u32));
        let stats = compare(
            &gpu,
            &font_set,
            &font.curves,
            &line,
            size,
            swash_font,
            &mut scale_context,
            &path,
        )?;
        println!(
            "  {:>2} px: mean |error| {:.4}, worst pixel {:.4} on '{}' at ({}, {}) — {}",
            size as u32,
            stats.mean,
            stats.worst,
            stats.worst_char,
            stats.worst_at.0,
            stats.worst_at.1,
            path.display()
        );
        // The per-glyph line is the one that would catch a port that is wrong
        // about a single letter, which is what a whole-strip mean can hide.
        let mut by_glyph = stats.per_glyph.clone();
        by_glyph.sort_by(|a, b| b.1.total_cmp(&a.1));
        let worst_three: Vec<String> = by_glyph
            .iter()
            .take(3)
            .map(|(ch, mean, worst)| format!("'{ch}' {mean:.4}/{worst:.4}"))
            .collect();
        let g = stats
            .per_glyph
            .iter()
            .find(|(ch, _, _)| *ch == 'g')
            .map(|(_, mean, worst)| format!("'g' {mean:.4}/{worst:.4}"))
            .unwrap_or_else(|| "no 'g'".to_string());
        println!(
            "         worst glyphs (mean/worst): {}; {g}",
            worst_three.join(", ")
        );
    }

    // --- the icons ---------------------------------------------------------

    println!("\nicons, from {STATUS_ICONS}:");
    if !Path::new(STATUS_ICONS).is_dir() {
        println!("  skipped: the Adwaita icon theme is not installed");
    } else {
        let mut icon_builder = Builder::new();
        let mut loaded = Vec::new();
        for name in ICONS {
            let path = Path::new(STATUS_ICONS).join(format!("{name}.svg"));
            let one = icon::load_file(&mut icon_builder, &path)
                .with_context(|| format!("{} would not load", path.display()))?;
            println!(
                "  {name}: {} filled paths, {} contours, {} curves{}",
                one.report.filled_paths,
                one.report.contours,
                one.report.curves,
                if one.report.clean() {
                    String::new()
                } else {
                    format!(" — IGNORED SOMETHING: {:?}", one.report)
                }
            );
            loaded.push(one);
        }
        let icon_set = icon_builder.finish();
        println!(
            "  {} icons in {} B of buffers, worst band {} curves",
            loaded.len(),
            icon_set.bytes(),
            icon_set.worst_band()
        );

        // One icon on its own, to answer "what buffer size does one icon cost".
        {
            let mut one = Builder::new();
            let path = Path::new(STATUS_ICONS).join(format!("{}.svg", ICONS[0]));
            icon::load_file(&mut one, &path)?;
            let set = one.finish();
            let [p, b, c, g] = set.buffer_bytes();
            println!(
                "  {} alone: {} B (points {p}, bands {b}, band curves {c}, glyph {g})",
                ICONS[0],
                set.bytes()
            );
        }

        let icon_buffers = gpu.upload(&icon_set, "icons")?;
        for size in ICON_SIZES {
            let path = out.join(format!("icons-{}.png", size as u32));
            draw_icons(&gpu, &icon_buffers, &icon_set, &loaded, size, &path)?;
            println!("  wrote {}", path.display());
        }
    }

    // --- the out-of-range glyph -------------------------------------------
    // The one check this harness makes about a *wrong* index rather than a right
    // one. wgpu enables `robustBufferAccess` on every Vulkan device it makes,
    // so an out-of-range read cannot become a wild pointer there; but
    // `text.slang` bounds its four buffers itself, and the promise worth
    // pinning is the module's own: an index it has no record for draws nothing,
    // on any backend.
    println!("\nout-of-range glyph ids (the bounds `text.slang` keeps):");
    let records = font.curves.glyphs.len() as u32;
    for glyph in [records, records + 1, u32::MAX / 2, u32::MAX] {
        let placements = [Placement {
            glyph,
            px_per_em: 32.0,
            origin: [8.0, 32.0],
        }];
        let cover = gpu.coverage(&font_set, &placements, 40, 40)?;
        let worst = cover.iter().fold(0.0f32, |acc, c| acc.max(c.abs()));
        if worst == 0.0 {
            println!("  id {glyph} (of {records} records): no ink, no fault");
        } else {
            bail!("glyph id {glyph} is past the {records} records written, but drew {worst}");
        }
    }

    println!("\nadapter used: {}", gpu.name);
    println!(
        "the dGPU check: `nvidia-smi --query-compute-apps=pid,process_name --format=csv` must \
         list nothing of this run's"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// The device
// ---------------------------------------------------------------------------

struct Gpu {
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: wgpu::ComputePipeline,
    /// The demo's own set 0: output, placements, params.
    dispatch_layout: wgpu::BindGroupLayout,
    /// `text.slang`'s set 1: the four curve buffers.
    font_layout: wgpu::BindGroupLayout,
    name: String,
}

/// The curve buffers on the device, as one bind group for set 1.
struct FontBuffers {
    group: wgpu::BindGroup,
    // Kept alive for the group's sake; nothing reads them again.
    _points: wgpu::Buffer,
    _bands: wgpu::Buffer,
    _band_curves: wgpu::Buffer,
    _glyphs: wgpu::Buffer,
}

impl Gpu {
    fn new() -> Result<Gpu> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::VULKAN,
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });

        let adapters = pollster::block_on(instance.enumerate_adapters(wgpu::Backends::VULKAN));
        println!("Vulkan adapters:");
        let mut chosen: Option<wgpu::Adapter> = None;
        for adapter in adapters {
            let info = adapter.get_info();
            let upper = info.name.to_ascii_uppercase();
            // `Cpu` is taken on the same grounds the dGPU is refused — a
            // software rasteriser cannot be the NVIDIA card — which is what
            // lets this harness run on a machine whose only ICD is lavapipe.
            let integrated = info.device_type == wgpu::DeviceType::IntegratedGpu
                || info.device_type == wgpu::DeviceType::Cpu;
            println!(
                "  {} ({:?}, {}) — {}",
                info.name,
                info.device_type,
                info.driver_info,
                if upper.contains("NVIDIA") {
                    "refused: discrete NVIDIA, and a letter is not worth waking it"
                } else if integrated {
                    "taken"
                } else {
                    "skipped: not an integrated or software adapter"
                }
            );
            if chosen.is_none() && integrated && !upper.contains("NVIDIA") {
                chosen = Some(adapter);
            }
        }
        let adapter = chosen.context(
            "no integrated or software adapter; this harness refuses to run anywhere else, because \
             the only GPU left on this machine is the one that must stay asleep",
        )?;

        let info = adapter.get_info();
        if !adapter
            .features()
            .contains(wgpu::Features::PASSTHROUGH_SHADERS)
        {
            bail!(
                "adapter '{}' lacks PASSTHROUGH_SHADERS; a Slang->SPIR-V shader cannot be \
                 loaded here",
                info.name
            );
        }

        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("punchcutter-text-device"),
            required_features: wgpu::Features::PASSTHROUGH_SHADERS,
            required_limits: wgpu::Limits::default(),
            ..Default::default()
        }))
        .context("Vulkan device creation failed")?;

        let storage = |binding: u32, read_only: bool| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::COMPUTE,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Storage { read_only },
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        };

        let dispatch_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("punchcutter-text-dispatch-bgl"),
            entries: &[
                storage(0, false),
                storage(1, true),
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: std::num::NonZeroU64::new(size_of::<Params>() as u64),
                    },
                    count: None,
                },
            ],
        });
        let font_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("punchcutter-text-font-bgl"),
            entries: &[
                storage(0, true),
                storage(1, true),
                storage(2, true),
                storage(3, true),
            ],
        });

        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("punchcutter-text-pl"),
            // Order is the descriptor-set number: set 0 is the demo's, set 1 is
            // `text.slang`'s own, which is what lets a consumer add the font to
            // its own shader without renumbering any of its bindings.
            bind_group_layouts: &[Some(&dispatch_layout), Some(&font_layout)],
            ..Default::default()
        });

        let scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
        let pipeline = punchcutter_core::gpu::passthrough_pipeline(
            &device,
            &layout,
            "punchcutter-text-demo",
            punchcutter_text::DEMO_ENTRY,
            punchcutter_text::DEMO_SPV,
            (8, 8, 1),
        );
        if let Some(err) = pollster::block_on(scope.pop()) {
            bail!("the demo pipeline was rejected: {err}");
        }

        Ok(Gpu {
            device,
            queue,
            pipeline,
            dispatch_layout,
            font_layout,
            name: info.name,
        })
    }

    fn read_only<T: punchcutter_core::plain::Plain>(
        &self,
        label: &str,
        values: &[T],
    ) -> wgpu::Buffer {
        // An empty buffer is not bindable, so a set with no icons still gets one
        // element of zeroes.
        let bytes = bytes_of_slice(values);
        let size = (bytes.len().max(size_of::<T>()) as u64).next_multiple_of(4);
        let buffer = punchcutter_core::gpu::storage_buffer(&self.device, label, size);
        if !bytes.is_empty() {
            self.queue.write_buffer(&buffer, 0, bytes);
        }
        buffer
    }

    fn upload(&self, set: &CurveSet, label: &str) -> Result<FontBuffers> {
        let points = self.read_only(&format!("{label}-points"), &set.points);
        let bands = self.read_only(&format!("{label}-bands"), &set.bands);
        let band_curves = self.read_only(&format!("{label}-band-curves"), &set.band_curves);
        let glyphs = self.read_only(&format!("{label}-glyphs"), &set.glyphs);
        let group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some(label),
            layout: &self.font_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: points.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: bands.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: band_curves.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: glyphs.as_entire_binding(),
                },
            ],
        });
        Ok(FontBuffers {
            group,
            _points: points,
            _bands: bands,
            _band_curves: band_curves,
            _glyphs: glyphs,
        })
    }

    /// One dispatch: coverage for every pixel of a `width × height` image, from
    /// the placements given.
    fn coverage(
        &self,
        font: &FontBuffers,
        placements: &[Placement],
        width: u32,
        height: u32,
    ) -> Result<Vec<f32>> {
        let pixels = (width as usize) * (height as usize);
        anyhow::ensure!(pixels > 0, "a zero-sized image");
        let bytes = (pixels * size_of::<f32>()) as u64;

        let out = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("punchcutter-text-coverage"),
            size: bytes,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let readback = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("punchcutter-text-readback"),
            size: bytes,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let placed = self.read_only("punchcutter-text-placements", placements);
        let params = Params {
            width,
            height,
            count: placements.len() as u32,
            _rsvd0: 0,
        };
        let params_buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("punchcutter-text-params"),
            size: size_of::<Params>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        self.queue.write_buffer(
            &params_buffer,
            0,
            punchcutter_core::plain::bytes_of(&params),
        );

        let group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("punchcutter-text-dispatch"),
            layout: &self.dispatch_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: out.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: placed.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: params_buffer.as_entire_binding(),
                },
            ],
        });

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("punchcutter-text-encoder"),
            });
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("punchcutter-text-pass"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &group, &[]);
            pass.set_bind_group(1, &font.group, &[]);
            pass.dispatch_workgroups(width.div_ceil(8), height.div_ceil(8), 1);
        }
        encoder.copy_buffer_to_buffer(&out, 0, &readback, 0, bytes);
        self.queue.submit([encoder.finish()]);

        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        readback.slice(..).map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        self.device
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|err| anyhow::anyhow!("the coverage poll failed: {err}"))?;
        rx.try_recv()
            .context("the map callback did not fire")?
            .context("the coverage buffer would not map")?;

        let values = {
            let view = readback
                .slice(..)
                .get_mapped_range()
                .map_err(|err| anyhow::anyhow!("the coverage range is unavailable: {err}"))?;
            view.chunks_exact(4)
                .map(|word| f32::from_le_bytes([word[0], word[1], word[2], word[3]]))
                .collect::<Vec<f32>>()
        };
        readback.unmap();
        Ok(values)
    }
}

// ---------------------------------------------------------------------------
// Cells: one glyph's box, the same for every glyph at a size, so a strip of
// them is one dispatch and one comparison window per cell.
// ---------------------------------------------------------------------------

struct Cell {
    width: u32,
    height: u32,
    /// The pen, within a cell.
    origin: (i32, i32),
}

fn cell_for(set: &CurveSet, glyphs: impl Iterator<Item = u32>, size: f32) -> Cell {
    let mut max_adv = 0.0f32;
    let mut min_x = 0.0f32;
    let mut max_x = 0.0f32;
    let mut min_y = 0.0f32;
    let mut max_y = 0.0f32;
    for gid in glyphs {
        let glyph = set.glyphs[gid as usize];
        max_adv = max_adv.max(glyph.advance);
        min_x = min_x.min(glyph.bounds[0]);
        max_x = max_x.max(glyph.bounds[2]);
        min_y = min_y.min(glyph.bounds[1]);
        max_y = max_y.max(glyph.bounds[3]);
    }
    let ox = PAD + (-min_x * size).ceil().max(0.0) as i32;
    let oy = PAD + (max_y * size).ceil().max(0.0) as i32;
    let right = (max_adv.max(max_x) * size).ceil() as i32;
    Cell {
        width: (ox + right + PAD) as u32,
        height: (oy + (-min_y * size).ceil().max(0.0) as i32 + PAD) as u32,
        origin: (ox, oy),
    }
}

// ---------------------------------------------------------------------------
// The three pictures
// ---------------------------------------------------------------------------

/// A line of glyphs, placed by advance — no kerning, no ligatures, since shaping
/// is `punchcutter-shape`'s and this crate must not depend on it.
fn draw_line(
    gpu: &Gpu,
    font: &FontBuffers,
    set: &CurveSet,
    line: &[(char, u32)],
    size: f32,
    path: &Path,
) -> Result<()> {
    let cell = cell_for(set, line.iter().map(|(_, gid)| *gid), size);
    let mut placements = Vec::new();
    let mut pen = PAD as f32;
    for (_, gid) in line {
        placements.push(Placement {
            glyph: *gid,
            px_per_em: size,
            origin: [pen, cell.origin.1 as f32],
        });
        pen += set.glyphs[*gid as usize].advance * size;
    }
    let width = (pen.ceil() as u32) + PAD as u32 * 2;
    let coverage = gpu.coverage(font, &placements, width, cell.height)?;
    write_png(path, width, cell.height, &ink(&coverage))
}

/// The four icons, at one size, in a row.
fn draw_icons(
    gpu: &Gpu,
    buffers: &FontBuffers,
    set: &CurveSet,
    icons: &[icon::Icon],
    size: f32,
    path: &Path,
) -> Result<()> {
    let gutter = 8.0f32;
    let height = (size + gutter * 2.0).ceil() as u32;
    let mut placements = Vec::new();
    let mut pen = gutter;
    for one in icons {
        placements.push(Placement {
            glyph: one.glyph,
            px_per_em: size,
            // An icon's em square sits on its bottom edge, so the "baseline" is
            // the bottom of the box.
            origin: [pen, height as f32 - gutter],
        });
        pen += set.glyphs[one.glyph as usize].advance * size + gutter;
    }
    let width = pen.ceil() as u32;
    let coverage = gpu.coverage(buffers, &placements, width, height)?;
    write_png(path, width, height, &ink(&coverage))
}

struct Stats {
    mean: f32,
    worst: f32,
    worst_char: char,
    worst_at: (u32, u32),
    /// Per glyph: the character, its mean absolute error over its own cell, and
    /// its worst pixel. A port that is wrong about one letter shows up here by
    /// name, which a mean over the whole strip would bury.
    per_glyph: Vec<(char, f32, f32)>,
}

/// Ours, swash's, and the difference, as three strips of one PNG, plus the
/// numbers.
///
/// The two rasterisers are given the same outlines at the same size with the
/// glyph origin on a whole pixel, so they are directly comparable. They are not
/// the same algorithm: swash (through zeno) integrates the exact area of each
/// pixel the outline covers, while Slug point-samples the pixel centre and
/// filters analytically from the two rays' crossing distances. A small
/// systematic difference on diagonal edges is therefore expected; a *large* one,
/// or one concentrated on a single glyph, is a bug in the port.
#[allow(clippy::too_many_arguments)]
fn compare(
    gpu: &Gpu,
    font: &FontBuffers,
    set: &CurveSet,
    line: &[(char, u32)],
    size: f32,
    swash_font: swash::FontRef<'_>,
    context: &mut swash::scale::ScaleContext,
    path: &Path,
) -> Result<Stats> {
    use swash::scale::{Render, Source};
    use swash::zeno::Format;

    let cell = cell_for(set, line.iter().map(|(_, gid)| *gid), size);
    let (cw, ch) = (cell.width, cell.height);
    let count = line.len() as u32;
    let strip_w = cw * count;

    // Ours: one dispatch for the whole strip, a cell per glyph.
    let placements: Vec<Placement> = line
        .iter()
        .enumerate()
        .map(|(i, (_, gid))| Placement {
            glyph: *gid,
            px_per_em: size,
            origin: [
                (i as u32 * cw) as f32 + cell.origin.0 as f32,
                cell.origin.1 as f32,
            ],
        })
        .collect();
    let ours = gpu.coverage(font, &placements, strip_w, ch)?;

    // swash's, drawn into the same cells.
    let mut theirs = vec![0.0f32; ours.len()];
    let mut scaler = context.builder(swash_font).size(size).hint(false).build();
    for (i, (ch_char, gid)) in line.iter().enumerate() {
        let Some(image) = Render::new(&[Source::Outline])
            .format(Format::Alpha)
            .render(&mut scaler, *gid as u16)
        else {
            println!("    note: swash rendered nothing for '{ch_char}'");
            continue;
        };
        let place = image.placement;
        for row in 0..place.height {
            for col in 0..place.width {
                let x = i as i32 * cw as i32 + cell.origin.0 + place.left + col as i32;
                let y = cell.origin.1 - place.top + row as i32;
                if x < 0 || y < 0 || x >= strip_w as i32 || y >= ch as i32 {
                    continue;
                }
                let alpha = image.data[(row * place.width + col) as usize];
                theirs[y as usize * strip_w as usize + x as usize] = alpha as f32 / 255.0;
            }
        }
    }

    // The numbers, over the whole strip and per cell.
    let mut sum = 0.0f64;
    let mut worst = 0.0f32;
    let mut worst_at = (0u32, 0u32);
    let mut worst_char = ' ';
    let mut cell_sum = vec![0.0f64; line.len()];
    let mut cell_worst = vec![0.0f32; line.len()];
    for y in 0..ch {
        for x in 0..strip_w {
            let index = (y * strip_w + x) as usize;
            let diff = (ours[index] - theirs[index]).abs();
            sum += f64::from(diff);
            let cell = (x / cw) as usize;
            cell_sum[cell] += f64::from(diff);
            cell_worst[cell] = cell_worst[cell].max(diff);
            if diff > worst {
                worst = diff;
                // Reported in the cell's own coordinates, which is what a
                // screenshot of the sheet shows.
                worst_at = (x % cw, y);
                worst_char = line[cell].0;
            }
        }
    }
    let mean = (sum / ours.len() as f64) as f32;
    let per_cell = f64::from(cw * ch);
    let per_glyph: Vec<(char, f32, f32)> = line
        .iter()
        .enumerate()
        .map(|(i, (c, _))| (*c, (cell_sum[i] / per_cell) as f32, cell_worst[i]))
        .collect();

    // The sheet: ours, swash's, and four times the difference so a faint
    // disagreement is visible at all.
    let sep = 1u32;
    let sheet_h = ch * 3 + sep * 2;
    let mut sheet = vec![255u8; (strip_w * sheet_h) as usize];
    let ours_ink = ink(&ours);
    let theirs_ink = ink(&theirs);
    let diff_ink: Vec<u8> = ours
        .iter()
        .zip(&theirs)
        .map(|(a, b)| 255 - (((a - b).abs() * 4.0).clamp(0.0, 1.0) * 255.0).round() as u8)
        .collect();
    for (strip, pixels) in [&ours_ink, &theirs_ink, &diff_ink].into_iter().enumerate() {
        let top = strip as u32 * (ch + sep);
        for y in 0..ch {
            let from = (y * strip_w) as usize;
            let to = ((top + y) * strip_w) as usize;
            sheet[to..to + strip_w as usize]
                .copy_from_slice(&pixels[from..from + strip_w as usize]);
        }
        if strip < 2 {
            let line_at = ((top + ch) * strip_w) as usize;
            sheet[line_at..line_at + strip_w as usize].fill(128);
        }
    }
    write_png(path, strip_w, sheet_h, &sheet)?;

    Ok(Stats {
        mean,
        worst,
        worst_char,
        worst_at,
        per_glyph,
    })
}

// ---------------------------------------------------------------------------
// PNG
// ---------------------------------------------------------------------------

/// Coverage to eight-bit luminance, dark ink on white — text is far easier to
/// judge that way round than as a white-on-black mask.
fn ink(coverage: &[f32]) -> Vec<u8> {
    coverage
        .iter()
        .map(|c| 255 - (c.clamp(0.0, 1.0) * 255.0).round() as u8)
        .collect()
}

fn write_png(path: &Path, width: u32, height: u32, grey: &[u8]) -> Result<()> {
    anyhow::ensure!(
        grey.len() == (width as usize) * (height as usize),
        "{} is {}x{} but {} bytes were given",
        path.display(),
        width,
        height,
        grey.len()
    );
    let file = std::fs::File::create(path)
        .with_context(|| format!("could not create {}", path.display()))?;
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), width, height);
    encoder.set_color(png::ColorType::Grayscale);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder
        .write_header()
        .with_context(|| format!("could not write the header of {}", path.display()))?;
    writer
        .write_image_data(grey)
        .with_context(|| format!("could not write {}", path.display()))?;
    Ok(())
}
