//! 薄渲染层：全局共享一套 wgpu 设备，每窗一个 surface + 一张 320×180 纹理。
//!
//! 渲染路径：帧 RGBA（直通 alpha）→ `queue.write_texture` → 全屏三角形
//! nearest 采样放大 → surface。clear (0,0,0,0)、无混合覆盖写（alpha 通道
//! 一并写入），配合 `PostMultiplied` 实现透明窗口逐像素合成。

use std::sync::Arc;

use winit::window::Window;

use crate::library::{FRAME_H, FRAME_W};

/// 点击 Q 弹形变的总时长（毫秒）
pub const SQUASH_DUR_MS: f64 = 300.0;

/// 点击 Q 弹的高度比例曲线：压缩幅度随时间指数衰减，按余弦节奏回弹。
/// `t_ms` 超出 `[0, SQUASH_DUR_MS)` 时返回 1（无形变）
pub fn squash_scale(t_ms: f64) -> f64 {
    if !(0.0..SQUASH_DUR_MS).contains(&t_ms) {
        return 1.0;
    }
    let env = (-(t_ms / 120.0)).exp();
    let osc = 0.5 + 0.5 * (std::f64::consts::TAU * t_ms / SQUASH_DUR_MS).cos();
    1.0 - 0.35 * env * osc
}

/// 探头姿态角（度）：身体探出屏幕边缘时的倾斜幅度
pub const PROBE_ANGLE_DEG: f64 = 45.0;
/// 探头自动退回的时长（秒）
pub const PROBE_RETREAT_SECS: f64 = 5.0;
/// 松手时距屏幕左右边缘多近才进入探头（逻辑点）
pub const PROBE_EDGE_MARGIN: f64 = 40.0;

/// 松手时按窗口与屏幕边缘的关系决定是否进入探头。
/// 返回姿态角：靠近左边缘时为 -45°（身体向左倾），靠近右边缘时为 +45°，不在边缘附近返回 None
pub fn probe_angle_at(win_x: f64, win_w: f64, screen_w: f64) -> Option<f64> {
    let left_gap = win_x;
    let right_gap = screen_w - (win_x + win_w);
    if left_gap <= PROBE_EDGE_MARGIN && left_gap <= right_gap {
        Some(-PROBE_ANGLE_DEG)
    } else if right_gap <= PROBE_EDGE_MARGIN {
        Some(PROBE_ANGLE_DEG)
    } else {
        None
    }
}

/// 把整帧绕 `center` 旋转 `angle_deg` 后写入 dst（屏幕坐标 y 向下，正角为顺时针）。
/// 采样时反向旋转取源图的最近像素，超出源图范围的像素保持透明
pub fn rotate_into(dst: &mut [u8], src: &[u8], angle_deg: f64, center: (f64, f64)) {
    const W: usize = FRAME_W as usize;
    const H: usize = FRAME_H as usize;
    let rad = angle_deg.to_radians();
    let (cos, sin) = (rad.cos(), rad.sin());
    for y in 0..H {
        for x in 0..W {
            let dx = f64::from(x as u16) - center.0;
            let dy = f64::from(y as u16) - center.1;
            let sx = center.0 + dx * cos + dy * sin;
            let sy = center.1 - dx * sin + dy * cos;
            let d = (y * W + x) * 4;
            if (0.0..W as f64).contains(&sx) && (0.0..H as f64).contains(&sy) {
                let s = ((sy as usize) * W + sx as usize) * 4;
                dst[d..d + 4].copy_from_slice(&src[s..s + 4]);
            } else {
                dst[d] = 0;
                dst[d + 1] = 0;
                dst[d + 2] = 0;
                dst[d + 3] = 0;
            }
        }
    }
}

/// 回旋起始角速度（度/秒）
pub const SPIN_START: f64 = 240.0;
/// 每次菜单触发对角速度的放大倍数
pub const SPIN_ACCEL: f64 = 1.8;
/// 角速度上限（度/秒，约每秒三圈）
pub const SPIN_MAX: f64 = 1080.0;
/// 角速度每秒的衰减比例（摩擦）
pub const SPIN_DECAY_PER_S: f64 = 0.5;
/// 低于此角速度停止回旋（度/秒）
pub const SPIN_STOP: f64 = 30.0;

/// 触发一次回旋后的角速度：第一次从起步值开始，连续触发会加速，到上限封顶
pub fn spin_next_omega(current: Option<f64>) -> f64 {
    match current {
        None => SPIN_START,
        Some(o) => (o * SPIN_ACCEL).min(SPIN_MAX),
    }
}

/// 撞飞倾斜的最大角度（度）
pub const KNOCK_MAX_TILT: f64 = 45.0;
/// 被撞飞的宠物落稳后，停在屏幕边缘时重新进入探头姿态要等待的秒数
pub const KNOCK_REPROBE_SECS: f64 = 5.0;
/// 撞飞倾斜的参考水平速度（px/s，达到这个速度就倾斜到最大角度）
pub const KNOCK_REF_VX: f64 = 600.0;

/// 被撞飞时的身体倾角：随水平速度的方向倾斜，速度低时趋于零
pub fn knock_tilt_of(vx: f64) -> f64 {
    (vx / KNOCK_REF_VX * KNOCK_MAX_TILT).clamp(-KNOCK_MAX_TILT, KNOCK_MAX_TILT)
}

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
            label: Some("frame"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
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
            tex_w: width,
            tex_h: height,
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
            label: Some("frame"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
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
            tex_w: width,
            tex_h: height,
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
    /// 纹理宽高（上传时用，宠物窗 320×180、气泡窗自定义）
    #[allow(dead_code)]
    tex_w: u32,
    #[allow(dead_code)]
    tex_h: u32,
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

#[cfg(test)]
mod squash_tests {
    use super::*;

    fn close(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-9, "{a} != {b}");
    }

    #[test]
    fn squash_scale_bounds_and_phases() {
        // 起始时刻压缩最大：1 - 0.35 * env(0) * osc(0)
        close(squash_scale(0.0), 1.0 - 0.35);
        // 半程时回到原高：osc 为 0
        close(squash_scale(SQUASH_DUR_MS / 2.0), 1.0);
        // 超出时长后和负时间都返回无形变
        close(squash_scale(SQUASH_DUR_MS), 1.0);
        close(squash_scale(-1.0), 1.0);
        // 全程都在 [1-0.35, 1]
        let t = |i: usize| SQUASH_DUR_MS * f64::from(i as u32) / 16.0;
        for i in 0..17 {
            let s = squash_scale(t(i));
            assert!((0.65..=1.0).contains(&s), "t={} s={}", t(i), s);
        }
    }
}

#[cfg(test)]
mod spin_tests {
    use super::*;

    #[test]
    fn spin_omega_ladder() {
        let o0 = spin_next_omega(None);
        assert_eq!(o0, SPIN_START);
        let o1 = spin_next_omega(Some(o0));
        assert_eq!(o1, SPIN_START * SPIN_ACCEL);
        // 连续触发封顶
        let mut o = o1;
        for _ in 0..10 {
            o = spin_next_omega(Some(o));
        }
        assert_eq!(o, SPIN_MAX);
    }
}

#[cfg(test)]
mod knock_tests {
    use super::*;

    #[test]
    fn knock_tilt_scales_and_clamps() {
        assert_eq!(knock_tilt_of(0.0), 0.0);
        assert_eq!(knock_tilt_of(KNOCK_REF_VX), KNOCK_MAX_TILT);
        assert_eq!(knock_tilt_of(-KNOCK_REF_VX), -KNOCK_MAX_TILT);
        // 超过参考速度封顶；低速按比例
        assert_eq!(knock_tilt_of(KNOCK_REF_VX * 10.0), KNOCK_MAX_TILT);
        assert_eq!(knock_tilt_of(KNOCK_REF_VX / 2.0), KNOCK_MAX_TILT / 2.0);
    }
}
