use std::sync::OnceLock;

use bevy::{
	ecs::system::Res,
	render::renderer::{RenderAdapter, RenderDevice, RenderInstance, RenderQueue},
};

pub static WGPU_CONTEXT: OnceLock<WgpuContext> = OnceLock::new();

#[expect(dead_code)]
pub struct WgpuContext {
	pub instance: RenderInstance,
	pub adapter: RenderAdapter,
	pub dev: RenderDevice,
	pub queue: RenderQueue,
}
pub fn setup_wgpu_context(
	dev: Res<RenderDevice>,
	instance: Res<RenderInstance>,
	adapter: Res<RenderAdapter>,
	queue: Res<RenderQueue>,
) {
	if WGPU_CONTEXT.get().is_some() {
		return;
	}
	let ctx = WgpuContext {
		instance: instance.clone(),
		adapter: adapter.clone(),
		dev: dev.clone(),
		queue: queue.clone(),
	};
	_ = WGPU_CONTEXT.set(ctx);
}
