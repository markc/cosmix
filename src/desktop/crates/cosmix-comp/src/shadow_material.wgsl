#import bevy_sprite::{
    mesh2d_functions as mesh_functions,
    mesh2d_vertex_output::VertexOutput,
    mesh2d_view_bindings::view,
}

#ifdef TONEMAP_IN_SHADER
#import bevy_core_pipeline::tonemapping
#endif
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

struct ShadowMaterial {
    size: vec2<f32>,
    window_origin: vec2<f32>,
    window_size: vec2<f32>,
    corner_radius: f32,
    softness: f32,
    offset_y: f32,
    color: vec4<f32>,
};

@group(#{MATERIAL_BIND_GROUP}) @binding(0) var<uniform> material: ShadowMaterial;

@vertex
fn vertex(vertex: Vertex) -> VertexOutput {
    var out: VertexOutput;
#ifdef VERTEX_UVS
    out.uv = vertex.uv;
#endif
#ifdef VERTEX_POSITIONS
    let world_from_local = mesh_functions::get_world_from_local(vertex.instance_index);
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

fn rounded_rect_distance(position: vec2<f32>, size: vec2<f32>, radius: f32) -> f32 {
    let clamped_radius = min(radius, min(size.x, size.y) * 0.5);
    let half_size = size * 0.5;
    let q = abs(position - half_size) - half_size + vec2<f32>(clamped_radius);
    return length(max(q, vec2<f32>(0.0))) + min(max(q.x, q.y), 0.0) - clamped_radius;
}

@fragment
fn fragment(mesh: VertexOutput) -> @location(0) vec4<f32> {
    let local = mesh.uv * material.size;
    let shifted_origin = material.window_origin + vec2<f32>(0.0, material.offset_y);
    let distance = rounded_rect_distance(
        local - shifted_origin,
        material.window_size,
        material.corner_radius,
    );
    let softness = max(material.softness, 0.0001);
    let falloff = 1.0 - smoothstep(0.0, 1.0, clamp(distance / softness, 0.0, 1.0));
    if falloff <= 0.0 {
        discard;
    }

    var output_color = vec4<f32>(material.color.rgb, material.color.a * falloff);
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
