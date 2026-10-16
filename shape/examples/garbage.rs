//! What the shaper does with an arena that is not an arena.
//!
//! One case is a plausible buffer of the wrong layout where the arena belongs:
//! a flattened `cmap`, a sorted list of `(codepoint, gid)` pairs, which a
//! consumer that bound the wrong buffer would hand the shader. The question
//! this answers is how long one dispatch over a user interface's strings takes
//! against that, against random bytes, and against all ones. `ot.slang`
//! promises a malformed arena *never stalls*; this measures what "never
//! stalls" costs in milliseconds, which is what decides whether a consumer
//! with a bad font can miss its frame budget.
//!
//! On a machine with two GPUs, point `VK_DRIVER_FILES` at the ICD of the one
//! under test (for example `/usr/share/vulkan/icd.d/radeon_icd.x86_64.json`).
//!
//! ```text
//!   cargo run -p punchcutter-shape --example garbage            # the mismatch and random bytes
//!   cargo run -p punchcutter-shape --example garbage -- --hostile   # all 0xFF and a hostile directory
//!   cargo run -p punchcutter-shape --example garbage -- --dump-mismatch a.bin  # the mismatch's array, on disk
//! ```
//!
//! **`--hostile` is why the shaper is bounded.** Without the caps and the
//! operation budget, an all-0xFF arena can hang the graphics ring (RADV reports
//! `ring gfx timeout` and resets it), and a mismatched directory costs a
//! dispatch several times what the real arena does. With the caps, the budget
//! and the in-shader deadline every garbage case is a few milliseconds. If you change a bound in `ot.slang` or `shape.slang`,
//! run the hostile cases under lavapipe first
//! (`VK_DRIVER_FILES=/usr/share/vulkan/icd.d/lvp_icd.x86_64.json`), where a
//! stall is a slow process rather than a ring reset.
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use punchcutter_core::plain::{bytes_of, bytes_of_slice};
use punchcutter_core::slang;
use punchcutter_shape::{
    Arena, Batch, SHAPE_ABI_VERSION, SHAPE_RUN_DEADLINE, SHAPE_RUN_NOTDEF, SHAPE_RUN_OUT_OF_OPS,
    SHAPE_RUN_UNSUPPORTED, ShapeFont, ShapeParams, ShapeRun, ShapedGlyph,
};

const FONT: &str = "/usr/share/fonts/adwaita-sans-fonts/AdwaitaSans-Regular.ttf";

fn main() -> Result<()> {
    // **Before the device**, and before slangc: writing the mismatch arena is
    // reading a font and sorting pairs, and a test that asks for it (a
    // consumer's own malformed-arena check) wants the bytes and not a
    // dispatch. Asked after `Gpu::new`, it would need slangc and the Slang
    // modules on a machine with no checkout, and fail with a message that says
    // nothing about fonts.
    if let Some(path) = dump_path() {
        let data = std::fs::read(FONT).with_context(|| format!("reading {FONT}"))?;
        let bytes = old_cmap_array(&data)?;
        std::fs::write(&path, &bytes).with_context(|| format!("writing {path}"))?;
        println!("wrote the mismatch arena to {path}: {} bytes", bytes.len());
        return Ok(());
    }

    let mut gpu = Gpu::new()?;
    let data = std::fs::read(FONT).with_context(|| format!("reading {FONT}"))?;

    // A representative set of a user interface's strings: a clock, launcher
    // labels, menu rows and window titles.
    let strings: Vec<&str> = vec![
        "14:03",
        "Fri 5 Sep",
        "Firefox",
        "Files",
        "Text Editor",
        "Terminal",
        "Software",
        "Settings",
        "Music",
        "Calendar",
        "Mail",
        "Document Viewer",
        "Image Viewer",
        "Lock",
        "Suspend",
        "Log out",
        "Restart",
        "Shut down",
        "Caffeine",
        "Bluetooth",
        "Wi-Fi",
        "Volume",
        "Microphone",
        "Battery 78%",
        "~/notes — Terminal",
        "src/lib.rs — Text Editor",
        "Inbox (3) — Mail",
        "Example Domain — Firefox",
        "日本語のウィンドウ — Terminal",
        "☷",
        "◑ Inbox (3)",
        "Unpin",
        "Pin",
        "Close",
    ];
    let mut batch = Batch::new();
    for s in &strings {
        batch.push(s, 14.0);
    }

    let real = Arena::load(FONT)?;
    let font = real.font();
    println!(
        "real arena: {} bytes, {} runs",
        real.bytes().len(),
        batch.len()
    );

    // The in-kernel deadline (`SHAPE_RUN_DEADLINE`). The rate is measured rather
    // than assumed: `getRealtimeClock` is a wall clock at a vendor-fixed rate
    // and no Vulkan limit reports it.
    if gpu.clock == punchcutter_core::gpu::ShaderClock::Device {
        let per_ms = gpu.measure_clock()?;
        gpu.deadline_ticks = (per_ms * f64::from(DEADLINE_MS)) as u32;
        println!(
            "device clock: {:.0} ticks/ms measured; deadline {} ticks ({DEADLINE_MS} ms a run)",
            per_ms, gpu.deadline_ticks
        );
    } else {
        println!(
            "device clock: VK_KHR_shader_clock is NOT enabled on this adapter — the run \
             deadline is off and the operation budget is the only bound"
        );
    }

    // A flattened `cmap`: `(codepoint, gid)` sorted by codepoint, one `u32` each,
    // sent where the arena belongs.
    let old_cmap = old_cmap_array(&data)?;
    println!(
        "old cmap array: {} pairs, {} bytes; the header a shaper would read out of it is \
         version={} face_count={} arena_bytes={}",
        old_cmap.len() / 8,
        old_cmap.len(),
        u32::from_le_bytes(old_cmap[0..4].try_into()?),
        u32::from_le_bytes(old_cmap[4..8].try_into()?),
        u32::from_le_bytes(old_cmap[8..12].try_into()?),
    );

    let mut rng = 0x9E37_79B9_7F4A_7C15u64;
    let mut random = vec![0u8; old_cmap.len()];
    for b in &mut random {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        *b = rng as u8;
    }
    let ones = vec![0xFFu8; old_cmap.len()];

    // A directory that says "sixteen faces, every table huge, every offset
    // small": the adversarial shape, where every count the walker reads is
    // as large as a u16 allows.
    let mut hostile = vec![0xFFu8; 64 * 1024];
    hostile[0..4].copy_from_slice(&1u32.to_le_bytes());
    hostile[4..8].copy_from_slice(&16u32.to_le_bytes());
    let hostile_len = hostile.len() as u32;
    hostile[8..12].copy_from_slice(&hostile_len.to_le_bytes());
    for f in 0..16usize {
        let rec = 16 + f * 64;
        let w = |hostile: &mut Vec<u8>, i: usize, v: u32| {
            hostile[rec + i * 4..rec + i * 4 + 4].copy_from_slice(&v.to_le_bytes());
        };
        w(&mut hostile, 0, 0); // glyph_base
        w(&mut hostile, 1, 2937); // glyph_count
        w(&mut hostile, 2, 2048); // upem
        w(&mut hostile, 3, 2937); // num_h_metrics
        for t in 0..5 {
            w(&mut hostile, 4 + t * 2, 1040); // every table at the tail
            w(&mut hostile, 5 + t * 2, 60_000);
        }
    }

    // The fuzzer: the real arena with seeded damage, a thousand times over,
    // every dispatch under a wall-clock cap. Run it under lavapipe, where a
    // stall is a slow process rather than a ring reset.
    if let Some(mutations) = mutation_count() {
        return mutate(&gpu, real.bytes(), font, &batch, mutations);
    }

    let hostile_too = std::env::args().any(|a| a == "--hostile");
    let mut cases: Vec<(&str, &[u8])> = vec![
        ("real arena", real.bytes()),
        (
            "old cmap array (a layout the shader misreads)",
            &old_cmap[..],
        ),
        ("random bytes", &random[..]),
    ];
    if hostile_too {
        cases.push(("all 0xFF", &ones[..]));
        cases.push(("hostile directory", &hostile[..]));
    } else {
        println!(
            "(skipping the all-0xFF and hostile-directory cases: pass --hostile, and not on the host's iGPU)"
        );
    }
    for (name, bytes) in cases {
        let t = gpu.run(bytes, font, &batch)?;
        let mut notdef = 0;
        let mut out_of_ops = 0;
        let mut unsupported = 0;
        let mut deadline = 0;
        for r in &t.runs {
            deadline += usize::from(r.flags & SHAPE_RUN_DEADLINE != 0);
            notdef += usize::from(r.flags & SHAPE_RUN_NOTDEF != 0);
            out_of_ops += usize::from(r.flags & SHAPE_RUN_OUT_OF_OPS != 0);
            unsupported += usize::from(r.flags & SHAPE_RUN_UNSUPPORTED != 0);
        }
        println!(
            "{name}: one dispatch {:.2} ms (submit to fence), kernel {:.2} ms/pass over {} \
             repeats; runs: {} notdef, {} out of ops, {} unsupported, {deadline} past the deadline",
            t.dispatch_ms, t.pass_ms, t.repeats, notdef, out_of_ops, unsupported
        );
    }

    // And the proof that the guardrail is live rather than merely compiled: the
    // **real** arena with a deadline of one microsecond, which no run can meet.
    // If this ever comes back zero, the clock read has been optimised away or
    // the extension is not really on, and the deadline is a comment.
    if gpu.clock == punchcutter_core::gpu::ShaderClock::Device {
        let mut tight = Gpu {
            deadline_ticks: gpu.deadline_ticks / (DEADLINE_MS * 1000),
            ..gpu
        };
        tight.deadline_ticks = tight.deadline_ticks.max(1);
        let t = tight.run(real.bytes(), font, &batch)?;
        let hit = t
            .runs
            .iter()
            .filter(|r| r.flags & SHAPE_RUN_DEADLINE != 0)
            .count();
        println!(
            "deadline self-test: {} ticks (1 µs) stopped {hit} of {} runs",
            tight.deadline_ticks,
            t.runs.len()
        );
        if hit == 0 {
            bail!("the deadline never fired at one microsecond; it is not live");
        }
    }
    Ok(())
}

/// Wall clock one run of the shaper may spend before `SHAPE_RUN_DEADLINE` ends
/// it. A tunable, and a generous one: its job is to stop a runaway walk, not to
/// enforce a frame budget — the caps and the operation budget do that.
///
/// Four milliseconds looks right and is not. A lane's *wall* clock spans its
/// whole wave, not its own string: thirty-two lanes shaping thirty-two
/// different strings execute the union of their paths, so a run's elapsed time
/// is the dispatch's. At four milliseconds six of this probe's thirty-four **real**
/// runs were cut on RADV, which is truncated text on screen. Two hundred and fifty is above any dispatch the adapters measured produce from a real font
/// and two orders of magnitude below the seconds a driver waits before it resets a ring, which these bounds exist to stay away from.
const DEADLINE_MS: u32 = 250;

/// What a mutated dispatch may cost before the fuzzer calls it a failure.
///
/// Generous against lavapipe in a virtual machine, and two orders of magnitude under the
/// seconds a driver waits before it resets a ring, which these bounds exist to stay away from.
const MUTATION_CAP_MS: f64 = 250.0;

/// A flattened `cmap` array, the plausible wrong buffer:
/// `(codepoint, gid)` sorted by codepoint, one `u32` each. A shaper reading it as an arena reads the first pair as a
/// version and a face count.
fn old_cmap_array(data: &[u8]) -> Result<Vec<u8>> {
    let face = ttf_parser::Face::parse(data, 0)?;
    let mut pairs: Vec<(u32, u32)> = Vec::new();
    if let Some(cmap) = face.tables().cmap {
        for sub in cmap.subtables {
            if !sub.is_unicode() {
                continue;
            }
            sub.codepoints(|cp| {
                if let Some(g) = sub.glyph_index(cp) {
                    pairs.push((cp, u32::from(g.0)));
                }
            });
        }
    }
    pairs.sort_unstable();
    pairs.dedup_by_key(|p| p.0);
    if pairs.len() < 4 {
        bail!("{FONT} has no usable cmap; there is no mismatch to build");
    }
    let mut bytes = Vec::with_capacity(pairs.len() * 8);
    for (cp, g) in &pairs {
        bytes.extend_from_slice(&cp.to_le_bytes());
        bytes.extend_from_slice(&g.to_le_bytes());
    }
    Ok(bytes)
}

/// `--dump-mismatch PATH`: write the old `cmap` array there and exit.
fn dump_path() -> Option<String> {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if let Some(rest) = arg.strip_prefix("--dump-mismatch=") {
            return Some(rest.to_owned());
        }
        if arg == "--dump-mismatch" {
            return args.next();
        }
    }
    None
}

/// `--mutate` with an optional count, or `None` for the four fixed cases.
fn mutation_count() -> Option<u32> {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if let Some(rest) = arg.strip_prefix("--mutate=") {
            return Some(rest.parse().unwrap_or(1000));
        }
        if arg == "--mutate" {
            return Some(args.next().and_then(|n| n.parse().ok()).unwrap_or(1000));
        }
    }
    None
}

/// The real arena, damaged `count` times over, one dispatch each.
///
/// Each mutation flips a handful of bits and overwrites a handful of whole
/// words — the second matters more than the first, because a word is an offset
/// or a length in the directory and a bit is usually noise inside a table. The
/// seed is fixed, so a failure names a mutation number someone can reproduce.
fn mutate(gpu: &Gpu, real: &[u8], font: ShapeFont, batch: &Batch, count: u32) -> Result<()> {
    // The first dispatch on lavapipe is the cold JIT of a 1.5 MB module and
    // has nothing to do with shaping, so it is taken before the clock starts.
    let warm = gpu.run_with(real, font, batch, false)?;
    println!(
        "warm-up (the real arena): {:.2} ms; mutating {count} times, cap {MUTATION_CAP_MS} ms",
        warm.dispatch_ms
    );

    let mut rng = 0x243F_6A88_85A3_08D3u64;
    let mut next = move || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };
    let mut worst = 0.0f64;
    let mut worst_at = 0u32;
    let mut deadlines = 0u32;
    let mut out_of_ops = 0u32;
    let mut damaged = real.to_vec();
    for m in 0..count {
        damaged.copy_from_slice(real);
        // Between one and sixteen of each, so a mutation is sometimes a nudge
        // and sometimes a wrecking ball.
        let flips = 1 + (next() % 16) as usize;
        for _ in 0..flips {
            let at = (next() % damaged.len() as u64) as usize;
            damaged[at] ^= 1u8 << (next() % 8);
        }
        let words = 1 + (next() % 16) as usize;
        for _ in 0..words {
            let at = ((next() % (damaged.len() as u64 / 4)) * 4) as usize;
            let value = (next() as u32).to_le_bytes();
            damaged[at..at + 4].copy_from_slice(&value);
        }
        let t = gpu.run_with(&damaged, font, batch, false)?;
        for r in &t.runs {
            deadlines += u32::from(r.flags & SHAPE_RUN_DEADLINE != 0);
            out_of_ops += u32::from(r.flags & SHAPE_RUN_OUT_OF_OPS != 0);
        }
        if t.dispatch_ms > worst {
            worst = t.dispatch_ms;
            worst_at = m;
        }
        if t.dispatch_ms > MUTATION_CAP_MS {
            bail!(
                "mutation {m} took {:.2} ms, over the {MUTATION_CAP_MS} ms cap — a loop in \
                 ot.slang or shape.slang is taking an unbounded trip count from the arena",
                t.dispatch_ms
            );
        }
    }
    println!(
        "{count} mutations: worst dispatch {worst:.2} ms (mutation {worst_at}), \
         {deadlines} runs hit the deadline, {out_of_ops} ran out of ops"
    );
    Ok(())
}

struct Timing {
    runs: Vec<ShapeRun>,
    dispatch_ms: f64,
    pass_ms: f64,
    repeats: u32,
}

/// One dispatch first; repeats only if that one was cheap, so a slow walk
/// cannot drive the driver into a hang recovery.
const TIMING_BUDGET_MS: f64 = 200.0;
const MAX_REPEATS: u32 = 10;

struct Gpu {
    device: wgpu::Device,
    queue: wgpu::Queue,
    layout: wgpu::BindGroupLayout,
    pipeline: wgpu::ComputePipeline,
    /// One thread, one clock read: what `measure_clock` dispatches.
    probe: wgpu::ComputePipeline,
    clock: punchcutter_core::gpu::ShaderClock,
    /// What goes into `ShapeParams::deadline_ticks`; 0 where the device has no
    /// `VK_KHR_shader_clock`, and the caller says so rather than going quiet.
    deadline_ticks: u32,
}

impl Gpu {
    fn new() -> Result<Gpu> {
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
        // The shared device path (`punchcutter_core::gpu::open_device`): wgpu will not ask for
        // `VK_KHR_shader_clock`, and without it the shaper's SPIR-V — which
        // carries the capability — cannot be loaded at all.
        let (device, queue, clock) = punchcutter_core::gpu::open_device(
            adapter,
            &wgpu::DeviceDescriptor {
                label: Some("punchcutter-shape-garbage"),
                required_features: wgpu::Features::PASSTHROUGH_SHADERS,
                required_limits: limits,
                ..Default::default()
            },
        )?;
        println!("shader clock: {clock:?}");
        // $PUNCHCUTTER_SLANG_DIR first, the source checkout second: this
        // probe can run on a test machine that has the modules but no
        // checkout.
        let source = std::env::var_os("PUNCHCUTTER_SLANG_DIR")
            .map(PathBuf::from)
            .filter(|dir| dir.is_dir())
            .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../core/slang"))
            .join("shape_demo.slang");
        let spirv = slang::compile(&source, punchcutter_shape::DEMO_ENTRY, &[], &[])
            .map_err(|err| anyhow::anyhow!("{err}"))?;
        let probe_spirv = slang::compile(&source, "clock_probe", &[], &[])
            .map_err(|err| anyhow::anyhow!("{err}"))?;
        let layout = create_layout(&device);
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("punchcutter-shape-pl"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
        let pipeline = punchcutter_core::gpu::passthrough_pipeline(
            &device,
            &pipeline_layout,
            "punchcutter-shape-pipeline",
            "shape_batch",
            &spirv,
            (punchcutter_shape::SHAPE_WORKGROUP, 1, 1),
        );
        let probe = punchcutter_core::gpu::passthrough_pipeline(
            &device,
            &pipeline_layout,
            "punchcutter-shape-clock-probe",
            "clock_probe",
            &probe_spirv,
            (1, 1, 1),
        );
        if let Some(err) = pollster::block_on(scope.pop()) {
            bail!("wgpu rejected the shaping module: {err}");
        }
        Ok(Gpu {
            device,
            queue,
            layout,
            pipeline,
            probe,
            clock,
            deadline_ticks: 0,
        })
    }

    fn run(&self, arena: &[u8], font: ShapeFont, batch: &Batch) -> Result<Timing> {
        self.run_with(arena, font, batch, true)
    }

    /// `repeats` false is one dispatch and no timing pass — what the mutation
    /// loop wants, because repeating a pass that might be slow is the thing
    /// this probe must not do a thousand times over.
    fn run_with(
        &self,
        arena: &[u8],
        font: ShapeFont,
        batch: &Batch,
        repeats_allowed: bool,
    ) -> Result<Timing> {
        let params = ShapeParams {
            font,
            request_count: batch.len() as u32,
            glyph_capacity: batch.glyph_capacity(),
            version: SHAPE_ABI_VERSION,
            deadline_ticks: self.deadline_ticks,
            // This probe forces no feature; every request's range is empty.
            features: [[0; 4]; 4],
        };
        let words = batch.utf8_words();
        let params_buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("shape-params"),
            size: size_of::<ShapeParams>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        self.queue
            .write_buffer(&params_buffer, 0, bytes_of(&params));
        let requests = self.storage("shape-requests", bytes_of_slice(batch.requests()));
        let ot = self.storage("shape-ot", arena);
        let utf8 = self.storage("shape-utf8", words_bytes(&words));
        let glyph_bytes = u64::from(batch.glyph_capacity()) * size_of::<ShapedGlyph>() as u64;
        let run_bytes = (batch.len().max(1) * size_of::<ShapeRun>()) as u64;
        let out_glyphs = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("shape-out-glyphs"),
            size: glyph_bytes,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let out_runs = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("shape-out-runs"),
            size: run_bytes,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let run_readback = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("shape-run-readback"),
            size: run_bytes,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("shape-bg"),
            layout: &self.layout,
            entries: &[
                entry(0, &params_buffer),
                entry(1, &requests),
                entry(2, &ot),
                entry(3, &utf8),
                entry(4, &out_glyphs),
                entry(5, &out_runs),
            ],
        });
        self.queue.submit([]);
        self.device.poll(wgpu::PollType::wait_indefinitely())?;

        let started = Instant::now();
        let mut encoder = self.device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_compute_pass(&Default::default());
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &bind_group, &[]);
            pass.dispatch_workgroups(batch.workgroups(), 1, 1);
        }
        encoder.copy_buffer_to_buffer(&out_runs, 0, &run_readback, 0, run_bytes);
        self.queue.submit([encoder.finish()]);
        self.device.poll(wgpu::PollType::wait_indefinitely())?;
        let dispatch_ms = started.elapsed().as_secs_f64() * 1e3;

        if !repeats_allowed {
            let runs = self.read_runs(&run_readback)?;
            return Ok(Timing {
                runs,
                dispatch_ms,
                pass_ms: dispatch_ms,
                repeats: 1,
            });
        }
        let repeats = ((TIMING_BUDGET_MS / dispatch_ms.max(0.01)) as u32).clamp(1, MAX_REPEATS);
        let started = Instant::now();
        let mut encoder = self.device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_compute_pass(&Default::default());
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &bind_group, &[]);
            for _ in 0..repeats {
                pass.dispatch_workgroups(batch.workgroups(), 1, 1);
            }
        }
        self.queue.submit([encoder.finish()]);
        self.device.poll(wgpu::PollType::wait_indefinitely())?;
        let pass_ms = started.elapsed().as_secs_f64() * 1e3 / f64::from(repeats);

        let runs = self.read_runs(&run_readback)?;
        Ok(Timing {
            runs,
            dispatch_ms,
            pass_ms,
            repeats,
        })
    }

    fn read_runs(&self, run_readback: &wgpu::Buffer) -> Result<Vec<ShapeRun>> {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        run_readback
            .slice(..)
            .map_async(wgpu::MapMode::Read, move |r| {
                let _ = tx.send(r);
            });
        self.device.poll(wgpu::PollType::wait_indefinitely())?;
        rx.recv()??;
        let runs = {
            let view = run_readback.slice(..).get_mapped_range()?;
            let mut out = Vec::new();
            for chunk in view.chunks_exact(size_of::<ShapeRun>()) {
                let mut value = ShapeRun::default();
                assert!(punchcutter_core::plain::write_from_bytes(&mut value, chunk));
                out.push(value);
            }
            out
        };
        run_readback.unmap();
        Ok(runs)
    }

    /// The device clock's rate, in ticks a millisecond.
    ///
    /// No Vulkan limit reports it — `timestampPeriod` is the *timestamp* clock's
    /// and `getRealtimeClock` is a different one — so it is measured: two
    /// one-thread dispatches of `clock_probe` a known wall-clock interval apart,
    /// and the difference divided. The interval is long enough that the two
    /// submissions' own latency is noise.
    fn measure_clock(&self) -> Result<f64> {
        const INTERVAL_MS: u64 = 200;
        let first = self.read_clock()?;
        std::thread::sleep(std::time::Duration::from_millis(INTERVAL_MS));
        let second = self.read_clock()?;
        // Unsigned wrap is the clock's own: at 100 MHz the low word turns over
        // every forty-odd seconds, and the subtraction turns over with it.
        let ticks = second.wrapping_sub(first);
        Ok(f64::from(ticks) / INTERVAL_MS as f64)
    }

    fn read_clock(&self) -> Result<u32> {
        let params = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("clock-params"),
            size: size_of::<ShapeParams>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        // One buffer per binding: the layout has read-only and read-write
        // slots, and wgpu refuses a buffer used as both in one dispatch.
        let ro0 = self.storage("clock-ro0", &[0u8; 64]);
        let ro1 = self.storage("clock-ro1", &[0u8; 64]);
        let rw0 = self.storage("clock-rw0", &[0u8; 64]);
        let rw1 = self.storage("clock-rw1", &[0u8; 64]);
        let out = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("clock-out"),
            size: size_of::<ShapeRun>() as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let back = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("clock-readback"),
            size: size_of::<ShapeRun>() as u64,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("clock-bg"),
            layout: &self.layout,
            entries: &[
                entry(0, &params),
                entry(1, &ro0),
                entry(2, &ro1),
                entry(3, &rw0),
                entry(4, &rw1),
                entry(5, &out),
            ],
        });
        let mut encoder = self.device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_compute_pass(&Default::default());
            pass.set_pipeline(&self.probe);
            pass.set_bind_group(0, &bind_group, &[]);
            pass.dispatch_workgroups(1, 1, 1);
        }
        encoder.copy_buffer_to_buffer(&out, 0, &back, 0, size_of::<ShapeRun>() as u64);
        self.queue.submit([encoder.finish()]);
        self.device.poll(wgpu::PollType::wait_indefinitely())?;
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        back.slice(..).map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        self.device.poll(wgpu::PollType::wait_indefinitely())?;
        rx.recv()??;
        let ticks = {
            let view = back.slice(..).get_mapped_range()?;
            u32::from_le_bytes(view[0..4].try_into().unwrap())
        };
        back.unmap();
        Ok(ticks)
    }

    fn storage(&self, label: &str, bytes: &[u8]) -> wgpu::Buffer {
        let size = (bytes.len() as u64).max(16);
        let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some(label),
            size,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        if !bytes.is_empty() {
            self.queue.write_buffer(&buffer, 0, bytes);
        }
        buffer
    }
}

fn words_bytes<T: Copy>(words: &[T]) -> &[u8] {
    // SAFETY: `u32` has no padding and no invalid bit patterns.
    unsafe { std::slice::from_raw_parts(words.as_ptr().cast::<u8>(), size_of_val(words)) }
}

fn entry(binding: u32, buffer: &wgpu::Buffer) -> wgpu::BindGroupEntry<'_> {
    wgpu::BindGroupEntry {
        binding,
        resource: buffer.as_entire_binding(),
    }
}

fn create_layout(device: &wgpu::Device) -> wgpu::BindGroupLayout {
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
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("punchcutter-shape-bgl"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            },
            storage(1, true),
            storage(2, true),
            storage(3, false),
            storage(4, false),
            storage(5, false),
        ],
    })
}
