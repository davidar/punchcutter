//! What a cold start waits for: the driver's compile of each shaping pipeline.
//!
//! The example compiles `slang/modules/shape_demo.slang`, creates one compute
//! pipeline per shaping entry point and times each creation, then dispatches
//! each entry once, in order, over a small batch, and times each first
//! dispatch. Some drivers compile at creation (RADV); lavapipe compiles at the
//! first dispatch, so both columns count.
//!
//! Run it with the driver's shader cache off, or the second run measures the
//! cache:
//!
//! ```text
//!   MESA_SHADER_CACHE_DISABLE=true cargo run --release -p punchcutter-shape --example coldstart
//!   MESA_SHADER_CACHE_DISABLE=true cargo run --release -p punchcutter-shape --example coldstart -- --parallel
//!   MESA_SHADER_CACHE_DISABLE=true RADV_DEBUG=shaderstats cargo run ... --example coldstart
//! ```
//!
//! `--parallel` creates every pipeline on its own thread at once and reports
//! the wall time, which is what a host that builds them in parallel waits for.
//! `--entries a,b` times other entry points of the same file, and `--dump DIR`
//! writes each entry's SPIR-V there. On a machine with two GPUs, point
//! `VK_DRIVER_FILES` at the ICD of the one under test.
//!
//! The numbers are worthless on a loaded machine; the example prints the load
//! average beside them.
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use punchcutter_core::plain::{bytes_of, bytes_of_slice};
use punchcutter_shape::{Arena, Batch, ShapeRun, ShapedGlyph};
use punchcutter_slang::{BindingKind, Options};

const FONT: &str = "/usr/share/fonts/adwaita-sans-fonts/AdwaitaSans-Regular.ttf";

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let parallel = args.iter().any(|arg| arg == "--parallel");
    let entries: Vec<String> = match args.iter().position(|arg| arg == "--entries") {
        Some(at) => args
            .get(at + 1)
            .context("--entries wants a comma-separated list")?
            .split(',')
            .map(str::to_owned)
            .collect(),
        None => punchcutter_shape::STAGE_ENTRIES
            .iter()
            .map(|entry| (*entry).to_owned())
            .collect(),
    };
    let cache = std::env::var("MESA_SHADER_CACHE_DISABLE").unwrap_or_default();
    if cache != "true" {
        println!("warning: MESA_SHADER_CACHE_DISABLE is not true; a warm cache hides the compile");
    }
    println!("load average: {}", load_average());

    let (device, queue) = open()?;
    // $PUNCHCUTTER_SLANG_DIR first, the source checkout second, as in the
    // other harnesses.
    let source = std::env::var_os("PUNCHCUTTER_SLANG_DIR")
        .map(PathBuf::from)
        .filter(|dir| dir.is_dir())
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../slang/modules"))
        .join("shape_demo.slang");
    println!("source: {}", source.display());
    let names: Vec<&str> = entries.iter().map(String::as_str).collect();
    let started = Instant::now();
    let program = punchcutter_slang::compile_program(&source, &names, &Options::default())
        .map_err(|err| anyhow::anyhow!("{err}"))?;
    println!(
        "slang: {} entries in {:.0} ms",
        program.entries.len(),
        started.elapsed().as_secs_f64() * 1e3
    );
    if let Some(at) = args.iter().position(|arg| arg == "--dump") {
        let dir = PathBuf::from(args.get(at + 1).context("--dump wants a directory")?);
        std::fs::create_dir_all(&dir)?;
        for entry in &program.entries {
            std::fs::write(dir.join(format!("{}.spv", entry.name)), &entry.spirv)?;
        }
        println!("wrote the modules to {}", dir.display());
    }

    let layout_entries: Vec<wgpu::BindGroupLayoutEntry> = program
        .reflection
        .bindings
        .iter()
        .map(|binding| {
            if binding.set != 0 {
                bail!(
                    "`{}` is in set {}; this example binds set 0",
                    binding.name,
                    binding.set
                );
            }
            let ty = match binding.kind {
                BindingKind::Uniform => wgpu::BufferBindingType::Uniform,
                BindingKind::Storage { read_only, .. } => {
                    wgpu::BufferBindingType::Storage { read_only }
                }
                other => bail!("`{}` is a {other:?}, not a buffer", binding.name),
            };
            Ok(wgpu::BindGroupLayoutEntry {
                binding: binding.index,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Buffer {
                    ty,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            })
        })
        .collect::<Result<_>>()?;
    let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("coldstart-bgl"),
        entries: &layout_entries,
    });
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("coldstart-pl"),
        bind_group_layouts: &[Some(&layout)],
        immediate_size: 0,
    });

    // Every entry name has to outlive its pipeline's creation call.
    let leaked: Vec<&'static str> = names
        .iter()
        .map(|name| &*Box::leak(name.to_string().into_boxed_str()))
        .collect();

    let mut created = Vec::new();
    let mut create_ms = Vec::new();
    let scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
    if parallel {
        let started = Instant::now();
        let built: Vec<(wgpu::ComputePipeline, f64)> = std::thread::scope(|threads| {
            let handles: Vec<_> = program
                .entries
                .iter()
                .zip(&leaked)
                .map(|(entry, name)| {
                    let (device, pipeline_layout) = (&device, &pipeline_layout);
                    threads.spawn(move || {
                        // Error scopes are per thread.
                        let scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
                        let started = Instant::now();
                        let pipeline = punchcutter_core::gpu::passthrough_pipeline(
                            device,
                            pipeline_layout,
                            name,
                            name,
                            &entry.spirv,
                            (punchcutter_shape::SHAPE_WORKGROUP, 1, 1),
                        );
                        let ms = started.elapsed().as_secs_f64() * 1e3;
                        if let Some(err) = pollster::block_on(scope.pop()) {
                            panic!("wgpu rejected `{name}`: {err}");
                        }
                        (pipeline, ms)
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().expect("a creation thread panicked"))
                .collect()
        });
        println!(
            "parallel creation: {:.0} ms wall for {} pipelines",
            started.elapsed().as_secs_f64() * 1e3,
            built.len()
        );
        for (pipeline, ms) in built {
            created.push(pipeline);
            create_ms.push(ms);
        }
    } else {
        for (entry, name) in program.entries.iter().zip(&leaked) {
            let started = Instant::now();
            created.push(punchcutter_core::gpu::passthrough_pipeline(
                &device,
                &pipeline_layout,
                name,
                name,
                &entry.spirv,
                (punchcutter_shape::SHAPE_WORKGROUP, 1, 1),
            ));
            create_ms.push(started.elapsed().as_secs_f64() * 1e3);
        }
    }
    if let Some(err) = pollster::block_on(scope.pop()) {
        bail!("wgpu rejected a shaping module: {err}");
    }

    // A small batch, so a first dispatch is the compile and not the work.
    let arena = Arena::load(FONT).with_context(|| format!("loading {FONT}"))?;
    let mut batch = Batch::new();
    for text in ["14:03", "Settings", "office affinity", "Text Editor"] {
        batch.push(text, 16.0);
    }
    let params = batch.params(&arena);
    let words = batch.utf8_words();
    let buffer = |label: &str, size: u64, usage: wgpu::BufferUsages| {
        device.create_buffer(&wgpu::BufferDescriptor {
            label: Some(label),
            size: size.max(16),
            usage: usage | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        })
    };
    let storage = wgpu::BufferUsages::STORAGE;
    let mut buffers = Vec::new();
    for binding in &program.reflection.bindings {
        let (size, bytes): (u64, Option<&[u8]>) = match binding.name.as_str() {
            "params" => (size_of_val(&params) as u64, Some(bytes_of(&params))),
            "requests" => (
                size_of_val(batch.requests()) as u64,
                Some(bytes_of_slice(batch.requests())),
            ),
            "ot" => (arena.bytes().len() as u64, Some(arena.bytes())),
            "utf8" => (words.len() as u64 * 4, Some(words_bytes(&words))),
            "out_glyphs" => (
                u64::from(batch.glyph_capacity()) * size_of::<ShapedGlyph>() as u64,
                None,
            ),
            "out_runs" => ((batch.len() * size_of::<ShapeRun>()) as u64, None),
            "scratch" => (punchcutter_shape::scratch_bytes(batch.len() as u32), None),
            other => bail!("no buffer for the binding `{other}`"),
        };
        let usage = if binding.kind == BindingKind::Uniform {
            wgpu::BufferUsages::UNIFORM
        } else {
            storage
        };
        let made = buffer(&binding.name, size, usage);
        if let Some(bytes) = bytes {
            queue.write_buffer(&made, 0, bytes);
        }
        buffers.push((binding.index, made));
    }
    let bind_entries: Vec<wgpu::BindGroupEntry> = buffers
        .iter()
        .map(|(index, made)| wgpu::BindGroupEntry {
            binding: *index,
            resource: made.as_entire_binding(),
        })
        .collect();
    let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("coldstart-bg"),
        layout: &layout,
        entries: &bind_entries,
    });
    queue.submit([]);
    device
        .poll(wgpu::PollType::wait_indefinitely())
        .map_err(|err| anyhow::anyhow!("the upload never completed: {err}"))?;

    let mut dispatch_ms = Vec::new();
    for pipeline in &created {
        let started = Instant::now();
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("coldstart"),
        });
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("coldstart-pass"),
                timestamp_writes: None,
            });
            pass.set_pipeline(pipeline);
            pass.set_bind_group(0, &group, &[]);
            pass.dispatch_workgroups(batch.workgroups(), 1, 1);
        }
        queue.submit([encoder.finish()]);
        device
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|err| anyhow::anyhow!("a dispatch never completed: {err}"))?;
        dispatch_ms.push(started.elapsed().as_secs_f64() * 1e3);
    }

    println!(
        "{:<22} {:>10} {:>12} {:>16} {:>10}",
        "entry", "SPIR-V B", "create ms", "first dispatch", "sum ms"
    );
    let mut total = 0.0;
    let mut longest: f64 = 0.0;
    for (i, entry) in program.entries.iter().enumerate() {
        let sum = create_ms[i] + dispatch_ms[i];
        total += sum;
        longest = longest.max(sum);
        println!(
            "{:<22} {:>10} {:>12.0} {:>16.0} {:>10.0}",
            entry.name,
            entry.spirv.len(),
            create_ms[i],
            dispatch_ms[i],
            sum
        );
    }
    println!("total {total:.0} ms, longest {longest:.0} ms");
    println!("load average: {}", load_average());
    Ok(())
}

fn load_average() -> String {
    std::fs::read_to_string("/proc/loadavg")
        .map(|text| {
            text.split_whitespace()
                .take(3)
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_else(|_| "unknown".to_owned())
}

/// The integrated or software adapter, never a discrete NVIDIA one.
fn open() -> Result<(wgpu::Device, wgpu::Queue)> {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::VULKAN,
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    });
    let adapters = pollster::block_on(instance.enumerate_adapters(wgpu::Backends::VULKAN));
    let adapter = adapters
        .iter()
        .find(|adapter| {
            let info = adapter.get_info();
            let usable = info.device_type == wgpu::DeviceType::IntegratedGpu
                || info.device_type == wgpu::DeviceType::Cpu;
            usable && !info.name.to_ascii_uppercase().contains("NVIDIA")
        })
        .context("no integrated or CPU Vulkan adapter; refusing a discrete GPU")?;
    let info = adapter.get_info();
    println!(
        "adapter: {} ({:?}, {})",
        info.name, info.device_type, info.driver_info
    );
    if !adapter
        .features()
        .contains(wgpu::Features::PASSTHROUGH_SHADERS)
    {
        bail!("no PASSTHROUGH_SHADERS");
    }
    let mut limits = wgpu::Limits::default();
    limits.max_storage_buffers_per_shader_stage = 8;
    let (device, queue, _clock) = punchcutter_core::gpu::open_device(
        adapter,
        &wgpu::DeviceDescriptor {
            label: Some("punchcutter-shape-coldstart"),
            required_features: wgpu::Features::PASSTHROUGH_SHADERS,
            required_limits: limits,
            ..Default::default()
        },
    )?;
    Ok((device, queue))
}

fn words_bytes(words: &[u32]) -> &[u8] {
    // SAFETY: `u32` has no padding and no invalid bit patterns, and `u8` has
    // alignment 1; the lifetime is the borrow's.
    unsafe { std::slice::from_raw_parts(words.as_ptr().cast::<u8>(), size_of_val(words)) }
}
