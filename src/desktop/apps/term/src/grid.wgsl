// One oversized triangle covering the render pass's viewport, which iced has
// already set to the shader widget's bounds — so no geometry uniform is needed
// and a resize costs no buffer write.

@group(0) @binding(0) var grid: texture_2d<f32>;
@group(0) @binding(1) var grid_sampler: sampler;

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> VertexOutput {
    // index 0,1,2 -> uv (0,0), (2,0), (0,2): the part with uv in 0..1 is
    // exactly the viewport, and the rest is clipped away.
    let uv = vec2<f32>(f32((index << 1u) & 2u), f32(index & 2u));
    var out: VertexOutput;
    out.position = vec4<f32>(uv * vec2<f32>(2.0, -2.0) + vec2<f32>(-1.0, 1.0), 0.0, 1.0);
    out.uv = uv;
    return out;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    return textureSample(grid, grid_sampler, in.uv);
}
