use offset_allocator::{Allocation, Allocator};

/// A handle to a live allocation in the heap.
///
/// The element index of this allocation within the backing `Vec` is
/// `chunk_index * chunk_size + allocation.offset`.
#[derive(Clone, Copy)]
pub struct Handle {
    pub chunk_index: u32,
    pub allocation: Allocation,
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
    pub const CHUNK_SIZE: u32 = 2048;

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
                });
            }
        }

        // No existing chunk could fit it; add a new one.
        if size > Self::CHUNK_SIZE {
            return None;
        }

        let chunk_index = self.chunks.len() as u32;
        self.vec.resize_with(self.vec.len() + Self::CHUNK_SIZE as usize, T::default);
        let mut new_chunk = Allocator::new(Self::CHUNK_SIZE);
        let allocation = new_chunk.allocate(size).unwrap();
        self.chunks.push(new_chunk);

        Some(Handle {
            chunk_index,
            allocation,
        })
    }

    pub fn free(&mut self, handle: Handle) {
        self.chunks[handle.chunk_index as usize].free(handle.allocation);
    }

    /// Returns a slice of `size` elements for a live allocation.
    pub fn get(&self, handle: Handle, size: usize) -> &[T] {
        let start = handle.vec_index(Self::CHUNK_SIZE);
        &self.vec[start..start + size]
    }

    /// Returns a mutable slice of `size` elements for a live allocation.
    pub fn get_mut(&mut self, handle: Handle, size: usize) -> &mut [T] {
        let start = handle.vec_index(Self::CHUNK_SIZE);
        &mut self.vec[start..start + size]
    }

    pub fn as_slice(&self) -> &[T] {
        &self.vec
    }
}
