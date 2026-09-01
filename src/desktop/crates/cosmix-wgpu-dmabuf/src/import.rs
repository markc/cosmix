use std::{
    collections::{HashMap, HashSet},
    mem,
    os::fd::AsRawFd,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use ash::vk;
use bevy::{
    asset::{AssetEvent, AssetId, Assets, Handle, RenderAssetUsages},
    image::Image,
    prelude::{App, IntoScheduleConfigs, Plugin, Res, ResMut, Resource, SystemSet},
    render::{
        Render, RenderApp, RenderSystems,
        extract_resource::{ExtractResource, ExtractResourcePlugin},
        render_asset::{RenderAssets, prepare_assets},
        render_resource::{Texture, TextureView},
        renderer::{RenderDevice, RenderQueue},
        texture::GpuImage,
    },
    sprite_render::{Material2d, PreparedMaterial2d, SpriteAssetEvents, SpriteMaterial},
};
use thiserror::Error;
use tracing::{error, info};
use wgpu_hal::{
    MemoryFlags, TextureDescriptor as HalTextureDescriptor, api::Vulkan, vulkan::TextureMemory,
};
use wgpu_types::TextureUses;

use crate::{
    DmabufBufferId, DmabufDescriptor, ReleaseCallback,
    formats::{drm_to_vulkan, is_opaque, modifier_properties, vulkan_to_wgpu},
};

const MAX_CACHED_IMPORTS: usize = 64;
const MAX_CACHED_IMPORT_BYTES: usize = 512 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum ImportError {
    #[error("DMA-BUF dimensions must be non-zero")]
    InvalidDimensions,
    #[error("DMA-BUF has no planes")]
    NoPlanes,
    #[error("unsupported DRM fourcc {0:#010x}")]
    UnsupportedFourcc(u32),
    #[error("modifier {modifier:#018x} is unavailable for DRM fourcc {fourcc:#010x}")]
    UnsupportedModifier { fourcc: u32, modifier: u64 },
    #[error("modifier requires {expected} planes, buffer supplied {actual}")]
    PlaneCount { expected: u32, actual: usize },
    #[error("multi-plane modifier is not disjoint-importable")]
    NonDisjointMultiPlane,
    #[error("render device is not Vulkan")]
    NotVulkan,
    #[error("DMA-BUF image asset is not registered")]
    UnregisteredImage,
    #[error("no compatible Vulkan memory type")]
    NoMemoryType,
    #[error(
        "Vulkan created DMA-BUF image with modifier {created:#018x}, client supplied {requested:#018x}"
    )]
    CreatedModifierMismatch { requested: u64, created: u64 },
    #[error("Vulkan operation failed: {0}")]
    Vulkan(#[from] vk::Result),
}

struct ReleaseOnDrop(Option<ReleaseCallback>, Arc<AtomicBool>);

impl ReleaseOnDrop {
    fn new(callback: ReleaseCallback, publication_suppressed: Arc<AtomicBool>) -> Self {
        Self(Some(callback), publication_suppressed)
    }
}

impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        if let Some(callback) = self.0.take()
            && !self.1.load(Ordering::Acquire)
        {
            callback();
        }
    }
}

/// Selects which renderer boundary returns a protocol ownership token.
pub enum DmabufRelease {
    /// Preserve the implicit-sync path: release after the retired use's GPU
    /// work completes and queue ownership has returned to the client.
    Implicit(ReleaseCallback),
    /// Signal the explicit release point after render-world state can no longer
    /// submit this use and queue ownership has returned to the client.
    Explicit(ReleaseCallback),
}

enum ReleaseLease {
    Physical(ReleaseOnDrop),
    Logical(ReleaseOnDrop),
}

impl ReleaseLease {
    #[cfg(test)]
    fn new(release: DmabufRelease) -> Self {
        Self::with_teardown_latch(release, Arc::default())
    }

    fn with_teardown_latch(
        release: DmabufRelease,
        publication_suppressed: Arc<AtomicBool>,
    ) -> Self {
        match release {
            DmabufRelease::Implicit(callback) => Self::Physical(ReleaseOnDrop::new(
                callback,
                Arc::clone(&publication_suppressed),
            )),
            DmabufRelease::Explicit(callback) => {
                Self::Logical(ReleaseOnDrop::new(callback, publication_suppressed))
            }
        }
    }

    fn split(self) -> (Option<ReleaseOnDrop>, Option<ReleaseOnDrop>) {
        match self {
            Self::Physical(release) => (Some(release), None),
            Self::Logical(release) => (None, Some(release)),
        }
    }
}

struct PendingImport<T = ImportedTexture> {
    buffer_id: DmabufBufferId,
    descriptor: DmabufDescriptor,
    release: ReleaseLease,
    previous: Option<T>,
    cacheable: bool,
}

struct CachedTexture {
    texture: Texture,
    texture_view: TextureView,
    descriptor: wgpu::TextureDescriptor<'static>,
    probe: Option<DmabufProbeIdentity>,
}

#[derive(Clone, Copy)]
struct DmabufProbeIdentity {
    buffer_id: DmabufBufferId,
    import_number: u64,
    rows: [u32; 3],
    width: u32,
}

#[derive(Clone, Copy, Default)]
struct DmabufDebugOptions {
    no_cache: bool,
    probe: bool,
    log_imports: bool,
}

#[derive(Default)]
struct DmabufDebugState {
    options: DmabufDebugOptions,
    import_counts: HashMap<DmabufBufferId, u64>,
    logged_buffers: HashSet<DmabufBufferId>,
    logged_formats: HashSet<u32>,
    pending_sample_probes: Vec<Arc<CachedTexture>>,
    pending_output_probe: bool,
}

#[derive(Clone, Copy)]
struct ImportInstrumentation {
    buffer_id: DmabufBufferId,
    import_number: u64,
    log_metadata: bool,
    log_format_metadata: bool,
    probe: bool,
}

impl Default for ImportInstrumentation {
    fn default() -> Self {
        Self {
            buffer_id: DmabufBufferId(0),
            import_number: 0,
            log_metadata: false,
            log_format_metadata: false,
            probe: false,
        }
    }
}

struct ImportedUse<T> {
    backing: Arc<T>,
    _physical_release: Option<ReleaseOnDrop>,
    _logical_release: Option<ReleaseOnDrop>,
}

impl<T> ImportedUse<T> {
    fn new(backing: Arc<T>, release: ReleaseLease) -> Self {
        let (physical_release, logical_release) = release.split();
        Self {
            backing,
            _physical_release: physical_release,
            _logical_release: logical_release,
        }
    }
}

type ImportedTexture = ImportedUse<CachedTexture>;

struct ReadyImport<T> {
    current: T,
    previous: Option<T>,
    newly_imported: bool,
    probe_after_acquire: bool,
}

type PendingImportResult<T, E> = Result<ReadyImport<ImportedUse<T>>, (E, Option<ImportedUse<T>>)>;

enum ImportState<T = ImportedTexture> {
    Idle,
    Pending(PendingImport<T>),
    Imported(ReadyImport<T>),
    Applied(T),
}

struct CacheEntry<T> {
    backing: Arc<T>,
    bytes: usize,
    last_used: u64,
}

struct ImportCache<T> {
    entries: HashMap<DmabufBufferId, CacheEntry<T>>,
    bytes: usize,
    clock: u64,
    max_entries: usize,
    max_bytes: usize,
}

impl<T> Default for ImportCache<T> {
    fn default() -> Self {
        Self::with_limits(MAX_CACHED_IMPORTS, MAX_CACHED_IMPORT_BYTES)
    }
}

impl<T> ImportCache<T> {
    fn with_limits(max_entries: usize, max_bytes: usize) -> Self {
        Self {
            entries: HashMap::new(),
            bytes: 0,
            clock: 0,
            max_entries,
            max_bytes,
        }
    }

    fn next_tick(&mut self) -> u64 {
        self.clock = self.clock.wrapping_add(1);
        self.clock
    }

    fn get(&mut self, buffer_id: DmabufBufferId) -> Option<Arc<T>> {
        let tick = self.next_tick();
        let entry = self.entries.get_mut(&buffer_id)?;
        entry.last_used = tick;
        Some(Arc::clone(&entry.backing))
    }

    fn contains(&self, buffer_id: DmabufBufferId) -> bool {
        self.entries.contains_key(&buffer_id)
    }

    fn insert(&mut self, buffer_id: DmabufBufferId, backing: Arc<T>, bytes: usize) -> bool {
        if self.max_entries == 0 || bytes > self.max_bytes {
            return false;
        }
        while self.entries.len() >= self.max_entries
            || self.bytes.saturating_add(bytes) > self.max_bytes
        {
            let Some(eviction) = self
                .entries
                .iter()
                .filter(|(_, entry)| Arc::strong_count(&entry.backing) == 1)
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(buffer_id, _)| *buffer_id)
            else {
                // Every cached backing is in flight. Keep the cache within its
                // hard budget by leaving this import uncached; a later use can
                // retry once an older entry becomes evictable.
                return false;
            };
            self.remove(eviction);
        }
        let tick = self.next_tick();
        if let Some(replaced) = self.entries.insert(
            buffer_id,
            CacheEntry {
                backing,
                bytes,
                last_used: tick,
            },
        ) {
            self.bytes = self.bytes.saturating_sub(replaced.bytes);
        }
        self.bytes = self.bytes.saturating_add(bytes);
        true
    }

    fn remove(&mut self, buffer_id: DmabufBufferId) -> Option<Arc<T>> {
        let removed = self.entries.remove(&buffer_id)?;
        self.bytes = self.bytes.saturating_sub(removed.bytes);
        Some(removed.backing)
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.bytes = 0;
    }

    fn retain(&mut self, mut keep: impl FnMut(&Arc<T>) -> bool) {
        let mut bytes = self.bytes;
        self.entries.retain(|_, entry| {
            if keep(&entry.backing) {
                true
            } else {
                bytes = bytes.saturating_sub(entry.bytes);
                false
            }
        });
        self.bytes = bytes;
    }
}

#[derive(Default)]
struct ImportRegistry {
    active: HashMap<AssetId<Image>, ImportState>,
    retired: HashMap<AssetId<Image>, Vec<ImportedTexture>>,
    cache: ImportCache<CachedTexture>,
    ever_imported: bool,
    preacquired: HashSet<usize>,
    local_owned: HashSet<usize>,
    ownership_retired: Vec<ImportedTexture>,
    debug: DmabufDebugState,
}

fn invalidate_buffer_cache<T>(
    cache: &mut ImportCache<T>,
    active: &mut HashMap<AssetId<Image>, ImportState<ImportedUse<T>>>,
    buffer_id: DmabufBufferId,
) {
    cache.remove(buffer_id);
    for state in active.values_mut() {
        if let ImportState::Pending(pending) = state
            && pending.buffer_id == buffer_id
        {
            pending.cacheable = false;
        }
    }
}

fn invalidate_all_buffer_caches<T>(
    cache: &mut ImportCache<T>,
    active: &mut HashMap<AssetId<Image>, ImportState<ImportedUse<T>>>,
) {
    cache.clear();
    for state in active.values_mut() {
        if let ImportState::Pending(pending) = state {
            pending.cacheable = false;
        }
    }
}

fn evict_cache_backings<T>(cache: &mut ImportCache<T>, backings: &[Arc<T>]) {
    let evicted = backings
        .iter()
        .map(|backing| Arc::as_ptr(backing) as usize)
        .collect::<HashSet<_>>();
    cache.retain(|backing| !evicted.contains(&(Arc::as_ptr(backing) as usize)));
}

/// Main/render-world shared import registry keyed by Bevy image asset ID.
#[derive(Clone, Default, ExtractResource, Resource)]
pub struct ImportedDmabufImages(Arc<Mutex<ImportRegistry>>, Arc<AtomicBool>);

/// Cloneable Vulkan-side validator for linux-dmabuf parameter creation.
///
/// The compositor probes with this before reporting a linux-dmabuf import as
/// successful, on a dedicated validation thread rather than on the protocol
/// thread — the probe touches Vulkan and must not stall client dispatch. The
/// probe creates, binds, then immediately destroys an image from duplicated
/// plane FDs; render-time import still owns its own duplicates.
#[derive(Clone)]
pub struct DmabufValidator(RenderDevice);

/// Renderer-specific seam used by the DMA-BUF validation worker and offline
/// fakes.
///
/// The production implementor is [`DmabufValidator`], which needs a Vulkan
/// `RenderDevice` and so cannot exist without a GPU. Naming the capability
/// instead lets the compositor's validation worker be exercised offline — the
/// worker's own success and rejection arms, its queue bound, and the invariant
/// that the probe never runs on the protocol thread are all properties of the
/// worker rather than of Vulkan.
///
/// `&mut self` mirrors [`WaitForSubmittedWork`](crate::WaitForSubmittedWork):
/// the worker owns its probe exclusively, so shared access would buy nothing
/// while forcing scripted fakes into interior mutability.
///
/// The error is a `String` because no consumer branches on it — the worker
/// logs it and refuses the buffer. [`ImportError`] is deliberately not used:
/// it also spans render-time import failures such as
/// [`ImportError::UnregisteredImage`], which validation cannot produce.
pub trait ValidateDmabuf: Send + 'static {
    /// Probe one buffer's parameters. `Ok(())` means the compositor may report
    /// the import successful to the client.
    fn validate(&mut self, descriptor: DmabufDescriptor) -> Result<(), String>;
}

impl ValidateDmabuf for DmabufValidator {
    fn validate(&mut self, descriptor: DmabufDescriptor) -> Result<(), String> {
        // The inherent method takes `&self`; this resolves to it, not to the
        // trait method being defined.
        DmabufValidator::validate(self, descriptor).map_err(|error| error.to_string())
    }
}

impl DmabufValidator {
    pub(crate) fn new(render_device: RenderDevice) -> Self {
        Self(render_device)
    }

    pub fn validate(&self, descriptor: DmabufDescriptor) -> Result<(), ImportError> {
        texture_descriptor(&descriptor, false)?;
        let vulkan_format = drm_to_vulkan(descriptor.fourcc)
            .ok_or(ImportError::UnsupportedFourcc(descriptor.fourcc))?;
        // SAFETY: The probe owns duplicated plane FDs and destroys every
        // Vulkan object before returning.
        unsafe {
            let Some(device) = self.0.wgpu_device().as_hal::<Vulkan>() else {
                return Err(ImportError::NotVulkan);
            };
            let (image, memories, _) = import_vulkan_image(
                &device,
                descriptor,
                vulkan_format,
                vk::ImageUsageFlags::SAMPLED,
                &[],
            )?;
            cleanup_vulkan_import(&device, image, &memories);
        }
        Ok(())
    }
}

impl ImportedDmabufImages {
    /// Enable sealed live-path diagnostics before the first DMA-BUF is registered.
    ///
    /// Callers deliberately supply the switches instead of this library reading
    /// process environment, so nested compositors cannot accidentally enable
    /// KMS instrumentation.
    pub fn configure_debug(&self, no_cache: bool, probe: bool) {
        let mut imports = self
            .0
            .lock()
            .expect("DMA-BUF import registry mutex poisoned");
        assert!(
            !imports.ever_imported,
            "DMA-BUF debug switches must be configured before the first import"
        );
        let log_imports = imports.debug.options.log_imports;
        imports.debug.options = DmabufDebugOptions {
            no_cache,
            probe,
            log_imports,
        };
        if no_cache {
            imports.cache.clear();
        }
    }

    /// Log metadata for each newly observed buffer and format without changing imports.
    /// Must be enabled before the first DMA-BUF is registered.
    pub fn enable_import_logging(&self) {
        let mut imports = self
            .0
            .lock()
            .expect("DMA-BUF import registry mutex poisoned");
        assert!(
            !imports.ever_imported,
            "DMA-BUF debug switches must be configured before the first import"
        );
        imports.debug.options.log_imports = true;
    }

    /// Consume the compositor-output half of the next sampled-import probe.
    ///
    /// The imported-texture and physical-output probes deliberately have
    /// separate latches: either post-render system may run first without
    /// stealing the other stage's request.
    pub fn take_output_probe_request(&self) -> bool {
        let mut imports = self
            .0
            .lock()
            .expect("DMA-BUF import registry mutex poisoned");
        mem::take(&mut imports.debug.pending_output_probe)
    }

    /// Create a placeholder Bevy image whose GPU backing will be replaced in
    /// `RenderSystems::PrepareAssets`.
    pub fn import(
        &self,
        images: &mut Assets<Image>,
        buffer_id: DmabufBufferId,
        cacheable: bool,
        descriptor: DmabufDescriptor,
        release: DmabufRelease,
    ) -> Result<Handle<Image>, ImportError> {
        let release = ReleaseLease::with_teardown_latch(release, Arc::clone(&self.1));
        let mut imports = self
            .0
            .lock()
            .expect("DMA-BUF import registry mutex poisoned");
        let wgpu_descriptor = texture_descriptor(&descriptor, imports.debug.options.probe)?;
        imports.ever_imported = true;
        let handle = images.add(Image::new_uninit(
            wgpu_descriptor.size,
            wgpu_descriptor.dimension,
            wgpu_descriptor.format,
            RenderAssetUsages::MAIN_WORLD | RenderAssetUsages::RENDER_WORLD,
        ));
        imports.active.insert(
            handle.id(),
            ImportState::Pending(PendingImport {
                buffer_id,
                descriptor,
                release,
                previous: None,
                cacheable,
            }),
        );
        drop(imports);
        Ok(handle)
    }

    /// Replace the imported backing of an existing placeholder image.
    ///
    /// The Bevy asset ID deliberately stays stable while the Vulkan image view
    /// changes on every client commit. `acquire_external_images` replaces the
    /// render-world `GpuImage` and invalidates the sprite image bind-group cache
    /// after the committed backing has acquired queue ownership.
    pub fn replace(
        &self,
        handle: &Handle<Image>,
        buffer_id: DmabufBufferId,
        cacheable: bool,
        descriptor: DmabufDescriptor,
        release: DmabufRelease,
    ) -> Result<(), ImportError> {
        let release = ReleaseLease::with_teardown_latch(release, Arc::clone(&self.1));
        let mut imports = self
            .0
            .lock()
            .expect("DMA-BUF import registry mutex poisoned");
        texture_descriptor(&descriptor, imports.debug.options.probe)?;
        let Some(current) = imports.active.remove(&handle.id()) else {
            return Err(ImportError::UnregisteredImage);
        };
        let previous = previous_for_replacement(current);
        imports.active.insert(
            handle.id(),
            ImportState::Pending(PendingImport {
                buffer_id,
                descriptor,
                release,
                previous,
                cacheable,
            }),
        );
        Ok(())
    }

    /// Evict one destroyed `wl_buffer` from the strong import cache.
    ///
    /// Active render uses retain their backing until normal replacement or
    /// surface teardown. A pending first import is marked non-cacheable so a
    /// commit followed by `wl_buffer.destroy` in one protocol batch can still
    /// render, without resurrecting the destroyed cache entry afterwards.
    pub fn invalidate_buffer(&self, buffer_id: DmabufBufferId) {
        let mut imports = self
            .0
            .lock()
            .expect("DMA-BUF import registry mutex poisoned");
        let ImportRegistry { cache, active, .. } = &mut *imports;
        invalidate_buffer_cache(cache, active, buffer_id);
    }

    /// Evict every cached protocol buffer identity.
    ///
    /// Used when the bounded protocol outbox folds a destroy storm into one
    /// epoch invalidation. Active uses remain valid until normal replacement;
    /// pending imports cannot repopulate the cache from the superseded epoch.
    pub fn invalidate_all_buffers(&self) {
        let mut imports = self
            .0
            .lock()
            .expect("DMA-BUF import registry mutex poisoned");
        let ImportRegistry { cache, active, .. } = &mut *imports;
        invalidate_all_buffer_caches(cache, active);
    }

    /// Permanently suppress protocol release publication for terminal App teardown.
    ///
    /// This latch is shared by the main and render worlds and is monotonic. Once
    /// set, dropping any retained DMA-BUF use discards its callback rather than
    /// claiming that FOREIGN ownership was restored. The session teardown will
    /// disconnect the clients; leaking their final ownership uses is fail-closed.
    pub fn begin_terminal_teardown(&self) {
        self.1.store(true, Ordering::Release);
    }

    /// Remove a surface-owned image registration.
    ///
    /// Render-world preparation never unregisters images: a missing
    /// `GpuImage` can be a transient extraction condition. The main world
    /// calls this only when the owning surface stops using the image.
    pub fn unregister(&self, handle: &Handle<Image>) {
        let mut imports = self
            .0
            .lock()
            .expect("DMA-BUF import registry mutex poisoned");
        let Some(current) = imports.active.remove(&handle.id()) else {
            return;
        };
        let retired = retired_textures_for_unregister(current);
        imports
            .retired
            .entry(handle.id())
            .or_default()
            .extend(retired);
    }
}

pub struct DmabufImportPlugin;

#[derive(SystemSet, Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum DmabufImportSystems {
    Apply,
    Acquire,
}

impl Plugin for DmabufImportPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ImportedDmabufImages>()
            .add_plugins(ExtractResourcePlugin::<ImportedDmabufImages>::default());
        let Some(render_app) = app.get_sub_app_mut(RenderApp) else {
            return;
        };
        // Both import systems publish Image::Modified notifications through
        // this render-world queue. SpriteMaterialPlugin normally creates it,
        // but DMA-BUF installation must also work for non-sprite consumers.
        // Bevy's init_resource preserves an existing value when the sprite
        // plugin has already installed the resource.
        render_app.init_resource::<SpriteAssetEvents>();
        render_app.add_systems(
            Render,
            (
                apply_imports.in_set(DmabufImportSystems::Apply),
                acquire_external_images.in_set(DmabufImportSystems::Acquire),
            )
                .chain()
                .in_set(RenderSystems::PrepareAssets)
                .after(prepare_assets::<GpuImage>)
                .before(prepare_assets::<PreparedMaterial2d<SpriteMaterial>>),
        );
        render_app.add_systems(
            Render,
            release_external_images.in_set(RenderSystems::Cleanup),
        );
    }
}

/// Adds a render-schedule edge from external-image installation to one
/// compositor-owned [`Material2d`] preparation system.
///
/// The bridge cannot name downstream material types itself. A renderer that
/// samples imported images registers each such material after installing
/// [`DmabufImportPlugin`], making the imported `GpuImage` visible before Bevy
/// creates that material's bind group.
pub trait DmabufMaterial2dRegistrationExt {
    fn register_dmabuf_material_2d<M: Material2d>(&mut self) -> &mut Self;
}

impl DmabufMaterial2dRegistrationExt for App {
    fn register_dmabuf_material_2d<M: Material2d>(&mut self) -> &mut Self {
        let Some(render_app) = self.get_sub_app_mut(RenderApp) else {
            return self;
        };
        render_app.add_systems(
            Render,
            dmabuf_material_prepare_barrier::<M>
                .in_set(RenderSystems::PrepareAssets)
                .after(acquire_external_images)
                .before(prepare_assets::<PreparedMaterial2d<M>>),
        );
        self
    }
}

/// Type-stable marker used to verify one material's installed DMA-BUF ordering edge.
#[doc(hidden)]
pub fn dmabuf_material_prepare_barrier<M: Material2d>() {}

/// Adds the second, post-render readback stage for the opt-in live probe.
///
/// This plugin is installed only when the KMS entry point has accepted
/// `COSMIX_DMABUF_PROBE=1`; ordinary and nested render schedules contain no
/// probe system at all.
pub struct DmabufProbePlugin;

impl Plugin for DmabufProbePlugin {
    fn build(&self, app: &mut App) {
        let Some(render_app) = app.get_sub_app_mut(RenderApp) else {
            return;
        };
        render_app.add_systems(
            Render,
            probe_sampled_external_images
                .after(RenderSystems::Render)
                .before(RenderSystems::Cleanup),
        );
    }
}

trait ImportPlatform<T> {
    type Error;

    fn import_texture(
        &self,
        descriptor: DmabufDescriptor,
        instrumentation: ImportInstrumentation,
    ) -> Result<T, Self::Error>;
}

struct RenderImportPlatform<'a>(&'a RenderDevice);

impl ImportPlatform<CachedTexture> for RenderImportPlatform<'_> {
    type Error = ImportError;

    fn import_texture(
        &self,
        descriptor: DmabufDescriptor,
        instrumentation: ImportInstrumentation,
    ) -> Result<CachedTexture, Self::Error> {
        import_texture(self.0, descriptor, instrumentation)
    }
}

fn attempt_pending_import<T, P>(
    platform: &P,
    cache: &mut ImportCache<T>,
    pending: PendingImport<ImportedUse<T>>,
    no_cache: bool,
    instrumentation: ImportInstrumentation,
) -> PendingImportResult<T, P::Error>
where
    P: ImportPlatform<T>,
{
    let PendingImport {
        buffer_id,
        descriptor,
        release,
        previous,
        cacheable,
    } = pending;
    let retained_bytes = dmabuf_retained_bytes(&descriptor);
    let cached = (!no_cache).then(|| cache.get(buffer_id)).flatten();
    let (backing, newly_imported) = if let Some(backing) = cached {
        (backing, false)
    } else {
        let backing = match platform.import_texture(descriptor, instrumentation) {
            Ok(backing) => Arc::new(backing),
            Err(error) => return Err((error, previous)),
        };
        if cacheable && !no_cache {
            cache.insert(buffer_id, Arc::clone(&backing), retained_bytes);
        }
        (backing, true)
    };
    Ok(ReadyImport {
        current: ImportedUse::new(backing, release),
        previous,
        newly_imported,
        probe_after_acquire: newly_imported && instrumentation.probe,
    })
}

fn dmabuf_retained_bytes(descriptor: &DmabufDescriptor) -> usize {
    descriptor
        .planes
        .iter()
        .map(|plane| {
            let fallback = usize::try_from(plane.offset)
                .unwrap_or(usize::MAX)
                .saturating_add(
                    usize::try_from(plane.stride)
                        .unwrap_or(usize::MAX)
                        .saturating_mul(usize::try_from(descriptor.height).unwrap_or(usize::MAX)),
                );
            // DMA-BUF supports SEEK_END as the cheap allocation-size query.
            // Prefer that honest retained size; unusual FDs that reject the
            // query (and zero-length test fixtures) still pay for the validated
            // plane offset plus its complete row span.
            let allocation_end = unsafe { libc::lseek(plane.fd.as_raw_fd(), 0, libc::SEEK_END) };
            usize::try_from(allocation_end)
                .ok()
                .filter(|bytes| *bytes != 0)
                .unwrap_or(fallback)
        })
        .fold(0_usize, usize::saturating_add)
}

fn plan_import_instrumentation(
    debug: &DmabufDebugState,
    buffer_id: DmabufBufferId,
    fourcc: u32,
    will_import: bool,
) -> ImportInstrumentation {
    if !will_import
        || (!debug.options.no_cache && !debug.options.probe && !debug.options.log_imports)
    {
        return ImportInstrumentation {
            buffer_id,
            ..Default::default()
        };
    }
    let import_number = debug
        .import_counts
        .get(&buffer_id)
        .copied()
        .unwrap_or_default()
        .saturating_add(1);
    ImportInstrumentation {
        buffer_id,
        import_number,
        log_metadata: !debug.logged_buffers.contains(&buffer_id),
        log_format_metadata: !debug.logged_formats.contains(&fourcc),
        probe: debug.options.probe && (import_number <= 3 || import_number % 60 == 0),
    }
}

fn record_successful_import(
    debug: &mut DmabufDebugState,
    fourcc: u32,
    instrumentation: ImportInstrumentation,
) {
    if instrumentation.import_number == 0 {
        return;
    }
    debug
        .import_counts
        .insert(instrumentation.buffer_id, instrumentation.import_number);
    if instrumentation.log_metadata {
        debug.logged_buffers.insert(instrumentation.buffer_id);
    }
    if instrumentation.log_format_metadata {
        debug.logged_formats.insert(fourcc);
    }
}

fn collect_retired_without_gpu<K, T>(
    retired: &mut HashMap<K, Vec<T>>,
    mut gpu_present: impl FnMut(&K) -> bool,
) -> (Vec<K>, Vec<T>)
where
    K: Copy + Eq + std::hash::Hash,
{
    let ids = retired
        .keys()
        .copied()
        .filter(|id| !gpu_present(id))
        .collect::<Vec<_>>();
    let evictions = ids
        .iter()
        .flat_map(|id| retired.remove(id).unwrap_or_default())
        .collect();
    (ids, evictions)
}

fn finish_import_updates<T>(
    modified_images: impl IntoIterator<Item = AssetId<Image>>,
    sprite_asset_events: &mut SpriteAssetEvents,
    logical_evictions: Vec<T>,
) -> Vec<T> {
    sprite_asset_events.images.extend(
        modified_images
            .into_iter()
            .map(|id| AssetEvent::Modified { id }),
    );
    logical_evictions
}

fn retire_imported_uses<T>(
    ownership_retired: &mut Vec<ImportedUse<T>>,
    local_owned: &HashSet<usize>,
    evictions: Vec<ImportedUse<T>>,
) {
    for imported in evictions {
        if local_owned.contains(&(Arc::as_ptr(&imported.backing) as usize)) {
            ownership_retired.push(imported);
        } else {
            // This use never acquired queue ownership, so no local -> FOREIGN
            // handback remains to sequence before its rejection callback runs.
            // Failed releases never reach here: `complete_release` strands
            // those uses permanently instead of removing their callback guard.
            drop(imported);
        }
    }
}

fn apply_imports(
    gpu_images: ResMut<RenderAssets<GpuImage>>,
    imports: Res<ImportedDmabufImages>,
    render_device: Res<RenderDevice>,
    mut sprite_asset_events: ResMut<SpriteAssetEvents>,
) {
    let mut imports = imports
        .0
        .lock()
        .expect("DMA-BUF import registry mutex poisoned");
    let (removed_ids, logical_evictions) =
        collect_retired_without_gpu(&mut imports.retired, |id| gpu_images.get(*id).is_some());
    let ids = imports.active.keys().copied().collect::<Vec<_>>();

    for id in ids {
        if gpu_images.get(id).is_none() {
            // Asset extraction and GPU preparation can lag main-world
            // registration. Only the surface-owning main world may unregister
            // this ID, so retain its pending/applied state and retry.
            continue;
        }

        if matches!(imports.active.get(&id), Some(ImportState::Pending(_))) {
            let Some(ImportState::Pending(pending)) = imports.active.remove(&id) else {
                continue;
            };
            let no_cache = imports.debug.options.no_cache;
            let will_import = no_cache || !imports.cache.contains(pending.buffer_id);
            let fourcc = pending.descriptor.fourcc;
            let instrumentation =
                plan_import_instrumentation(&imports.debug, pending.buffer_id, fourcc, will_import);
            let ImportRegistry { cache, .. } = &mut *imports;
            match attempt_pending_import(
                &RenderImportPlatform(&render_device),
                cache,
                pending,
                no_cache,
                instrumentation,
            ) {
                Ok(ready) => {
                    if ready.newly_imported {
                        record_successful_import(&mut imports.debug, fourcc, instrumentation);
                        imports
                            .preacquired
                            .insert(Arc::as_ptr(&ready.current.backing) as usize);
                    }
                    imports.active.insert(id, ImportState::Imported(ready));
                }
                Err((import_error, previous)) => {
                    error!(%import_error, ?id, "failed to import DMA-BUF texture");
                    // The failed request drops its release callback. Keep the
                    // previous applied texture live, ownership-tracked and
                    // sampled; only a first-ever import falls back to Idle.
                    imports.active.insert(id, failed_import_fallback(previous));
                    continue;
                }
            }
        }
    }

    let evictions = finish_import_updates(removed_ids, &mut sprite_asset_events, logical_evictions);
    let local_owned = imports.local_owned.clone();
    retire_imported_uses(&mut imports.ownership_retired, &local_owned, evictions);
}

fn previous_for_replacement<T>(current: ImportState<T>) -> Option<T> {
    match current {
        ImportState::Idle => None,
        ImportState::Pending(mut pending) => pending.previous.take(),
        ImportState::Imported(ready) => {
            assert!(
                ready.previous.is_none(),
                "Imported replacement state cannot escape apply_imports with a previous texture"
            );
            Some(ready.current)
        }
        ImportState::Applied(imported) => Some(imported),
    }
}

fn current_import<T>(state: &ImportState<T>) -> Option<&T> {
    match state {
        ImportState::Imported(ready) => Some(&ready.current),
        ImportState::Applied(imported) => Some(imported),
        ImportState::Idle | ImportState::Pending(_) => None,
    }
}

struct AcquireBatch<T> {
    submitted_backings: Vec<Arc<T>>,
    submitted_ids: HashSet<AssetId<Image>>,
    ready_without_submission: HashSet<AssetId<Image>>,
    ready_backings: Vec<Arc<T>>,
    probe_backings: Vec<Arc<T>>,
}

fn pending_acquire_batch<T>(
    active: &HashMap<AssetId<Image>, ImportState<ImportedUse<T>>>,
    preacquired: &HashSet<usize>,
    local_owned: &HashSet<usize>,
) -> AcquireBatch<T> {
    let mut submitted_seen = HashSet::new();
    let mut ready_seen = HashSet::new();
    let mut batch = AcquireBatch {
        submitted_backings: Vec::new(),
        submitted_ids: HashSet::new(),
        ready_without_submission: HashSet::new(),
        ready_backings: Vec::new(),
        probe_backings: Vec::new(),
    };
    for (id, state) in active {
        let ImportState::Imported(ready) = state else {
            continue;
        };
        let backing = &ready.current.backing;
        let identity = Arc::as_ptr(backing) as usize;
        if ready_seen.insert(identity) {
            batch.ready_backings.push(Arc::clone(backing));
        }
        if ready.probe_after_acquire {
            batch.probe_backings.push(Arc::clone(backing));
        }
        if preacquired.contains(&identity) || local_owned.contains(&identity) {
            batch.ready_without_submission.insert(*id);
        } else {
            batch.submitted_ids.insert(*id);
            if submitted_seen.insert(identity) {
                batch.submitted_backings.push(Arc::clone(backing));
            }
        }
    }
    batch
}

fn apply_import_state<T>(state: ImportState<T>) -> (ImportState<T>, Vec<T>) {
    match state {
        ImportState::Imported(ready) => (
            ImportState::Applied(ready.current),
            ready.previous.into_iter().collect(),
        ),
        other => (other, Vec::new()),
    }
}

fn retired_textures_for_unregister<T>(state: ImportState<T>) -> Vec<T> {
    match state {
        ImportState::Idle => Vec::new(),
        ImportState::Pending(mut pending) => pending.previous.take().into_iter().collect(),
        ImportState::Imported(ready) => {
            let mut retired = ready.previous.into_iter().collect::<Vec<_>>();
            retired.push(ready.current);
            retired
        }
        ImportState::Applied(imported) => vec![imported],
    }
}

fn failed_import_fallback<T>(previous: Option<T>) -> ImportState<T> {
    previous.map_or(ImportState::Idle, ImportState::Applied)
}

struct AcquireUpdates<T> {
    install: Vec<AssetId<Image>>,
    uninstall: Vec<AssetId<Image>>,
    retired: Vec<T>,
}

fn complete_acquire<T, E>(
    active: &mut HashMap<AssetId<Image>, ImportState<T>>,
    result: Result<(), E>,
    submitted_ids: &HashSet<AssetId<Image>>,
    ready_without_submission: &HashSet<AssetId<Image>>,
) -> Result<AcquireUpdates<T>, ErrWithUpdates<T, E>> {
    match result {
        Ok(()) => {
            let ids = submitted_ids
                .union(ready_without_submission)
                .copied()
                .filter(|id| matches!(active.get(id), Some(ImportState::Imported(_))))
                .collect::<Vec<_>>();
            let mut retired = Vec::new();
            for id in &ids {
                let Some(state) = active.remove(id) else {
                    continue;
                };
                let (applied, previous) = apply_import_state(state);
                active.insert(*id, applied);
                retired.extend(previous);
            }
            Ok(AcquireUpdates {
                install: ids,
                uninstall: Vec::new(),
                retired,
            })
        }
        Err(error) => {
            let safe_ids = ready_without_submission
                .iter()
                .copied()
                .filter(|id| matches!(active.get(id), Some(ImportState::Imported(_))))
                .collect::<Vec<_>>();
            let mut retired = Vec::new();
            for id in &safe_ids {
                let Some(state) = active.remove(id) else {
                    continue;
                };
                let (applied, previous) = apply_import_state(state);
                active.insert(*id, applied);
                retired.extend(previous);
            }
            let failed_ids = submitted_ids
                .iter()
                .copied()
                .filter(|id| matches!(active.get(id), Some(ImportState::Imported(_))))
                .collect::<Vec<_>>();
            for id in &failed_ids {
                if let Some(state) = active.remove(id) {
                    retired.extend(retired_textures_for_unregister(state));
                    active.insert(*id, ImportState::Idle);
                }
            }
            Err(ErrWithUpdates::new(error, safe_ids, failed_ids, retired))
        }
    }
}

// Keeps the generic state transition testable without requiring a Vulkan
// device while still returning the original acquire error to the render path.
struct ErrWithUpdates<T, E> {
    error: E,
    updates: AcquireUpdates<T>,
}

impl<T, E> ErrWithUpdates<T, E> {
    fn new(
        error: E,
        install: Vec<AssetId<Image>>,
        uninstall: Vec<AssetId<Image>>,
        retired: Vec<T>,
    ) -> Self {
        Self {
            error,
            updates: AcquireUpdates {
                install,
                uninstall,
                retired,
            },
        }
    }
}

fn install_gpu_images(
    gpu_images: &mut RenderAssets<GpuImage>,
    active: &HashMap<AssetId<Image>, ImportState>,
    ids: &[AssetId<Image>],
) -> Vec<AssetId<Image>> {
    let mut installed = Vec::new();
    for id in ids {
        let Some(sampler) = gpu_images
            .get(*id)
            .map(|gpu_image| gpu_image.sampler.clone())
        else {
            continue;
        };
        let Some(imported) = active.get(id).and_then(current_import) else {
            continue;
        };

        // Replace the complete render asset, rather than mutating selected
        // fields in place. The stable Image AssetId is the surface identity;
        // the GpuImage identity is the committed wl_buffer and must change on
        // every cache hit just as it does after a fresh import. A main-world
        // Image::Modified event would be wrong here: Bevy would prepare the
        // uninitialised placeholder again and overwrite this external image.
        gpu_images.insert(
            *id,
            GpuImage {
                texture: imported.backing.texture.clone(),
                texture_view: imported.backing.texture_view.clone(),
                sampler,
                texture_descriptor: imported.backing.descriptor.clone(),
                texture_view_descriptor: None,
                had_data: false,
            },
        );
        installed.push(*id);
    }
    installed
}

fn acquire_external_images(
    device: Res<RenderDevice>,
    queue: Res<RenderQueue>,
    imports: Res<ImportedDmabufImages>,
    mut gpu_images: ResMut<RenderAssets<GpuImage>>,
    mut sprite_asset_events: ResMut<SpriteAssetEvents>,
) {
    let shared_registry = Arc::clone(&imports.0);
    let batch = {
        let imports = imports
            .0
            .lock()
            .expect("DMA-BUF import registry mutex poisoned");
        pending_acquire_batch(&imports.active, &imports.preacquired, &imports.local_owned)
    };
    let barrier_result = submit_ownership_barrier(
        &device,
        &batch.submitted_backings,
        OwnershipDirection::Acquire,
    );
    let mut imports = imports
        .0
        .lock()
        .expect("DMA-BUF import registry mutex poisoned");
    imports.preacquired.clear();

    if barrier_result.is_err() {
        evict_cache_backings(&mut imports.cache, &batch.submitted_backings);
    }

    let updates = match complete_acquire(
        &mut imports.active,
        barrier_result,
        &batch.submitted_ids,
        &batch.ready_without_submission,
    ) {
        Ok(updates) => {
            for backing in &batch.ready_backings {
                imports.local_owned.insert(Arc::as_ptr(backing) as usize);
            }
            updates
        }
        Err(failure) => {
            for id in &failure.updates.install {
                let Some(imported) = imports.active.get(id).and_then(current_import) else {
                    continue;
                };
                let identity = Arc::as_ptr(&imported.backing) as usize;
                imports.local_owned.insert(identity);
            }
            error!(barrier_error = %failure.error, "failed to acquire DMA-BUF queue ownership");
            // `complete_acquire` has made every affected state Idle. Remove the
            // GPU assets as well, so no later render system can sample a texture
            // whose ownership/layout transition did not complete.
            let mut modified_images = failure
                .updates
                .uninstall
                .iter()
                .copied()
                .filter(|id| gpu_images.remove(*id).is_some())
                .collect::<Vec<_>>();
            modified_images.extend(install_gpu_images(
                &mut gpu_images,
                &imports.active,
                &failure.updates.install,
            ));
            let retired = finish_import_updates(
                modified_images,
                &mut sprite_asset_events,
                failure.updates.retired,
            );
            let local_owned = imports.local_owned.clone();
            retire_imported_uses(&mut imports.ownership_retired, &local_owned, retired);
            return;
        }
    };

    let mut modified_images = updates
        .uninstall
        .iter()
        .copied()
        .filter(|id| gpu_images.remove(*id).is_some())
        .collect::<Vec<_>>();
    modified_images.extend(install_gpu_images(
        &mut gpu_images,
        &imports.active,
        &updates.install,
    ));
    let retired = finish_import_updates(modified_images, &mut sprite_asset_events, updates.retired);
    let local_owned = imports.local_owned.clone();
    retire_imported_uses(&mut imports.ownership_retired, &local_owned, retired);
    drop(imports);

    if !batch.probe_backings.is_empty() {
        probe_external_images(&device, &queue, &batch.probe_backings, "import");
        let mut imports = shared_registry
            .lock()
            .expect("DMA-BUF import registry mutex poisoned");
        imports
            .debug
            .pending_sample_probes
            .extend(batch.probe_backings);
        imports.debug.pending_output_probe = true;
    }
}

fn probe_sampled_external_images(
    device: Res<RenderDevice>,
    queue: Res<RenderQueue>,
    imports: Res<ImportedDmabufImages>,
) {
    let pending = {
        let mut imports = imports
            .0
            .lock()
            .expect("DMA-BUF import registry mutex poisoned");
        mem::take(&mut imports.debug.pending_sample_probes)
    };
    probe_external_images(&device, &queue, &pending, "post-render");
}

fn probe_external_images(
    device: &RenderDevice,
    queue: &RenderQueue,
    backings: &[Arc<CachedTexture>],
    stage: &'static str,
) {
    for backing in backings {
        let Some(identity) = backing.probe else {
            continue;
        };
        match readback_probe(device, queue, backing, identity) {
            Ok(summary) => info!(
                stage,
                buffer_id = identity.buffer_id.0,
                import_number = identity.import_number,
                rows = ?identity.rows,
                width = identity.width,
                nonzero_bytes = ?summary.nonzero_bytes,
                alpha_zero = ?summary.alpha_zero,
                alpha_ff = ?summary.alpha_ff,
                alpha_other = ?summary.alpha_other,
                checksum = format_args!("{:016x}", summary.checksum),
                "DMA-BUF GPU probe"
            ),
            Err(probe_error) => error!(
                stage,
                buffer_id = identity.buffer_id.0,
                import_number = identity.import_number,
                %probe_error,
                "DMA-BUF GPU probe failed"
            ),
        }
    }
}

struct DmabufProbeSummary {
    nonzero_bytes: [usize; 3],
    alpha_zero: [usize; 3],
    alpha_ff: [usize; 3],
    alpha_other: [usize; 3],
    checksum: u64,
}

fn readback_probe(
    device: &RenderDevice,
    queue: &RenderQueue,
    backing: &CachedTexture,
    identity: DmabufProbeIdentity,
) -> Result<DmabufProbeSummary, String> {
    const BYTES_PER_PIXEL: u32 = 4;

    let row_bytes = identity.width.saturating_mul(BYTES_PER_PIXEL);
    let padded_row_bytes = RenderDevice::align_copy_bytes_per_row(row_bytes as usize);
    let buffer_size = u64::try_from(padded_row_bytes.saturating_mul(identity.rows.len()))
        .map_err(|_| "probe buffer size is not representable".to_string())?;
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("DMA-BUF content probe readback"),
        size: buffer_size,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("DMA-BUF content probe copy"),
    });
    for (index, row) in identity.rows.into_iter().enumerate() {
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &backing.texture,
                mip_level: 0,
                origin: wgpu::Origin3d { x: 0, y: row, z: 0 },
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: u64::try_from(index.saturating_mul(padded_row_bytes))
                        .map_err(|_| "probe row offset is not representable".to_string())?,
                    bytes_per_row: Some(
                        u32::try_from(padded_row_bytes)
                            .map_err(|_| "probe row pitch is not representable".to_string())?,
                    ),
                    rows_per_image: Some(1),
                },
            },
            wgpu::Extent3d {
                width: identity.width,
                height: 1,
                depth_or_array_layers: 1,
            },
        );
    }
    queue.submit([encoder.finish()]);

    let slice = buffer.slice(..);
    let (mapped_sender, mapped_receiver) = std::sync::mpsc::sync_channel(1);
    slice.map_async(wgpu::MapMode::Read, move |result| {
        let _ = mapped_sender.send(result.map_err(|error| error.to_string()));
    });
    device
        .poll(wgpu::PollType::wait_indefinitely())
        .map_err(|error| error.to_string())?;
    mapped_receiver
        .recv()
        .map_err(|_| "probe mapping callback was dropped".to_string())??;

    let data = slice.get_mapped_range();
    let row_bytes = usize::try_from(row_bytes)
        .map_err(|_| "probe row width is not representable".to_string())?;
    let summary = summarize_probe_data(&data, padded_row_bytes, row_bytes)?;
    drop(data);
    buffer.unmap();
    Ok(summary)
}

fn summarize_probe_data(
    data: &[u8],
    padded_row_bytes: usize,
    row_bytes: usize,
) -> Result<DmabufProbeSummary, String> {
    const BYTES_PER_PIXEL: usize = 4;
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

    let mut summary = DmabufProbeSummary {
        nonzero_bytes: [0; 3],
        alpha_zero: [0; 3],
        alpha_ff: [0; 3],
        alpha_other: [0; 3],
        checksum: FNV_OFFSET,
    };
    for index in 0..summary.nonzero_bytes.len() {
        let start = index.saturating_mul(padded_row_bytes);
        let end = start.saturating_add(row_bytes);
        let row = data
            .get(start..end)
            .ok_or_else(|| "probe mapped range is shorter than requested rows".to_string())?;
        summary.nonzero_bytes[index] = row.iter().filter(|byte| **byte != 0).count();
        for alpha in row.chunks_exact(BYTES_PER_PIXEL).map(|pixel| pixel[3]) {
            match alpha {
                0x00 => summary.alpha_zero[index] += 1,
                0xff => summary.alpha_ff[index] += 1,
                _ => summary.alpha_other[index] += 1,
            }
        }
        for byte in row {
            summary.checksum ^= u64::from(*byte);
            summary.checksum = summary.checksum.wrapping_mul(FNV_PRIME);
        }
    }
    Ok(summary)
}

fn active_retains_backing<T>(
    active: &HashMap<AssetId<Image>, ImportState<ImportedUse<T>>>,
    identity: usize,
) -> bool {
    let matches = |imported: &ImportedUse<T>| Arc::as_ptr(&imported.backing) as usize == identity;
    active.values().any(|state| match state {
        ImportState::Idle => false,
        ImportState::Pending(pending) => pending.previous.as_ref().is_some_and(matches),
        ImportState::Imported(ready) => {
            matches(&ready.current) || ready.previous.as_ref().is_some_and(matches)
        }
        ImportState::Applied(imported) => matches(imported),
    })
}

fn releasable_retired_backings<T>(
    active: &HashMap<AssetId<Image>, ImportState<ImportedUse<T>>>,
    ownership_retired: &[ImportedUse<T>],
    local_owned: &HashSet<usize>,
) -> Vec<Arc<T>> {
    let mut seen = HashSet::new();
    ownership_retired
        .iter()
        .filter(|imported| {
            let identity = Arc::as_ptr(&imported.backing) as usize;
            local_owned.contains(&identity)
                && seen.insert(identity)
                && !active_retains_backing(active, identity)
        })
        .map(|imported| Arc::clone(&imported.backing))
        .collect()
}

fn take_retired_for_backings<T>(
    ownership_retired: &mut Vec<ImportedUse<T>>,
    backings: &[Arc<T>],
) -> Vec<ImportedUse<T>> {
    let identities = backings
        .iter()
        .map(|backing| Arc::as_ptr(backing) as usize)
        .collect::<HashSet<_>>();
    let mut completed = Vec::new();
    let mut retained = Vec::with_capacity(ownership_retired.len());
    for imported in mem::take(ownership_retired) {
        if identities.contains(&(Arc::as_ptr(&imported.backing) as usize)) {
            completed.push(imported);
        } else {
            retained.push(imported);
        }
    }
    *ownership_retired = retained;
    completed
}

struct ReleaseFailure<E> {
    error: E,
}

fn complete_release<T, E>(
    local_owned: &mut HashSet<usize>,
    cache: &mut ImportCache<T>,
    ownership_retired: &mut Vec<ImportedUse<T>>,
    backings: &[Arc<T>],
    result: Result<(), E>,
) -> Result<Vec<ImportedUse<T>>, ReleaseFailure<E>> {
    let identities = backings
        .iter()
        .map(|backing| Arc::as_ptr(backing) as usize)
        .collect::<HashSet<_>>();
    let completed = take_retired_for_backings(ownership_retired, backings);
    for identity in &identities {
        local_owned.remove(identity);
    }
    match result {
        Ok(()) => Ok(completed),
        Err(error) => {
            // A failed release leaves the Vulkan ownership/layout unknowable.
            // Eviction makes the backing terminal for reuse; the next commit
            // of this wl_buffer must import and acquire a new VkImage. The old
            // use cannot be dropped because that would publish wl_buffer.release
            // or its explicit release point without a completed FOREIGN
            // handback. Leak the complete use fail-closed, mirroring the guarded
            // render-worker ownership policy.
            evict_cache_backings(cache, backings);
            mem::forget(completed);
            Err(ReleaseFailure { error })
        }
    }
}

fn release_external_images(
    device: Res<RenderDevice>,
    queue: Res<RenderQueue>,
    imports: Res<ImportedDmabufImages>,
) {
    let release_backings = {
        let imports = imports
            .0
            .lock()
            .expect("DMA-BUF import registry mutex poisoned");
        releasable_retired_backings(
            &imports.active,
            &imports.ownership_retired,
            &imports.local_owned,
        )
    };
    if release_backings.is_empty() {
        return;
    }
    transition_imported_images_to_resource(&device, &queue, &release_backings);
    let barrier_result =
        submit_ownership_barrier(&device, &release_backings, OwnershipDirection::Release);
    let mut imports = imports
        .0
        .lock()
        .expect("DMA-BUF import registry mutex poisoned");
    let ImportRegistry {
        local_owned,
        cache,
        ownership_retired,
        ..
    } = &mut *imports;
    let completion = complete_release(
        local_owned,
        cache,
        ownership_retired,
        &release_backings,
        barrier_result,
    );
    drop(imports);
    match completion {
        Ok(completed) => drop(completed),
        Err(failure) => {
            error!(barrier_error = %failure.error, "failed to release DMA-BUF queue ownership; backing and release use stranded fail-closed");
        }
    }
}

fn transition_imported_images_to_resource(
    render_device: &RenderDevice,
    render_queue: &RenderQueue,
    backings: &[Arc<CachedTexture>],
) {
    let textures = backings
        .iter()
        .map(|backing| backing.texture.clone())
        .collect::<Vec<_>>();
    if textures.is_empty() {
        return;
    }

    // The patched HAL import path seeds RESOURCE, matching the raw acquire.
    // Keep it there before the shader-read -> GENERAL ownership release even
    // when a sprite was culled and therefore never sampled this frame.
    let mut encoder = render_device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("normalise imported DMA-BUF before release"),
    });
    encoder.transition_resources(
        std::iter::empty(),
        textures.iter().map(|texture| wgpu::TextureTransition {
            texture: &**texture,
            selector: None,
            state: TextureUses::RESOURCE,
        }),
    );
    render_queue.submit([encoder.finish()]);
}

#[derive(Clone, Copy)]
pub(crate) enum OwnershipDirection {
    Acquire,
    Release,
}

#[derive(Clone, Copy)]
pub(crate) enum OwnershipRole {
    Sampled,
    CaptureDestination,
}

fn submit_ownership_barrier(
    render_device: &RenderDevice,
    backings: &[Arc<CachedTexture>],
    direction: OwnershipDirection,
) -> Result<(), ImportError> {
    let images = backings
        .iter()
        .filter_map(|backing| {
            // SAFETY: We only copy the Vulkan handle. `backings` retains every
            // owning wgpu texture until the synchronous submission completes.
            unsafe {
                backing
                    .texture
                    .as_hal::<Vulkan>()
                    .map(|texture| texture.raw_handle())
            }
        })
        .collect::<Vec<_>>();
    if images.is_empty() {
        return Ok(());
    }

    // SAFETY: All Vulkan work is submitted on wgpu's own queue from the render
    // thread, and the fence is waited before temporary command resources drop.
    unsafe {
        let Some(device) = render_device.wgpu_device().as_hal::<Vulkan>() else {
            return Err(ImportError::NotVulkan);
        };
        submit_raw_sampled_ownership_barrier(&device, &images, direction)?;
    }
    Ok(())
}

unsafe fn submit_raw_sampled_ownership_barrier(
    device: &wgpu_hal::vulkan::Device,
    images: &[vk::Image],
    direction: OwnershipDirection,
) -> Result<(), ImportError> {
    let raw = device.raw_device();
    unsafe {
        let pool = raw.create_command_pool(
            &vk::CommandPoolCreateInfo::default()
                .flags(vk::CommandPoolCreateFlags::TRANSIENT)
                .queue_family_index(device.queue_family_index()),
            None,
        )?;
        let allocated = match raw.allocate_command_buffers(
            &vk::CommandBufferAllocateInfo::default()
                .command_pool(pool)
                .level(vk::CommandBufferLevel::PRIMARY)
                .command_buffer_count(1),
        ) {
            Ok(allocated) => allocated,
            Err(error) => {
                raw.destroy_command_pool(pool, None);
                return Err(ImportError::Vulkan(error));
            }
        };
        let command_buffer = match allocated.into_iter().next() {
            Some(command_buffer) => command_buffer,
            None => {
                raw.destroy_command_pool(pool, None);
                return Err(ImportError::Vulkan(vk::Result::ERROR_INITIALIZATION_FAILED));
            }
        };
        if let Err(error) = raw.begin_command_buffer(
            command_buffer,
            &vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
        ) {
            raw.destroy_command_pool(pool, None);
            return Err(ImportError::Vulkan(error));
        }

        let (src_stage, dst_stage, barriers) = ownership_barriers(
            device.queue_family_index(),
            images,
            direction,
            OwnershipRole::Sampled,
        );
        raw.cmd_pipeline_barrier(
            command_buffer,
            src_stage,
            dst_stage,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &barriers,
        );
        if let Err(error) = raw.end_command_buffer(command_buffer) {
            raw.destroy_command_pool(pool, None);
            return Err(ImportError::Vulkan(error));
        }
        let fence = match raw.create_fence(&vk::FenceCreateInfo::default(), None) {
            Ok(fence) => fence,
            Err(error) => {
                raw.destroy_command_pool(pool, None);
                return Err(ImportError::Vulkan(error));
            }
        };
        let command_buffers = [command_buffer];
        let submit = [vk::SubmitInfo::default().command_buffers(&command_buffers)];
        let result = raw.queue_submit(device.raw_queue(), &submit, fence);
        if let Err(submit_error) = result {
            raw.destroy_fence(fence, None);
            raw.destroy_command_pool(pool, None);
            return Err(ImportError::Vulkan(submit_error));
        }
        let wait_result = raw.wait_for_fences(&[fence], true, u64::MAX);
        raw.destroy_fence(fence, None);
        raw.destroy_command_pool(pool, None);
        wait_result?;
    }
    Ok(())
}

pub(crate) unsafe fn encode_ownership_barrier(
    render_device: &RenderDevice,
    encoder: &mut wgpu::CommandEncoder,
    images: &[vk::Image],
    direction: OwnershipDirection,
    role: OwnershipRole,
) -> Result<(), ImportError> {
    if images.is_empty() {
        return Ok(());
    }
    let Some(device) = (unsafe { render_device.wgpu_device().as_hal::<Vulkan>() }) else {
        return Err(ImportError::NotVulkan);
    };
    let (src_stage, dst_stage, barriers) =
        ownership_barriers(device.queue_family_index(), images, direction, role);
    unsafe {
        encoder.as_hal_mut::<Vulkan, _, _>(|hal_encoder| {
            let hal_encoder = hal_encoder.ok_or(ImportError::NotVulkan)?;
            device.raw_device().cmd_pipeline_barrier(
                hal_encoder.raw_handle(),
                src_stage,
                dst_stage,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &barriers,
            );
            Ok(())
        })
    }
}

fn ownership_barriers(
    queue_family_index: u32,
    images: &[vk::Image],
    direction: OwnershipDirection,
    role: OwnershipRole,
) -> (
    vk::PipelineStageFlags,
    vk::PipelineStageFlags,
    Vec<vk::ImageMemoryBarrier<'static>>,
) {
    let (local_layout, local_access, local_stage) = match role {
        OwnershipRole::Sampled => (
            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            vk::AccessFlags::SHADER_READ,
            vk::PipelineStageFlags::ALL_COMMANDS,
        ),
        OwnershipRole::CaptureDestination => (
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            vk::AccessFlags::TRANSFER_WRITE,
            vk::PipelineStageFlags::TRANSFER,
        ),
    };
    let (src_stage, dst_stage) = match direction {
        OwnershipDirection::Acquire => (vk::PipelineStageFlags::TOP_OF_PIPE, local_stage),
        OwnershipDirection::Release => (local_stage, vk::PipelineStageFlags::BOTTOM_OF_PIPE),
    };
    let barriers = images
        .iter()
        .map(|image| {
            let (src_family, dst_family, old_layout, new_layout, src_access, dst_access) =
                match direction {
                    OwnershipDirection::Acquire => (
                        vk::QUEUE_FAMILY_FOREIGN_EXT,
                        queue_family_index,
                        vk::ImageLayout::GENERAL,
                        local_layout,
                        vk::AccessFlags::empty(),
                        local_access,
                    ),
                    OwnershipDirection::Release => (
                        queue_family_index,
                        vk::QUEUE_FAMILY_FOREIGN_EXT,
                        local_layout,
                        vk::ImageLayout::GENERAL,
                        local_access,
                        vk::AccessFlags::empty(),
                    ),
                };
            vk::ImageMemoryBarrier::default()
                .src_access_mask(src_access)
                .dst_access_mask(dst_access)
                .old_layout(old_layout)
                .new_layout(new_layout)
                .src_queue_family_index(src_family)
                .dst_queue_family_index(dst_family)
                .image(*image)
                .subresource_range(
                    vk::ImageSubresourceRange::default()
                        .aspect_mask(vk::ImageAspectFlags::COLOR)
                        .level_count(1)
                        .layer_count(1),
                )
        })
        .collect();
    (src_stage, dst_stage, barriers)
}

fn texture_descriptor(
    descriptor: &DmabufDescriptor,
    probe: bool,
) -> Result<wgpu::TextureDescriptor<'static>, ImportError> {
    if descriptor.width == 0 || descriptor.height == 0 {
        return Err(ImportError::InvalidDimensions);
    }
    if descriptor.planes.is_empty() {
        return Err(ImportError::NoPlanes);
    }
    let vulkan_format = drm_to_vulkan(descriptor.fourcc)
        .ok_or(ImportError::UnsupportedFourcc(descriptor.fourcc))?;
    let format =
        vulkan_to_wgpu(vulkan_format).ok_or(ImportError::UnsupportedFourcc(descriptor.fourcc))?;
    Ok(wgpu::TextureDescriptor {
        label: Some("Wayland DMA-BUF"),
        size: wgpu::Extent3d {
            width: descriptor.width,
            height: descriptor.height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING
            | if probe {
                wgpu::TextureUsages::COPY_SRC
            } else {
                wgpu::TextureUsages::empty()
            },
        view_formats: &[],
    })
}

fn import_texture(
    render_device: &RenderDevice,
    descriptor: DmabufDescriptor,
    instrumentation: ImportInstrumentation,
) -> Result<CachedTexture, ImportError> {
    let wgpu_descriptor = texture_descriptor(&descriptor, instrumentation.probe)?;
    let vulkan_format = drm_to_vulkan(descriptor.fourcc)
        .ok_or(ImportError::UnsupportedFourcc(descriptor.fourcc))?;
    let fourcc = descriptor.fourcc;
    let client_modifier = descriptor.modifier;
    let width = descriptor.width;
    let height = descriptor.height;
    let plane_count = descriptor.planes.len();
    let strides = instrumentation.log_metadata.then(|| {
        descriptor
            .planes
            .iter()
            .map(|plane| plane.stride)
            .collect::<Vec<_>>()
    });
    let probe = instrumentation.probe.then_some(DmabufProbeIdentity {
        buffer_id: instrumentation.buffer_id,
        import_number: instrumentation.import_number,
        rows: [0, height / 2, height - 1],
        width: width.min(256),
    });

    let hal_descriptor = HalTextureDescriptor {
        label: Some("Wayland DMA-BUF"),
        size: wgpu_descriptor.size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu_descriptor.format,
        usage: TextureUses::RESOURCE
            | if instrumentation.probe {
                TextureUses::COPY_SRC
            } else {
                TextureUses::empty()
            },
        memory_flags: MemoryFlags::empty(),
        view_formats: Vec::new(),
    };
    let render_device_for_drop = render_device.clone();
    let hal_texture = unsafe {
        let Some(device) = render_device.wgpu_device().as_hal::<Vulkan>() else {
            return Err(ImportError::NotVulkan);
        };
        let (image, memories, created_modifier) = import_vulkan_image(
            &device,
            descriptor,
            vulkan_format,
            vk::ImageUsageFlags::SAMPLED
                | if instrumentation.probe {
                    vk::ImageUsageFlags::TRANSFER_SRC
                } else {
                    vk::ImageUsageFlags::empty()
                },
            &[],
        )?;
        if let Err(error) =
            submit_raw_sampled_ownership_barrier(&device, &[image], OwnershipDirection::Acquire)
        {
            cleanup_vulkan_import(&device, image, &memories);
            return Err(error);
        }
        if let Some(strides) = &strides {
            info!(
                buffer_id = instrumentation.buffer_id.0,
                import_number = instrumentation.import_number,
                fourcc = format_args!("{fourcc:#010x}"),
                client_modifier = format_args!("{client_modifier:#018x}"),
                planes = plane_count,
                plane_strides = ?strides,
                size = ?(width, height),
                vulkan_tiling = "DRM_FORMAT_MODIFIER_EXT",
                vulkan_modifier = format_args!("{created_modifier:#018x}"),
                "DMA-BUF Vulkan import created"
            );
        }
        let drop_callback: wgpu_hal::DropCallback = Box::new(move || {
            // SAFETY: The imported memory and image are destroyed only after
            // all wgpu texture references have gone away. Capturing
            // RenderDevice keeps the Vulkan device alive through cleanup.
            if let Some(device) = render_device_for_drop.wgpu_device().as_hal::<Vulkan>() {
                device.raw_device().destroy_image(image, None);
                for memory in memories {
                    device.raw_device().free_memory(memory, None);
                }
            }
        });
        device.texture_from_raw(
            image,
            &hal_descriptor,
            Some(drop_callback),
            TextureMemory::External,
        )
    };
    let (wgpu_texture, tracker_seed) = unsafe {
        // The fallible raw acquire above completed GENERAL ->
        // SHADER_READ_ONLY_OPTIMAL before this call claims RESOURCE in
        // wgpu-core. An acquire failure returns through the ordinary import
        // failure path without installing or sampling this texture.
        render_device
            .wgpu_device()
            .create_texture_from_hal_with_initial_usage::<Vulkan>(
                hal_texture,
                &wgpu_descriptor,
                TextureUses::RESOURCE,
            )
    };
    debug_assert_eq!(tracker_seed, TextureUses::RESOURCE);
    let texture = Texture::from(wgpu_texture);
    let texture_view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    if instrumentation.log_format_metadata {
        let components = vk::ComponentMapping::default();
        info!(
            fourcc = format_args!("{fourcc:#010x}"),
            ?vulkan_format,
            wgpu_format = ?wgpu_descriptor.format,
            view_component_r = ?components.r,
            view_component_g = ?components.g,
            view_component_b = ?components.b,
            view_component_a = ?components.a,
            drm_alpha = if is_opaque(fourcc) { "undefined-x" } else { "real" },
            "DMA-BUF format sampling contract"
        );
    }
    Ok(CachedTexture {
        texture,
        texture_view,
        descriptor: wgpu_descriptor,
        probe,
    })
}

pub(crate) unsafe fn import_vulkan_image(
    device: &wgpu_hal::vulkan::Device,
    descriptor: DmabufDescriptor,
    format: vk::Format,
    usage: vk::ImageUsageFlags,
    // When non-empty this is the complete Vulkan format list, including the
    // base image format as wgpu-hal does for mutable-format textures.
    view_formats: &[vk::Format],
) -> Result<(vk::Image, Vec<vk::DeviceMemory>, u64), ImportError> {
    let properties = modifier_properties(
        device.shared_instance().raw_instance(),
        device.raw_physical_device(),
        format,
        descriptor.modifier,
    )
    .ok_or(ImportError::UnsupportedModifier {
        fourcc: descriptor.fourcc,
        modifier: descriptor.modifier,
    })?;
    if properties.drm_format_modifier_plane_count as usize != descriptor.planes.len() {
        return Err(ImportError::PlaneCount {
            expected: properties.drm_format_modifier_plane_count,
            actual: descriptor.planes.len(),
        });
    }
    let disjoint = descriptor.planes.len() > 1
        && properties
            .drm_format_modifier_tiling_features
            .contains(vk::FormatFeatureFlags2::DISJOINT);
    if descriptor.planes.len() > 1 && !disjoint {
        return Err(ImportError::NonDisjointMultiPlane);
    }
    if descriptor.planes.len() > 4 {
        return Err(ImportError::PlaneCount {
            expected: 4,
            actual: descriptor.planes.len(),
        });
    }

    let layouts = descriptor
        .planes
        .iter()
        .map(|plane| {
            vk::SubresourceLayout::default()
                .offset(u64::from(plane.offset))
                .row_pitch(u64::from(plane.stride))
        })
        .collect::<Vec<_>>();
    let mut modifier_info = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
        .drm_format_modifier(descriptor.modifier)
        .plane_layouts(&layouts);
    let mut external_info = vk::ExternalMemoryImageCreateInfo::default()
        .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let mut flags = if disjoint {
        vk::ImageCreateFlags::DISJOINT
    } else {
        vk::ImageCreateFlags::empty()
    };
    if !view_formats.is_empty() {
        flags |= vk::ImageCreateFlags::MUTABLE_FORMAT;
    }
    let mut format_list = vk::ImageFormatListCreateInfo::default().view_formats(view_formats);
    let mut image_info = vk::ImageCreateInfo::default()
        .flags(flags)
        .image_type(vk::ImageType::TYPE_2D)
        .format(format)
        .extent(vk::Extent3D {
            width: descriptor.width,
            height: descriptor.height,
            depth: 1,
        })
        .mip_levels(1)
        .array_layers(1)
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
        .usage(usage)
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
        .initial_layout(vk::ImageLayout::UNDEFINED)
        .push_next(&mut modifier_info)
        .push_next(&mut external_info);
    if !view_formats.is_empty() {
        image_info = image_info.push_next(&mut format_list);
    }
    let image = unsafe { device.raw_device().create_image(&image_info, None)? };
    let modifier_extension = ash::ext::image_drm_format_modifier::Device::new(
        device.shared_instance().raw_instance(),
        device.raw_device(),
    );
    let mut created_properties = vk::ImageDrmFormatModifierPropertiesEXT::default();
    if let Err(error) = unsafe {
        modifier_extension.get_image_drm_format_modifier_properties(image, &mut created_properties)
    } {
        unsafe {
            device.raw_device().destroy_image(image, None);
        }
        return Err(ImportError::Vulkan(error));
    }
    let created_modifier = created_properties.drm_format_modifier;
    if created_modifier != descriptor.modifier {
        unsafe {
            device.raw_device().destroy_image(image, None);
        }
        return Err(ImportError::CreatedModifierMismatch {
            requested: descriptor.modifier,
            created: created_modifier,
        });
    }
    let memory_properties = unsafe {
        device
            .shared_instance()
            .raw_instance()
            .get_physical_device_memory_properties(device.raw_physical_device())
    };
    let mut memories = Vec::with_capacity(descriptor.planes.len());
    let external_memory_fd = ash::khr::external_memory_fd::Device::new(
        device.shared_instance().raw_instance(),
        device.raw_device(),
    );

    for (index, plane) in descriptor.planes.into_iter().enumerate() {
        let aspect = if disjoint {
            memory_plane_aspect(index)?
        } else {
            vk::ImageAspectFlags::COLOR
        };
        let mut plane_requirements =
            vk::ImagePlaneMemoryRequirementsInfo::default().plane_aspect(aspect);
        let requirements_info = if disjoint {
            vk::ImageMemoryRequirementsInfo2::default()
                .image(image)
                .push_next(&mut plane_requirements)
        } else {
            vk::ImageMemoryRequirementsInfo2::default().image(image)
        };
        let mut dedicated_requirements = vk::MemoryDedicatedRequirements::default();
        let mut requirements =
            vk::MemoryRequirements2::default().push_next(&mut dedicated_requirements);
        unsafe {
            device
                .raw_device()
                .get_image_memory_requirements2(&requirements_info, &mut requirements);
        }
        let mut fd_properties = vk::MemoryFdPropertiesKHR::default();
        if let Err(error) = unsafe {
            external_memory_fd.get_memory_fd_properties(
                vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT,
                plane.fd.as_raw_fd(),
                &mut fd_properties,
            )
        } {
            cleanup_vulkan_import(device, image, &memories);
            return Err(ImportError::Vulkan(error));
        }
        let compatible_memory_types =
            requirements.memory_requirements.memory_type_bits & fd_properties.memory_type_bits;
        let Some(memory_type_index) =
            select_memory_type(&memory_properties, compatible_memory_types)
        else {
            cleanup_vulkan_import(device, image, &memories);
            return Err(ImportError::NoMemoryType);
        };

        let raw_fd = plane.fd.as_raw_fd();
        let mut fd_info = vk::ImportMemoryFdInfoKHR::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
            .fd(raw_fd);
        let mut dedicated_info = vk::MemoryDedicatedAllocateInfo::default().image(image);
        let mut allocation_info = vk::MemoryAllocateInfo::default()
            .allocation_size(requirements.memory_requirements.size)
            .memory_type_index(memory_type_index)
            .push_next(&mut fd_info);
        if dedicated_requirements.requires_dedicated_allocation != 0 {
            allocation_info = allocation_info.push_next(&mut dedicated_info);
        }
        let memory = match unsafe { device.raw_device().allocate_memory(&allocation_info, None) } {
            Ok(memory) => {
                mem::forget(plane.fd);
                memory
            }
            Err(allocation_error) => {
                cleanup_vulkan_import(device, image, &memories);
                return Err(ImportError::Vulkan(allocation_error));
            }
        };
        memories.push(memory);

        let bind_result = if disjoint {
            let mut plane_info = vk::BindImagePlaneMemoryInfo::default().plane_aspect(aspect);
            let bind = vk::BindImageMemoryInfo::default()
                .image(image)
                .memory(memory)
                .push_next(&mut plane_info);
            unsafe { device.raw_device().bind_image_memory2(&[bind]) }
        } else {
            let bind = vk::BindImageMemoryInfo::default()
                .image(image)
                .memory(memory);
            unsafe { device.raw_device().bind_image_memory2(&[bind]) }
        };
        if let Err(bind_error) = bind_result {
            cleanup_vulkan_import(device, image, &memories);
            return Err(ImportError::Vulkan(bind_error));
        }
    }

    Ok((image, memories, created_modifier))
}

fn select_memory_type(
    properties: &vk::PhysicalDeviceMemoryProperties,
    allowed: u32,
) -> Option<u32> {
    let types = properties.memory_types_as_slice();
    types
        .iter()
        .enumerate()
        .find(|(index, memory_type)| {
            allowed & (1 << index) != 0
                && memory_type
                    .property_flags
                    .contains(vk::MemoryPropertyFlags::DEVICE_LOCAL)
        })
        .or_else(|| {
            types
                .iter()
                .enumerate()
                .find(|(index, _)| allowed & (1 << index) != 0)
        })
        .and_then(|(index, _)| u32::try_from(index).ok())
}

fn memory_plane_aspect(index: usize) -> Result<vk::ImageAspectFlags, ImportError> {
    match index {
        0 => Ok(vk::ImageAspectFlags::MEMORY_PLANE_0_EXT),
        1 => Ok(vk::ImageAspectFlags::MEMORY_PLANE_1_EXT),
        2 => Ok(vk::ImageAspectFlags::MEMORY_PLANE_2_EXT),
        3 => Ok(vk::ImageAspectFlags::MEMORY_PLANE_3_EXT),
        _ => Err(ImportError::PlaneCount {
            expected: 4,
            actual: index + 1,
        }),
    }
}

fn cleanup_vulkan_import(
    device: &wgpu_hal::vulkan::Device,
    image: vk::Image,
    memories: &[vk::DeviceMemory],
) {
    // SAFETY: Called only for resources created by this import attempt and not
    // handed to wgpu.
    unsafe {
        device.raw_device().destroy_image(image, None);
        for memory in memories {
            device.raw_device().free_memory(*memory, None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::{
        MinimalPlugins,
        app::SubApp,
        asset::{Asset, AssetPlugin},
        camera::CameraPlugin,
        core_pipeline::CorePipelinePlugin,
        ecs::{
            error::{FallbackErrorHandler, ignore},
            system::RunSystemOnce,
            world::World,
        },
        image::ImagePlugin,
        mesh::MeshPlugin,
        reflect::TypePath,
        render::{
            ExtractSchedule, RenderApp, RenderPlugin,
            render_asset::ExtractedAssets,
            render_resource::{AsBindGroup, OwnedBindingResource, TextureViewId},
            renderer::{RenderAdapter, RenderAdapterInfo, RenderInstance, WgpuWrapper},
            settings::RenderCreation,
        },
        sprite_render::{
            Material2dPlugin, Mesh2dRenderPlugin, PreparedMaterial2d, SpriteMaterial,
            SpriteMaterialPlugin,
        },
        transform::TransformPlugin,
        window::{ExitCondition, WindowPlugin},
    };
    use drm_fourcc::{DrmFourcc, DrmModifier};
    use std::{
        future::Future,
        pin::pin,
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        task::{Context, Poll, Waker},
    };

    struct DropWitness {
        dropped: Arc<AtomicUsize>,
        cache_cleared: Arc<AtomicBool>,
    }

    impl Drop for DropWitness {
        fn drop(&mut self) {
            assert!(
                self.cache_cleared.load(Ordering::SeqCst),
                "logical eviction must follow view-cache invalidation"
            );
            self.dropped.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn capture_ownership_barriers_use_foreign_and_transfer() {
        use ash::vk::Handle;

        let image = vk::Image::from_raw(7);
        let (src_stage, dst_stage, acquire) = ownership_barriers(
            3,
            &[image],
            OwnershipDirection::Acquire,
            OwnershipRole::CaptureDestination,
        );
        assert_eq!(src_stage, vk::PipelineStageFlags::TOP_OF_PIPE);
        assert_eq!(dst_stage, vk::PipelineStageFlags::TRANSFER);
        assert_eq!(
            acquire[0].src_queue_family_index,
            vk::QUEUE_FAMILY_FOREIGN_EXT
        );
        assert_eq!(acquire[0].dst_queue_family_index, 3);
        assert_eq!(acquire[0].old_layout, vk::ImageLayout::GENERAL);
        assert_eq!(acquire[0].new_layout, vk::ImageLayout::TRANSFER_DST_OPTIMAL);
        assert_eq!(acquire[0].dst_access_mask, vk::AccessFlags::TRANSFER_WRITE);

        let (src_stage, dst_stage, release) = ownership_barriers(
            3,
            &[image],
            OwnershipDirection::Release,
            OwnershipRole::CaptureDestination,
        );
        assert_eq!(src_stage, vk::PipelineStageFlags::TRANSFER);
        assert_eq!(dst_stage, vk::PipelineStageFlags::BOTTOM_OF_PIPE);
        assert_eq!(release[0].src_queue_family_index, 3);
        assert_eq!(
            release[0].dst_queue_family_index,
            vk::QUEUE_FAMILY_FOREIGN_EXT
        );
        assert_eq!(release[0].old_layout, vk::ImageLayout::TRANSFER_DST_OPTIMAL);
        assert_eq!(release[0].new_layout, vk::ImageLayout::GENERAL);
        assert_eq!(release[0].src_access_mask, vk::AccessFlags::TRANSFER_WRITE);
    }

    struct FailingImportPlatform;

    impl ImportPlatform<&'static str> for FailingImportPlatform {
        type Error = &'static str;

        fn import_texture(
            &self,
            _descriptor: DmabufDescriptor,
            _instrumentation: ImportInstrumentation,
        ) -> Result<&'static str, Self::Error> {
            Err("synthetic import failure")
        }
    }

    struct CountingImportPlatform(Arc<AtomicUsize>);

    impl ImportPlatform<usize> for CountingImportPlatform {
        type Error = &'static str;

        fn import_texture(
            &self,
            _descriptor: DmabufDescriptor,
            _instrumentation: ImportInstrumentation,
        ) -> Result<usize, Self::Error> {
            Ok(self.0.fetch_add(1, Ordering::SeqCst) + 1)
        }
    }

    struct UnexpectedCachedImportPlatform(Arc<AtomicUsize>);

    impl ImportPlatform<CachedTexture> for UnexpectedCachedImportPlatform {
        type Error = &'static str;

        fn import_texture(
            &self,
            _descriptor: DmabufDescriptor,
            _instrumentation: ImportInstrumentation,
        ) -> Result<CachedTexture, Self::Error> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err("cache miss")
        }
    }

    fn dummy_descriptor() -> DmabufDescriptor {
        let fd = std::fs::File::open("/dev/null")
            .expect("/dev/null is available")
            .into();
        DmabufDescriptor {
            width: 64,
            height: 32,
            fourcc: DrmFourcc::Argb8888 as u32,
            modifier: u64::from(DrmModifier::Linear),
            planes: vec![crate::DmabufPlane {
                fd,
                offset: 0,
                stride: 256,
            }],
        }
    }

    #[test]
    fn descriptor_maps_to_sampled_bgra_texture() {
        let descriptor =
            texture_descriptor(&dummy_descriptor(), false).expect("supported descriptor");
        assert_eq!(descriptor.format, wgpu::TextureFormat::Bgra8Unorm);
        assert_eq!(descriptor.usage, wgpu::TextureUsages::TEXTURE_BINDING);
        assert_eq!(descriptor.size.width, 64);
        assert_eq!(descriptor.size.height, 32);
    }

    #[test]
    fn probe_usage_is_absent_by_default_and_added_only_when_requested() {
        let ordinary = texture_descriptor(&dummy_descriptor(), false).expect("ordinary descriptor");
        let probed = texture_descriptor(&dummy_descriptor(), true).expect("probe descriptor");

        assert!(!ordinary.usage.contains(wgpu::TextureUsages::COPY_SRC));
        assert!(probed.usage.contains(wgpu::TextureUsages::COPY_SRC));
        assert!(probed.usage.contains(wgpu::TextureUsages::TEXTURE_BINDING));
    }

    #[test]
    fn no_cache_import_bypasses_an_existing_backing_and_does_not_repopulate() {
        let imports = Arc::new(AtomicUsize::new(0));
        let platform = CountingImportPlatform(Arc::clone(&imports));
        let buffer_id = DmabufBufferId(17);
        let cached = Arc::new(99_usize);
        let mut cache = ImportCache::default();
        assert!(cache.insert(buffer_id, Arc::clone(&cached), 1));

        let Ok(ready) = attempt_pending_import(
            &platform,
            &mut cache,
            PendingImport {
                buffer_id,
                descriptor: dummy_descriptor(),
                release: ReleaseLease::new(DmabufRelease::Explicit(Box::new(|| {}))),
                previous: None,
                cacheable: true,
            },
            true,
            ImportInstrumentation {
                buffer_id,
                import_number: 1,
                ..Default::default()
            },
        ) else {
            panic!("no-cache import succeeds");
        };

        assert!(ready.newly_imported);
        assert_eq!(*ready.current.backing, 1);
        assert_eq!(imports.load(Ordering::SeqCst), 1);
        assert!(Arc::ptr_eq(&cache.entries[&buffer_id].backing, &cached));
    }

    #[test]
    fn probe_rate_limit_is_first_three_then_every_sixtieth_per_buffer() {
        let buffer_id = DmabufBufferId(23);
        let fourcc = DrmFourcc::Argb8888 as u32;
        let mut debug = DmabufDebugState {
            options: DmabufDebugOptions {
                no_cache: true,
                probe: true,
                log_imports: false,
            },
            ..Default::default()
        };

        for expected in 1..=4 {
            let planned = plan_import_instrumentation(&debug, buffer_id, fourcc, true);
            assert_eq!(planned.import_number, expected);
            assert_eq!(planned.probe, expected <= 3);
            assert_eq!(planned.log_metadata, expected == 1);
            assert_eq!(planned.log_format_metadata, expected == 1);
            record_successful_import(&mut debug, fourcc, planned);
        }
        debug.import_counts.insert(buffer_id, 59);
        let sixtieth = plan_import_instrumentation(&debug, buffer_id, fourcc, true);
        assert_eq!(sixtieth.import_number, 60);
        assert!(sixtieth.probe);
        assert!(!sixtieth.log_metadata);
        assert!(!sixtieth.log_format_metadata);
    }

    #[test]
    fn import_logging_is_silent_by_default_and_observational_when_enabled() {
        let buffer_id = DmabufBufferId(29);
        let argb = DrmFourcc::Argb8888 as u32;
        let default_debug = DmabufDebugState::default();
        let silent = plan_import_instrumentation(&default_debug, buffer_id, argb, true);
        assert_eq!(silent.import_number, 0);
        assert!(!silent.log_metadata);
        assert!(!silent.log_format_metadata);
        assert!(!silent.probe);

        let registry = ImportedDmabufImages::default();
        registry.enable_import_logging();
        let mut imports = registry
            .0
            .lock()
            .expect("DMA-BUF import registry mutex poisoned");
        let log_only = &mut imports.debug;
        assert!(
            !log_only.options.no_cache,
            "logging must leave caching enabled"
        );
        assert!(
            !log_only.options.probe,
            "logging must not enable sampling probes"
        );

        let first = plan_import_instrumentation(log_only, buffer_id, argb, true);
        assert_eq!(first.import_number, 1);
        assert!(first.log_metadata);
        assert!(first.log_format_metadata);
        assert!(!first.probe);
        record_successful_import(log_only, argb, first);

        let repeated = plan_import_instrumentation(log_only, buffer_id, argb, true);
        assert!(!repeated.log_metadata);
        assert!(!repeated.log_format_metadata);

        let new_buffer = plan_import_instrumentation(
            log_only,
            DmabufBufferId(30),
            DrmFourcc::Abgr8888 as u32,
            true,
        );
        assert!(new_buffer.log_metadata);
        assert!(new_buffer.log_format_metadata);
        assert!(!new_buffer.probe);
    }

    #[test]
    fn configure_debug_preserves_previously_enabled_import_logging() {
        let registry = ImportedDmabufImages::default();
        registry.enable_import_logging();
        registry.configure_debug(true, true);

        let imports = registry
            .0
            .lock()
            .expect("DMA-BUF import registry mutex poisoned");
        assert!(imports.debug.options.no_cache);
        assert!(imports.debug.options.probe);
        assert!(
            imports.debug.options.log_imports,
            "orthogonal debug configuration must not silently disable import logging"
        );
    }

    #[test]
    fn debug_configuration_stays_sealed_after_the_last_import_is_unregistered() {
        fn imported_then_unregistered() -> ImportedDmabufImages {
            let imports = ImportedDmabufImages::default();
            let mut images = Assets::<Image>::default();
            let handle = imports
                .import(
                    &mut images,
                    DmabufBufferId(31),
                    true,
                    dummy_descriptor(),
                    DmabufRelease::Explicit(Box::new(|| {})),
                )
                .expect("valid first import is registered");
            imports.unregister(&handle);
            assert!(imports.0.lock().expect("registry mutex").active.is_empty());
            imports
        }

        let imports = imported_then_unregistered();
        assert!(
            std::panic::catch_unwind(|| imports.configure_debug(true, true)).is_err(),
            "configure_debug must remain sealed after import then unregister"
        );

        let imports = imported_then_unregistered();
        assert!(
            std::panic::catch_unwind(|| imports.enable_import_logging()).is_err(),
            "enable_import_logging must remain sealed after import then unregister"
        );
    }

    #[test]
    fn probe_summary_reports_alpha_bytes_separately_for_each_row() {
        let rows = [
            [1, 2, 3, 0x00, 4, 5, 6, 0xff],
            [7, 8, 9, 0x7f, 0, 0, 0, 0x00],
            [10, 11, 12, 0xff, 13, 14, 15, 0x80],
        ];
        let data = rows.into_iter().flatten().collect::<Vec<_>>();
        let summary = summarize_probe_data(&data, 8, 8).expect("three complete rows");

        assert_eq!(summary.alpha_zero, [1, 1, 0]);
        assert_eq!(summary.alpha_ff, [1, 0, 1]);
        assert_eq!(summary.alpha_other, [0, 1, 1]);
        assert_eq!(summary.nonzero_bytes, [7, 4, 8]);
    }

    #[test]
    fn hal_import_seed_makes_the_first_resource_use_already_initialised() {
        use wgpu_hal::Device as _;

        let (device, queue) = wgpu::Device::noop(&wgpu::DeviceDescriptor::default());
        let descriptor = wgpu::TextureDescriptor {
            label: Some("initial-usage noop probe"),
            size: wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        };
        let hal_descriptor = HalTextureDescriptor {
            label: descriptor.label,
            size: descriptor.size,
            mip_level_count: descriptor.mip_level_count,
            sample_count: descriptor.sample_count,
            dimension: descriptor.dimension,
            format: descriptor.format,
            usage: TextureUses::RESOURCE,
            memory_flags: MemoryFlags::empty(),
            view_formats: Vec::new(),
        };
        let hal_texture = unsafe {
            device
                .as_hal::<wgpu_hal::api::Noop>()
                .expect("noop device exposes noop HAL")
                .create_texture(&hal_descriptor)
                .expect("noop HAL texture")
        };
        let (texture, tracker_seed) = unsafe {
            device.create_texture_from_hal_with_initial_usage::<wgpu_hal::api::Noop>(
                hal_texture,
                &descriptor,
                TextureUses::RESOURCE,
            )
        };

        assert_eq!(tracker_seed, TextureUses::RESOURCE);
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("first imported texture sample transition probe"),
        });
        encoder.transition_resources(
            std::iter::empty(),
            [wgpu::TextureTransition {
                texture: &texture,
                selector: None,
                state: TextureUses::RESOURCE,
            }]
            .into_iter(),
        );
        queue.submit([encoder.finish()]);
        device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("noop first-use submission completes");
    }

    #[test]
    fn xrgb_descriptor_uses_the_same_sampled_bgra_storage() {
        let mut descriptor = dummy_descriptor();
        descriptor.fourcc = DrmFourcc::Xrgb8888 as u32;
        let texture = texture_descriptor(&descriptor, false).expect("supported opaque descriptor");

        assert_eq!(texture.format, wgpu::TextureFormat::Bgra8Unorm);
        assert!(descriptor.is_opaque());
    }

    #[test]
    fn zero_sized_descriptor_is_rejected() {
        let mut descriptor = dummy_descriptor();
        descriptor.width = 0;
        assert!(matches!(
            texture_descriptor(&descriptor, false),
            Err(ImportError::InvalidDimensions)
        ));
    }

    #[test]
    fn rejected_first_import_releases_logical_callback_without_registering_an_asset() {
        let imports = ImportedDmabufImages::default();
        let mut images = Assets::<Image>::default();
        let released = Arc::new(AtomicUsize::new(0));
        let callback_released = Arc::clone(&released);
        let mut descriptor = dummy_descriptor();
        descriptor.width = 0;

        let result = imports.import(
            &mut images,
            DmabufBufferId(1),
            true,
            descriptor,
            DmabufRelease::Explicit(Box::new(move || {
                callback_released.fetch_add(1, Ordering::SeqCst);
            })),
        );

        assert!(matches!(result, Err(ImportError::InvalidDimensions)));
        assert_eq!(released.load(Ordering::SeqCst), 1);
        assert!(images.is_empty());
        assert!(imports.0.lock().expect("registry mutex").active.is_empty());
    }

    #[test]
    fn replacement_keeps_asset_id_and_releases_superseded_pending_buffer() {
        let imports = ImportedDmabufImages::default();
        let mut images = Assets::<Image>::default();
        let released = Arc::new(AtomicUsize::new(0));
        let first_released = Arc::clone(&released);
        let handle = imports
            .import(
                &mut images,
                DmabufBufferId(1),
                true,
                dummy_descriptor(),
                DmabufRelease::Explicit(Box::new(move || {
                    first_released.fetch_add(1, Ordering::SeqCst);
                })),
            )
            .expect("first import is registered");
        let original_id = handle.id();

        imports
            .replace(
                &handle,
                DmabufBufferId(2),
                true,
                dummy_descriptor(),
                DmabufRelease::Implicit(Box::new(|| {})),
            )
            .expect("registered image can be replaced");

        assert_eq!(handle.id(), original_id);
        assert_eq!(released.load(Ordering::SeqCst), 1);
        assert!(matches!(
            imports
                .0
                .lock()
                .expect("registry mutex")
                .active
                .get(&original_id),
            Some(ImportState::Pending(_))
        ));
    }

    #[test]
    fn registration_survives_many_replacements_until_explicit_unregister() {
        let imports = ImportedDmabufImages::default();
        let mut images = Assets::<Image>::default();
        let released = Arc::new(AtomicUsize::new(0));
        let first_released = Arc::clone(&released);
        let handle = imports
            .import(
                &mut images,
                DmabufBufferId(1),
                true,
                dummy_descriptor(),
                DmabufRelease::Implicit(Box::new(move || {
                    first_released.fetch_add(1, Ordering::SeqCst);
                })),
            )
            .expect("first import is registered");

        for _ in 0..8 {
            let replacement_released = Arc::clone(&released);
            imports
                .replace(
                    &handle,
                    DmabufBufferId(2),
                    true,
                    dummy_descriptor(),
                    DmabufRelease::Implicit(Box::new(move || {
                        replacement_released.fetch_add(1, Ordering::SeqCst);
                    })),
                )
                .expect("surface registration survives replacement");
        }
        assert_eq!(released.load(Ordering::SeqCst), 8);

        imports.unregister(&handle);
        assert_eq!(released.load(Ordering::SeqCst), 9);
        imports.unregister(&handle);
        assert_eq!(released.load(Ordering::SeqCst), 9);
        assert!(
            !imports
                .0
                .lock()
                .expect("registry mutex")
                .active
                .contains_key(&handle.id())
        );
    }

    #[test]
    fn rejected_replacement_releases_its_buffer_callback() {
        let imports = ImportedDmabufImages::default();
        let mut images = Assets::<Image>::default();
        let handle = images.add(Image::default());
        let released = Arc::new(AtomicUsize::new(0));
        let rejected_released = Arc::clone(&released);

        assert!(matches!(
            imports.replace(
                &handle,
                DmabufBufferId(1),
                true,
                dummy_descriptor(),
                DmabufRelease::Implicit(Box::new(move || {
                    rejected_released.fetch_add(1, Ordering::SeqCst);
                })),
            ),
            Err(ImportError::UnregisteredImage)
        ));
        assert_eq!(released.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn invalid_replacement_releases_only_new_request_and_preserves_current_pending_import() {
        let imports = ImportedDmabufImages::default();
        let mut images = Assets::<Image>::default();
        let current_released = Arc::new(AtomicUsize::new(0));
        let current_callback = Arc::clone(&current_released);
        let handle = imports
            .import(
                &mut images,
                DmabufBufferId(1),
                true,
                dummy_descriptor(),
                DmabufRelease::Explicit(Box::new(move || {
                    current_callback.fetch_add(1, Ordering::SeqCst);
                })),
            )
            .expect("initial pending import is registered");
        let rejected_released = Arc::new(AtomicUsize::new(0));
        let rejected_callback = Arc::clone(&rejected_released);
        let mut invalid = dummy_descriptor();
        invalid.height = 0;

        let result = imports.replace(
            &handle,
            DmabufBufferId(2),
            true,
            invalid,
            DmabufRelease::Explicit(Box::new(move || {
                rejected_callback.fetch_add(1, Ordering::SeqCst);
            })),
        );

        assert!(matches!(result, Err(ImportError::InvalidDimensions)));
        assert_eq!(rejected_released.load(Ordering::SeqCst), 1);
        assert_eq!(current_released.load(Ordering::SeqCst), 0);
        assert!(matches!(
            imports
                .0
                .lock()
                .expect("registry mutex")
                .active
                .get(&handle.id()),
            Some(ImportState::Pending(_))
        ));
        imports.unregister(&handle);
        assert_eq!(current_released.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn failed_replacement_restores_previous_applied_value() {
        let previous = previous_for_replacement(ImportState::Applied("previous texture"));
        assert!(matches!(
            failed_import_fallback(previous),
            ImportState::Applied("previous texture")
        ));
        assert!(matches!(
            failed_import_fallback::<&str>(None),
            ImportState::Idle
        ));
    }

    #[test]
    #[should_panic(
        expected = "Imported replacement state cannot escape apply_imports with a previous texture"
    )]
    fn imported_replacement_with_previous_cannot_reach_replacement_selection() {
        let _ = previous_for_replacement(ImportState::Imported(ReadyImport {
            current: "current texture",
            previous: Some("previous texture"),
            newly_imported: true,
            probe_after_acquire: false,
        }));
    }

    #[test]
    fn unregistering_imported_state_retires_current_and_previous_textures() {
        let retired = retired_textures_for_unregister(ImportState::Imported(ReadyImport {
            current: "current texture",
            previous: Some("previous texture"),
            newly_imported: true,
            probe_after_acquire: false,
        }));

        assert_eq!(retired, ["previous texture", "current texture"]);
    }

    #[test]
    fn failed_import_releases_new_logical_lease_and_returns_previous_texture() {
        let released = Arc::new(AtomicUsize::new(0));
        let released_by_callback = Arc::clone(&released);
        let pending = PendingImport {
            buffer_id: DmabufBufferId(1),
            descriptor: dummy_descriptor(),
            release: ReleaseLease::new(DmabufRelease::Explicit(Box::new(move || {
                released_by_callback.fetch_add(1, Ordering::SeqCst);
            }))),
            previous: Some(ImportedUse::new(
                Arc::new("previous texture"),
                ReleaseLease::new(DmabufRelease::Explicit(Box::new(|| {}))),
            )),
            cacheable: true,
        };

        let result = attempt_pending_import(
            &FailingImportPlatform,
            &mut ImportCache::default(),
            pending,
            false,
            ImportInstrumentation::default(),
        );

        let Err((error, previous)) = result else {
            panic!("fake import must fail");
        };
        assert_eq!(error, "synthetic import failure");
        assert_eq!(
            previous.as_ref().map(|imported| *imported.backing),
            Some("previous texture")
        );
        assert_eq!(released.load(Ordering::SeqCst), 1);
    }

    #[derive(Resource)]
    struct OwnershipCadenceHarness {
        active: HashMap<AssetId<Image>, ImportState<ImportedUse<usize>>>,
        cache: ImportCache<usize>,
        preacquired: HashSet<usize>,
        local_owned: HashSet<usize>,
        ownership_retired: Vec<ImportedUse<usize>>,
        acquire_submissions: usize,
        release_submissions: usize,
        sampled_updates: usize,
    }

    fn run_rendered_ownership_update(mut harness: ResMut<OwnershipCadenceHarness>) {
        let batch =
            pending_acquire_batch(&harness.active, &harness.preacquired, &harness.local_owned);
        if !batch.submitted_backings.is_empty() {
            harness.acquire_submissions += 1;
        }
        let Ok(updates) = complete_acquire(
            &mut harness.active,
            Ok::<(), &'static str>(()),
            &batch.submitted_ids,
            &batch.ready_without_submission,
        ) else {
            panic!("synthetic acquire succeeds");
        };
        harness.preacquired.clear();
        for backing in &batch.ready_backings {
            harness.local_owned.insert(Arc::as_ptr(backing) as usize);
        }
        let local_owned = harness.local_owned.clone();
        retire_imported_uses(
            &mut harness.ownership_retired,
            &local_owned,
            updates.retired,
        );
        if harness
            .active
            .values()
            .any(|state| matches!(state, ImportState::Applied(_)))
        {
            harness.sampled_updates += 1;
        }
        let release_backings = releasable_retired_backings(
            &harness.active,
            &harness.ownership_retired,
            &harness.local_owned,
        );
        if release_backings.is_empty() {
            return;
        }
        harness.release_submissions += 1;
        let OwnershipCadenceHarness {
            cache,
            local_owned,
            ownership_retired,
            ..
        } = &mut *harness;
        let Ok(completed) = complete_release(
            local_owned,
            cache,
            ownership_retired,
            &release_backings,
            Ok::<(), &'static str>(()),
        ) else {
            panic!("synthetic release succeeds");
        };
        drop(completed);
    }

    #[test]
    fn one_commit_remains_owned_across_rendered_updates_until_replacement() {
        const STATIC_UPDATES: usize = 8;

        let imports = Arc::new(AtomicUsize::new(0));
        let releases = Arc::new(AtomicUsize::new(0));
        let platform = CountingImportPlatform(Arc::clone(&imports));
        let mut images = Assets::<Image>::default();
        let image_id = images.add(Image::default()).id();
        let mut cache = ImportCache::default();
        let first_release = Arc::clone(&releases);
        let Ok(first) = attempt_pending_import(
            &platform,
            &mut cache,
            PendingImport {
                buffer_id: DmabufBufferId(41),
                descriptor: dummy_descriptor(),
                release: ReleaseLease::new(DmabufRelease::Explicit(Box::new(move || {
                    first_release.fetch_add(1, Ordering::SeqCst);
                }))),
                previous: None,
                cacheable: true,
            },
            false,
            ImportInstrumentation::default(),
        ) else {
            panic!("first fake import succeeds");
        };
        assert!(first.newly_imported);
        let first_identity = Arc::as_ptr(&first.current.backing) as usize;
        let harness = OwnershipCadenceHarness {
            active: HashMap::from([(image_id, ImportState::Imported(first))]),
            cache,
            preacquired: HashSet::from([first_identity]),
            local_owned: HashSet::new(),
            ownership_retired: Vec::new(),
            // Production performs the fresh-import acquire inside import_texture.
            acquire_submissions: 1,
            release_submissions: 0,
            sampled_updates: 0,
        };
        let mut app = App::new();
        app.insert_resource(harness)
            .add_systems(bevy::app::Update, run_rendered_ownership_update);

        for _ in 0..STATIC_UPDATES {
            app.update();
        }
        {
            let harness = app.world().resource::<OwnershipCadenceHarness>();
            assert_eq!(harness.sampled_updates, STATIC_UPDATES);
            assert_eq!(harness.acquire_submissions, 1);
            assert_eq!(harness.release_submissions, 0);
            assert_eq!(releases.load(Ordering::SeqCst), 0);
            assert!(harness.local_owned.contains(&first_identity));
            assert!(matches!(
                harness.active.get(&image_id),
                Some(ImportState::Applied(_))
            ));
        }

        let replacement_release = Arc::clone(&releases);
        let replacement_identity = {
            let mut harness = app.world_mut().resource_mut::<OwnershipCadenceHarness>();
            let previous = previous_for_replacement(
                harness
                    .active
                    .remove(&image_id)
                    .expect("displayed first use remains installed"),
            );
            let OwnershipCadenceHarness { cache, .. } = &mut *harness;
            assert!(cache.insert(DmabufBufferId(42), Arc::new(42), 1));
            let Ok(replacement) = attempt_pending_import(
                &platform,
                cache,
                PendingImport {
                    buffer_id: DmabufBufferId(42),
                    descriptor: dummy_descriptor(),
                    release: ReleaseLease::new(DmabufRelease::Explicit(Box::new(move || {
                        replacement_release.fetch_add(1, Ordering::SeqCst);
                    }))),
                    previous,
                    cacheable: true,
                },
                false,
                ImportInstrumentation::default(),
            ) else {
                panic!("replacement fake import succeeds");
            };
            assert!(!replacement.newly_imported);
            let replacement_identity = Arc::as_ptr(&replacement.current.backing) as usize;
            harness
                .active
                .insert(image_id, ImportState::Imported(replacement));
            replacement_identity
        };

        app.update();
        let harness = app.world().resource::<OwnershipCadenceHarness>();
        assert_eq!(imports.load(Ordering::SeqCst), 1);
        assert_eq!(harness.acquire_submissions, 2);
        assert_eq!(harness.release_submissions, 1);
        assert_eq!(releases.load(Ordering::SeqCst), 1);
        assert!(!harness.local_owned.contains(&first_identity));
        assert!(harness.local_owned.contains(&replacement_identity));

        for _ in 0..STATIC_UPDATES {
            app.update();
        }
        let harness = app.world().resource::<OwnershipCadenceHarness>();
        assert_eq!(harness.sampled_updates, STATIC_UPDATES * 2 + 1);
        assert_eq!(harness.acquire_submissions, 2);
        assert_eq!(harness.release_submissions, 1);
        assert_eq!(releases.load(Ordering::SeqCst), 1);
    }

    fn noop_cached_texture(device: &wgpu::Device, label: &'static str) -> Arc<CachedTexture> {
        let descriptor = wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        };
        let texture = device.create_texture(&descriptor);
        let texture_view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        Arc::new(CachedTexture {
            texture: texture.into(),
            texture_view: texture_view.into(),
            descriptor,
            probe: None,
        })
    }

    fn noop_gpu_image_with_format(
        device: &wgpu::Device,
        label: &'static str,
        format: wgpu::TextureFormat,
    ) -> GpuImage {
        let descriptor = wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        };
        let texture = device.create_texture(&descriptor);
        GpuImage {
            texture_view: texture
                .create_view(&wgpu::TextureViewDescriptor::default())
                .into(),
            texture: texture.into(),
            sampler: device
                .create_sampler(&wgpu::SamplerDescriptor::default())
                .into(),
            texture_descriptor: descriptor,
            texture_view_descriptor: None,
            had_data: false,
        }
    }

    fn noop_gpu_image(device: &wgpu::Device, label: &'static str) -> GpuImage {
        noop_gpu_image_with_format(device, label, wgpu::TextureFormat::Rgba8Unorm)
    }

    fn poll_ready<F: Future>(future: F) -> F::Output {
        let mut future = pin!(future);
        let mut context = Context::from_waker(Waker::noop());
        match future.as_mut().poll(&mut context) {
            Poll::Ready(output) => output,
            Poll::Pending => panic!("noop wgpu future unexpectedly remained pending"),
        }
    }

    fn noop_render_plugin() -> (RenderPlugin, wgpu::Device, wgpu::Queue) {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::NOOP,
            backend_options: wgpu::BackendOptions {
                noop: wgpu::NoopBackendOptions { enable: true },
                ..Default::default()
            },
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });
        let adapter = poll_ready(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
            .expect("noop adapter exists");
        let adapter_info = adapter.get_info();
        let (device, queue) = poll_ready(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("DMA-BUF bind-group regression device"),
            ..Default::default()
        }))
        .expect("noop device exists");
        let render_creation = RenderCreation::manual(
            device.clone().into(),
            RenderQueue(Arc::new(WgpuWrapper::new(queue.clone()))),
            RenderAdapterInfo(WgpuWrapper::new(adapter_info)),
            RenderAdapter(Arc::new(WgpuWrapper::new(adapter))),
            RenderInstance(Arc::new(WgpuWrapper::new(instance))),
        );
        (
            RenderPlugin {
                render_creation,
                synchronous_pipeline_compilation: true,
                ..Default::default()
            },
            device,
            queue,
        )
    }

    fn add_headless_render_plugins(app: &mut App, render_plugin: RenderPlugin) {
        // Keep this list explicit: DefaultPlugins changes with Cargo feature
        // unification, which can silently add Winit and pipelined rendering when
        // these tests are built beside a windowed Bevy package.
        app.add_plugins(MinimalPlugins).add_plugins((
            TransformPlugin,
            WindowPlugin {
                primary_window: None,
                exit_condition: ExitCondition::DontExit,
                close_when_requested: false,
                ..Default::default()
            },
            AssetPlugin::default(),
            render_plugin,
            ImagePlugin::default(),
            MeshPlugin,
            CameraPlugin,
            CorePipelinePlugin,
            Mesh2dRenderPlugin,
            SpriteMaterialPlugin,
        ));
    }

    fn prepared_sprite_material_texture_view(
        render_world: &World,
        material: AssetId<SpriteMaterial>,
    ) -> TextureViewId {
        render_world
            .resource::<RenderAssets<PreparedMaterial2d<SpriteMaterial>>>()
            .get(material)
            .expect("sprite material is prepared")
            .bindings
            .iter()
            .find_map(|(binding, resource)| match (binding, resource) {
                (1, OwnedBindingResource::TextureView(_, view)) => Some(view.id()),
                _ => None,
            })
            .expect("sprite material binding 1 is its image texture view")
    }

    #[derive(Asset, AsBindGroup, Clone, TypePath)]
    struct TestClientSurfaceMaterial {
        #[texture(1)]
        #[sampler(2)]
        image: Handle<Image>,
    }

    impl Material2d for TestClientSurfaceMaterial {}

    fn prepared_client_surface_material_texture_view(
        render_world: &World,
        material: AssetId<TestClientSurfaceMaterial>,
    ) -> TextureViewId {
        render_world
            .resource::<RenderAssets<PreparedMaterial2d<TestClientSurfaceMaterial>>>()
            .get(material)
            .expect("client surface material is prepared")
            .bindings
            .iter()
            .find_map(|(binding, resource)| match (binding, resource) {
                (1, OwnedBindingResource::TextureView(_, view)) => Some(view.id()),
                _ => None,
            })
            .expect("client surface material binding 1 is its image texture view")
    }

    fn exercise_registered_client_surface_material_replacement()
    -> (TextureViewId, TextureViewId, TextureViewId, bool, bool) {
        let (render_plugin, device, _queue) = noop_render_plugin();
        let mut app = App::new();
        add_headless_render_plugins(&mut app, render_plugin);
        app.add_plugins((
            Material2dPlugin::<TestClientSurfaceMaterial>::default(),
            DmabufImportPlugin,
        ));
        app.register_dmabuf_material_2d::<TestClientSurfaceMaterial>();
        app.finish();
        app.cleanup();
        app.update();

        let mut images = Assets::<Image>::default();
        let replaced_image = images.add(Image::default());
        let sibling_image = images.add(Image::default());
        let mut materials = Assets::<TestClientSurfaceMaterial>::default();
        let replaced_material = materials
            .add(TestClientSurfaceMaterial {
                image: replaced_image.clone(),
            })
            .id();
        let sibling_material = materials
            .add(TestClientSurfaceMaterial {
                image: sibling_image.clone(),
            })
            .id();
        let replaced_material_asset = materials
            .get(replaced_material)
            .expect("replaced client surface material exists")
            .clone();
        let sibling_material_asset = materials
            .get(sibling_material)
            .expect("sibling client surface material exists")
            .clone();
        let render_world = app.sub_app_mut(RenderApp).world_mut();
        {
            let mut gpu_images = render_world.resource_mut::<RenderAssets<GpuImage>>();
            gpu_images.insert(
                replaced_image.id(),
                noop_gpu_image(&device, "old client surface image"),
            );
            gpu_images.insert(
                sibling_image.id(),
                noop_gpu_image(&device, "sibling client surface image"),
            );
        }
        {
            let mut extracted = render_world
                .resource_mut::<ExtractedAssets<PreparedMaterial2d<TestClientSurfaceMaterial>>>();
            extracted.extracted.extend([
                (replaced_material, replaced_material_asset.clone()),
                (sibling_material, sibling_material_asset),
            ]);
        }
        render_world
            .run_system_once(prepare_assets::<PreparedMaterial2d<TestClientSurfaceMaterial>>)
            .expect("initial client-surface material preparation runs");

        let old_view =
            prepared_client_surface_material_texture_view(render_world, replaced_material);
        let replaced_bind_group = render_world
            .resource::<RenderAssets<PreparedMaterial2d<TestClientSurfaceMaterial>>>()
            .get(replaced_material)
            .expect("replaced material is initially prepared")
            .bind_group
            .id();
        let sibling_bind_group = render_world
            .resource::<RenderAssets<PreparedMaterial2d<TestClientSurfaceMaterial>>>()
            .get(sibling_material)
            .expect("sibling material is initially prepared")
            .bind_group
            .id();
        let replacement = noop_cached_texture(&device, "new client surface image");
        let replacement_view = replacement.texture_view.id();
        assert_ne!(old_view, replacement_view);

        {
            let imports = render_world.resource::<ImportedDmabufImages>();
            let mut registry = imports
                .0
                .lock()
                .expect("DMA-BUF import registry mutex is available");
            let identity = Arc::as_ptr(&replacement) as usize;
            registry.local_owned.insert(identity);
            registry.active.insert(
                replaced_image.id(),
                ImportState::Imported(ReadyImport {
                    current: ImportedUse::new(
                        replacement,
                        ReleaseLease::new(DmabufRelease::Explicit(Box::new(|| {}))),
                    ),
                    previous: None,
                    newly_imported: false,
                    probe_after_acquire: false,
                }),
            );
        }
        render_world
            .resource_mut::<ExtractedAssets<PreparedMaterial2d<TestClientSurfaceMaterial>>>()
            .extracted
            .push((replaced_material, replaced_material_asset));

        render_world.run_schedule(Render);

        let prepared_view =
            prepared_client_surface_material_texture_view(render_world, replaced_material);
        let replaced_rebound = render_world
            .resource::<RenderAssets<PreparedMaterial2d<TestClientSurfaceMaterial>>>()
            .get(replaced_material)
            .expect("replaced material stays prepared")
            .bind_group
            .id()
            != replaced_bind_group;
        let sibling_unchanged = render_world
            .resource::<RenderAssets<PreparedMaterial2d<TestClientSurfaceMaterial>>>()
            .get(sibling_material)
            .expect("sibling material stays prepared")
            .bind_group
            .id()
            == sibling_bind_group;
        (
            old_view,
            replacement_view,
            prepared_view,
            replaced_rebound,
            sibling_unchanged,
        )
    }

    #[test]
    fn dmabuf_install_precedes_client_surface_material_prepare() {
        let (old_view, replacement_view, prepared_view, _, _) =
            exercise_registered_client_surface_material_replacement();
        assert_ne!(old_view, replacement_view);
        assert_eq!(prepared_view, replacement_view);
    }

    #[test]
    fn dmabuf_replacement_rebinds_only_the_changed_client_material() {
        let (_, _, _, replaced_rebound, sibling_unchanged) =
            exercise_registered_client_surface_material_replacement();
        assert!(
            replaced_rebound,
            "the changed material gets a new bind group"
        );
        assert!(
            sibling_unchanged,
            "the unchanged sibling keeps its cached bind group"
        );
    }

    #[test]
    fn dmabuf_import_systems_run_on_render_schedule_without_sprite_plugins() {
        let (_render_plugin, device, queue) = noop_render_plugin();
        let mut app = App::new();
        let mut render_app = SubApp::new();
        render_app.init_schedule(ExtractSchedule);
        render_app.init_schedule(Render);
        render_app
            .world_mut()
            .insert_resource(RenderDevice::from(device.clone()));
        render_app
            .world_mut()
            .insert_resource(RenderQueue(Arc::new(WgpuWrapper::new(queue))));
        render_app
            .world_mut()
            .insert_resource(RenderAssets::<GpuImage>::default());
        app.insert_sub_app(RenderApp, render_app);
        assert!(!app.is_plugin_added::<SpriteMaterialPlugin>());
        assert!(
            app.sub_app(RenderApp)
                .world()
                .get_resource::<SpriteAssetEvents>()
                .is_none(),
            "the no-sprite fixture must begin without SpriteAssetEvents",
        );

        let mut ids = Assets::<Image>::default();
        app.add_plugins(DmabufImportPlugin);
        app.finish();
        app.cleanup();
        let imports = app.world().resource::<ImportedDmabufImages>().clone();
        let render_world = app.sub_app_mut(RenderApp).world_mut();
        render_world.insert_resource(imports);
        render_world.insert_resource(FallbackErrorHandler(ignore));

        let installed_id = ids.add(Image::default()).id();
        let retired_id = ids.add(Image::default()).id();
        let replacement = noop_cached_texture(&device, "no-sprite imported image");
        let replacement_view = replacement.texture_view.id();
        let retired_release_count = Arc::new(AtomicUsize::new(0));
        let released = Arc::clone(&retired_release_count);
        render_world
            .resource_mut::<RenderAssets<GpuImage>>()
            .insert(
                installed_id,
                noop_gpu_image(&device, "no-sprite placeholder"),
            );
        {
            let imports = render_world.resource::<ImportedDmabufImages>();
            let mut registry = imports
                .0
                .lock()
                .expect("DMA-BUF import registry mutex is available");
            registry
                .preacquired
                .insert(Arc::as_ptr(&replacement) as usize);
            registry.active.insert(
                installed_id,
                ImportState::Imported(ReadyImport {
                    current: ImportedUse::new(
                        replacement,
                        ReleaseLease::new(DmabufRelease::Explicit(Box::new(|| {}))),
                    ),
                    previous: None,
                    newly_imported: false,
                    probe_after_acquire: false,
                }),
            );
            registry.retired.insert(
                retired_id,
                vec![ImportedUse::new(
                    noop_cached_texture(&device, "no-sprite retired image"),
                    ReleaseLease::new(DmabufRelease::Explicit(Box::new(move || {
                        released.fetch_add(1, Ordering::SeqCst);
                    }))),
                )],
            );
        }

        render_world.run_schedule(Render);
        let render_world: &World = render_world;

        assert!(
            matches!(
                render_world
                    .resource::<ImportedDmabufImages>()
                    .0
                    .lock()
                    .expect("DMA-BUF import registry mutex is available")
                    .active
                    .get(&installed_id),
                Some(ImportState::Applied(_))
            ),
            "the installed Render schedule must execute DMA-BUF apply/acquire without sprite plugins; if SpriteAssetEvents is absent the systems are skipped",
        );
        assert_eq!(
            retired_release_count.load(Ordering::SeqCst),
            1,
            "apply_imports must retire an unreferenced imported image",
        );
        assert_eq!(
            render_world
                .resource::<RenderAssets<GpuImage>>()
                .get(installed_id)
                .expect("acquire installs the imported GPU image")
                .texture_view
                .id(),
            replacement_view,
            "acquire_external_images must install the imported texture view",
        );
        let events = render_world.get_resource::<SpriteAssetEvents>();
        assert!(
            events.is_some(),
            "DmabufImportPlugin must initialise SpriteAssetEvents for its scheduled ResMut systems",
        );
        let events = &events.expect("resource existence asserted").images;
        assert!(events.contains(&AssetEvent::Modified { id: retired_id }));
        assert!(events.contains(&AssetEvent::Modified { id: installed_id }));
    }

    #[test]
    fn dmabuf_install_precedes_real_sprite_material_prepare_and_leaves_sibling_cached() {
        let (render_plugin, device, _queue) = noop_render_plugin();
        let mut app = App::new();
        add_headless_render_plugins(&mut app, render_plugin);
        app.add_plugins(DmabufImportPlugin);
        app.finish();
        app.cleanup();
        app.update();

        let mut images = Assets::<Image>::default();
        let replaced_image = images.add(Image::default());
        let sibling_image = images.add(Image::default());
        let mut materials = Assets::<SpriteMaterial>::default();
        let replaced_material = materials
            .add(SpriteMaterial {
                image: replaced_image.clone(),
                ..Default::default()
            })
            .id();
        let sibling_material = materials
            .add(SpriteMaterial {
                image: sibling_image.clone(),
                ..Default::default()
            })
            .id();
        let replaced_material_asset = materials
            .get(replaced_material)
            .expect("replaced sprite material exists")
            .clone();
        let sibling_material_asset = materials
            .get(sibling_material)
            .expect("sibling sprite material exists")
            .clone();
        let render_world = app.sub_app_mut(RenderApp).world_mut();
        {
            let mut gpu_images = render_world.resource_mut::<RenderAssets<GpuImage>>();
            gpu_images.insert(
                replaced_image.id(),
                noop_gpu_image(&device, "old replaced image"),
            );
            gpu_images.insert(sibling_image.id(), noop_gpu_image(&device, "sibling image"));
        }
        {
            let mut extracted =
                render_world.resource_mut::<ExtractedAssets<PreparedMaterial2d<SpriteMaterial>>>();
            extracted.extracted.extend([
                (replaced_material, replaced_material_asset.clone()),
                (sibling_material, sibling_material_asset),
            ]);
        }
        render_world
            .run_system_once(prepare_assets::<PreparedMaterial2d<SpriteMaterial>>)
            .expect("initial Bevy sprite-material preparation runs");

        let old_view = prepared_sprite_material_texture_view(render_world, replaced_material);
        let sibling_bind_group = render_world
            .resource::<RenderAssets<PreparedMaterial2d<SpriteMaterial>>>()
            .get(sibling_material)
            .expect("sibling material is prepared")
            .bind_group
            .id();
        let replacement = noop_cached_texture(&device, "new replaced image");
        let replacement_view = replacement.texture_view.id();
        assert_ne!(old_view, replacement_view);

        {
            let imports = render_world.resource::<ImportedDmabufImages>();
            let mut registry = imports
                .0
                .lock()
                .expect("DMA-BUF import registry mutex is available");
            let identity = Arc::as_ptr(&replacement) as usize;
            registry.local_owned.insert(identity);
            registry.active.insert(
                replaced_image.id(),
                ImportState::Imported(ReadyImport {
                    current: ImportedUse::new(
                        replacement,
                        ReleaseLease::new(DmabufRelease::Explicit(Box::new(|| {}))),
                    ),
                    previous: None,
                    newly_imported: false,
                    probe_after_acquire: false,
                }),
            );
        }
        render_world
            .resource_mut::<ExtractedAssets<PreparedMaterial2d<SpriteMaterial>>>()
            .extracted
            .push((replaced_material, replaced_material_asset));

        render_world.run_schedule(Render);

        assert_eq!(
            prepared_sprite_material_texture_view(render_world, replaced_material),
            replacement_view,
            "the next real Bevy sprite-material prepare must bind the installed texture view",
        );
        assert_eq!(
            render_world
                .resource::<RenderAssets<PreparedMaterial2d<SpriteMaterial>>>()
                .get(sibling_material)
                .expect("sibling material stays prepared")
                .bind_group
                .id(),
            sibling_bind_group,
            "a replacement must not rebuild a sibling material bind group",
        );
    }

    #[test]
    fn alternating_cached_buffers_install_distinct_gpu_images_on_every_commit() {
        let (device, _queue) = wgpu::Device::noop(&wgpu::DeviceDescriptor::default());
        let buffer_a = noop_cached_texture(&device, "cached buffer A");
        let buffer_b = noop_cached_texture(&device, "cached buffer B");
        let mut cache = ImportCache::default();
        assert!(cache.insert(DmabufBufferId(51), Arc::clone(&buffer_a), 1));
        assert!(cache.insert(DmabufBufferId(52), Arc::clone(&buffer_b), 1));

        let mut images = Assets::<Image>::default();
        let image_id = images.add(Image::default()).id();
        let mut gpu_images = RenderAssets::<GpuImage>::default();
        gpu_images.insert(image_id, noop_gpu_image(&device, "surface placeholder"));
        let mut active = HashMap::from([(image_id, ImportState::Idle)]);
        let mut local_owned = HashSet::new();
        let mut ownership_retired = Vec::new();
        let imports = Arc::new(AtomicUsize::new(0));
        let platform = UnexpectedCachedImportPlatform(Arc::clone(&imports));

        for (buffer_id, expected) in [
            (DmabufBufferId(51), &buffer_a),
            (DmabufBufferId(52), &buffer_b),
            (DmabufBufferId(51), &buffer_a),
            (DmabufBufferId(52), &buffer_b),
        ] {
            let previous = previous_for_replacement(
                active
                    .remove(&image_id)
                    .expect("surface import registration remains live"),
            );
            let Ok(ready) = attempt_pending_import(
                &platform,
                &mut cache,
                PendingImport {
                    buffer_id,
                    descriptor: dummy_descriptor(),
                    release: ReleaseLease::new(DmabufRelease::Explicit(Box::new(|| {}))),
                    previous,
                    cacheable: true,
                },
                false,
                ImportInstrumentation::default(),
            ) else {
                panic!("cached commit resolves");
            };
            assert!(!ready.newly_imported, "both ring buffers stay cached");
            active.insert(image_id, ImportState::Imported(ready));

            let batch = pending_acquire_batch(&active, &HashSet::new(), &local_owned);
            assert_eq!(batch.submitted_ids, HashSet::from([image_id]));
            let Ok(updates) = complete_acquire(
                &mut active,
                Ok::<(), &'static str>(()),
                &batch.submitted_ids,
                &batch.ready_without_submission,
            ) else {
                panic!("cached buffer acquires on this commit");
            };
            for backing in &batch.ready_backings {
                local_owned.insert(Arc::as_ptr(backing) as usize);
            }

            assert_eq!(
                install_gpu_images(&mut gpu_images, &active, &updates.install),
                vec![image_id],
            );
            assert_eq!(
                gpu_images
                    .get(image_id)
                    .expect("surface GpuImage remains installed")
                    .texture
                    .id(),
                expected.texture.id(),
                "the stable surface asset must sample the committed cached buffer",
            );
            let mut sprite_asset_events = SpriteAssetEvents::default();
            let retired =
                finish_import_updates([image_id], &mut sprite_asset_events, updates.retired);
            assert_eq!(
                sprite_asset_events.images,
                vec![AssetEvent::Modified { id: image_id }],
            );
            retire_imported_uses(&mut ownership_retired, &local_owned, retired);

            let release_backings =
                releasable_retired_backings(&active, &ownership_retired, &local_owned);
            if !release_backings.is_empty() {
                let Ok(completed) = complete_release(
                    &mut local_owned,
                    &mut cache,
                    &mut ownership_retired,
                    &release_backings,
                    Ok::<(), &'static str>(()),
                ) else {
                    panic!("superseded cached buffer returns to FOREIGN ownership");
                };
                drop(completed);
            }
        }

        assert_eq!(
            imports.load(Ordering::SeqCst),
            0,
            "the alternating commits exercised cache hits only",
        );
    }

    #[test]
    fn import_cache_lru_stays_bounded_and_reimports_evicted_buffers() {
        let imports = Arc::new(AtomicUsize::new(0));
        let platform = CountingImportPlatform(Arc::clone(&imports));
        let mut cache = ImportCache::with_limits(3, usize::MAX);

        fn use_buffer(
            platform: &CountingImportPlatform,
            cache: &mut ImportCache<usize>,
            buffer_id: u64,
        ) {
            let Ok(ready) = attempt_pending_import(
                platform,
                cache,
                PendingImport {
                    buffer_id: DmabufBufferId(buffer_id),
                    descriptor: dummy_descriptor(),
                    release: ReleaseLease::new(DmabufRelease::Explicit(Box::new(|| {}))),
                    previous: None,
                    cacheable: true,
                },
                false,
                ImportInstrumentation::default(),
            ) else {
                panic!("synthetic cache import succeeds");
            };
            drop(ready);
        }

        use_buffer(&platform, &mut cache, 1);
        use_buffer(&platform, &mut cache, 2);
        use_buffer(&platform, &mut cache, 3);
        assert_eq!(imports.load(Ordering::SeqCst), 3);
        use_buffer(&platform, &mut cache, 2);
        assert_eq!(
            imports.load(Ordering::SeqCst),
            3,
            "recent use is a cache hit"
        );

        use_buffer(&platform, &mut cache, 4);
        assert_eq!(cache.entries.len(), 3);
        assert!(!cache.entries.contains_key(&DmabufBufferId(1)));
        assert!(cache.entries.contains_key(&DmabufBufferId(2)));

        use_buffer(&platform, &mut cache, 1);
        assert_eq!(
            imports.load(Ordering::SeqCst),
            5,
            "evicted buffer reimports"
        );
        assert_eq!(cache.entries.len(), 3);
        use_buffer(&platform, &mut cache, 1);
        assert_eq!(
            imports.load(Ordering::SeqCst),
            5,
            "reimported buffer immediately returns to the cache-hit path"
        );
    }

    #[test]
    fn cache_budget_charges_the_dmabuf_allocation_not_only_the_visible_row_span() {
        use std::{ffi::CString, os::fd::FromRawFd};

        const ALLOCATION_BYTES: u64 = 64 * 1024 * 1024;
        const LARGE_OFFSET: u32 = 48 * 1024 * 1024;
        let name = CString::new("cosmix-dmabuf-cache-budget-test").expect("static memfd name");
        let raw_fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
        assert!(
            raw_fd >= 0,
            "memfd_create failed: {}",
            std::io::Error::last_os_error()
        );
        let file = unsafe { std::fs::File::from_raw_fd(raw_fd) };
        file.set_len(ALLOCATION_BYTES)
            .expect("sparse memfd allocation can be sized");
        let descriptor = DmabufDescriptor {
            width: 1,
            height: 1,
            fourcc: DrmFourcc::Argb8888 as u32,
            modifier: u64::from(DrmModifier::Linear),
            planes: vec![crate::DmabufPlane {
                fd: file.into(),
                offset: LARGE_OFFSET,
                stride: 4,
            }],
        };
        let allocation_bytes =
            usize::try_from(ALLOCATION_BYTES).expect("test allocation fits usize");
        let imports = Arc::new(AtomicUsize::new(0));
        let platform = CountingImportPlatform(Arc::clone(&imports));
        let mut cache = ImportCache::with_limits(2, allocation_bytes * 2);

        let Ok(_ready) = attempt_pending_import(
            &platform,
            &mut cache,
            PendingImport {
                buffer_id: DmabufBufferId(91),
                descriptor,
                release: ReleaseLease::new(DmabufRelease::Explicit(Box::new(|| {}))),
                previous: None,
                cacheable: true,
            },
            false,
            ImportInstrumentation::default(),
        ) else {
            panic!("synthetic import succeeds");
        };

        assert_eq!(imports.load(Ordering::SeqCst), 1);
        assert_eq!(cache.bytes, allocation_bytes);
        assert_eq!(cache.entries[&DmabufBufferId(91)].bytes, allocation_bytes);
    }

    #[test]
    fn injected_acquire_failure_installs_nothing_and_retires_the_import() {
        let releases = Arc::new(AtomicUsize::new(0));
        let current_release = Arc::clone(&releases);
        let previous_release = Arc::clone(&releases);
        let mut images = Assets::<Image>::default();
        let image_id = images.add(Image::default()).id();
        let mut active = HashMap::from([(
            image_id,
            ImportState::Imported(ReadyImport {
                current: ImportedUse::new(
                    Arc::new(2_usize),
                    ReleaseLease::new(DmabufRelease::Explicit(Box::new(move || {
                        current_release.fetch_add(1, Ordering::SeqCst);
                    }))),
                ),
                previous: Some(ImportedUse::new(
                    Arc::new(1_usize),
                    ReleaseLease::new(DmabufRelease::Explicit(Box::new(move || {
                        previous_release.fetch_add(1, Ordering::SeqCst);
                    }))),
                )),
                newly_imported: false,
                probe_after_acquire: false,
            }),
        )]);

        let Err(failure) = complete_acquire(
            &mut active,
            Err("injected acquire failure"),
            &HashSet::from([image_id]),
            &HashSet::new(),
        ) else {
            panic!("injected barrier failure is surfaced");
        };

        assert_eq!(failure.error, "injected acquire failure");
        assert!(
            failure.updates.install.is_empty(),
            "failed import is never installed"
        );
        assert_eq!(failure.updates.uninstall, [image_id]);
        assert!(matches!(active.get(&image_id), Some(ImportState::Idle)));
        assert!(current_import(active.get(&image_id).unwrap()).is_none());
        assert_eq!(releases.load(Ordering::SeqCst), 0);
        drop(failure);
        assert_eq!(releases.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn cache_hit_acquire_failure_does_not_touch_static_siblings() {
        let static_releases = Arc::new(AtomicUsize::new(0));
        let failed_releases = Arc::new(AtomicUsize::new(0));
        let mut images = Assets::<Image>::default();
        let static_a_id = images.add(Image::default()).id();
        let failed_id = images.add(Image::default()).id();
        let static_c_id = images.add(Image::default()).id();
        let static_a_release = Arc::clone(&static_releases);
        let static_c_release = Arc::clone(&static_releases);
        let failed_release = Arc::clone(&failed_releases);
        let static_a = Arc::new("static A");
        let failed_b = Arc::new("cache-hit B");
        let static_c = Arc::new("static C");
        let mut cache = ImportCache::default();
        assert!(cache.insert(DmabufBufferId(1), Arc::clone(&static_a), 1));
        assert!(cache.insert(DmabufBufferId(2), Arc::clone(&failed_b), 1));
        assert!(cache.insert(DmabufBufferId(3), Arc::clone(&static_c), 1));
        let mut active = HashMap::from([
            (
                static_a_id,
                ImportState::Applied(ImportedUse::new(
                    Arc::clone(&static_a),
                    ReleaseLease::new(DmabufRelease::Explicit(Box::new(move || {
                        static_a_release.fetch_add(1, Ordering::SeqCst);
                    }))),
                )),
            ),
            (
                failed_id,
                ImportState::Imported(ReadyImport {
                    current: ImportedUse::new(
                        Arc::clone(&failed_b),
                        ReleaseLease::new(DmabufRelease::Explicit(Box::new(move || {
                            failed_release.fetch_add(1, Ordering::SeqCst);
                        }))),
                    ),
                    previous: None,
                    newly_imported: false,
                    probe_after_acquire: false,
                }),
            ),
            (
                static_c_id,
                ImportState::Applied(ImportedUse::new(
                    Arc::clone(&static_c),
                    ReleaseLease::new(DmabufRelease::Explicit(Box::new(move || {
                        static_c_release.fetch_add(1, Ordering::SeqCst);
                    }))),
                )),
            ),
        ]);
        let batch = pending_acquire_batch(&active, &HashSet::new(), &HashSet::new());
        assert_eq!(batch.submitted_ids, HashSet::from([failed_id]));
        evict_cache_backings(&mut cache, &batch.submitted_backings);

        let Err(failure) = complete_acquire(
            &mut active,
            Err("injected cache-hit acquire failure"),
            &batch.submitted_ids,
            &batch.ready_without_submission,
        ) else {
            panic!("cache-hit barrier failure is surfaced");
        };

        assert!(failure.updates.install.is_empty());
        assert_eq!(failure.updates.uninstall, [failed_id]);
        assert!(matches!(
            active.get(&static_a_id),
            Some(ImportState::Applied(_))
        ));
        assert_eq!(
            current_import(active.get(&static_a_id).expect("static A remains"))
                .map(|imported| *imported.backing),
            Some("static A")
        );
        assert!(matches!(
            active.get(&static_c_id),
            Some(ImportState::Applied(_))
        ));
        assert_eq!(
            current_import(active.get(&static_c_id).expect("static C remains"))
                .map(|imported| *imported.backing),
            Some("static C")
        );
        assert!(matches!(active.get(&failed_id), Some(ImportState::Idle)));
        assert!(cache.entries.contains_key(&DmabufBufferId(1)));
        assert!(!cache.entries.contains_key(&DmabufBufferId(2)));
        assert!(cache.entries.contains_key(&DmabufBufferId(3)));
        assert_eq!(static_releases.load(Ordering::SeqCst), 0);
        assert_eq!(failed_releases.load(Ordering::SeqCst), 0);
        drop(failure);
        assert_eq!(static_releases.load(Ordering::SeqCst), 0);
        assert_eq!(failed_releases.load(Ordering::SeqCst), 1);
        drop(active);
        assert_eq!(static_releases.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn failed_release_strands_use_without_callback_and_next_commit_reimports() {
        let buffer_id = DmabufBufferId(73);
        let backing = Arc::new(900_usize);
        let old_identity = Arc::as_ptr(&backing) as usize;
        let releases = Arc::new(AtomicUsize::new(0));
        let release = Arc::clone(&releases);
        let mut cache = ImportCache::default();
        assert!(cache.insert(buffer_id, Arc::clone(&backing), 1));
        let mut local_owned = HashSet::from([old_identity]);
        let mut ownership_retired = vec![ImportedUse::new(
            Arc::clone(&backing),
            ReleaseLease::new(DmabufRelease::Explicit(Box::new(move || {
                release.fetch_add(1, Ordering::SeqCst);
            }))),
        )];
        let release_backings = vec![Arc::clone(&backing)];

        let Err(failure) = complete_release(
            &mut local_owned,
            &mut cache,
            &mut ownership_retired,
            &release_backings,
            Err("injected release failure"),
        ) else {
            panic!("release failure is surfaced");
        };

        let ReleaseFailure { error } = failure;
        assert_eq!(error, "injected release failure");
        assert!(local_owned.is_empty());
        assert!(ownership_retired.is_empty());
        assert!(!cache.entries.contains_key(&buffer_id));
        assert_eq!(
            releases.load(Ordering::SeqCst),
            0,
            "a failed FOREIGN handback must strand the release callback"
        );

        let imports = Arc::new(AtomicUsize::new(0));
        let platform = CountingImportPlatform(Arc::clone(&imports));
        let Ok(ready) = attempt_pending_import(
            &platform,
            &mut cache,
            PendingImport {
                buffer_id,
                descriptor: dummy_descriptor(),
                release: ReleaseLease::new(DmabufRelease::Explicit(Box::new(|| {}))),
                previous: None,
                cacheable: true,
            },
            false,
            ImportInstrumentation::default(),
        ) else {
            panic!("terminal cache entry forces a clean re-import");
        };
        assert!(ready.newly_imported);
        assert_eq!(imports.load(Ordering::SeqCst), 1);
        assert!(!Arc::ptr_eq(&ready.current.backing, &backing));
        assert_eq!(releases.load(Ordering::SeqCst), 0);

        let mut images = Assets::<Image>::default();
        let image_id = images.add(Image::default()).id();
        let fresh_identity = Arc::as_ptr(&ready.current.backing) as usize;
        let active = HashMap::from([(image_id, ImportState::Imported(ready))]);
        let batch =
            pending_acquire_batch(&active, &HashSet::from([fresh_identity]), &HashSet::new());
        assert!(batch.submitted_backings.is_empty());
        assert_eq!(batch.ready_without_submission, HashSet::from([image_id]));
    }

    #[test]
    fn terminal_teardown_strands_actively_displayed_cached_use() {
        let imports = ImportedDmabufImages::default();
        let releases = Arc::new(AtomicUsize::new(0));
        let callback_releases = Arc::clone(&releases);
        let backing = Arc::new(901_usize);
        let mut cache = ImportCache::default();
        assert!(cache.insert(DmabufBufferId(74), Arc::clone(&backing), 1));

        let mut images = Assets::<Image>::default();
        let image_id = images.add(Image::default()).id();
        let active = HashMap::from([(
            image_id,
            ImportState::Applied(ImportedUse::new(
                Arc::clone(&backing),
                ReleaseLease::with_teardown_latch(
                    DmabufRelease::Explicit(Box::new(move || {
                        callback_releases.fetch_add(1, Ordering::SeqCst);
                    })),
                    Arc::clone(&imports.1),
                ),
            )),
        )]);

        imports.begin_terminal_teardown();
        drop(active);
        drop(cache);
        drop(backing);
        drop(imports);

        assert_eq!(
            releases.load(Ordering::SeqCst),
            0,
            "terminal App teardown must not publish an unbarriered release"
        );
    }

    #[test]
    fn buffer_destroy_evicts_cache_and_prevents_pending_reinsertion() {
        let imports = Arc::new(AtomicUsize::new(0));
        let platform = CountingImportPlatform(Arc::clone(&imports));
        let buffer_id = DmabufBufferId(72);
        let mut cache = ImportCache::with_limits(4, usize::MAX);
        assert!(cache.insert(buffer_id, Arc::new(99_usize), 1));
        let mut images = Assets::<Image>::default();
        let image_id = images.add(Image::default()).id();
        let mut active = HashMap::from([(
            image_id,
            ImportState::Pending(PendingImport {
                buffer_id,
                descriptor: dummy_descriptor(),
                release: ReleaseLease::new(DmabufRelease::Explicit(Box::new(|| {}))),
                previous: None,
                cacheable: true,
            }),
        )]);

        invalidate_buffer_cache(&mut cache, &mut active, buffer_id);

        assert!(!cache.entries.contains_key(&buffer_id));
        let Some(ImportState::Pending(pending)) = active.remove(&image_id) else {
            panic!("destroyed buffer retains its pending render use");
        };
        assert!(!pending.cacheable);
        let Ok(_) = attempt_pending_import(
            &platform,
            &mut cache,
            pending,
            false,
            ImportInstrumentation::default(),
        ) else {
            panic!("destroyed pending use may still render");
        };
        assert_eq!(imports.load(Ordering::SeqCst), 1);
        assert!(!cache.entries.contains_key(&buffer_id));
    }

    #[test]
    fn applied_replacement_invalidates_view_cache_before_previous_logical_lease_drops() {
        let dropped = Arc::new(AtomicUsize::new(0));
        let cache_cleared = Arc::new(AtomicBool::new(false));
        let previous = DropWitness {
            dropped: Arc::clone(&dropped),
            cache_cleared: Arc::clone(&cache_cleared),
        };
        let current = DropWitness {
            dropped: Arc::clone(&dropped),
            cache_cleared: Arc::clone(&cache_cleared),
        };
        let state = ImportState::Imported(ReadyImport {
            current,
            previous: Some(previous),
            newly_imported: true,
            probe_after_acquire: false,
        });

        let (applied, evictions) = apply_import_state(state);

        assert!(matches!(&applied, ImportState::Applied(_)));
        assert_eq!(dropped.load(Ordering::SeqCst), 0);
        let mut sprite_asset_events = SpriteAssetEvents::default();
        let evictions =
            finish_import_updates([AssetId::invalid()], &mut sprite_asset_events, evictions);
        assert_eq!(sprite_asset_events.images.len(), 1);
        cache_cleared.store(true, Ordering::SeqCst);
        drop(evictions);
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn unregister_releases_pending_request_but_holds_applied_previous_until_gpu_removal() {
        let pending_released = Arc::new(AtomicUsize::new(0));
        let pending_callback = Arc::clone(&pending_released);
        let dropped = Arc::new(AtomicUsize::new(0));
        let cache_cleared = Arc::new(AtomicBool::new(false));
        let previous = DropWitness {
            dropped: Arc::clone(&dropped),
            cache_cleared: Arc::clone(&cache_cleared),
        };
        let state = ImportState::Pending(PendingImport {
            buffer_id: DmabufBufferId(1),
            descriptor: dummy_descriptor(),
            release: ReleaseLease::new(DmabufRelease::Explicit(Box::new(move || {
                pending_callback.fetch_add(1, Ordering::SeqCst);
            }))),
            previous: Some(previous),
            cacheable: true,
        });

        let textures = retired_textures_for_unregister(state);

        assert_eq!(pending_released.load(Ordering::SeqCst), 1);
        assert_eq!(dropped.load(Ordering::SeqCst), 0);
        let mut retired = HashMap::from([(7_u32, textures)]);
        let (removed_ids, evictions) = collect_retired_without_gpu(&mut retired, |_| true);
        assert!(removed_ids.is_empty());
        assert!(evictions.is_empty());
        assert!(retired.contains_key(&7));
        assert_eq!(dropped.load(Ordering::SeqCst), 0);

        let (removed_ids, evictions) = collect_retired_without_gpu(&mut retired, |_| false);
        assert_eq!(removed_ids, vec![7]);
        assert!(!evictions.is_empty());
        assert!(!retired.contains_key(&7));
        assert_eq!(dropped.load(Ordering::SeqCst), 0);
        let image_id = AssetId::invalid();
        let mut sprite_asset_events = SpriteAssetEvents::default();
        let evictions = finish_import_updates([image_id], &mut sprite_asset_events, evictions);
        assert_eq!(
            sprite_asset_events.images,
            vec![AssetEvent::Modified { id: image_id }],
        );
        cache_cleared.store(true, Ordering::SeqCst);
        drop(evictions);
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn empty_import_update_does_not_invalidate_view_cache() {
        let mut sprite_asset_events = SpriteAssetEvents::default();

        finish_import_updates::<()>([], &mut sprite_asset_events, Vec::new());

        assert!(sprite_asset_events.images.is_empty());
    }

    #[test]
    fn imported_view_change_without_eviction_invalidates_view_cache() {
        let image_id = AssetId::invalid();
        let mut sprite_asset_events = SpriteAssetEvents::default();

        finish_import_updates::<()>([image_id], &mut sprite_asset_events, Vec::new());

        assert_eq!(
            sprite_asset_events.images,
            vec![AssetEvent::Modified { id: image_id }],
        );
    }

    #[test]
    fn release_mode_splits_implicit_physical_from_explicit_logical_lifetime() {
        let implicit_released = Arc::new(AtomicUsize::new(0));
        let implicit_callback = Arc::clone(&implicit_released);
        let (physical, logical) = ReleaseLease::new(DmabufRelease::Implicit(Box::new(move || {
            implicit_callback.fetch_add(1, Ordering::SeqCst);
        })))
        .split();
        assert!(physical.is_some());
        assert!(logical.is_none());
        assert_eq!(implicit_released.load(Ordering::SeqCst), 0);
        drop(physical);
        assert_eq!(implicit_released.load(Ordering::SeqCst), 1);

        let explicit_released = Arc::new(AtomicUsize::new(0));
        let explicit_callback = Arc::clone(&explicit_released);
        let (physical, logical) = ReleaseLease::new(DmabufRelease::Explicit(Box::new(move || {
            explicit_callback.fetch_add(1, Ordering::SeqCst);
        })))
        .split();
        assert!(physical.is_none());
        assert!(logical.is_some());
        assert_eq!(explicit_released.load(Ordering::SeqCst), 0);
        drop(logical);
        assert_eq!(explicit_released.load(Ordering::SeqCst), 1);
    }
}
