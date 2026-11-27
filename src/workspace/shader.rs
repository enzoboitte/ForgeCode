//! Live preview of fragment shaders (raylib's `.fs`, OpenGL's `.frag`): the shader runs on the GPU
//! through wgpu over a full-screen rectangle, its uniforms are inputs, `uTime` plays and
//! `uResolution` follows the preview's size.
//!
//! wgpu reads Vulkan-style GLSL, so the shader is first rewritten: loose uniforms go into one
//! block, `sampler2D`s become a texture and a sampler (white), inputs and the output get their
//! locations. The image is flipped so that `gl_FragCoord` counts from the bottom, as in OpenGL.

use std::sync::OnceLock;

use anyhow::{Context as _, Result, anyhow, bail};

/// A uniform the user can set.
#[derive(Clone, Debug, PartialEq)]
pub struct Uniform {
    pub name: String,
    /// GLSL type: float, int, uint, bool, vec2..4, ivec2..4, mat4, sampler2D.
    pub ty: String,
    /// The comment after it on its line, shown as its help.
    pub comment: String,
    /// An array's size as written (`MAX_WALLS`, `8`), and its value (1 when not an array).
    pub array: Option<String>,
    pub len: usize,
}

impl Uniform {
    pub fn is_sampler(&self) -> bool {
        self.ty.starts_with("sampler")
    }

    /// Number of float components (0: not settable).
    pub fn components(&self) -> usize {
        match self.ty.as_str() {
            "float" | "int" | "uint" | "bool" => 1,
            "vec2" | "ivec2" => 2,
            "vec3" | "ivec3" => 3,
            "vec4" | "ivec4" => 4,
            "mat4" => 16,
            _ => 0,
        }
    }

    /// Set by the preview itself: time and resolution.
    pub fn automatic(&self) -> Option<Auto> {
        let n = self.name.to_ascii_lowercase();
        match (self.ty.as_str(), n.trim_start_matches(['u', 'i']).trim_start_matches('_')) {
            ("float", "time" | "globaltime") => Some(Auto::Time),
            ("vec2" | "vec3", "resolution" | "screensize" | "size") => Some(Auto::Resolution),
            _ => None,
        }
    }

    /// What it starts at: colors and `colDiffuse` white, matrices identity, the rest 0.
    pub fn default_value(&self) -> Vec<f32> {
        let n = self.components();
        if self.ty == "mat4" {
            return (0..16).map(|i| if i % 5 == 0 { 1. } else { 0. }).collect();
        }
        let lower = self.name.to_ascii_lowercase();
        let colorish = lower.contains("color") || lower.contains("colour") || lower == "coldiffuse" || lower.ends_with("col");
        vec![if colorish && n >= 3 { 1. } else { 0. }; n * self.len]
    }

    /// Settable with the preview's inputs (arrays and matrices are left at their default).
    pub fn editable(&self) -> bool {
        self.automatic().is_none() && !self.is_sampler() && self.array.is_none() && (1..=4).contains(&self.components())
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Auto {
    Time,
    Resolution,
}

/// `uniform vec4 uWalls[MAX_WALLS]; // comment` → (type, name, array size, comment).
fn uniform_line(line: &str) -> Option<(String, String, Option<String>, String)> {
    let re = regex::Regex::new(r"^\s*uniform\s+(?:(?:lowp|mediump|highp)\s+)?(\w+)\s+(\w+)\s*(?:\[\s*(\w+)\s*\])?\s*;\s*(?://\s*(.*))?$").unwrap();
    let c = re.captures(line)?;
    Some((c[1].to_string(), c[2].to_string(), c.get(3).map(|m| m.as_str().to_string()), c.get(4).map_or("", |m| m.as_str()).trim().to_string()))
}

/// The shader's uniforms, in order (array sizes read from their `#define`).
pub fn uniforms(src: &str) -> Vec<Uniform> {
    let size_of = |token: &str| -> usize {
        token.parse().ok().or_else(|| {
            let re = regex::Regex::new(&format!(r"(?m)^\s*#\s*define\s+{}\s+(\d+)", regex::escape(token))).ok()?;
            re.captures(src)?[1].parse().ok()
        }).unwrap_or(1)
    };
    src.lines()
        .filter_map(uniform_line)
        .map(|(ty, name, array, comment)| {
            let len = array.as_deref().map_or(1, size_of);
            Uniform { name, ty, comment, array, len }
        })
        .collect()
}

/// Vertex outputs the preview gives every fragment shader (raylib's names).
const VARYINGS: [(&str, &str, u32); 2] = [("fragTexCoord", "vec2", 0), ("fragColor", "vec4", 1)];

/// The shader as wgpu's GLSL front end takes it.
pub fn rewrite(src: &str) -> Result<String> {
    let all = uniforms(src);
    let mut head = vec!["#version 450".to_string()];
    // GLSL 1.x's texture2D(): texture() now (`texture2D` is the texture type in Vulkan GLSL).
    let legacy_call = regex::Regex::new(r"\btexture2D\s*\(").unwrap();
    // One block for the plain uniforms, where the first of them was: after the `#define`s their
    // array sizes use, and on one line so the shader's line numbers stay the same.
    let members: Vec<String> = all
        .iter()
        .filter(|u| !u.is_sampler())
        .map(|u| match &u.array {
            Some(size) => format!("{} {}[{size}];", u.ty, u.name),
            None => format!("{} {};", u.ty, u.name),
        })
        .collect();
    let mut block = (!members.is_empty()).then(|| format!("layout(set = 0, binding = 0) uniform ForgeParams {{ {} }};", members.join(" ")));
    let mut binding = 1;
    for s in all.iter().filter(|u| u.is_sampler()) {
        if s.ty != "sampler2D" {
            bail!("{}: only sampler2D textures are supported in the preview", s.name);
        }
        head.push(format!(
            "layout(set = 0, binding = {}) uniform texture2D {n}_forgeTex;\nlayout(set = 0, binding = {}) uniform sampler {n}_forgeSmp;\n#define {n} sampler2D({n}_forgeTex, {n}_forgeSmp)",
            binding,
            binding + 1,
            n = s.name
        ));
        binding += 2;
    }
    let mut body = vec![];
    let mut has_out = false;
    let input = regex::Regex::new(r"^\s*(?:in|varying)\s+(?:(?:lowp|mediump|highp|flat|smooth)\s+)*(\w+)\s+(\w+)\s*;").unwrap();
    let output = regex::Regex::new(r"^\s*out\s+(?:(?:lowp|mediump|highp)\s+)?(\w+)\s+(\w+)\s*;").unwrap();
    for line in src.lines() {
        if uniform_line(line).is_some_and(|(ty, ..)| !ty.starts_with("sampler")) {
            body.push(block.take().unwrap_or_default());
        } else if line.trim_start().starts_with("#version") || uniform_line(line).is_some() {
            body.push(String::new()); // keeps line numbers for errors
        } else if let Some(c) = input.captures(line) {
            let (ty, name) = (&c[1], &c[2]);
            body.push(match VARYINGS.iter().find(|(n, t, _)| *n == name && *t == ty) {
                Some((_, _, location)) => format!("layout(location = {location}) in {ty} {name};"),
                // Nothing provides it here: a constant zero.
                None => format!("const {ty} {name} = {ty}(0);"),
            });
        } else if let Some(c) = output.captures(line) {
            has_out = true;
            body.push(format!("layout(location = 0) out {} {};", &c[1], &c[2]));
        } else {
            body.push(legacy_call.replace_all(line, "texture(").into_owned());
        }
    }
    if !has_out {
        // GLSL 1.x writes gl_FragColor.
        head.push("layout(location = 0) out vec4 forge_FragColor;\n#define gl_FragColor forge_FragColor".into());
    }
    // The added lines shift the shader's: errors are mapped back by `source_line`.
    Ok(format!("{}\n{}\n", head.join("\n"), body.join("\n")))
}

/// Lines `rewrite` puts before the shader's own.
fn header_lines(rewritten: &str, src: &str) -> usize {
    rewritten.lines().count().saturating_sub(src.lines().count()).saturating_sub(0)
}

const VERTEX: &str = r#"
struct Out {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) color: vec4<f32>,
};
@vertex
fn main(@builtin(vertex_index) i: u32) -> Out {
    let p = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u)) * 2.0 - 1.0;
    var o: Out;
    o.pos = vec4<f32>(p, 0.0, 1.0);
    // Shown flipped (OpenGL's origin is at the bottom): the top of the picture is uv.y = 0.
    o.uv = vec2<f32>((p.x + 1.0) * 0.5, (p.y + 1.0) * 0.5);
    o.color = vec4<f32>(1.0);
    return o;
}
"#;

struct Gpu {
    device: wgpu::Device,
    queue: wgpu::Queue,
    vertex: wgpu::ShaderModule,
    white: wgpu::TextureView,
    sampler: wgpu::Sampler,
}

/// The GPU, opened once; None when there is none wgpu can use.
fn gpu() -> Result<&'static Gpu> {
    static GPU: OnceLock<Result<Gpu, String>> = OnceLock::new();
    GPU.get_or_init(|| open_gpu().map_err(|e| e.to_string())).as_ref().map_err(|e| anyhow!("{e}"))
}

fn open_gpu() -> Result<Gpu> {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
    let adapter = smol::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default())).context("no GPU for the shader preview")?;
    let (device, queue) = smol::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))?;
    let vertex = device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some("forge vertex"), source: wgpu::ShaderSource::Wgsl(VERTEX.into()) });
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("white"),
        size: wgpu::Extent3d { width: 1, height: 1, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    queue.write_texture(texture.as_image_copy(), &[255; 4], wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(4), rows_per_image: None }, wgpu::Extent3d { width: 1, height: 1, depth_or_array_layers: 1 });
    let white = texture.create_view(&Default::default());
    let sampler = device.create_sampler(&wgpu::SamplerDescriptor { mag_filter: wgpu::FilterMode::Linear, min_filter: wgpu::FilterMode::Linear, ..Default::default() });
    Ok(Gpu { device, queue, vertex, white, sampler })
}

/// A compiled shader, ready to draw.
pub struct Compiled {
    pipeline: wgpu::RenderPipeline,
    pub uniforms: Vec<Uniform>,
}

/// `message` with its line numbers (`:12:`) moved back to the shader's own lines.
fn shader_lines(message: &str, shift: usize) -> String {
    let re = regex::Regex::new(r":(\d+):").unwrap();
    re.replace_all(message, |c: &regex::Captures| format!(":{}:", c[1].parse::<usize>().unwrap_or(0).saturating_sub(shift).max(1))).into_owned()
}

/// Compile the shader; the error is the compiler's message.
pub fn compile(src: &str) -> Result<Compiled> {
    let gpu = gpu()?;
    let glsl = rewrite(src)?;
    let shift = header_lines(&glsl, src);
    let scope = gpu.device.push_error_scope(wgpu::ErrorFilter::Validation);
    let fragment = gpu.device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("preview"),
        source: wgpu::ShaderSource::Glsl { shader: glsl.into(), stage: wgpu::naga::ShaderStage::Fragment, defines: &[] },
    });
    let pipeline = gpu.device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("preview"),
        layout: None,
        vertex: wgpu::VertexState { module: &gpu.vertex, entry_point: Some("main"), compilation_options: Default::default(), buffers: &[] },
        fragment: Some(wgpu::FragmentState {
            module: &fragment,
            entry_point: Some("main"),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState { format: wgpu::TextureFormat::Rgba8Unorm, blend: Some(wgpu::BlendState::ALPHA_BLENDING), write_mask: wgpu::ColorWrites::ALL })],
        }),
        primitive: Default::default(),
        depth_stencil: None,
        multisample: Default::default(),
        multiview_mask: None,
        cache: None,
    });
    if let Some(e) = smol::block_on(scope.pop()) {
        bail!("{}", shader_lines(&e.to_string(), shift));
    }
    Ok(Compiled { pipeline, uniforms: uniforms(src) })
}

/// std140: where each uniform of the block goes, and the block's size.
fn std140(uniforms: &[Uniform]) -> (Vec<usize>, usize) {
    let mut offset = 0usize;
    let offsets = uniforms
        .iter()
        .map(|u| {
            let (align, size) = match (u.components(), u.array.is_some()) {
                // Array elements are 16 bytes apart (a mat4 takes 64).
                (n, true) => (16, if n > 4 { 64 } else { 16 } * u.len),
                (1, _) => (4, 4),
                (2, _) => (8, 8),
                (3, _) => (16, 12),
                (4, _) => (16, 16),
                _ => (16, 64),
            };
            offset = offset.div_ceil(align) * align;
            let at = offset;
            offset += size;
            at
        })
        .collect();
    (offsets, offset.div_ceil(16).max(1) * 16)
}

/// Draw one frame: `values` per uniform (as `Compiled::uniforms`), RGBA rows top first.
pub fn render(c: &Compiled, values: &[Vec<f32>], width: u32, height: u32) -> Result<Vec<u8>> {
    let gpu = gpu()?;
    let device = &gpu.device;
    let target = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("preview"),
        size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = target.create_view(&Default::default());
    // The uniform block, std140.
    let params: Vec<&Uniform> = c.uniforms.iter().filter(|u| !u.is_sampler()).collect();
    let owned: Vec<Uniform> = params.iter().map(|u| (*u).clone()).collect();
    let (offsets, size) = std140(&owned);
    let mut bytes = vec![0u8; size];
    for ((u, at), value) in params.iter().zip(&offsets).zip(c.uniforms.iter().zip(values).filter(|(u, _)| !u.is_sampler()).map(|(_, v)| v)) {
        let n = u.components().max(1);
        let stride = if u.array.is_some() { if n > 4 { 64 } else { 16 } } else { 0 };
        for (i, x) in value.iter().take(n * u.len).enumerate() {
            let word = if u.ty.starts_with('i') || u.ty == "int" || u.ty == "bool" { (*x as i32).to_le_bytes() } else if u.ty == "uint" { (*x as u32).to_le_bytes() } else { x.to_le_bytes() };
            let start = at + (i / n) * stride + (i % n) * 4;
            if let Some(slot) = bytes.get_mut(start..start + 4) {
                slot.copy_from_slice(&word);
            }
        }
    }
    let mut entries = vec![];
    let buffer = device.create_buffer(&wgpu::BufferDescriptor { label: None, size: size as u64, usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false });
    gpu.queue.write_buffer(&buffer, 0, &bytes);
    if !params.is_empty() {
        entries.push(wgpu::BindGroupEntry { binding: 0, resource: buffer.as_entire_binding() });
    }
    let samplers = c.uniforms.iter().filter(|u| u.is_sampler()).count() as u32;
    for i in 0..samplers {
        entries.push(wgpu::BindGroupEntry { binding: 1 + 2 * i, resource: wgpu::BindingResource::TextureView(&gpu.white) });
        entries.push(wgpu::BindGroupEntry { binding: 2 + 2 * i, resource: wgpu::BindingResource::Sampler(&gpu.sampler) });
    }
    let scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
    // Unused uniforms are left out by the compiler: only what the shader reads is bound.
    let bind_group = (!entries.is_empty()).then(|| {
        let layout = c.pipeline.get_bind_group_layout(0);
        device.create_bind_group(&wgpu::BindGroupDescriptor { label: None, layout: &layout, entries: &entries })
    });
    let row = (width * 4).div_ceil(256) * 256;
    let readback = device.create_buffer(&wgpu::BufferDescriptor { label: None, size: (row * height) as u64, usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ, mapped_at_creation: false });
    let mut encoder = device.create_command_encoder(&Default::default());
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: None,
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT), store: wgpu::StoreOp::Store },
            })],
            ..Default::default()
        });
        pass.set_pipeline(&c.pipeline);
        if let Some(group) = &bind_group {
            pass.set_bind_group(0, group, &[]);
        }
        pass.draw(0..3, 0..1);
    }
    encoder.copy_texture_to_buffer(
        target.as_image_copy(),
        wgpu::TexelCopyBufferInfo { buffer: &readback, layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(row), rows_per_image: None } },
        wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
    );
    gpu.queue.submit([encoder.finish()]);
    if let Some(e) = smol::block_on(scope.pop()) {
        bail!("{e}");
    }
    readback.slice(..).map_async(wgpu::MapMode::Read, |_| {});
    device.poll(wgpu::PollType::wait_indefinitely()).map_err(|e| anyhow!("{e}"))?;
    let data = readback.slice(..).get_mapped_range().map_err(|e| anyhow!("{e:?}"))?;
    // Bottom row first, as OpenGL draws it.
    let mut out = Vec::with_capacity((width * height * 4) as usize);
    for y in (0..height).rev() {
        let start = (y * row) as usize;
        out.extend_from_slice(&data[start..start + (width * 4) as usize]);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const VIGNETTE: &str = "#version 330\nin vec2 fragTexCoord;\nin vec4 fragColor;\nout vec4 finalColor;\nuniform vec2  uResolution;   // taille de la fenêtre\nuniform float uIntensity;    // 0 = aucun rouge\nuniform sampler2D texture0;\nvoid main() {\n    vec2 p = gl_FragCoord.xy / uResolution;\n    vec4 t = texture(texture0, fragTexCoord);\n    finalColor = vec4(uIntensity * t.r, p.y, 0.0, 1.0);\n}\n";

    #[test]
    fn reads_uniforms_and_rewrites_for_wgpu() {
        let u = uniforms(VIGNETTE);
        assert_eq!(u.iter().map(|u| (u.ty.as_str(), u.name.as_str())).collect::<Vec<_>>(), [("vec2", "uResolution"), ("float", "uIntensity"), ("sampler2D", "texture0")]);
        assert_eq!(u[0].comment, "taille de la fenêtre");
        assert_eq!(u[0].automatic(), Some(Auto::Resolution));
        let glsl = rewrite(VIGNETTE).unwrap();
        assert!(glsl.contains("uniform ForgeParams { vec2 uResolution; float uIntensity; };"), "{glsl}");
        // Arrays keep their #define'd size, read for the buffer layout.
        let walls = uniforms("#define MAX 3\nuniform vec4 uWalls[MAX];\nuniform float uA[MAX];\n");
        assert_eq!((walls[0].len, walls[0].editable()), (3, false));
        assert_eq!(std140(&walls), (vec![0, 48], 96));
        assert!(glsl.contains("layout(location = 0) in vec2 fragTexCoord;") && glsl.contains("layout(location = 0) out vec4 finalColor;"));
        assert_eq!(std140(&u[..2]), (vec![0, 8], 16));
    }

    /// On the GPU: the vignette draws, red by its intensity, green rising to the top like OpenGL.
    #[test]
    fn renders_a_raylib_shader() {
        if gpu().is_err() {
            return eprintln!("no GPU here: skipped");
        }
        let c = compile(VIGNETTE).unwrap();
        let (w, h) = (8, 4);
        let px = render(&c, &[vec![w as f32, h as f32], vec![1.0], vec![]], w, h).unwrap();
        let at = |x: u32, y: u32| &px[((y * w + x) * 4) as usize..((y * w + x) * 4 + 4) as usize];
        assert!(at(0, 0)[0] > 250, "red from uIntensity: {:?}", at(0, 0));
        // gl_FragCoord.y grows upward: the top row (shown first) is the greenest.
        assert!(at(0, 0)[1] > at(0, h - 1)[1], "top {:?} bottom {:?}", at(0, 0), at(0, h - 1));
        // A mistake is reported on the shader's own line.
        let err = compile("#version 330\nout vec4 finalColor;\nvoid main() {\n    finalColor = vec4(nope);\n}\n").err().unwrap().to_string();
        assert!(err.contains("nope"), "{err}");
    }

    /// The user's own shaders (run by hand).
    #[test]
    #[ignore]
    fn compiles_the_sigmasix_shaders() {
        let dir = dirs::home_dir().unwrap().join("Documents/cpp/sigmasix/assets/shaders");
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let src = std::fs::read_to_string(entry.path()).unwrap();
            match compile(&src) {
                Ok(c) => {
                    eprintln!("OK {:?}: {:?}", entry.file_name(), c.uniforms.iter().map(|u| &u.name).collect::<Vec<_>>());
                    // A frame to look at: sizes and time set, the rest at mid values.
                    let values: Vec<Vec<f32>> = c.uniforms.iter().map(|u| match u.automatic() {
                        Some(Auto::Time) => vec![1.0],
                        Some(Auto::Resolution) => vec![320., 180.],
                        None if u.editable() && u.components() == 1 => vec![if u.name.to_lowercase().contains("radius") { 60. } else { 0.8 }],
                        None if u.editable() && u.components() == 2 => vec![160., 90.],
                        None => { let mut v = u.default_value(); if u.components() == 4 { v[3] = 1.; } v }
                    }).collect();
                    if let (Ok(px), Ok(out)) = (render(&c, &values, 320, 180), std::env::var("FORGE_SHADER_OUT")) {
                        let name = entry.path().file_stem().unwrap().to_string_lossy().to_string();
                        image::RgbaImage::from_raw(320, 180, px).unwrap().save(std::path::Path::new(&out).join(format!("{name}.png"))).unwrap();
                    }
                }
                Err(e) => eprintln!("FAIL {:?}: {e}", entry.file_name()),
            }
        }
    }
}

