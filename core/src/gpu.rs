//! The wgpu helpers the engine's harnesses use, and a consumer may.
//!
//! Everything here is mechanism: opening a device with the shader clock the
//! shaper's deadline reads, creating a pipeline from a SPIR-V module, and
//! allocating a storage buffer. The decisions (which bind group, which
//! dispatch shape, when to dispatch) stay with the caller.

/// Whether a device came up with `VK_KHR_shader_clock`, which is the
/// difference between a shaper that can hold a deadline and one that can only
/// count its own steps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShaderClock {
    /// The extension is on and `shaderDeviceClock` is enabled: `getRealtimeClock`
    /// in a shader reads the device's wall clock.
    Device,
    /// The adapter does not advertise it. The shader's SPIR-V carries the
    /// capability, so this is **not** a silent fallback: the caller says so and
    /// runs on the operation budget alone, and a pipeline that reads the clock
    /// will be refused by the device rather than drawing wrong.
    Absent,
}

/// Open a Vulkan device with `VK_KHR_shader_clock` enabled where the adapter
/// has it.
///
/// wgpu has no feature bit for this extension and will not ask for it, so the
/// device goes up through wgpu-hal's `open_with_callback`, which hands out the
/// extension list and the `VkDeviceCreateInfo` before `vkCreateDevice`, and
/// comes back as a wgpu `Device` through `create_device_from_hal`.
///
/// Why any of it: a malformed font arena (random bytes under a valid directory) took the shaper to 1.39 s a
/// dispatch on lavapipe and hung RADV's graphics ring outright
/// (`shape/examples/garbage.rs` reproduces it). The shaper's caps and
/// operation budget bound that by *counting*, and counting is only ever as
/// good as the accounting. A clock read inside the kernel is the guardrail
/// that holds when the accounting is wrong: a runaway shader must not take
/// down the whole GPU.
///
/// A non-Vulkan backend, or an adapter without the extension, comes back
/// [`ShaderClock::Absent`] and the caller must say so.
pub fn open_device(
    adapter: &wgpu::Adapter,
    descriptor: &wgpu::DeviceDescriptor<'_>,
) -> Result<(wgpu::Device, wgpu::Queue, ShaderClock), wgpu::RequestDeviceError> {
    let wanted = ash::khr::shader_clock::NAME;
    let hal = unsafe { adapter.as_hal::<wgpu::hal::api::Vulkan>() };
    let opened = match &hal {
        Some(hal)
            if hal
                .physical_device_capabilities()
                .supports_extension(wanted) =>
        {
            // The features struct has to outlive the callback that chains it
            // into the create info, so it is declared here and borrowed in.
            let mut clock =
                ash::vk::PhysicalDeviceShaderClockFeaturesKHR::default().shader_device_clock(true);
            unsafe {
                hal.open_with_callback(
                    descriptor.required_features,
                    &descriptor.required_limits,
                    &descriptor.memory_hints,
                    Some(Box::new(
                        |args: wgpu::hal::vulkan::CreateDeviceCallbackArgs<'_, '_, '_>| {
                            args.extensions.push(wanted);
                            let info = std::mem::take(args.create_info);
                            *args.create_info = info.push_next(&mut clock);
                        },
                    )),
                )
                .ok()
            }
        }
        _ => None,
    };
    let Some(opened) = opened else {
        drop(hal);
        let (device, queue) = pollster::block_on(adapter.request_device(descriptor))?;
        return Ok((device, queue, ShaderClock::Absent));
    };
    drop(hal);
    let (device, queue) =
        unsafe { adapter.create_device_from_hal::<wgpu::hal::api::Vulkan>(opened, descriptor)? };
    Ok((device, queue, ShaderClock::Device))
}

/// Create one compute pipeline from a passthrough SPIR-V module.
///
/// The caller is trusted to have produced `spv` from the caller's own Slang sources
/// — a build script's slangc output, or a runtime compile that
/// [`crate::slang`] has run `spirv-val` over — with bindings written against
/// `layout`. The passthrough path performs no validation of its own, so this
/// must be called inside a validation error scope: a module wgpu *can* reject
/// then surfaces as an `Err` rather than reaching the device's
/// uncaptured-error handler, which panics.
pub fn passthrough_pipeline(
    device: &wgpu::Device,
    layout: &wgpu::PipelineLayout,
    label: &str,
    entry: &'static str,
    spv: &[u8],
    workgroup: (u32, u32, u32),
) -> wgpu::ComputePipeline {
    // SAFETY: as documented above — the module is the caller's own slangc output,
    // and wgpu-hal numbers the Vulkan bindings by position in `layout`'s bind
    // group, which is exactly what the `[[vk::binding(n)]]` literals in the
    // Slang sources are written against.
    let module = unsafe {
        device.create_shader_module_passthrough(wgpu::ShaderModuleDescriptorPassthrough {
            label: Some(label),
            entry_points: std::borrow::Cow::Owned(vec![wgpu::PassthroughShaderEntryPoint {
                name: std::borrow::Cow::Borrowed(entry),
                // Metal-only; SPIR-V carries its own LocalSize.
                workgroup_size: workgroup,
            }]),
            spirv: Some(wgpu::util::make_spirv_raw(spv)),
            ..Default::default()
        })
    };
    device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some(label),
        layout: Some(layout),
        module: &module,
        entry_point: Some(entry),
        compilation_options: wgpu::PipelineCompilationOptions::default(),
        cache: None,
    })
}

/// A storage buffer the CPU writes and a shader reads.
pub fn storage_buffer(device: &wgpu::Device, label: &str, size: u64) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}
