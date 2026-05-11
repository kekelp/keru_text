use offset_allocator::{Allocation, Allocator};

pub const CHUNK_SIZE: u32 = 2048;

/// A handle to a live allocation in the heap.
///
/// The element index of this allocation within the backing `Vec` is
/// `chunk_index * chunk_size + allocation.offset`.
#[derive(Clone, Copy)]
pub struct Handle {
    pub chunk_index: u32,
    pub allocation: Allocation,
    pub size: u32,
}

impl Handle {
    /// The element index of this allocation within the backing `Vec`.
    pub fn vec_index(&self, chunk_size: u32) -> usize {
        (self.chunk_index * chunk_size + self.allocation.offset) as usize
    }
}

/// A growable heap backed by a single `Vec<T>`, managed by a collection of
/// fixed-size [`Allocator`] chunks.
///
/// Sizes and offsets are in units of `T`. When no existing chunk can satisfy an
/// allocation, a new chunk is appended to the vec and a fresh allocator is
/// created for it.
pub struct OffsetHeap<T> {
    vec: Vec<T>,
    chunks: Vec<Allocator>,
}

impl<T: Default + Clone> OffsetHeap<T> {
    pub fn new() -> Self {
        Self {
            vec: Vec::new(),
            chunks: Vec::new(),
        }
    }

    /// Allocates `size` elements of `T.
    ///
    /// Returns `None` if `size` exceeds `chunk_size`.
    pub fn allocate(&mut self, size: u32) -> Option<Handle> {
        // Try existing chunks first.
        for (i, chunk) in self.chunks.iter_mut().enumerate() {
            if let Some(allocation) = chunk.allocate(size) {
                return Some(Handle {
                    chunk_index: i as u32,
                    allocation,
                    size,
                });
            }
        }

        // No existing chunk could fit it; add a new one.
        if size > CHUNK_SIZE {
            return None;
        }

        let chunk_index = self.chunks.len() as u32;
        self.vec.resize_with(self.vec.len() + CHUNK_SIZE as usize, T::default);
        let mut new_chunk = Allocator::new(CHUNK_SIZE);
        let allocation = new_chunk.allocate(size).unwrap();
        self.chunks.push(new_chunk);

        Some(Handle {
            chunk_index,
            allocation,
            size,
        })
    }

    pub fn free(&mut self, handle: Handle) {
        self.chunks[handle.chunk_index as usize].free(handle.allocation);
    }

    pub fn get_mut(&mut self, handle: Handle) -> &mut [T] {
        let start = handle.vec_index(CHUNK_SIZE);
        let size = handle.size as usize;
        &mut self.vec[start..start + size]
    }

    pub fn _get(&self, handle: Handle) -> &[T] {
        let start = handle.vec_index(CHUNK_SIZE);
        let size = handle.size as usize;
        &self.vec[start..start + size]
    }

    pub fn as_slice(&self) -> &[T] {
        &self.vec
    }

    pub fn _as_slice_mut(&mut self) -> &mut [T] {
        &mut self.vec
    }
}
