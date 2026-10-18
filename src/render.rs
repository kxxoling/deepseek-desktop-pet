//! 薄渲染层：全局共享一套 wgpu 设备，每窗一个 surface + 一张 320×180 纹理。
//!
//! 渲染路径：帧 RGBA（直通 alpha）→ `queue.write_texture` → 全屏三角形
//! nearest 采样放大 → surface。clear (0,0,0,0)、无混合覆盖写（alpha 通道
//! 一并写入），配合 `PostMultiplied` 实现透明窗口逐像素合成。

use std::sync::Arc;

use winit::window::Window;

use crate::library::{FRAME_H, FRAME_W};

const SHADER: &str = r"
struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

// 覆盖整个 NDC [-1,1] 的大三角形
@vertex
fn vs(@builtin(vertex_index) vi: u32) -> VsOut {
    var xy = array<vec2<f32>, 3>(vec2(-1.0, -3.0), vec2(3.0, 1.0), vec2(-1.0, 1.0));
    var out: VsOut;
    let p = xy[vi];
    out.pos = vec4(p, 0.0, 1.0);
    // 纹理首行对应窗口顶部：v 轴翻转
    out.uv = vec2(p.x * 0.5 + 0.5, 0.5 - p.y * 0.5);
    return out;
}

@group(0) @binding(0) var tex: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;

@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> {
    return textureSample(tex, samp, in.uv);
}
";

/// alpha 合成模式：`DS_ALPHA`（post/pre/auto）在能力支持时生效，否则回退 Post → Pre → Auto
#[cfg(not(target_os = "windows"))]
fn pick_alpha_mode(supported: &[wgpu::CompositeAlphaMode]) -> wgpu::CompositeAlphaMode {
    use wgpu::CompositeAlphaMode;
    let want = match std::env::var("DS_ALPHA").as_deref() {
        Ok("pre") => CompositeAlphaMode::PreMultiplied,
        Ok("auto") => CompositeAlphaMode::Auto,
        _ => CompositeAlphaMode::PostMultiplied,
    };
    if supported.contains(&want) {
        return want;
    }
    [
        CompositeAlphaMode::PostMultiplied,
        CompositeAlphaMode::PreMultiplied,
    ]
    .into_iter()
    .find(|m| supported.contains(m))
    .unwrap_or(CompositeAlphaMode::Auto)
}

pub struct Gpu {
    #[cfg(not(target_os = "windows"))]
    instance: wgpu::Instance,
    #[cfg(not(target_os = "windows"))]
    adapter: wgpu::Adapter,
    device: wgpu::Device,
    queue: wgpu::Queue,
    module: wgpu::ShaderModule,
    sampler: wgpu::Sampler,
    tex_bgl: wgpu::BindGroupLayout,
}

impl Gpu {
    /// surface 创建要求 instance 持有 display handle（wgpu 29），
    /// 取自 `event_loop.owned_display_handle()`
    pub fn new(display: winit::event_loop::OwnedDisplayHandle) -> Option<Self> {
        // 只启用主力后端（Metal/Vulkan/DX12）：GL 后端会把整套系统 GL
        // 驱动库映射进进程，内存开销过大
        let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
        desc.backends = wgpu::Backends::PRIMARY;
        desc.display = Some(Box::new(display));
        let instance = wgpu::Instance::new(desc);
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            ..Default::default()
        }))
        .ok()?;
        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default())).ok()?;

        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("ds-pet-blit"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let tex_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("frame-tex-bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("nearest"),
            mag_filter: wgpu::FilterMode::Nearest, // 整数倍缩放像素精确
            min_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });
        #[cfg(target_os = "windows")]
        drop((instance, adapter)); // Windows 离屏路径不保留 surface 相关句柄
        Some(Self {
            #[cfg(not(target_os = "windows"))]
            instance,
            #[cfg(not(target_os = "windows"))]
            adapter,
            device,
            queue,
            module,
            sampler,
            tex_bgl,
        })
    }

    /// macOS/Linux：surface 呈现
    #[cfg(not(target_os = "windows"))]
    pub fn create_surface(&self, window: Arc<Window>) -> Option<PetSurface> {
        let size = window.inner_size();
        let (w, h) = (size.width.max(1), size.height.max(1));
        let surface = self
            .instance
            .create_surface(wgpu::SurfaceTarget::Window(Box::new(window)))
            .map_err(|e| log::error!("create_surface 失败：{e:?}"))
            .ok()?;
        let caps = surface.get_capabilities(&self.adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(wgpu::TextureFormat::is_srgb)
            .unwrap_or(caps.formats[0]);
        Some(self.build_surface(format, w, h, surface))
    }

    /// Windows：swapchain 无 per-pixel alpha（dx12 后端对普通窗口只支持 Opaque），
    /// 离屏渲染 + 回读 + 分层窗口合成，无失败点
    #[cfg(target_os = "windows")]
    pub fn create_surface(&self, window: &Arc<Window>) -> PetSurface {
        let size = window.inner_size();
        let (w, h) = (size.width.max(1), size.height.max(1));
        self.build_surface(wgpu::TextureFormat::Bgra8UnormSrgb, w, h)
    }

    #[cfg(not(target_os = "windows"))]
    fn build_surface(
        &self,
        format: wgpu::TextureFormat,
        width: u32,
        height: u32,
        surface: wgpu::Surface<'static>,
    ) -> PetSurface {
        let pl = self
            .device
            .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("blit-pl"),
                bind_group_layouts: &[Some(&self.tex_bgl)],
                immediate_size: 0,
            });
        let pipeline = self
            .device
            .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("blit-pipeline"),
                layout: Some(&pl),
                vertex: wgpu::VertexState {
                    module: &self.module,
                    entry_point: Some("vs"),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    buffers: &[],
                },
                fragment: Some(wgpu::FragmentState {
                    module: &self.module,
                    entry_point: Some("fs"),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: None,
            });

        let frame_tex = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("frame-320x180"),
            size: frame_extent(),
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let view = frame_tex.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("frame-bg"),
            layout: &self.tex_bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        });

        // macOS/Linux：配置 surface；Windows：准备离屏 target + 分层窗口
        #[cfg(not(target_os = "windows"))]
        let (surface, config) = {
            let config = wgpu::SurfaceConfiguration {
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                format,
                width,
                height,
                present_mode: wgpu::PresentMode::Fifo,
                alpha_mode: pick_alpha_mode(&surface.get_capabilities(&self.adapter).alpha_modes),
                view_formats: vec![],
                desired_maximum_frame_latency: 2,
            };
            surface.configure(&self.device, &config);
            (surface, config)
        };
        #[cfg(target_os = "windows")]
        {
            let _ = (format, width, height, surface);
        }

        PetSurface {
            surface,
            config,
            pipeline,
            bind_group,
            frame_tex,
        }
    }

    /// Windows：无 surface，离屏渲染 + 分层窗口
    #[cfg(target_os = "windows")]
    fn build_surface(&self, format: wgpu::TextureFormat, width: u32, height: u32) -> PetSurface {
        let pl = self
            .device
            .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("blit-pl"),
                bind_group_layouts: &[Some(&self.tex_bgl)],
                immediate_size: 0,
            });
        let pipeline = self
            .device
            .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("blit-pipeline"),
                layout: Some(&pl),
                vertex: wgpu::VertexState {
                    module: &self.module,
                    entry_point: Some("vs"),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    buffers: &[],
                },
                fragment: Some(wgpu::FragmentState {
                    module: &self.module,
                    entry_point: Some("fs"),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: None,
            });
        let frame_tex = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("frame-320x180"),
            size: frame_extent(),
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let view = frame_tex.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("frame-bg"),
            layout: &self.tex_bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        });
        PetSurface {
            pipeline,
            bind_group,
            frame_tex,
            offscreen: OffscreenTarget::new(&self.device, width, height),
        }
    }
}

fn frame_extent() -> wgpu::Extent3d {
    wgpu::Extent3d {
        width: FRAME_W,
        height: FRAME_H,
        depth_or_array_layers: 1,
    }
}

/// surface 失效类型
#[derive(Debug)]
pub enum SurfaceLost {
    #[allow(dead_code)] // 仅非 Windows 的 surface 路径构造
    Outdated,
    Lost,
}

/// 每窗渲染面
pub struct PetSurface {
    #[cfg(not(target_os = "windows"))]
    surface: wgpu::Surface<'static>,
    #[cfg(not(target_os = "windows"))]
    config: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    bind_group: wgpu::BindGroup,
    frame_tex: wgpu::Texture,
    #[cfg(target_os = "windows")]
    offscreen: OffscreenTarget,
}

impl PetSurface {
    /// 上传一帧（RGBA，长度须为 `FRAME_BYTES`）
    pub fn upload(&self, gpu: &Gpu, frame: &[u8]) {
        gpu.queue.write_texture(
            self.frame_tex.as_image_copy(),
            frame,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(FRAME_W * 4),
                rows_per_image: Some(FRAME_H),
            },
            frame_extent(),
        );
    }

    /// 绘制编码（平台无关）：全屏三角形把 320×180 帧纹理放大到目标视图
    fn encode_blit(&self, gpu: &Gpu, view: &wgpu::TextureView) -> wgpu::CommandEncoder {
        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("blit"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("blit-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.0,
                            g: 0.0,
                            b: 0.0,
                            a: 0.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &self.bind_group, &[]);
            pass.draw(0..3, 0..1);
        }
        encoder
    }

    /// 绘制并呈现；Outdated/Lost 返回 Err 由上层重配/重建
    #[cfg(not(target_os = "windows"))]
    pub fn render(&mut self, gpu: &Gpu) -> Result<(), SurfaceLost> {
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f)
            | wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
            wgpu::CurrentSurfaceTexture::Outdated => {
                self.surface.configure(&gpu.device, &self.config);
                return Err(SurfaceLost::Outdated);
            }
            wgpu::CurrentSurfaceTexture::Lost | wgpu::CurrentSurfaceTexture::Validation => {
                return Err(SurfaceLost::Lost);
            }
            _ => return Ok(()),
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let encoder = self.encode_blit(gpu, &view);
        gpu.queue.submit(Some(encoder.finish()));
        frame.present();
        Ok(())
    }

    /// Windows：渲染到离屏纹理 → 回读 → 预乘 → `UpdateLayeredWindow`
    #[cfg(target_os = "windows")]
    pub fn render(
        &mut self,
        gpu: &Gpu,
        hwnd: windows::Win32::Foundation::HWND,
        bmp: &mut crate::platform::windows_layered::LayeredBitmap,
        alpha: u8,
    ) -> Result<(), SurfaceLost> {
        let target = &self.offscreen;
        let view = target
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self.encode_blit(gpu, &view);
        encoder.copy_texture_to_buffer(
            target.texture.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &target.buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(target.width * 4),
                    rows_per_image: Some(target.height),
                },
            },
            wgpu::Extent3d {
                width: target.width,
                height: target.height,
                depth_or_array_layers: 1,
            },
        );
        let submitted = gpu.queue.submit(Some(encoder.finish()));

        // 同步等待回读（15fps 的小位图，等待开销可忽略）
        let slice = target.buffer.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        gpu.device
            .poll(wgpu::PollType::Wait {
                submission_index: Some(submitted),
                timeout: None,
            })
            .ok();
        match rx.recv() {
            Ok(Ok(())) => {}
            _ => return Err(SurfaceLost::Lost),
        }

        let data = slice.get_mapped_range();
        let ok = bmp.write_premultiplied(&data, target.width, target.height);
        drop(data);
        target.buffer.unmap();
        if !ok || !bmp.present(hwnd, alpha) {
            log::error!("UpdateLayeredWindow 提交失败");
        }
        Ok(())
    }

    /// 窗口物理尺寸变化后重配
    #[cfg(not(target_os = "windows"))]
    pub fn resize(&mut self, gpu: &Gpu, width: u32, height: u32) {
        if width > 0 && height > 0 && (self.config.width != width || self.config.height != height) {
            self.config.width = width;
            self.config.height = height;
            self.surface.configure(&gpu.device, &self.config);
        }
    }

    #[cfg(target_os = "windows")]
    pub fn resize(&mut self, gpu: &Gpu, width: u32, height: u32) {
        if width > 0
            && height > 0
            && (self.offscreen.width != width || self.offscreen.height != height)
        {
            self.offscreen = OffscreenTarget::new(&gpu.device, width, height);
        }
    }
}

/// Windows 离屏渲染目标：纹理 + 回读 buffer
#[cfg(target_os = "windows")]
struct OffscreenTarget {
    texture: wgpu::Texture,
    buffer: wgpu::Buffer,
    width: u32,
    height: u32,
}

#[cfg(target_os = "windows")]
impl OffscreenTarget {
    fn new(device: &wgpu::Device, width: u32, height: u32) -> Self {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("offscreen"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Bgra8UnormSrgb,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback"),
            size: u64::from(width * height * 4),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        Self {
            texture,
            buffer,
            width,
            height,
        }
    }
}
