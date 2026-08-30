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

struct ChromeFrameMaterial {
    size: vec2<f32>,
    corner_radius: f32,
    titlebar_bottom: f32,
    divider_thickness: f32,
    border_insets: vec4<f32>,
    titlebar_color: vec4<f32>,
    divider_color: vec4<f32>,
    border_color: vec4<f32>,
};

@group(#{MATERIAL_BIND_GROUP}) @binding(0) var<uniform> material: ChromeFrameMaterial;

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
    let outer_distance = rounded_rect_distance(local, material.size, material.corner_radius);
    let antialias_width = max(fwidth(outer_distance), 0.0001);
    let outer_coverage = 1.0 - smoothstep(-antialias_width, antialias_width, outer_distance);
    if outer_coverage <= 0.0 {
        discard;
    }

    var output_color = vec4<f32>(0.0);
    if local.y < material.titlebar_bottom {
        output_color = material.titlebar_color;
    }
    if material.divider_thickness > 0.0
        && local.y >= material.titlebar_bottom - material.divider_thickness
        && local.y < material.titlebar_bottom
    {
        output_color = material.divider_color;
    }

    if any(material.border_insets > vec4<f32>(0.0)) {
        let inner_origin = material.border_insets.xy;
        let inner_size = max(
            material.size
                - material.border_insets.xy
                - material.border_insets.zw,
            vec2<f32>(0.0),
        );
        let inner_position = local - inner_origin;
        let inner_radius = max(
            material.corner_radius
                - min(
                    min(material.border_insets.x, material.border_insets.y),
                    min(material.border_insets.z, material.border_insets.w),
                ),
            0.0,
        );
        let inner_distance = rounded_rect_distance(inner_position, inner_size, inner_radius);
        let inner_antialias_width = max(fwidth(inner_distance), 0.0001);
        let inner_coverage =
            1.0 - smoothstep(-inner_antialias_width, inner_antialias_width, inner_distance);
        // Source-over, NOT a 4-channel mix(). This material is submitted through
        // straight-alpha blending (SrcAlpha / OneMinusSrcAlpha), so interpolating
        // RGB toward the content colour would multiply by coverage a second time
        // at the blend stage. In the content region the frame is transparent
        // *black*, which would drag the antialiased edge to a dark seam. Weighting
        // by alpha keeps border RGB constant across the band and varies only alpha
        // -- the same discipline the outer edge follows with `a *= outer_coverage`.
        let border_alpha = material.border_color.a * (1.0 - inner_coverage);
        let content_alpha = output_color.a * (1.0 - border_alpha);
        let composited_alpha = border_alpha + content_alpha;
        let composited_rgb = select(
            vec3<f32>(0.0),
            (material.border_color.rgb * border_alpha + output_color.rgb * content_alpha)
                / composited_alpha,
            composited_alpha > 0.0,
        );
        output_color = vec4<f32>(composited_rgb, composited_alpha);
    }

    output_color.a *= outer_coverage;
    if output_color.a <= 0.0 {
        discard;
    }

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
