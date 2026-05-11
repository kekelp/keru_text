use std::mem::size_of;

use crate::offset_allocator_heap::{Handle, OffsetHeap};

pub struct GpuHeap<T: Copy + Default + Clone> {
    heap: OffsetHeap<T>,
    buffer: wgpu::Buffer,
    buffer_capacity: usize,
    label: String,
    need_gpu_sync: bool,
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
            need_gpu_sync: false,
            usage,
        }
    }

    pub fn allocate(&mut self, size: u32) -> Option<Handle> {
        let handle = self.heap.allocate(size)?;
        Some(handle)
    }

    pub fn free_and_clear(&mut self, handle: Handle) {
        self.heap.get_mut(handle).fill(T::default());
        self.heap.free(handle);
        self.need_gpu_sync = true;
    }

    pub fn get_mut(&mut self, handle: Handle) -> &mut [T] {
        self.need_gpu_sync = true;
        self.heap.get_mut(handle)
    }

    pub fn get(&mut self, handle: Handle) -> &[T] {
        self.heap.get(handle)
    }

    /// Updates the underlying gpu buffer with the heap's backing slice.
    /// Returns true if the buffer was reallocated.
    pub fn load_to_gpu(&mut self, device: &wgpu::Device, queue: &wgpu::Queue) -> bool {
        if !self.need_gpu_sync {
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

        self.need_gpu_sync = false;
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

    pub fn len(&self) -> usize {
        self.heap.as_slice().len()
    }

    pub fn needs_gpu_sync(&self) -> bool {
        self.need_gpu_sync
    }

    pub fn as_slice_mut(&mut self) -> &mut [T] {
        self.heap.as_slice_mut()
    }

    pub fn as_slice(&self) -> &[T] {
        self.heap.as_slice()
    }
}
