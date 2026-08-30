use bevy::{
    asset::{Asset, AssetPath, AssetServer, Assets, embedded_asset, embedded_path},
    color::{Color, ColorToComponents},
    math::{Vec2, Vec4},
    prelude::{App, Plugin},
    reflect::TypePath,
    render::render_resource::{AsBindGroup, AsBindGroupShaderType, ShaderType},
    shader::ShaderRef,
    sprite_render::{AlphaMode2d, Material2d, Material2dPlugin},
};

#[derive(Asset, AsBindGroup, Clone, Debug, PartialEq, TypePath)]
#[uniform(0, ChromeFrameUniform)]
pub(crate) struct ChromeFrameMaterial {
    pub(crate) size: Vec2,
    pub(crate) corner_radius: f32,
    pub(crate) titlebar_bottom: f32,
    pub(crate) divider_thickness: f32,
    pub(crate) border_insets: Vec4,
    pub(crate) titlebar_color: Color,
    pub(crate) divider_color: Color,
    pub(crate) border_color: Color,
}

#[derive(Clone, Copy, Default, ShaderType)]
struct ChromeFrameUniform {
    size: Vec2,
    corner_radius: f32,
    titlebar_bottom: f32,
    divider_thickness: f32,
    border_insets: Vec4,
    titlebar_color: Vec4,
    divider_color: Vec4,
    border_color: Vec4,
}

impl AsBindGroupShaderType<ChromeFrameUniform> for ChromeFrameMaterial {
    fn as_bind_group_shader_type(
        &self,
        _images: &bevy::render::render_asset::RenderAssets<bevy::render::texture::GpuImage>,
    ) -> ChromeFrameUniform {
        ChromeFrameUniform {
            size: self.size,
            corner_radius: self.corner_radius,
            titlebar_bottom: self.titlebar_bottom,
            divider_thickness: self.divider_thickness,
            border_insets: self.border_insets,
            titlebar_color: self.titlebar_color.to_linear().to_vec4(),
            divider_color: self.divider_color.to_linear().to_vec4(),
            border_color: self.border_color.to_linear().to_vec4(),
        }
    }
}

impl Material2d for ChromeFrameMaterial {
    fn vertex_shader() -> ShaderRef {
        ShaderRef::Path(
            AssetPath::from_path_buf(embedded_path!("chrome_frame_material.wgsl"))
                .with_source("embedded"),
        )
    }

    fn fragment_shader() -> ShaderRef {
        Self::vertex_shader()
    }

    fn alpha_mode(&self) -> AlphaMode2d {
        AlphaMode2d::Blend
    }
}

pub(crate) struct ChromeFrameMaterialPlugin;

impl Plugin for ChromeFrameMaterialPlugin {
    fn build(&self, app: &mut App) {
        if app.world().contains_resource::<AssetServer>() {
            embedded_asset!(app, "chrome_frame_material.wgsl");
            app.add_plugins(Material2dPlugin::<ChromeFrameMaterial>::default());
        } else {
            app.init_resource::<Assets<ChromeFrameMaterial>>();
        }
    }
}

#[cfg(test)]
mod tests {
    /// Rust mirror of the inner-border composite in `chrome_frame_material.wgsl`.
    ///
    /// The shader is the production code; this exists so the *algebra* has real
    /// coverage without a GPU. Keep the two in step — a divergence here is a
    /// silently wrong test, not a silently wrong border.
    fn composite_inner_border(
        border: [f32; 4],
        content: [f32; 4],
        inner_coverage: f32,
    ) -> [f32; 4] {
        let border_alpha = border[3] * (1.0 - inner_coverage);
        let content_alpha = content[3] * (1.0 - border_alpha);
        let composited_alpha = border_alpha + content_alpha;
        if composited_alpha <= 0.0 {
            return [0.0, 0.0, 0.0, 0.0];
        }
        let channel = |index: usize| {
            (border[index] * border_alpha + content[index] * content_alpha) / composited_alpha
        };
        [channel(0), channel(1), channel(2), composited_alpha]
    }

    /// The defect this pins: a 4-channel `mix()` toward the content colour is
    /// wrong for a straight-alpha pipeline. In the content region the frame is
    /// transparent *black*, so `mix` drags the antialiased edge's RGB to black
    /// and the blend stage multiplies by alpha a second time — a dark seam where
    /// the border meets client content. Source-over must hold border RGB
    /// constant across the coverage band and vary only alpha.
    #[test]
    fn inner_border_edge_over_transparent_content_keeps_border_rgb_and_only_fades_alpha() {
        let border = [0.8, 0.2, 0.2, 1.0];
        let transparent_content = [0.0, 0.0, 0.0, 0.0];

        for coverage in [0.0, 0.25, 0.5, 0.75, 0.99] {
            let composited = composite_inner_border(border, transparent_content, coverage);
            for channel in 0..3 {
                assert!(
                    (composited[channel] - border[channel]).abs() < 1e-6,
                    "coverage {coverage}: channel {channel} drifted to {} (naive mix would give {})",
                    composited[channel],
                    border[channel] * (1.0 - coverage),
                );
            }
            assert!((composited[3] - (1.0 - coverage)).abs() < 1e-6);
        }
    }

    /// A limiting case, deliberately NOT an F7 regression detector: over opaque
    /// content, source-over and the rejected four-channel `mix` are algebraically
    /// identical, so this test passes under either. It exists to pin the other
    /// end of the range — the titlebar interior must not change appearance — and
    /// the transparent-content test above is what actually catches a reversion.
    #[test]
    fn inner_border_composite_reduces_to_a_plain_lerp_over_opaque_content() {
        // Inside the titlebar the frame is opaque, so source-over must agree
        // with the simple interpolation the old hard threshold approximated.
        let border = [0.8, 0.2, 0.2, 1.0];
        let opaque_content = [0.1, 0.1, 0.12, 1.0];

        for coverage in [0.0, 0.5, 1.0] {
            let composited = composite_inner_border(border, opaque_content, coverage);
            for channel in 0..3 {
                let expected =
                    border[channel] * (1.0 - coverage) + opaque_content[channel] * coverage;
                assert!((composited[channel] - expected).abs() < 1e-6);
            }
            assert!((composited[3] - 1.0).abs() < 1e-6);
        }
    }

    #[test]
    fn inner_border_composite_is_defined_when_both_sides_are_transparent() {
        assert_eq!(
            composite_inner_border([0.8, 0.2, 0.2, 0.0], [0.0, 0.0, 0.0, 0.0], 0.5),
            [0.0, 0.0, 0.0, 0.0]
        );
    }

    /// Source scan, and only that: it cannot see what the GPU does, so it exists
    /// purely to stop the two forms the review already rejected from returning —
    /// the hard threshold with no coverage, and the straight-alpha-hostile
    /// 4-channel `mix`. Real proof is owed to the GPU pixel tests in the Phase 3
    /// closing smoke.
    #[test]
    fn inner_border_shader_keeps_derivative_coverage_and_avoids_the_rejected_forms() {
        let shader = include_str!("chrome_frame_material.wgsl");

        assert!(shader.contains("fwidth(inner_distance)"));
        assert!(
            !shader.contains("mix(material.border_color, output_color, inner_coverage)"),
            "4-channel mix is wrong for straight-alpha blending"
        );
        assert!(
            !shader
                .contains("rounded_rect_distance(inner_position, inner_size, inner_radius) >= 0.0"),
            "the hard SDF threshold stair-steps the inner corner"
        );
    }
}
