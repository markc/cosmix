use bevy::{
    asset::{Asset, AssetPath, AssetServer, Assets, Handle, embedded_asset, embedded_path},
    image::Image,
    math::{Affine2, Mat3, Rect, Vec2, primitives::Rectangle, vec2},
    mesh::{Mesh, Mesh2d},
    prelude::{App, Plugin, Resource},
    reflect::TypePath,
    render::{
        render_asset::RenderAssets,
        render_resource::{AsBindGroup, AsBindGroupShaderType, ShaderType},
        texture::GpuImage,
    },
    shader::ShaderRef,
    sprite_render::{AlphaMode2d, Material2d, Material2dPlugin},
};
use cosmix_wgpu_dmabuf::DmabufMaterial2dRegistrationExt;

const FLAG_FLIP_X: u32 = 1;
const FLAG_FLIP_Y: u32 = 1 << 1;
const FLAG_OPAQUE: u32 = 1 << 2;

/// An image whose UNORM bytes are implicit-sRGB and premultiplied in encoded space.
///
/// This is a review-enforced convention, not a capability boundary: construction
/// sites use the tag to distinguish client buffers from general-purpose Bevy images,
/// and [`ClientSurfaceMaterial`] owns the matching unpremultiply and transfer-function
/// contract, but the constructor cannot validate an arbitrary `Handle<Image>` and the
/// raw handle still has to escape for asset and DMA-BUF registry lifecycle operations.
/// Full enforcement would require one owner API to control image insertion, sampling,
/// replacement, and removal without exposing the underlying handle.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ClientSurfaceImage(Handle<Image>);

impl ClientSurfaceImage {
    pub(crate) fn encoded_premultiplied_unorm(image: Handle<Image>) -> Self {
        Self(image)
    }

    pub(crate) fn handle(&self) -> &Handle<Image> {
        &self.0
    }

    pub(crate) fn id(&self) -> bevy::asset::AssetId<Image> {
        self.0.id()
    }
}

#[derive(Asset, AsBindGroup, Clone, Debug, PartialEq, TypePath)]
#[uniform(0, ClientSurfaceUniform)]
pub(crate) struct ClientSurfaceMaterial {
    #[texture(1)]
    #[sampler(2)]
    image: Handle<Image>,
    pub(crate) flip_x: bool,
    pub(crate) flip_y: bool,
    pub(crate) custom_size: Vec2,
    pub(crate) source_rect: Option<Rect>,
    pub(crate) clip_from_uv: Mat3,
    pub(crate) clip_size: Vec2,
    pub(crate) corner_radius: f32,
    pub(crate) opaque: bool,
    pub(crate) alpha_mode: AlphaMode2d,
}

impl ClientSurfaceMaterial {
    pub(crate) fn new(image: &ClientSurfaceImage, opaque: bool) -> Self {
        Self {
            image: image.0.clone(),
            flip_x: false,
            flip_y: false,
            custom_size: Vec2::ZERO,
            source_rect: None,
            clip_from_uv: Mat3::IDENTITY,
            clip_size: Vec2::ZERO,
            corner_radius: 0.0,
            opaque,
            alpha_mode: if opaque {
                AlphaMode2d::Opaque
            } else {
                AlphaMode2d::Blend
            },
        }
    }

    pub(crate) fn set_rounded_clip(
        &mut self,
        clip_from_uv: Mat3,
        clip_size: Vec2,
        corner_radius: f32,
    ) {
        self.clip_from_uv = clip_from_uv;
        self.clip_size = clip_size;
        self.corner_radius = corner_radius.max(0.0);
        self.alpha_mode = if self.opaque && self.corner_radius == 0.0 {
            AlphaMode2d::Opaque
        } else {
            AlphaMode2d::Blend
        };
    }

    pub(crate) fn sampling_contract(&self) -> ClientSurfaceSamplingContract {
        ClientSurfaceSamplingContract {
            opaque: self.opaque,
            alpha_mode: self.alpha_mode,
            corner_radius: self.corner_radius,
        }
    }

    pub(crate) fn log_surface_sampling_contract(&self, surface_id: u64) {
        self.log_sampling_contract_fields(Some(surface_id), None);
    }

    pub(crate) fn log_cursor_sampling_contract(&self, cursor_id: u32) {
        self.log_sampling_contract_fields(None, Some(cursor_id));
    }

    fn log_sampling_contract_fields(&self, surface_id: Option<u64>, cursor_id: Option<u32>) {
        let blend = match self.alpha_mode {
            AlphaMode2d::Blend => "standard-alpha",
            AlphaMode2d::Opaque | AlphaMode2d::Mask(_) => "disabled",
        };
        tracing::info!(
            ?surface_id,
            ?cursor_id,
            opaque = self.opaque,
            alpha_mode = ?self.alpha_mode,
            blend,
            rounded_coverage = self.corner_radius > 0.0,
            corner_radius = self.corner_radius,
            "Client surface material sampling contract"
        );
    }

    #[cfg(test)]
    pub(crate) fn image_id(&self) -> bevy::asset::AssetId<Image> {
        self.image.id()
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ClientSurfaceSamplingContract {
    opaque: bool,
    alpha_mode: AlphaMode2d,
    corner_radius: f32,
}

#[derive(Clone, Copy, Default, ShaderType)]
struct ClientSurfaceUniform {
    uv_transform: Mat3,
    clip_from_uv: Mat3,
    size: Vec2,
    clip_size: Vec2,
    corner_radius: f32,
    flags: u32,
}

impl AsBindGroupShaderType<ClientSurfaceUniform> for ClientSurfaceMaterial {
    fn as_bind_group_shader_type(&self, images: &RenderAssets<GpuImage>) -> ClientSurfaceUniform {
        let Some(image) = images.get(self.image.id()) else {
            return ClientSurfaceUniform::default();
        };
        let image_size = image.size_2d().as_vec2();
        let mut uv_transform = Affine2::IDENTITY;
        if let Some(source) = self.source_rect {
            let source_size = source.size();
            uv_transform *= Affine2::from_scale(source_size / image_size);
            uv_transform *= Affine2::from_translation(vec2(
                source.min.x / source_size.x,
                source.min.y / source_size.y,
            ));
        }
        let mut flags = 0;
        if self.flip_x {
            flags |= FLAG_FLIP_X;
        }
        if self.flip_y {
            flags |= FLAG_FLIP_Y;
        }
        if self.opaque {
            flags |= FLAG_OPAQUE;
        }
        ClientSurfaceUniform {
            uv_transform: uv_transform.into(),
            clip_from_uv: self.clip_from_uv,
            size: self.custom_size,
            clip_size: self.clip_size,
            corner_radius: self.corner_radius,
            flags,
        }
    }
}

impl Material2d for ClientSurfaceMaterial {
    fn vertex_shader() -> ShaderRef {
        ShaderRef::Path(
            AssetPath::from_path_buf(embedded_path!("client_surface_material.wgsl"))
                .with_source("embedded"),
        )
    }

    fn fragment_shader() -> ShaderRef {
        Self::vertex_shader()
    }

    fn alpha_mode(&self) -> AlphaMode2d {
        self.alpha_mode
    }
}

#[derive(Resource)]
pub(crate) struct ClientSurfaceRenderAssets {
    pub(crate) quad: Handle<Mesh>,
}

impl ClientSurfaceRenderAssets {
    pub(crate) fn mesh(&self) -> Mesh2d {
        Mesh2d(self.quad.clone())
    }
}

pub(crate) struct ClientSurfaceMaterialPlugin;

impl Plugin for ClientSurfaceMaterialPlugin {
    fn build(&self, app: &mut App) {
        if app.world().contains_resource::<AssetServer>() {
            embedded_asset!(app, "client_surface_material.wgsl");
            app.add_plugins(Material2dPlugin::<ClientSurfaceMaterial>::default());
        } else {
            app.init_resource::<Assets<ClientSurfaceMaterial>>();
        }
        app.init_resource::<Assets<Mesh>>();
        let quad = app
            .world_mut()
            .resource_mut::<Assets<Mesh>>()
            .add(Rectangle::new(1.0, 1.0));
        app.insert_resource(ClientSurfaceRenderAssets { quad })
            .register_dmabuf_material_2d::<ClientSurfaceMaterial>();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, mpsc};

    use bevy::{
        DefaultPlugins,
        app::{PluginGroup, SubApp, TerminalCtrlCHandlerPlugin},
        asset::RenderAssetUsages,
        camera::{
            Camera, ManualTextureViewHandle, OrthographicProjection, Projection, RenderTarget,
            ScalingMode,
        },
        ecs::{
            schedule::{NodeId, ScheduleGraph, graph::Direction},
            system::{IntoSystem, System},
        },
        image::ImageSampler,
        log::LogPlugin,
        prelude::{Camera2d, ClearColor, Color, Msaa, Transform, Vec3},
        render::{
            Render, RenderApp, RenderPlugin,
            pipelined_rendering::PipelinedRenderingPlugin,
            render_asset::prepare_assets,
            renderer::{
                RenderAdapter, RenderAdapterInfo, RenderInstance, RenderQueue, WgpuWrapper,
            },
            settings::RenderCreation,
            texture::{ManualTextureView, ManualTextureViews},
        },
        sprite_render::PreparedMaterial2d,
        window::{ExitCondition, WindowPlugin},
        winit::WinitPlugin,
    };
    use cosmix_wgpu_dmabuf::DmabufImportPlugin;

    use super::*;

    const TARGET_WIDTH: u32 = 64;
    const TARGET_HEIGHT: u32 = 32;
    const TARGET_HANDLE: ManualTextureViewHandle = ManualTextureViewHandle(0x3b);
    const BACKGROUND: [u8; 4] = [8, 12, 16, 255];
    const NO_VULKAN_OPT_OUT: &str = "COSMIX_COMP_ALLOW_NO_VULKAN_TESTS";

    fn srgb_eotf(encoded: f32) -> f32 {
        if encoded <= 0.04045 {
            encoded / 12.92
        } else {
            ((encoded + 0.055) / 1.055).powf(2.4)
        }
    }

    fn linear_byte(encoded: u8) -> u8 {
        (srgb_eotf(f32::from(encoded) / 255.0) * 255.0).round() as u8
    }

    fn bgra_image(pixel: [u8; 4]) -> Image {
        let mut image = Image::new(
            bevy::render::render_resource::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
            bevy::render::render_resource::TextureDimension::D2,
            pixel.to_vec(),
            bevy::render::render_resource::TextureFormat::Bgra8Unorm,
            RenderAssetUsages::MAIN_WORLD | RenderAssetUsages::RENDER_WORLD,
        );
        image.sampler = ImageSampler::nearest();
        image
    }

    fn real_render_device() -> Option<(wgpu::Instance, wgpu::Adapter, wgpu::Device, wgpu::Queue)> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::VULKAN,
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });
        let adapter =
            bevy::tasks::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: None,
                force_fallback_adapter: false,
            }));
        let Ok(adapter) = adapter else {
            return unavailable_gpu_test(
                "ClientSurfaceMaterial GPU test found no usable headless Vulkan adapter",
            );
        };
        let adapter_info = adapter.get_info();
        let requested = bevy::tasks::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("ClientSurfaceMaterial offscreen regression device"),
            ..Default::default()
        }));
        let Ok((device, queue)) = requested else {
            return unavailable_gpu_test(&format!(
                "ClientSurfaceMaterial GPU test could not open adapter {} ({:?})",
                adapter_info.name, adapter_info.backend
            ));
        };
        eprintln!(
            "ClientSurfaceMaterial GPU test using {} ({:?})",
            adapter_info.name, adapter_info.backend
        );
        Some((instance, adapter, device, queue))
    }

    fn unavailable_gpu_test(
        reason: &str,
    ) -> Option<(wgpu::Instance, wgpu::Adapter, wgpu::Device, wgpu::Queue)> {
        if std::env::var_os(NO_VULKAN_OPT_OUT).is_some_and(|value| value == "1") {
            eprintln!(
                "ClientSurfaceMaterial GPU test explicitly opted out with {NO_VULKAN_OPT_OUT}=1: {reason}"
            );
            None
        } else {
            panic!(
                "{reason}; this pixel-level gate fails by default. Set {NO_VULKAN_OPT_OUT}=1 only on a genuinely GPU-less host"
            );
        }
    }

    fn read_target(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        texture: &wgpu::Texture,
    ) -> Vec<[u8; 4]> {
        let bytes_per_row = TARGET_WIDTH * 4;
        assert_eq!(bytes_per_row % wgpu::COPY_BYTES_PER_ROW_ALIGNMENT, 0);
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("ClientSurfaceMaterial offscreen readback"),
            size: u64::from(bytes_per_row * TARGET_HEIGHT),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("ClientSurfaceMaterial offscreen copy"),
        });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(bytes_per_row),
                    rows_per_image: Some(TARGET_HEIGHT),
                },
            },
            wgpu::Extent3d {
                width: TARGET_WIDTH,
                height: TARGET_HEIGHT,
                depth_or_array_layers: 1,
            },
        );
        queue.submit([encoder.finish()]);

        let slice = buffer.slice(..);
        let (sender, receiver) = mpsc::sync_channel(1);
        slice.map_async(wgpu::MapMode::Read, move |result| {
            sender
                .send(result)
                .expect("readback receiver remains alive");
        });
        device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("offscreen GPU completes its readback");
        receiver
            .recv()
            .expect("readback callback runs")
            .expect("readback buffer maps");
        let mapped = slice.get_mapped_range();
        let pixels = mapped
            .chunks_exact(4)
            .map(|pixel| [pixel[0], pixel[1], pixel[2], pixel[3]])
            .collect();
        drop(mapped);
        buffer.unmap();
        pixels
    }

    fn pixel(pixels: &[[u8; 4]], x: u32, y: u32) -> [u8; 4] {
        pixels[(y * TARGET_WIDTH + x) as usize]
    }

    fn assert_pixel_near(label: &str, actual: [u8; 4], expected: [u8; 4], tolerance: u8) {
        for channel in 0..4 {
            assert!(
                actual[channel].abs_diff(expected[channel]) <= tolerance,
                "{label} channel {channel}: actual {actual:?}, expected {expected:?} ±{tolerance}"
            );
        }
    }

    fn system_node<Out, Marker>(
        graph: &ScheduleGraph,
        system: impl IntoSystem<(), Out, Marker>,
    ) -> NodeId {
        let system_type = IntoSystem::into_system(system).system_type();
        graph
            .systems
            .iter()
            .find(|(_, candidate, _)| candidate.system_type() == system_type)
            .map(|(key, _, _)| NodeId::System(key))
            .expect("system is present in the render schedule")
    }

    fn has_configured_ordering(graph: &ScheduleGraph, before: NodeId, after: NodeId) -> bool {
        let dependency = graph.dependency().graph();
        let hierarchy = graph.hierarchy().graph();
        dependency.contains_edge(before, after)
            || hierarchy
                .neighbors_directed(before, Direction::Incoming)
                .any(|set| dependency.contains_edge(set, after))
            || hierarchy
                .neighbors_directed(after, Direction::Incoming)
                .any(|set| dependency.contains_edge(before, set))
    }

    #[test]
    fn production_client_surface_material_registers_dmabuf_prepare_ordering() {
        let mut app = App::new();
        let mut render_app = SubApp::new();
        render_app.init_schedule(Render);
        render_app.add_systems(
            Render,
            prepare_assets::<PreparedMaterial2d<ClientSurfaceMaterial>>,
        );
        app.insert_sub_app(RenderApp, render_app);
        app.add_plugins((DmabufImportPlugin, ClientSurfaceMaterialPlugin));

        let schedule = app
            .sub_app(RenderApp)
            .get_schedule(Render)
            .expect("Render schedule is installed");
        let graph = schedule.graph();
        let barrier_type = IntoSystem::into_system(
            cosmix_wgpu_dmabuf::dmabuf_material_prepare_barrier::<ClientSurfaceMaterial>,
        )
        .system_type();
        let barriers = graph
            .systems
            .iter()
            .filter(|(_, candidate, _)| candidate.system_type() == barrier_type)
            .map(|(key, _, _)| NodeId::System(key))
            .collect::<Vec<_>>();
        assert_eq!(
            barriers.len(),
            1,
            "production ClientSurfaceMaterialPlugin must register the real ClientSurfaceMaterial DMA-BUF prepare barrier",
        );
        let barrier = barriers[0];
        let prepare = system_node(
            graph,
            prepare_assets::<PreparedMaterial2d<ClientSurfaceMaterial>>,
        );
        assert!(
            has_configured_ordering(graph, barrier, prepare),
            "the real ClientSurfaceMaterial prepare must be ordered after the production DMA-BUF barrier",
        );
    }

    #[test]
    fn client_surface_material_renders_colour_alpha_and_rounded_coverage_on_a_real_gpu() {
        let Some((instance, adapter, device, queue)) = real_render_device() else {
            return;
        };
        let adapter_info = adapter.get_info();
        let render_creation = RenderCreation::manual(
            device.clone().into(),
            RenderQueue(Arc::new(WgpuWrapper::new(queue.clone()))),
            RenderAdapterInfo(WgpuWrapper::new(adapter_info)),
            RenderAdapter(Arc::new(WgpuWrapper::new(adapter))),
            RenderInstance(Arc::new(WgpuWrapper::new(instance))),
        );
        let render_plugin = RenderPlugin {
            render_creation,
            synchronous_pipeline_compilation: true,
            ..Default::default()
        };

        let mut app = App::new();
        app.add_plugins(
            DefaultPlugins
                .build()
                .disable::<LogPlugin>()
                .disable::<WinitPlugin>()
                .disable::<PipelinedRenderingPlugin>()
                .disable::<TerminalCtrlCHandlerPlugin>()
                .set(WindowPlugin {
                    primary_window: None,
                    exit_condition: ExitCondition::DontExit,
                    close_when_requested: false,
                    ..Default::default()
                })
                .set(render_plugin),
        )
        .add_plugins((DmabufImportPlugin, ClientSurfaceMaterialPlugin))
        .insert_resource(ClearColor(Color::linear_rgba(
            f32::from(BACKGROUND[0]) / 255.0,
            f32::from(BACKGROUND[1]) / 255.0,
            f32::from(BACKGROUND[2]) / 255.0,
            1.0,
        )));
        app.finish();
        app.cleanup();

        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("ClientSurfaceMaterial linear offscreen target"),
            size: wgpu::Extent3d {
                width: TARGET_WIDTH,
                height: TARGET_HEIGHT,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        app.world_mut().resource_mut::<ManualTextureViews>().insert(
            TARGET_HANDLE,
            ManualTextureView {
                texture_view: target
                    .create_view(&wgpu::TextureViewDescriptor::default())
                    .into(),
                size: bevy::math::UVec2::new(TARGET_WIDTH, TARGET_HEIGHT),
                view_format: wgpu::TextureFormat::Rgba8Unorm,
            },
        );
        app.world_mut().spawn((
            Camera2d,
            Camera::default(),
            Projection::Orthographic(OrthographicProjection {
                scaling_mode: ScalingMode::Fixed {
                    width: TARGET_WIDTH as f32,
                    height: TARGET_HEIGHT as f32,
                },
                ..OrthographicProjection::default_2d()
            }),
            RenderTarget::TextureView(TARGET_HANDLE),
            Msaa::Off,
        ));

        let opaque_image = app
            .world_mut()
            .resource_mut::<Assets<Image>>()
            .add(bgra_image([32, 128, 224, 51]));
        let translucent_image = app
            .world_mut()
            .resource_mut::<Assets<Image>>()
            .add(bgra_image([25, 50, 100, 128]));
        let zero_alpha_image = app
            .world_mut()
            .resource_mut::<Assets<Image>>()
            .add(bgra_image([200, 100, 50, 0]));
        let rounded_image = app
            .world_mut()
            .resource_mut::<Assets<Image>>()
            .add(bgra_image([100, 160, 220, 0]));

        let opaque_image = ClientSurfaceImage::encoded_premultiplied_unorm(opaque_image);
        let translucent_image = ClientSurfaceImage::encoded_premultiplied_unorm(translucent_image);
        let zero_alpha_image = ClientSurfaceImage::encoded_premultiplied_unorm(zero_alpha_image);
        let rounded_image = ClientSurfaceImage::encoded_premultiplied_unorm(rounded_image);

        let mut opaque = ClientSurfaceMaterial::new(&opaque_image, true);
        opaque.custom_size = Vec2::splat(4.0);
        let mut translucent = ClientSurfaceMaterial::new(&translucent_image, false);
        translucent.custom_size = Vec2::splat(4.0);
        let mut zero_alpha = ClientSurfaceMaterial::new(&zero_alpha_image, false);
        zero_alpha.custom_size = Vec2::splat(4.0);
        let mut rounded = ClientSurfaceMaterial::new(&rounded_image, true);
        rounded.custom_size = Vec2::splat(16.0);
        rounded.set_rounded_clip(
            Mat3::from_diagonal(Vec3::new(16.0, 16.0, 1.0)),
            Vec2::splat(16.0),
            6.0,
        );

        let opaque = app
            .world_mut()
            .resource_mut::<Assets<ClientSurfaceMaterial>>()
            .add(opaque);
        let translucent = app
            .world_mut()
            .resource_mut::<Assets<ClientSurfaceMaterial>>()
            .add(translucent);
        let zero_alpha = app
            .world_mut()
            .resource_mut::<Assets<ClientSurfaceMaterial>>()
            .add(zero_alpha);
        let rounded = app
            .world_mut()
            .resource_mut::<Assets<ClientSurfaceMaterial>>()
            .add(rounded);
        let quad = app.world().resource::<ClientSurfaceRenderAssets>().mesh();
        for (material, position) in [
            (opaque, Vec2::new(-24.0, 8.0)),
            (translucent, Vec2::new(-16.0, 8.0)),
            (zero_alpha, Vec2::new(-8.0, 8.0)),
            (rounded, Vec2::new(16.0, 0.0)),
        ] {
            app.world_mut().spawn((
                quad.clone(),
                bevy::sprite_render::MeshMaterial2d(material),
                Transform::from_xyz(position.x, position.y, 0.0),
                bevy::camera::visibility::NoFrustumCulling,
            ));
        }

        for _ in 0..12 {
            app.update();
        }
        device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("material render completes");
        let pixels = read_target(&device, &queue, &target);

        let opaque_pixel = pixel(&pixels, 8, 8);
        let opaque_expected = [linear_byte(224), linear_byte(128), linear_byte(32), 255];
        // 128 is encoded sRGB. The exact EOTF gives
        // ((128/255 + 0.055) / 1.055)^2.4 = 0.21586, or about byte 55 in this
        // deliberately linear Rgba8Unorm readback target (not encoded byte 128).
        assert!(
            opaque_pixel[1].abs_diff(55) <= 2 && opaque_pixel[1].abs_diff(128) > 50,
            "opaque sRGB midpoint was not decoded: {opaque_pixel:?}"
        );
        assert_pixel_near(
            "opaque asymmetric BGRA swatch",
            opaque_pixel,
            opaque_expected,
            2,
        );
        assert_eq!(
            opaque_pixel[3], 255,
            "opaque mode must ignore deliberately non-one source alpha"
        );

        let alpha = 128.0 / 255.0;
        let translucent_expected = [
            (srgb_eotf(100.0 / 128.0) * alpha * 255.0 + f32::from(BACKGROUND[0]) * (1.0 - alpha))
                .round() as u8,
            (srgb_eotf(50.0 / 128.0) * alpha * 255.0 + f32::from(BACKGROUND[1]) * (1.0 - alpha))
                .round() as u8,
            (srgb_eotf(25.0 / 128.0) * alpha * 255.0 + f32::from(BACKGROUND[2]) * (1.0 - alpha))
                .round() as u8,
            255,
        ];
        assert_pixel_near(
            "premultiplied translucent swatch over known background",
            pixel(&pixels, 16, 8),
            translucent_expected,
            3,
        );
        assert_pixel_near(
            "alpha-zero non-zero RGB swatch",
            pixel(&pixels, 24, 8),
            BACKGROUND,
            1,
        );

        let rounded_expected = [linear_byte(220), linear_byte(160), linear_byte(100), 255];
        assert_pixel_near(
            "rounded opaque interior",
            pixel(&pixels, 48, 16),
            rounded_expected,
            2,
        );
        let mut untouched = 0;
        let mut partial = 0;
        let mut partial_rgb_invariant = false;
        for y in 8..24 {
            for x in 40..56 {
                let sample = pixel(&pixels, x, y);
                if sample
                    .iter()
                    .zip(BACKGROUND)
                    .all(|(actual, expected)| actual.abs_diff(expected) <= 1)
                {
                    untouched += 1;
                }
                let foreground_delta = [
                    f32::from(rounded_expected[0]) - f32::from(BACKGROUND[0]),
                    f32::from(rounded_expected[1]) - f32::from(BACKGROUND[1]),
                    f32::from(rounded_expected[2]) - f32::from(BACKGROUND[2]),
                ];
                let sample_delta = [
                    f32::from(sample[0]) - f32::from(BACKGROUND[0]),
                    f32::from(sample[1]) - f32::from(BACKGROUND[1]),
                    f32::from(sample[2]) - f32::from(BACKGROUND[2]),
                ];
                let coverage = sample_delta
                    .iter()
                    .zip(foreground_delta)
                    .map(|(sample, foreground)| sample * foreground)
                    .sum::<f32>()
                    / foreground_delta
                        .iter()
                        .map(|channel| channel * channel)
                        .sum::<f32>();
                if (0.2..0.8).contains(&coverage) {
                    partial += 1;
                    let straight = [
                        (f32::from(BACKGROUND[0]) + sample_delta[0] / coverage).round() as u8,
                        (f32::from(BACKGROUND[1]) + sample_delta[1] / coverage).round() as u8,
                        (f32::from(BACKGROUND[2]) + sample_delta[2] / coverage).round() as u8,
                        255,
                    ];
                    let rgb_matches = straight
                        .iter()
                        .zip(rounded_expected)
                        .all(|(actual, expected)| actual.abs_diff(expected) <= 2);
                    assert!(
                        rgb_matches,
                        "partial-coverage pixel {sample:?} reconstructed straight RGB {straight:?}, expected full-coverage {rounded_expected:?} ±2"
                    );
                    partial_rgb_invariant = true;
                }
            }
        }
        assert!(
            untouched > 0,
            "rounded corners left no untouched exterior pixels"
        );
        assert!(
            partial > 0,
            "rounded opaque edge had no partial coverage; opaque alpha was likely forced after coverage"
        );
        assert!(
            partial_rgb_invariant,
            "no partial-coverage pixel preserved the full-coverage straight RGB; rounded coverage must multiply alpha only"
        );
    }
}
