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

    pub fn free_and_clear(&mut self, handle: Handle) {
        self.heap.get_mut(handle).fill(T::default());
        self.heap.free(handle);
        self.need_gpu_sync = true;
    }

    /// If `handle` is `None`, allocates a new region with enough space for `data`, then updates `handle` to contain a handle to the new allocation, then writes `data` in the new region.
    /// 
    /// If `handle` is `Some` and points to a region that's too small to contain `data`, clears and frees the region and allocates a new one, sets `handle` to point at the new one, then writes `data`.
    /// 
    /// If `handle` is `Some` and points to a region that's big enough to contain `data`, then writes `data` into the region, and clears any remaining space at the end.
    pub fn allocate_or_grow_and_write(&mut self, handle: &mut Option<Handle>, data: &[T]) {
        if data.is_empty() {
            // Zero the existing allocation but keep it; the caller can reuse it later.
            if let Some(h) = *handle {
                self.heap.get_mut(h).fill(T::default());
                self.need_gpu_sync = true;
            }
            return;
        }

        let needs_realloc = handle.map_or(true, |h| h.size as usize != data.len());

        if needs_realloc {
            if let Some(h) = handle.take() {
                self.free_and_clear(h);
            }
            *handle = self.heap.allocate(data.len() as u32);
        }

        if let Some(h) = *handle {
            let slot = self.heap.get_mut(h);
            slot[..data.len()].copy_from_slice(data);
            slot[data.len()..].fill(T::default());
            self.need_gpu_sync = true;
        }
    }

    pub fn _get(&mut self, handle: Handle) -> &[T] {
        self.heap._get(handle)
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

    pub fn _as_slice_mut(&mut self) -> &mut [T] {
        self.heap._as_slice_mut()
    }

    pub fn _as_slice(&self) -> &[T] {
        self.heap.as_slice()
    }
}
