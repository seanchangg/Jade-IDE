//! Real GPU renderer for the 3D weight grid — the Metal port of the WebGL2
//! engine in `weight-grid-3d.ts` (§7.2). One instanced unit-box draw renders
//! every bar (6 floats per instance: grid x, grid z, signed height, r, g, b),
//! exactly like the original's `drawElementsInstanced`; the reference grid and
//! value axis are line draws with the original premultiplied blend.
//!
//! Presentation: gpui has no custom-shader hook, but its `surface()` element
//! draws a `CVPixelBuffer` zero-copy — and hard-requires the biplanar YCbCr
//! format (`420YpCbCr8BiPlanarFullRange`, see gpui_macos `draw_surfaces`). So
//! the scene renders into a private BGRA target (4× MSAA, resolved), then two
//! tiny fullscreen passes convert RGB → Y (full-res R8 plane) and CbCr
//! (half-res RG8 plane) straight into the pixel buffer's IOSurface planes.
//! The conversion is the exact inverse of gpui's `ycbcrToRGBTransform`
//! (full-range BT.601), so colors round-trip unchanged.
//!
//! Sync: three pixel buffers rotate so a frame gpui may still be sampling is
//! never overwritten; each render commits and waits (the whole GPU frame is
//! well under a millisecond at 256×256 instances).
//!
//! Shaders compile from source at first use — the same runtime-compile
//! approach as the TS `compileProgram` — and failure falls back to the CPU
//! painter in `render.rs`, keeping headless/test environments working.

use core_foundation::base::TCFType;
use core_foundation::boolean::CFBoolean;
use core_foundation::dictionary::CFDictionary;
use core_foundation::string::CFString;
use core_video::metal_texture::CVMetalTextureGetTexture;
use core_video::metal_texture_cache::CVMetalTextureCache;
use metal::foreign_types::ForeignTypeRef;
use core_video::pixel_buffer::{
    kCVPixelFormatType_420YpCbCr8BiPlanarFullRange, CVPixelBuffer, CVPixelBufferKeys,
};
use metal::{
    CompileOptions, DepthStencilDescriptor, Device, MTLClearColor, MTLCompareFunction,
    MTLIndexType, MTLLoadAction, MTLPixelFormat, MTLPrimitiveType, MTLResourceOptions,
    MTLStorageMode, MTLStoreAction, MTLTextureType, MTLTextureUsage, RenderPassDescriptor,
    RenderPipelineDescriptor, TextureDescriptor,
};

use super::camera::{OrbitCamera, HEIGHT_SCALE};
use super::grid::BarGrid;

/// Backdrop clear color — the overlay's near-opaque `#111214` (render.rs).
/// The scene clear — the jade-dark canvas (#1E1F22), so the GPU frame and
/// the GPUI backdrop around it are one surface.
const CLEAR: (f64, f64, f64) = (0x1E as f64 / 255.0, 0x1F as f64 / 255.0, 0x22 as f64 / 255.0);

/// How many pixel buffers rotate (gpui may sample frame N while N+1 renders).
const POOL: usize = 3;

/// The MSL translation of BARS_VS/FS, LINES_VS/FS (weight-grid-3d.ts:42-96)
/// plus the fullscreen RGB→YCbCr conversion passes. Vertex data is read as
/// raw `device float*` streams (no vertex descriptors), mirroring the TS
/// interleaved layouts: box verts are 6 floats (pos, nrm), instances 6 floats
/// (cell x, cell z, signed height, rgb), line verts 6 floats (pos, rgb).
const MSL: &str = r#"
#include <metal_stdlib>
using namespace metal;

struct Uniforms { float4x4 proj; float4x4 view; };

struct BarOut {
    float4 position [[position]];
    float3 nrm;
    float3 color;
};

vertex BarOut bars_vs(uint vid [[vertex_id]], uint iid [[instance_id]],
                      const device float* verts [[buffer(0)]],
                      const device float* inst [[buffer(1)]],
                      constant Uniforms& u [[buffer(2)]]) {
    float3 pos = float3(verts[vid*6+0], verts[vid*6+1], verts[vid*6+2]);
    float3 nrm = float3(verts[vid*6+3], verts[vid*6+4], verts[vid*6+5]);
    float3 cell = float3(inst[iid*6+0], inst[iid*6+1], inst[iid*6+2]);
    float3 color = float3(inst[iid*6+3], inst[iid*6+4], inst[iid*6+5]);
    float h = cell.z;
    float s = h < 0.0 ? -1.0 : 1.0;
    float3 world = float3(pos.x + cell.x, pos.y * h, pos.z + cell.y);
    BarOut o;
    o.nrm = float3(nrm.x, nrm.y * s, nrm.z);
    o.color = color;
    o.position = u.proj * u.view * float4(world, 1.0);
    return o;
}

// Lambert with the original three-light rig (ambient 0.65 + two directionals).
fragment float4 bars_fs(BarOut in [[stage_in]]) {
    float3 n = normalize(in.nrm);
    const float3 L1 = float3(0.4240, 0.8480, 0.3180);
    const float3 L2 = float3(-0.4243, 0.5657, -0.7071);
    float light = 0.65 + 0.9 * max(dot(n, L1), 0.0) + 0.3 * max(dot(n, L2), 0.0);
    return float4(in.color * min(light, 1.0), 1.0);
}

struct LineOut {
    float4 position [[position]];
    float3 color;
};

vertex LineOut lines_vs(uint vid [[vertex_id]],
                        const device float* verts [[buffer(0)]],
                        constant Uniforms& u [[buffer(2)]]) {
    LineOut o;
    o.color = float3(verts[vid*6+3], verts[vid*6+4], verts[vid*6+5]);
    o.position = u.proj * u.view
        * float4(verts[vid*6+0], verts[vid*6+1], verts[vid*6+2], 1.0);
    return o;
}

// Premultiplied output to match the (ONE, ONE_MINUS_SRC_ALPHA) blend mode.
fragment float4 lines_fs(LineOut in [[stage_in]],
                         constant float& opacity [[buffer(0)]]) {
    return float4(in.color * opacity, opacity);
}

struct QuadOut {
    float4 position [[position]];
    float2 uv;
};

// Fullscreen triangle; v flipped so row 0 of the target reads row 0 of the
// source (both are top-left origin).
vertex QuadOut fsq_vs(uint vid [[vertex_id]]) {
    float2 xy = float2((vid << 1) & 2, vid & 2);
    QuadOut o;
    o.position = float4(xy * 2.0 - 1.0, 0.0, 1.0);
    o.uv = float2(xy.x, 1.0 - xy.y);
    return o;
}

// Forward full-range BT.601 — the exact inverse of gpui's surface shader's
// ycbcrToRGBTransform, so the round trip is identity.
fragment float4 luma_fs(QuadOut in [[stage_in]],
                        texture2d<float> src [[texture(0)]]) {
    constexpr sampler s(mag_filter::linear, min_filter::linear);
    float3 c = src.sample(s, in.uv).rgb;
    return float4(dot(c, float3(0.299, 0.587, 0.114)), 0.0, 0.0, 1.0);
}

fragment float4 chroma_fs(QuadOut in [[stage_in]],
                          texture2d<float> src [[texture(0)]]) {
    constexpr sampler s(mag_filter::linear, min_filter::linear);
    float3 c = src.sample(s, in.uv).rgb;
    float cb = dot(c, float3(-0.168736, -0.331264, 0.5)) + 0.5;
    float cr = dot(c, float3(0.5, -0.418688, -0.081312)) + 0.5;
    return float4(cb, cr, 0.0, 1.0);
}
"#;

#[repr(C)]
struct Uniforms {
    proj: [f32; 16],
    view: [f32; 16],
}

/// Metal-convention perspective (depth 0..1; the math.rs matrix is the WebGL
/// one whose −1..1 z range would clip the near half of the scene in Metal).
fn perspective_metal(fov_y_deg: f32, aspect: f32, near: f32, far: f32) -> [f32; 16] {
    let f = 1.0 / (fov_y_deg * std::f32::consts::PI / 360.0).tan();
    let mut m = [0.0f32; 16];
    m[0] = f / aspect;
    m[5] = f;
    m[10] = far / (near - far);
    m[11] = -1.0;
    m[14] = (far * near) / (near - far);
    m
}

/// The shared unit box (`buildBoxGeometry`, weight-grid-3d.ts:126-159):
/// 0.9×0.9 footprint, y ∈ [0,1]; 24 interleaved pos+normal verts, 36 indices.
fn box_geometry() -> ([f32; 24 * 6], [u16; 36]) {
    const X: f32 = 0.45;
    const Z: f32 = 0.45;
    let faces: [([f32; 3], [[f32; 3]; 4]); 6] = [
        ([0., 1., 0.], [[-X, 1., -Z], [-X, 1., Z], [X, 1., Z], [X, 1., -Z]]),
        ([0., -1., 0.], [[-X, 0., -Z], [X, 0., -Z], [X, 0., Z], [-X, 0., Z]]),
        ([1., 0., 0.], [[X, 0., -Z], [X, 1., -Z], [X, 1., Z], [X, 0., Z]]),
        ([-1., 0., 0.], [[-X, 0., -Z], [-X, 0., Z], [-X, 1., Z], [-X, 1., -Z]]),
        ([0., 0., 1.], [[-X, 0., Z], [X, 0., Z], [X, 1., Z], [-X, 1., Z]]),
        ([0., 0., -1.], [[-X, 0., -Z], [-X, 1., -Z], [X, 1., -Z], [X, 0., -Z]]),
    ];
    let mut verts = [0.0f32; 24 * 6];
    let mut indices = [0u16; 36];
    let (mut vo, mut io) = (0, 0);
    for (f, (n, corners)) in faces.iter().enumerate() {
        for c in corners {
            verts[vo..vo + 3].copy_from_slice(c);
            verts[vo + 3..vo + 6].copy_from_slice(n);
            vo += 6;
        }
        let base = (f * 4) as u16;
        for k in [0, 1, 2, 0, 2, 3] {
            indices[io] = base + k;
            io += 1;
        }
    }
    (verts, indices)
}

/// Reference-grid + value-axis line vertices (pos3 + rgb3 per vert), ported
/// from `buildGrid` (weight-grid-3d.ts:693-723) and the axis upload (:790-796).
/// Returns (verts, grid_vertex_count); the axis is the trailing 2 verts.
fn line_geometry(rows: u32, cols: u32) -> (Vec<f32>, usize) {
    let size = rows.max(cols).max(1) as usize;
    let sx = cols as f32 / size as f32;
    let sz = rows as f32 / size as f32;
    let half = size as f32 / 2.0;
    const CENTER: [f32; 3] = [0x44 as f32 / 255.0, 0x44 as f32 / 255.0, 0x55 as f32 / 255.0];
    const LINE: [f32; 3] = [0x2a as f32 / 255.0, 0x2a as f32 / 255.0, 0x34 as f32 / 255.0];
    let mut v: Vec<f32> = Vec::with_capacity((size + 1) * 4 * 6 + 12);
    let mut push = |x: f32, y: f32, z: f32, c: &[f32; 3]| {
        v.extend_from_slice(&[x, y, z, c[0], c[1], c[2]]);
    };
    for i in 0..=size {
        let k = -half + i as f32;
        let c = if i * 2 == size { &CENTER } else { &LINE };
        push(-half * sx, 0.0, k * sz, c);
        push(half * sx, 0.0, k * sz, c);
        push(k * sx, 0.0, -half * sz, c);
        push(k * sx, 0.0, half * sz, c);
    }
    let grid_count = (size + 1) * 4;
    // Value axis at the back-left corner (drawn at opacity 0.8).
    let margin = (rows.max(cols) as f32 * 0.07).max(2.5);
    let ax = -(cols as f32) / 2.0 - margin;
    let az = -(rows as f32) / 2.0 - margin;
    const AXIS: [f32; 3] = [0x66 as f32 / 255.0, 0x6a as f32 / 255.0, 0x7a as f32 / 255.0];
    push(ax, -HEIGHT_SCALE, az, &AXIS);
    push(ax, HEIGHT_SCALE, az, &AXIS);
    (v, grid_count)
}

pub struct MetalWg3d {
    device: Device,
    queue: metal::CommandQueue,
    bars_pso: metal::RenderPipelineState,
    lines_pso: metal::RenderPipelineState,
    luma_pso: metal::RenderPipelineState,
    chroma_pso: metal::RenderPipelineState,
    depth_state: metal::DepthStencilState,
    box_vbo: metal::Buffer,
    box_ibo: metal::Buffer,
    tex_cache: CVMetalTextureCache,

    // Scene data, rebuilt only when the BarGrid generation changes.
    instance_buf: Option<metal::Buffer>,
    instance_cap: usize,
    instance_count: usize,
    lines_buf: Option<metal::Buffer>,
    grid_vertex_count: usize,
    uploaded_generation: u64,

    // Render targets, rebuilt only on resize.
    size: (usize, usize),
    msaa_tex: Option<metal::Texture>,
    depth_tex: Option<metal::Texture>,
    resolve_tex: Option<metal::Texture>,
    pool: Vec<CVPixelBuffer>,
    next: usize,
}

impl MetalWg3d {
    /// Build the device, pipelines, and static geometry. `None` when Metal is
    /// unavailable or the shaders fail to compile (CPU painter takes over).
    pub fn new() -> Option<Self> {
        let device = Device::system_default()?;
        let queue = device.new_command_queue();
        let library = device
            .new_library_with_source(MSL, &CompileOptions::new())
            .map_err(|e| eprintln!("[jade] wg3d metal shader compile failed: {e}"))
            .ok()?;

        let pso = |vs: &str, fs: &str, samples: u64, color: MTLPixelFormat, depth: bool, blend: bool| {
            let desc = RenderPipelineDescriptor::new();
            let vf = library.get_function(vs, None).ok()?;
            let ff = library.get_function(fs, None).ok()?;
            desc.set_vertex_function(Some(&vf));
            desc.set_fragment_function(Some(&ff));
            desc.set_sample_count(samples);
            let att = desc.color_attachments().object_at(0)?;
            att.set_pixel_format(color);
            if blend {
                att.set_blending_enabled(true);
                att.set_source_rgb_blend_factor(metal::MTLBlendFactor::One);
                att.set_destination_rgb_blend_factor(metal::MTLBlendFactor::OneMinusSourceAlpha);
                att.set_source_alpha_blend_factor(metal::MTLBlendFactor::One);
                att.set_destination_alpha_blend_factor(metal::MTLBlendFactor::OneMinusSourceAlpha);
            }
            if depth {
                desc.set_depth_attachment_pixel_format(MTLPixelFormat::Depth32Float);
            }
            device.new_render_pipeline_state(&desc).ok()
        };
        let bars_pso = pso("bars_vs", "bars_fs", 4, MTLPixelFormat::BGRA8Unorm, true, false)?;
        let lines_pso = pso("lines_vs", "lines_fs", 4, MTLPixelFormat::BGRA8Unorm, true, true)?;
        let luma_pso = pso("fsq_vs", "luma_fs", 1, MTLPixelFormat::R8Unorm, false, false)?;
        let chroma_pso = pso("fsq_vs", "chroma_fs", 1, MTLPixelFormat::RG8Unorm, false, false)?;

        let ds = DepthStencilDescriptor::new();
        ds.set_depth_compare_function(MTLCompareFunction::Less);
        ds.set_depth_write_enabled(true);
        let depth_state = device.new_depth_stencil_state(&ds);

        let (verts, indices) = box_geometry();
        let box_vbo = device.new_buffer_with_data(
            verts.as_ptr() as *const _,
            std::mem::size_of_val(&verts) as u64,
            MTLResourceOptions::StorageModeShared,
        );
        let box_ibo = device.new_buffer_with_data(
            indices.as_ptr() as *const _,
            std::mem::size_of_val(&indices) as u64,
            MTLResourceOptions::StorageModeShared,
        );

        let tex_cache = CVMetalTextureCache::new(None, device.clone(), None).ok()?;

        Some(MetalWg3d {
            device,
            queue,
            bars_pso,
            lines_pso,
            luma_pso,
            chroma_pso,
            depth_state,
            box_vbo,
            box_ibo,
            tex_cache,
            instance_buf: None,
            instance_cap: 0,
            instance_count: 0,
            lines_buf: None,
            grid_vertex_count: 0,
            uploaded_generation: u64::MAX,
            size: (0, 0),
            msaa_tex: None,
            depth_tex: None,
            resolve_tex: None,
            pool: Vec::new(),
            next: 0,
        })
    }

    /// Upload instance + line data for a new BarGrid (one `bufferSubData`
    /// equivalent per frame change, exactly like the TS `applyFrame`).
    fn upload_grid(&mut self, grid: &BarGrid, generation: u64) {
        if generation == self.uploaded_generation {
            return;
        }
        self.uploaded_generation = generation;
        self.instance_count = grid.bars.len();

        let mut inst: Vec<f32> = Vec::with_capacity(grid.bars.len() * 6);
        for b in &grid.bars {
            inst.extend_from_slice(&[b.x, b.z, b.height, b.r, b.g, b.b]);
        }
        let bytes = inst.len() * 4;
        match &self.instance_buf {
            Some(buf) if bytes <= self.instance_cap => unsafe {
                std::ptr::copy_nonoverlapping(
                    inst.as_ptr() as *const u8,
                    buf.contents() as *mut u8,
                    bytes,
                );
            },
            _ => {
                if bytes > 0 {
                    self.instance_buf = Some(self.device.new_buffer_with_data(
                        inst.as_ptr() as *const _,
                        bytes as u64,
                        MTLResourceOptions::StorageModeShared,
                    ));
                    self.instance_cap = bytes;
                }
            }
        }

        let (lines, grid_count) = line_geometry(grid.rows, grid.cols);
        self.grid_vertex_count = grid_count;
        self.lines_buf = Some(self.device.new_buffer_with_data(
            lines.as_ptr() as *const _,
            (lines.len() * 4) as u64,
            MTLResourceOptions::StorageModeShared,
        ));
    }

    /// (Re)build the render targets + pixel-buffer pool for a scene size.
    fn ensure_targets(&mut self, w: usize, h: usize) -> bool {
        // Biplanar 4:2:0 needs even dimensions.
        let w = (w.max(2) + 1) & !1;
        let h = (h.max(2) + 1) & !1;
        if self.size == (w, h) && !self.pool.is_empty() {
            return true;
        }

        let tex = |format, samples, usage| {
            let d = TextureDescriptor::new();
            d.set_texture_type(if samples > 1 {
                MTLTextureType::D2Multisample
            } else {
                MTLTextureType::D2
            });
            d.set_pixel_format(format);
            d.set_width(w as u64);
            d.set_height(h as u64);
            d.set_sample_count(samples);
            d.set_usage(usage);
            d.set_storage_mode(MTLStorageMode::Private);
            self.device.new_texture(&d)
        };
        self.msaa_tex = Some(tex(MTLPixelFormat::BGRA8Unorm, 4, MTLTextureUsage::RenderTarget));
        self.depth_tex = Some(tex(MTLPixelFormat::Depth32Float, 4, MTLTextureUsage::RenderTarget));
        self.resolve_tex = Some(tex(
            MTLPixelFormat::BGRA8Unorm,
            1,
            MTLTextureUsage::RenderTarget | MTLTextureUsage::ShaderRead,
        ));

        // IOSurface-backed, Metal-compatible biplanar pixel buffers.
        let mc_key: CFString = CVPixelBufferKeys::MetalCompatibility.into();
        let io_key: CFString = CVPixelBufferKeys::IOSurfaceProperties.into();
        let empty: CFDictionary<CFString, core_foundation::base::CFType> =
            CFDictionary::from_CFType_pairs(&[]);
        let attrs = CFDictionary::from_CFType_pairs(&[
            (mc_key, CFBoolean::true_value().as_CFType()),
            (io_key, empty.as_CFType()),
        ]);
        self.pool.clear();
        for _ in 0..POOL {
            match CVPixelBuffer::new(
                kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
                w,
                h,
                Some(&attrs),
            ) {
                Ok(pb) => self.pool.push(pb),
                Err(e) => {
                    eprintln!("[jade] wg3d CVPixelBuffer create failed: {e}");
                    self.pool.clear();
                    return false;
                }
            }
        }
        self.size = (w, h);
        self.next = 0;
        true
    }

    /// Render one frame and return the pixel buffer to hand to `surface()`.
    /// `w`/`h` are device pixels. `None` falls back to the CPU painter.
    pub fn render(
        &mut self,
        grid: &BarGrid,
        generation: u64,
        camera: &OrbitCamera,
        w: usize,
        h: usize,
    ) -> Option<CVPixelBuffer> {
        if !self.ensure_targets(w, h) {
            return None;
        }
        self.upload_grid(grid, generation);
        if self.instance_count == 0 {
            return None;
        }
        let (w, h) = self.size;

        let pb = self.pool[self.next].clone();
        self.next = (self.next + 1) % self.pool.len();

        // Plane textures live in the pixel buffer's IOSurface — rendering into
        // them IS filling the buffer gpui will draw.
        let y_plane = self
            .tex_cache
            .create_texture_from_image(
                pb.as_concrete_TypeRef(),
                None,
                MTLPixelFormat::R8Unorm,
                pb.get_width_of_plane(0),
                pb.get_height_of_plane(0),
                0,
            )
            .ok()?;
        let cbcr_plane = self
            .tex_cache
            .create_texture_from_image(
                pb.as_concrete_TypeRef(),
                None,
                MTLPixelFormat::RG8Unorm,
                pb.get_width_of_plane(1),
                pb.get_height_of_plane(1),
                1,
            )
            .ok()?;
        // NOT `CVMetalTexture::get_texture()`: it wraps the GET-rule (borrowed)
        // pointer as an owned `Texture`, whose drop over-releases and crashes.
        // Borrowed refs are correct — the CVMetalTextures keep them alive
        // through encoding (same pattern as gpui's own `draw_surfaces`).
        let y_tex = unsafe {
            let t = CVMetalTextureGetTexture(y_plane.as_concrete_TypeRef());
            if t.is_null() {
                return None;
            }
            metal::TextureRef::from_ptr(t as *mut _)
        };
        let cbcr_tex = unsafe {
            let t = CVMetalTextureGetTexture(cbcr_plane.as_concrete_TypeRef());
            if t.is_null() {
                return None;
            }
            metal::TextureRef::from_ptr(t as *mut _)
        };

        let uniforms = Uniforms {
            proj: perspective_metal(camera.fov_y, w as f32 / h as f32, 0.1, 4000.0),
            view: camera.view_matrix().0,
        };

        let cmd = self.queue.new_command_buffer();

        // ── Pass 1: bars + lines into MSAA color, resolved to BGRA ──
        {
            let desc = RenderPassDescriptor::new();
            let att = desc.color_attachments().object_at(0)?;
            att.set_texture(self.msaa_tex.as_deref());
            att.set_resolve_texture(self.resolve_tex.as_deref());
            att.set_load_action(MTLLoadAction::Clear);
            att.set_clear_color(MTLClearColor::new(CLEAR.0, CLEAR.1, CLEAR.2, 1.0));
            att.set_store_action(MTLStoreAction::MultisampleResolve);
            let depth = desc.depth_attachment()?;
            depth.set_texture(self.depth_tex.as_deref());
            depth.set_clear_depth(1.0);
            depth.set_load_action(MTLLoadAction::Clear);
            depth.set_store_action(MTLStoreAction::DontCare);

            let enc = cmd.new_render_command_encoder(&desc);
            enc.set_depth_stencil_state(&self.depth_state);

            enc.set_render_pipeline_state(&self.bars_pso);
            enc.set_vertex_buffer(0, Some(&self.box_vbo), 0);
            enc.set_vertex_buffer(1, self.instance_buf.as_deref(), 0);
            enc.set_vertex_bytes(
                2,
                std::mem::size_of::<Uniforms>() as u64,
                &uniforms as *const Uniforms as *const _,
            );
            enc.draw_indexed_primitives_instanced(
                MTLPrimitiveType::Triangle,
                36,
                MTLIndexType::UInt16,
                &self.box_ibo,
                0,
                self.instance_count as u64,
            );

            if let Some(lines) = &self.lines_buf {
                enc.set_render_pipeline_state(&self.lines_pso);
                enc.set_vertex_buffer(0, Some(lines), 0);
                enc.set_vertex_bytes(
                    2,
                    std::mem::size_of::<Uniforms>() as u64,
                    &uniforms as *const Uniforms as *const _,
                );
                let grid_opacity: f32 = 0.35;
                enc.set_fragment_bytes(0, 4, &grid_opacity as *const f32 as *const _);
                enc.draw_primitives(MTLPrimitiveType::Line, 0, self.grid_vertex_count as u64);
                let axis_opacity: f32 = 0.8;
                enc.set_fragment_bytes(0, 4, &axis_opacity as *const f32 as *const _);
                enc.draw_primitives(MTLPrimitiveType::Line, self.grid_vertex_count as u64, 2);
            }
            enc.end_encoding();
        }

        // ── Pass 2 + 3: RGB → Y plane, RGB → CbCr plane ──
        for (pso, target) in [(&self.luma_pso, y_tex), (&self.chroma_pso, cbcr_tex)] {
            let desc = RenderPassDescriptor::new();
            let att = desc.color_attachments().object_at(0)?;
            att.set_texture(Some(target));
            att.set_load_action(MTLLoadAction::DontCare);
            att.set_store_action(MTLStoreAction::Store);
            let enc = cmd.new_render_command_encoder(&desc);
            enc.set_render_pipeline_state(pso);
            enc.set_fragment_texture(0, self.resolve_tex.as_deref());
            enc.draw_primitives(MTLPrimitiveType::Triangle, 0, 3);
            enc.end_encoding();
        }

        cmd.commit();
        cmd.wait_until_completed();
        Some(pb)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::training::TensorFrame;
    use core_video::pixel_buffer::kCVPixelBufferLock_ReadOnly;

    /// End-to-end GPU smoke: compile the shaders, render a tiny grid, and read
    /// the Y plane back — the backdrop must show up as nonzero luma and the
    /// bars must vary it. Skips (passes) on machines without a Metal device.
    #[test]
    fn renders_a_frame_into_the_pixel_buffer() {
        let Some(mut gpu) = MetalWg3d::new() else {
            eprintln!("no Metal device — skipping");
            return;
        };
        let frame = TensorFrame {
            step: 1,
            rows: 8,
            cols: 8,
            src_rows: None,
            src_cols: None,
            data: (0..64).map(|i| (i as f32 - 32.0) / 32.0).collect(),
        };
        let grid = super::super::grid::build_bars(&frame).expect("bars");
        let camera = {
            let mut c = OrbitCamera::new();
            c.frame(8, 8);
            c
        };
        let pb = gpu
            .render(&grid, 1, &camera, 320, 240)
            .expect("metal render produced a pixel buffer");
        assert_eq!(
            pb.get_pixel_format(),
            kCVPixelFormatType_420YpCbCr8BiPlanarFullRange
        );
        assert_eq!((pb.get_width(), pb.get_height()), (320, 240));

        pb.lock_base_address(kCVPixelBufferLock_ReadOnly);
        let (min, max) = unsafe {
            let base = pb.get_base_address_of_plane(0) as *const u8;
            let stride = pb.get_bytes_per_row_of_plane(0);
            let mut min = u8::MAX;
            let mut max = u8::MIN;
            for row in 0..pb.get_height_of_plane(0) {
                for col in 0..pb.get_width_of_plane(0) {
                    let v = *base.add(row * stride + col);
                    min = min.min(v);
                    max = max.max(v);
                }
            }
            (min, max)
        };
        pb.unlock_base_address(kCVPixelBufferLock_ReadOnly);
        // Backdrop #111214 has luma ~18; bars (white→red/blue) reach far higher.
        assert!(max > 100, "expected bright bar pixels, max luma {max}");
        assert!(min > 0 && min < 40, "expected dark backdrop, min luma {min}");
    }
}
