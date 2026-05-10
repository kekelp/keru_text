use std::mem::size_of;

use crate::offset_allocator_heap::{Handle, OffsetHeap};

pub struct GpuHeap<T: Copy + Default + Clone> {
    pub(crate) heap: OffsetHeap<T>,
    pub(crate) buffer: wgpu::Buffer,
    buffer_capacity: usize,
    label: String,
    pub(crate) dirty: bool,
    usage: wgpu::BufferUsages,
}

impl<T: Copy + Default + Clone> GpuHeap<T> {
    pub fn with_usage(device: &wgpu::Device, capacity: usize, label: &str, usage: wgpu::BufferUsages) -> Self {
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some(label),
            size: (size_of::<T>() * capacity) as u64,
            usage,
            mapped_at_creation: false,
        });

        Self {
            buffer,
            buffer_capacity: capacity,
            heap: OffsetHeap::new(),
            label: label.to_string(),
            dirty: false,
            usage,
        }
    }

    pub fn allocate(&mut self, size: u32) -> Option<Handle> {
        let handle = self.heap.allocate(size)?;
        self.dirty = true;
        Some(handle)
    }

    pub fn free(&mut self, handle: Handle) {
        self.heap.free(handle);
        self.dirty = true;
    }

    pub fn get(&self, handle: Handle, size: usize) -> &[T] {
        self.heap.get(handle, size)
    }

    pub fn get_mut(&mut self, handle: Handle, size: usize) -> &mut [T] {
        self.dirty = true;
        self.heap.get_mut(handle, size)
    }

    /// Updates the underlying gpu buffer with the heap's backing slice.
    /// Returns true if the buffer was reallocated.
    pub fn load_to_gpu(&mut self, device: &wgpu::Device, queue: &wgpu::Queue) -> bool {
        if !self.dirty {
            return false;
        }

        let data = self.heap.as_slice();
        let should_realloc = data.len() > self.buffer_capacity;
        if should_realloc {
            self.buffer_capacity = data.len().next_power_of_two();
            self.buffer = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(self.label.as_str()),
                size: (size_of::<T>() * self.buffer_capacity) as u64,
                usage: self.usage,
                mapped_at_creation: false,
            });
        }

        if !data.is_empty() {
            let size = data.len() * size_of::<T>();
            queue.write_buffer(&self.buffer, 0, unsafe {
                std::slice::from_raw_parts(data.as_ptr() as *const u8, size)
            });
        }

        self.dirty = false;
        should_realloc
    }

    pub fn bind_group_layout_entry(binding_index: u32) -> wgpu::BindGroupLayoutEntry {
        wgpu::BindGroupLayoutEntry {
            binding: binding_index,
            visibility: wgpu::ShaderStages::FRAGMENT | wgpu::ShaderStages::VERTEX,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Storage { read_only: true },
                has_dynamic_offset: false,
                min_binding_size: Some(std::num::NonZeroU64::new(size_of::<T>() as u64).unwrap()),
            },
            count: None,
        }
    }

    pub fn bind_group_entry(&self, binding_index: u32) -> wgpu::BindGroupEntry<'_> {
        wgpu::BindGroupEntry {
            binding: binding_index,
            resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                buffer: &self.buffer,
                offset: 0,
                size: None,
            }),
        }
    }

    pub fn buffer(&self) -> &wgpu::Buffer {
        &self.buffer
    }
}
