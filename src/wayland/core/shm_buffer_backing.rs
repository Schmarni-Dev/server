use super::shm_pool::ShmPool;
use crate::wayland::{RENDER_DEVICE, vulkano_data::VULKANO_CONTEXT};
use bevy::{
	asset::{Assets, Handle},
	image::Image,
};
use bevy_dmabuf::{
	dmatex::{Dmatex, DmatexPlane, Resolution},
	import::{DmatexUsage, DropCallback, ImportedDmatexs, ImportedTexture, import_texture},
};
use drm_fourcc::DrmFourcc;
use mint::Vector2;
use parking_lot::Mutex;
use std::{
	os::fd::OwnedFd,
	sync::{
		Arc, OnceLock,
		atomic::{AtomicU64, Ordering},
	},
	time::Duration,
};
use tokio::sync::{Notify, mpsc};
use tracing::{debug_span, info};
use vulkano::{
	Handle as _, VulkanError, VulkanObject,
	buffer::{BufferUsage, Subbuffer},
	command_buffer::{
		AutoCommandBufferBuilder, CommandBufferSubmitInfo, CommandBufferUsage,
		CopyBufferToImageInfo, SemaphoreSubmitInfo, SubmitInfo,
	},
	image::{
		ImageAspect, ImageCreateFlags, ImageCreateInfo, ImageMemory, ImageTiling, ImageUsage,
		sys::RawImage,
	},
	memory::{
		DedicatedAllocation, DeviceMemory, ExternalMemoryHandleType, MemoryAllocateInfo,
		MemoryPropertyFlags, ResourceMemory,
		allocator::{AllocationCreateInfo, MemoryTypeFilter},
	},
	sync::{
		fence::{FenceCreateFlags, FenceCreateInfo},
		semaphore::{Semaphore, SemaphoreType, SemaphoreWaitInfo},
	},
};
use waynest_protocols::server::core::wayland::wl_shm::Format;

static BACKINGS: AtomicU64 = AtomicU64::new(0);

/// Parameters for a shared memory buffer
pub struct ShmBufferBacking {
	pool: Arc<ShmPool>,
	offset: usize,
	stride: usize,
	size: Vector2<usize>,
	wl_format: Format,
	image: Arc<vulkano::image::Image>,
	upload_buffer: Subbuffer<[u8]>,
	tex: OnceLock<Handle<Image>>,
	pending_imported_dmatex: Mutex<Option<ImportedTexture>>,
}

impl std::fmt::Debug for ShmBufferBacking {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("ShmBufferBacking")
			.field("pool", &self.pool)
			.field("offset", &self.offset)
			.field("stride", &self.stride)
			.field("size", &self.size)
			.field("wl_format", &self.wl_format)
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
		let format = match wl_format {
			Format::Argb8888 | Format::Xrgb8888 => vulkano::format::Format::B8G8R8A8_SRGB,
			_ => unimplemented!(),
		};
		let modifiers = vk
			.phys_dev
			.format_properties(format)
			.unwrap()
			.drm_format_modifier_properties
			.into_iter()
			.filter_map(|v| {
				(v.drm_format_modifier_plane_count == 1).then_some(v.drm_format_modifier)
			})
			.collect();
		let raw_image = RawImage::new(
			vk.dev.clone(),
			ImageCreateInfo {
				flags: ImageCreateFlags::empty(),
				image_type: vulkano::image::ImageType::Dim2d,
				format,
				extent: [size.x as u32, size.y as u32, 1],
				tiling: ImageTiling::DrmFormatModifier,
				usage: ImageUsage::TRANSFER_DST,
				drm_format_modifiers: modifiers,
				external_memory_handle_types: ExternalMemoryHandleType::DmaBuf.into(),
				..Default::default()
			},
		)
		.unwrap();
		let (modifier, num_planes) = raw_image.drm_format_modifier().unwrap();

		let mem_reqs = raw_image.memory_requirements()[0];

		let props = vk.phys_dev.memory_properties();
		let index = props
			.memory_types
			.iter()
			.enumerate()
			.filter(|(i, _)| mem_reqs.memory_type_bits & (1 << i) != 0)
			.filter(|(_, v)| v.property_flags.contains(MemoryPropertyFlags::DEVICE_LOCAL))
			.reduce(|v1, v2| {
				if dbg!(props.memory_heaps[v1.1.heap_index as usize].size)
					> dbg!(props.memory_heaps[v2.1.heap_index as usize].size)
				{
					v1
				} else {
					v2
				}
			})
			.inspect(|(_, mem)| info!(?mem))
			.map(|(i, _)| i as u32)
			.expect("no valid memory type");

		let mem = ResourceMemory::new_dedicated(
			DeviceMemory::allocate(
				vk.dev.clone(),
				MemoryAllocateInfo {
					allocation_size: mem_reqs.layout.size(),
					memory_type_index: index,
					dedicated_allocation: Some(DedicatedAllocation::Image(&raw_image)),
					export_handle_types: ExternalMemoryHandleType::DmaBuf.into(),
					..Default::default()
				},
			)
			.unwrap(),
		);
		let Ok(image) = raw_image.bind_memory([mem]) else {
			panic!("unable to bind memory")
		};
		let image = Arc::new(image);
		let ImageMemory::Normal(mem) = image.memory() else {
			unreachable!()
		};

		let [mem] = mem.as_slice() else {
			unreachable!()
		};

		let fd = OwnedFd::from(
			mem.device_memory()
				.export_fd(ExternalMemoryHandleType::DmaBuf)
				.unwrap(),
		);

		let planes = (0..num_planes)
			.filter_map(|i| {
				Some(match i {
					0 => ImageAspect::MemoryPlane0,
					1 => ImageAspect::MemoryPlane1,
					2 => ImageAspect::MemoryPlane2,
					3 => ImageAspect::MemoryPlane3,
					_ => return None,
				})
			})
			.map(|aspect| {
				let plane_layout = image.subresource_layout(aspect, 0, 0).unwrap();

				DmatexPlane {
					dmabuf_fd: fd.try_clone().unwrap().into(),
					modifier,
					offset: plane_layout.offset as u32,
					stride: plane_layout.row_pitch as i32,
				}
			})
			.collect::<Vec<_>>();

		let dmatex = Dmatex {
			planes,
			res: Resolution {
				x: size.x as u32,
				y: size.y as u32,
			},
			format: DrmFourcc::Argb8888 as u32,
			flip_y: false,
			srgb: true,
		};
		let imported_dmatex = import_texture(
			RENDER_DEVICE.wait(),
			dmatex,
			DropCallback(None),
			DmatexUsage::Sampling,
		)
		.unwrap();
		let data_len = size.x * size.y * 4;
		let upload_buffer = vulkano::buffer::Buffer::new_slice::<u8>(
			vk.alloc.clone(),
			vulkano::buffer::BufferCreateInfo {
				usage: BufferUsage::TRANSFER_SRC,
				..Default::default()
			},
			AllocationCreateInfo {
				memory_type_filter: MemoryTypeFilter::HOST_SEQUENTIAL_WRITE,
				..Default::default()
			},
			data_len as u64,
		)
		.unwrap();
		Self {
			pool,
			offset,
			stride,
			size,
			wl_format,
			image,
			upload_buffer,
			pending_imported_dmatex: Mutex::new(Some(imported_dmatex)),
			tex: OnceLock::new(),
		}
	}
	pub async fn on_commit(&self) {
		tokio::task::block_in_place(|| {
			let _span = debug_span!("copy to gpu buffer").entered();
			let shm_data_lock = self.pool.data_lock();
			let mut gpu_slice = self.upload_buffer.write().unwrap();
			for (shm_offset, gpu_offset) in
				(0..self.size.y).map(|v| (self.offset + (v * self.stride), (v * (self.size.x * 4))))
			{
				let line_slice = &shm_data_lock[shm_offset..(shm_offset + (self.size.x * 4))];
				let gpu_subslice = &mut gpu_slice[gpu_offset..(gpu_offset + (self.size.x * 4))];
				gpu_subslice.copy_from_slice(line_slice);
			}
		});
		let notify = Arc::new(Notify::new());
		_ = BUFFER_COPY_CHANNEL.wait().send((
			self.image.clone(),
			self.upload_buffer.clone(),
			notify.clone(),
		));
		notify.notified().await;
	}

	#[tracing::instrument("debug", skip_all)]
	pub fn update_tex(
		&self,
		dmatexes: &ImportedDmatexs,
		images: &mut Assets<Image>,
	) -> Option<Handle<Image>> {
		self.pending_imported_dmatex
			.lock()
			.take()
			.map(|tex| dmatexes.insert_imported_dmatex(images, tex))
			.inspect(|handle| {
				_ = self.tex.set(handle.clone());
			});
		self.tex.get().cloned()
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

static BUFFER_COPY_CHANNEL: OnceLock<
	mpsc::UnboundedSender<(Arc<vulkano::image::Image>, Subbuffer<[u8]>, Arc<Notify>)>,
> = OnceLock::new();

pub async fn shm_upload_task() {
	let (tx, mut rx) = mpsc::unbounded_channel();
	_ = BUFFER_COPY_CHANNEL.set(tx);
	let mut notifies = Vec::new();
	let mut buffer = Vec::new();
	loop {
		rx.recv_many(&mut buffer, 256).await;
		let iter = buffer.drain(..).map(|(image, buf, notif)| {
			notifies.push(notif);
			(image, buf)
		});
		// not super happy, ideally this would spawn a new thread, should be mostly fine tho?
		tokio::task::block_in_place(|| copy_buffers_to_image(iter));
		for n in notifies.drain(..) {
			n.notify_one();
		}
	}
}

fn copy_buffers_to_image(
	iter: impl Iterator<Item = (Arc<vulkano::image::Image>, Subbuffer<[u8]>)>,
) {
	let vk = VULKANO_CONTEXT.wait();
	info!("queue: {:x}", vk.queue.handle().as_raw());
	vk.queue.with(|mut guard| {
		let mut command_buffer = AutoCommandBufferBuilder::primary(
			vk.command_buffer_alloc.clone(),
			vk.queue.queue_family_index(),
			CommandBufferUsage::OneTimeSubmit,
		)
		.unwrap();

		for (image, buffer) in iter {
			command_buffer
				.copy_buffer_to_image(CopyBufferToImageInfo::buffer_image(buffer, image))
				.unwrap();
		}

		let command_buffer = command_buffer.build().unwrap();
		// let fence = Arc::new(
		// 	vulkano::sync::fence::Fence::new(vk.dev.clone(), FenceCreateInfo::default())
		// 		.unwrap(),
		// );
		// let semaphore: Arc<_> = Semaphore::new(
		// 	vk.dev.clone(),
		// 	vulkano::sync::semaphore::SemaphoreCreateInfo {
		// 		semaphore_type: SemaphoreType::Timeline,
		// 		..Default::default()
		// 	},
		// )
		// .unwrap()
		// .into();
		unsafe {
			guard
				.submit(
					&[SubmitInfo {
						command_buffers: vec![CommandBufferSubmitInfo::new(command_buffer)],
						// signal_semaphores: vec![{
						// 	let mut info = SemaphoreSubmitInfo::new(semaphore.clone());
						// 	info.value = 1;
						// 	info
						// }],
						..Default::default()
					}],
					// Some(&fence),
					None,
				)
				.unwrap();
		}
		info!("b");
		// match semaphore.wait(
		// 	SemaphoreWaitInfo {
		// 		value: 1,
		// 		..Default::default()
		// 	},
		// 	None,
		// ) {
		// 	Ok(_) | Err(vulkano::Validated::Error(VulkanError::Timeout)) => {}
		// 	Err(err) => panic!("{}", err),
		// };
		// fence.wait(None).unwrap();
		guard.wait_idle().unwrap();
		info!("c");
	});
	info!("d");
}

impl Drop for ShmBufferBacking {
	fn drop(&mut self) {
		let num = BACKINGS.fetch_sub(1, Ordering::Relaxed) - 1;
		info!("shm backing dropped: {num}");
	}
}
