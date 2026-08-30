use bevy::{
    asset::{Asset, AssetPath, AssetServer, Assets, embedded_asset, embedded_path},
    color::{Color, ColorToComponents},
    math::Vec2,
    prelude::{App, Plugin},
    reflect::TypePath,
    render::render_resource::{AsBindGroup, AsBindGroupShaderType, ShaderType},
    shader::ShaderRef,
    sprite_render::{AlphaMode2d, Material2d, Material2dPlugin},
};

#[derive(Asset, AsBindGroup, Clone, Debug, PartialEq, TypePath)]
#[uniform(0, ShadowUniform)]
pub(crate) struct ShadowMaterial {
    pub(crate) size: Vec2,
    pub(crate) window_origin: Vec2,
    pub(crate) window_size: Vec2,
    pub(crate) corner_radius: f32,
    pub(crate) softness: f32,
    pub(crate) offset_y: f32,
    pub(crate) color: Color,
}

#[derive(Clone, Copy, Default, ShaderType)]
struct ShadowUniform {
    size: Vec2,
    window_origin: Vec2,
    window_size: Vec2,
    corner_radius: f32,
    softness: f32,
    offset_y: f32,
    color: bevy::math::Vec4,
}

impl AsBindGroupShaderType<ShadowUniform> for ShadowMaterial {
    fn as_bind_group_shader_type(
        &self,
        _images: &bevy::render::render_asset::RenderAssets<bevy::render::texture::GpuImage>,
    ) -> ShadowUniform {
        ShadowUniform {
            size: self.size,
            window_origin: self.window_origin,
            window_size: self.window_size,
            corner_radius: self.corner_radius,
            softness: self.softness,
            offset_y: self.offset_y,
            color: self.color.to_linear().to_vec4(),
        }
    }
}

impl Material2d for ShadowMaterial {
    fn vertex_shader() -> ShaderRef {
        ShaderRef::Path(
            AssetPath::from_path_buf(embedded_path!("shadow_material.wgsl"))
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

pub(crate) struct ShadowMaterialPlugin;

impl Plugin for ShadowMaterialPlugin {
    fn build(&self, app: &mut App) {
        if app.world().contains_resource::<AssetServer>() {
            embedded_asset!(app, "shadow_material.wgsl");
            app.add_plugins(Material2dPlugin::<ShadowMaterial>::default());
        } else {
            app.init_resource::<Assets<ShadowMaterial>>();
        }
    }
}
