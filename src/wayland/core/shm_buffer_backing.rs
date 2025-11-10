use super::shm_pool::ShmPool;
use crate::wayland::{vulkano_data::VULKANO_CONTEXT, wgpu_data::WGPU_CONTEXT};
use bevy::{
	asset::{Assets, Handle},
	image::Image,
	render::render_resource::{Texture, TextureView},
};
use bevy_dmabuf::import::{ImportedDmatexs, ImportedTexture};
use mint::Vector2;
use std::sync::{
	Arc, OnceLock,
	atomic::{AtomicU64, Ordering},
};
use tracing::{debug_span, info};
use waynest_protocols::server::core::wayland::wl_shm::Format;
use wgpu_types::{
	Extent3d, TextureAspect, TextureUsages, TextureViewDescriptor, TextureViewDimension,
};

static BACKINGS: AtomicU64 = AtomicU64::new(0);

/// Parameters for a shared memory buffer
pub struct ShmBufferBacking {
	pool: Arc<ShmPool>,
	offset: usize,
	stride: usize,
	size: Vector2<usize>,
	wl_format: Format,
	image: Texture,
	view: TextureView,
	extent: Extent3d,
	tex: OnceLock<Handle<Image>>,
}

impl std::fmt::Debug for ShmBufferBacking {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("ShmBufferBacking")
			.field("pool", &self.pool)
			.field("offset", &self.offset)
			.field("stride", &self.stride)
			.field("size", &self.size)
			.field("image", &self.image)
			.field("tex", &self.tex)
			.finish()
	}
}

impl ShmBufferBacking {
	pub fn new(
		pool: Arc<ShmPool>,
		offset: usize,
		stride: usize,
		size: Vector2<usize>,
		wl_format: Format,
	) -> Self {
		let num = BACKINGS.fetch_add(1, Ordering::Relaxed) + 1;
		info!("shm backing spawned: {num}");
		let vk = VULKANO_CONTEXT.wait();
		let wgpu = WGPU_CONTEXT.wait();
		let format = match wl_format {
			Format::Argb8888 | Format::Xrgb8888 => wgpu_types::TextureFormat::Bgra8UnormSrgb,
			_ => unimplemented!(),
		};
		let extent = Extent3d {
			width: size.x as u32,
			height: size.y as u32,
			depth_or_array_layers: 1,
		};
		let descriptor = wgpu_types::TextureDescriptor::<_, &[_]> {
			label: Some("Wayland Shm Image"),
			size: extent,
			mip_level_count: 1,
			sample_count: 1,
			dimension: wgpu_types::TextureDimension::D2,
			format,
			usage: TextureUsages::COPY_DST | TextureUsages::TEXTURE_BINDING,
			view_formats: &[format],
		};
		let image = wgpu.dev.create_texture(&descriptor);
		let view = image.create_view(&TextureViewDescriptor {
			label: Some("Wayland Shm Image View"),
			format: Some(format),
			dimension: Some(TextureViewDimension::D2),
			usage: Some(TextureUsages::COPY_DST | TextureUsages::TEXTURE_BINDING),
			aspect: TextureAspect::All,
			base_mip_level: 0,
			mip_level_count: None,
			base_array_layer: 0,
			array_layer_count: None,
		});
		Self {
			pool,
			offset,
			stride,
			size,
			wl_format,
			image,
			view,
			extent,
			tex: OnceLock::new(),
		}
	}
	pub fn on_commit(&self) {
		let wgpu = WGPU_CONTEXT.wait();
		// let mut data = vec![0u8; self.size.x * self.size.y * 4];
		// {
		// 	let _span = debug_span!("copy to gpu buffer").entered();
		// 	let shm_data_lock = self.pool.data_lock();
		// 	let mut gpu_slice = &mut data;
		// 	for (shm_offset, gpu_offset) in
		// 		(0..self.size.y).map(|v| (self.offset + (v * self.stride), (v * (self.size.x * 4))))
		// 	{
		// 		let line_slice = &shm_data_lock[shm_offset..(shm_offset + (self.size.x * 4))];
		// 		let gpu_subslice = &mut gpu_slice[gpu_offset..(gpu_offset + (self.size.x * 4))];
		// 		gpu_subslice.copy_from_slice(line_slice);
		// 	}
		// }
		info!("huh?!");
		wgpu.queue.write_texture(
			self.image.as_image_copy(),
			&self.pool.data_lock(),
			wgpu_types::TexelCopyBufferLayout {
				offset: self.offset as u64,
				bytes_per_row: Some(self.stride as u32),
				rows_per_image: Some(self.size.y as u32),
			},
			self.extent,
		);
		info!("help");
		wgpu.queue.submit([]);
		info!("me");
	}

	#[tracing::instrument("debug", skip_all)]
	pub fn update_tex(
		&self,
		dmatexes: &ImportedDmatexs,
		images: &mut Assets<Image>,
	) -> Option<Handle<Image>> {
		Some(
			self.tex
				.get_or_init(|| {
					let imported = ImportedTexture::new(self.image.clone(), self.view.clone());
					dmatexes.insert_imported_dmatex(images, imported)
				})
				.clone(),
		)
	}

	pub fn is_transparent(&self) -> bool {
		match self.wl_format {
			Format::Xrgb8888 => false,
			Format::Argb8888 => true,
			_ => true,
		}
	}

	pub fn size(&self) -> Vector2<usize> {
		self.size
	}
}
impl Drop for ShmBufferBacking {
	fn drop(&mut self) {
		let num = BACKINGS.fetch_sub(1, Ordering::Relaxed) - 1;
		info!("shm backing dropped: {num}");
	}
}
