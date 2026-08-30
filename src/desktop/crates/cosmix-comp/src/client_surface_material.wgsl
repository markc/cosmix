#import bevy_sprite::{
    mesh2d_functions as mesh_functions,
    mesh2d_vertex_output::VertexOutput,
    mesh2d_view_bindings::view,
}

#ifdef TONEMAP_IN_SHADER
#import bevy_core_pipeline::tonemapping
#endif
#import bevy_render::color_operations::srgb_to_linear
#ifdef SRGB_OUTPUT
#import bevy_render::color_operations::linear_to_srgb
#endif
#ifdef OKLAB_OUTPUT
#import bevy_render::color_operations::linear_rgb_to_oklab
#endif

struct Vertex {
    @builtin(instance_index) instance_index: u32,
#ifdef VERTEX_POSITIONS
    @location(0) position: vec3<f32>,
#endif
#ifdef VERTEX_NORMALS
    @location(1) normal: vec3<f32>,
#endif
#ifdef VERTEX_UVS
    @location(2) uv: vec2<f32>,
#endif
#ifdef VERTEX_TANGENTS
    @location(3) tangent: vec4<f32>,
#endif
#ifdef VERTEX_COLORS
    @location(4) color: vec4<f32>,
#endif
};

struct ClientSurfaceMaterial {
    uv_transform: mat3x3<f32>,
    clip_from_uv: mat3x3<f32>,
    size: vec2<f32>,
    clip_size: vec2<f32>,
    corner_radius: f32,
    flags: u32,
};

const FLAG_FLIP_X: u32 = 1u;
const FLAG_FLIP_Y: u32 = 2u;
const FLAG_OPAQUE: u32 = 4u;

@group(#{MATERIAL_BIND_GROUP}) @binding(0) var<uniform> material: ClientSurfaceMaterial;
@group(#{MATERIAL_BIND_GROUP}) @binding(1) var client_texture: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(2) var client_sampler: sampler;

@vertex
fn vertex(vertex: Vertex) -> VertexOutput {
    var out: VertexOutput;

#ifdef VERTEX_UVS
    out.uv = vertex.uv;
#endif

#ifdef VERTEX_POSITIONS
    var world_from_local = mesh_functions::get_world_from_local(vertex.instance_index);
    let position = vec4<f32>(vertex.position * vec3<f32>(material.size, 1.0), 1.0);
    out.world_position = mesh_functions::mesh2d_position_local_to_world(world_from_local, position);
    out.position = mesh_functions::mesh2d_position_world_to_clip(out.world_position);
#endif

#ifdef VERTEX_NORMALS
    out.world_normal = mesh_functions::mesh2d_normal_local_to_world(vertex.normal, vertex.instance_index);
#endif
#ifdef VERTEX_TANGENTS
    out.world_tangent = mesh_functions::mesh2d_tangent_local_to_world(
        world_from_local,
        vertex.tangent,
    );
#endif
#ifdef VERTEX_COLORS
    out.color = vertex.color;
#endif
    return out;
}

@fragment
fn fragment(mesh: VertexOutput) -> @location(0) vec4<f32> {
    var uv = mesh.uv;
    if (material.flags & FLAG_FLIP_X) != 0u {
        uv.x = 1.0 - uv.x;
    }
    if (material.flags & FLAG_FLIP_Y) != 0u {
        uv.y = 1.0 - uv.y;
    }
    uv = (material.uv_transform * vec3<f32>(uv, 1.0)).xy;

    // Phase 3b deliberately makes SHM and DMA-BUF share this UNORM path. Linear
    // sampling therefore interpolates encoded-sRGB, encoded-premultiplied bytes
    // before this shader unpremultiplies and applies the EOTF. The error is zero
    // at texel centres/nearest sampling and largest at fractional samples across
    // high-contrast edges (black/white halfway becomes 0.214 linear, not 0.5).
    // Phase 4 must convert each texel into a linear-premultiplied intermediate,
    // filter it, multiply both premultiplied RGB and alpha by rounded coverage,
    // then use One/OneMinusSrcAlpha blending. Although AlphaMode2d has no
    // Premultiplied variant in Bevy 0.19, Material2d::specialize can set that
    // blend state on the colour target.
    let sample = textureSample(client_texture, client_sampler, uv);
    var straight_srgb: vec3<f32>;
    var alpha: f32;
    if (material.flags & FLAG_OPAQUE) != 0u {
        straight_srgb = sample.rgb;
        alpha = 1.0;
    } else if sample.a > 0.0 {
        straight_srgb = clamp(sample.rgb / sample.a, vec3<f32>(0.0), vec3<f32>(1.0));
        alpha = sample.a;
    } else {
        straight_srgb = vec3<f32>(0.0);
        alpha = 0.0;
    }
    let straight_linear = srgb_to_linear(straight_srgb);

    if material.corner_radius > 0.0 {
        let window_position = (material.clip_from_uv * vec3<f32>(mesh.uv, 1.0)).xy;
        let radius = min(
            material.corner_radius,
            min(material.clip_size.x, material.clip_size.y) * 0.5,
        );
        let half_size = material.clip_size * 0.5;
        let q = abs(window_position - half_size) - half_size + vec2<f32>(radius);
        let distance = length(max(q, vec2<f32>(0.0))) + min(max(q.x, q.y), 0.0) - radius;
        let antialias_width = max(fwidth(distance), 0.0001);
        let coverage = 1.0 - smoothstep(-antialias_width, antialias_width, distance);
        if coverage <= 0.0 {
            discard;
        }
        alpha *= coverage;
    }
    var output_color = vec4<f32>(straight_linear, alpha);

#ifdef TONEMAP_IN_SHADER
    output_color = tonemapping::tone_mapping(output_color, view.color_grading);
#endif
#ifdef SRGB_OUTPUT
    output_color = vec4<f32>(linear_to_srgb(output_color.rgb), output_color.a);
#endif
#ifdef OKLAB_OUTPUT
    output_color = vec4<f32>(linear_rgb_to_oklab(output_color.rgb), output_color.a);
#endif

    return output_color;
}
