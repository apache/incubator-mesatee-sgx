// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License..

use buddy_system_allocator::LockedHeap;
use intrusive_collections::intrusive_adapter;
use intrusive_collections::singly_linked_list::CursorMut;
use intrusive_collections::singly_linked_list::{Link, SinglyLinkedList};
use intrusive_collections::UnsafeRef;
use sgx_tlibc_sys::ENOMEM;

use crate::sync::Once;
use crate::sync::SpinMutex as Mutex;
use core::alloc::{AllocError, Allocator, Layout};
use core::any::Any;
use core::mem::size_of;
use core::mem::MaybeUninit;
use core::ptr::NonNull;

use super::ema::EmaOptions;
use super::page::AllocFlags;
use super::vmmgr::{RangeType, VMMGR};
use super::{PageInfo, PageType, ProtFlags};
use sgx_types::error::OsResult;

// The size of fixed static memory for Static Allocator
const STATIC_MEM_SIZE: usize = 65536;

// The size of initial reserve memory for Reserve Allocator
const INIT_MEM_SIZE: usize = 65536;

// The size of guard pages
const GUARD_SIZE: usize = 0x8000;

// The max allocated size of Reserve Allocator
const MAX_EMALLOC_SIZE: usize = 0x10000000;

const ALLOC_MASK: usize = 1;
const SIZE_MASK: usize = !(EXACT_MATCH_INCREMENT - 1);

/// Static memory for allocation
static mut STATIC_MEM: [u8; STATIC_MEM_SIZE] = [0; STATIC_MEM_SIZE];

/// Lowest level: Allocator for static memory
///
/// TODO: reimplement static allocator with monotone increasing policies
static STATIC: Once<LockedHeap<32>> = Once::new();

/// Second level: Allocator for reserve memory
static RSRV_ALLOCATOR: Once<Mutex<Reserve>> = Once::new();

/// Init lowest level static memory allocator
pub fn init_static_alloc() {
    let _ = STATIC.call_once(|| {
        let static_alloc = LockedHeap::empty();
        unsafe {
            static_alloc
                .lock()
                .init(STATIC_MEM.as_ptr() as usize, STATIC_MEM_SIZE)
        };
        Ok(static_alloc)
    });
}

/// Init reserve memory allocator
/// init_reserve_alloc() need to be called after init_static_alloc()
pub fn init_reserve_alloc() {
    let _ = RSRV_ALLOCATOR.call_once(|| Ok(Mutex::new(Reserve::new(INIT_MEM_SIZE))));
}

pub trait EmmAllocator: Allocator + Any {
    fn as_any(&self) -> &dyn Any;
}

/// AllocType layout memory from reserve memory region
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RsrvAlloc;

unsafe impl Allocator for RsrvAlloc {
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        let size = layout.size();
        RSRV_ALLOCATOR
            .get()
            .unwrap()
            .lock()
            .emalloc(size)
            .map(|addr| NonNull::slice_from_raw_parts(NonNull::new(addr as *mut u8).unwrap(), size))
            .map_err(|_| AllocError)
    }

    #[inline]
    unsafe fn deallocate(&self, ptr: NonNull<u8>, _layout: Layout) {
        RSRV_ALLOCATOR.get().unwrap().lock().efree(ptr.addr().get())
    }
}

impl EmmAllocator for RsrvAlloc {
    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// AllocType layout memory from static memory region
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StaticAlloc;

unsafe impl Allocator for StaticAlloc {
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        STATIC
            .get()
            .unwrap()
            .lock()
            .alloc(layout)
            .map(|addr| NonNull::slice_from_raw_parts(addr, layout.size()))
            .map_err(|_| AllocError)
    }

    #[inline]
    unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
        STATIC.get().unwrap().lock().dealloc(ptr, layout);
    }
}

impl EmmAllocator for StaticAlloc {
    fn as_any(&self) -> &dyn Any {
        self
    }
}

// Enum for allocator types
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum AllocType {
    Static,
    Reserve,
}

impl AllocType {
    pub fn alloctor(&self) -> &'static dyn EmmAllocator {
        match self {
            AllocType::Static => &StaticAlloc,
            AllocType::Reserve => &RsrvAlloc,
        }
    }
}

// Chunk manages memory range.
// The Chunk structure is filled into the layout before the base pointer.
#[derive(Debug)]
struct Chunk {
    base: usize,
    size: usize,
    used: usize,
    link: Link, // singly intrusive linkedlist
}

impl Chunk {
    fn new(base: usize, size: usize) -> Self {
        Self {
            base,
            size,
            used: 0,
            link: Link::new(),
        }
    }
}

intrusive_adapter!(ChunkAda = UnsafeRef<Chunk>: Chunk { link: Link });

const NUM_EXACT_LIST: usize = 0x100;
const HEADER_SIZE: usize = size_of::<usize>();
const EXACT_MATCH_INCREMENT: usize = 0x8;
const MIN_BLOCK_SIZE: usize = 0x10;
const MAX_EXACT_SIZE: usize = MIN_BLOCK_SIZE + EXACT_MATCH_INCREMENT * (NUM_EXACT_LIST - 1);

// Free block for allocating memory with exact size
#[repr(C)]
#[derive(Debug)]
struct BlockFree {
    size: usize,
    link: Link, // singly intrusive linkedlist
}

// Used block for tracking allocated size and base pointer
#[repr(C)]
#[derive(Debug)]
struct BlockUsed {
    size: usize,
    payload: usize, // Act as placeholder
}

impl BlockFree {
    fn new(size: usize) -> Self {
        Self {
            size,
            link: Link::new(),
        }
    }

    fn set_size(&mut self, size: usize) {
        self.size = size;
    }

    fn block_size(&self) -> usize {
        self.size & SIZE_MASK
    }

    fn clear_alloced(&mut self) {
        self.size &= SIZE_MASK;
    }
}

impl BlockUsed {
    fn new(size: usize) -> Self {
        Self { size, payload: 0 }
    }

    fn set_size(&mut self, size: usize) {
        self.size = size;
    }

    fn block_size(&self) -> usize {
        self.size & SIZE_MASK
    }

    fn is_alloced(&self) -> bool {
        self.size & ALLOC_MASK == 0
    }

    fn set_alloced(&mut self) {
        self.size |= ALLOC_MASK;
    }

    fn clear_alloced(&mut self) {
        self.size &= SIZE_MASK;
    }

    // Return the ptr of payload
    fn payload_ptr(&self) -> usize {
        &self.payload as *const _ as usize
    }

    unsafe fn with_payload<'a>(payload_ptr: usize) -> &'a mut BlockUsed {
        let payload_ptr = payload_ptr as *const u8;
        let block = &mut *(payload_ptr.byte_offset(-(HEADER_SIZE as isize)) as *mut BlockUsed);
        block
    }
}

impl<'a> From<&'a mut BlockFree> for &'a mut BlockUsed {
    fn from(block_free: &'a mut BlockFree) -> Self {
        let block_used = unsafe { &mut *(block_free as *mut _ as *mut BlockUsed) };

        block_used.size = block_free.block_size();
        // Clear residual link information
        block_used.payload = 0;
        block_used.set_alloced();

        block_used
    }
}

impl<'a> From<&'a mut BlockUsed> for &'a mut BlockFree {
    fn from(block_used: &'a mut BlockUsed) -> Self {
        let block_free = unsafe { &mut *(block_used as *mut _ as *mut BlockFree) };

        block_free.size = block_used.block_size();
        block_free.link = Link::new();
        // Useless method to mark free tag
        block_free.clear_alloced();

        block_free
    }
}

intrusive_adapter!(BlockFreeAda = UnsafeRef<BlockFree>: BlockFree { link: Link });

/// Interior allocator for reserve memory management
///
/// TODO: implement slab allocator mechanism
pub struct Reserve {
    exact_blocks: [SinglyLinkedList<BlockFreeAda>; 256],
    large_blocks: SinglyLinkedList<BlockFreeAda>,
    chunks: SinglyLinkedList<ChunkAda>,
    // The size of memory increment
    incr_size: usize,
    // statistics
    allocated: usize,
    total: usize,
}

impl Reserve {
    fn new(size: usize) -> Self {
        let exact_blocks: [SinglyLinkedList<BlockFreeAda>; 256] = {
            let mut exact_blocks: [MaybeUninit<SinglyLinkedList<BlockFreeAda>>; 256] =
                MaybeUninit::uninit_array();
            for block in &mut exact_blocks {
                block.write(SinglyLinkedList::new(BlockFreeAda::new()));
            }
            unsafe { MaybeUninit::array_assume_init(exact_blocks) }
        };

        let mut reserve = Self {
            exact_blocks,
            large_blocks: SinglyLinkedList::new(BlockFreeAda::new()),
            chunks: SinglyLinkedList::new(ChunkAda::new()),
            incr_size: 65536,
            allocated: 0,
            total: 0,
        };

        // We shouldn't handle the allocation error of reserve memory when initializing,
        // If it returns error, the sdk should panic and crash.
        unsafe {
            reserve.add_chunks(size).unwrap();
        }
        reserve
    }

    // Find the available free block for memory allocation,
    // and bsize must be round to eight
    fn get_free_block(&mut self, bsize: usize) -> Option<UnsafeRef<BlockFree>> {
        if bsize <= MAX_EXACT_SIZE {
            // TODO: for exact size block, maybe we can reuse larger block
            // rather than allocating block from chunk
            return self.get_exact_block(bsize);
        }

        // Loop and find the most available large block
        let list = &mut self.large_blocks;
        let mut cursor = list.front_mut();
        let mut suit_block: Option<*const BlockFree> = None;
        let mut suit_block_size = 0;
        while !cursor.is_null() {
            let curr_block = cursor.get().unwrap();
            if curr_block.size >= bsize
                && (suit_block.is_none() || (suit_block_size > curr_block.size))
            {
                suit_block = Some(curr_block as *const BlockFree);
                suit_block_size = curr_block.block_size();
            }
            cursor.move_next();
        }

        suit_block?;

        cursor = list.front_mut();

        let mut curr_block_ptr = cursor.get().unwrap() as *const BlockFree;
        if curr_block_ptr == suit_block.unwrap() {
            return list.pop_front();
        }

        let mut cursor_next = cursor.peek_next();
        while !cursor_next.is_null() {
            curr_block_ptr = cursor_next.get().unwrap() as *const BlockFree;
            if curr_block_ptr == suit_block.unwrap() {
                return cursor.remove_next();
            }
            cursor.move_next();
            cursor_next = cursor.peek_next();
        }

        None
    }

    fn get_exact_block(&mut self, bsize: usize) -> Option<UnsafeRef<BlockFree>> {
        let idx = self.get_list_idx(bsize);
        let list = &mut self.exact_blocks[idx];
        list.pop_front()
    }

    fn put_free_block(&mut self, block: UnsafeRef<BlockFree>) {
        let block_size = block.block_size();
        if block_size <= MAX_EXACT_SIZE {
            // put block into exact block list
            let idx = self.get_list_idx(block_size);
            let list = &mut self.exact_blocks[idx];
            list.push_front(block);
        } else {
            // put block into large block list
            let list = &mut self.large_blocks;
            list.push_front(block);
        }
    }

    // Obtain the list index with exact block size
    fn get_list_idx(&self, size: usize) -> usize {
        assert!(size % EXACT_MATCH_INCREMENT == 0);
        if size < MIN_BLOCK_SIZE {
            return 0;
        }
        let idx = (size - MIN_BLOCK_SIZE) / EXACT_MATCH_INCREMENT;
        assert!(idx < NUM_EXACT_LIST);
        idx
    }

    // Reconstruct BlockUsed with BlockFree block_size() and set alloc, return payload ptr.
    // BlockFree -> BlockUsed -> Payload ptr (Used)
    fn block_to_payload(&self, mut block_free: UnsafeRef<BlockFree>) -> usize {
        // Inexplicily change inner data of pointer
        let block_used: &mut BlockUsed = block_free.as_mut().into();
        block_used.payload_ptr()
    }

    // Reconstruct a new BlockFree with BlockUsed block_size(), return payload ptr.
    // Payload ptr (Used) -> BlockUsed -> BlockFree
    fn payload_to_block(&self, payload_ptr: usize) -> UnsafeRef<BlockFree> {
        let block_used = unsafe { BlockUsed::with_payload(payload_ptr) };
        // Inexplicily change inner data of pointer
        let block_free: &mut BlockFree = block_used.into();
        unsafe { UnsafeRef::from_raw(block_free as *const BlockFree) }
    }

    /// Malloc memory
    pub fn emalloc(&mut self, size: usize) -> OsResult<usize> {
        let mut bsize = round_to!(size + HEADER_SIZE, EXACT_MATCH_INCREMENT);
        bsize = bsize.max(MIN_BLOCK_SIZE);

        // Find free block in lists
        let mut block = self.get_free_block(bsize);

        if let Some(block) = block {
            // No need to set size as free block contains size
            return Ok(self.block_to_payload(block));
        };

        // Alloc new block from chunks
        block = self.alloc_from_chunks(bsize);
        if block.is_none() {
            let chunk_size = size_of::<Chunk>();
            let new_reserve_size = round_to!(bsize + chunk_size, INIT_MEM_SIZE);
            unsafe { self.add_chunks(new_reserve_size)? };
            block = self.alloc_from_chunks(bsize);
            // Should never happen
            if block.is_none() {
                return Err(ENOMEM);
            }
        }

        Ok(self.block_to_payload(block.unwrap()))
    }

    fn alloc_from_chunks(&mut self, bsize: usize) -> Option<UnsafeRef<BlockFree>> {
        let mut addr: usize = 0;
        let mut cursor = self.chunks.front_mut();
        while !cursor.is_null() {
            let chunk = unsafe { cursor.get_mut().unwrap() };
            if (chunk.size - chunk.used) >= bsize {
                addr = chunk.base + chunk.used;
                chunk.used += bsize;
                break;
            }
            cursor.move_next();
        }

        if addr == 0 {
            None
        } else {
            let block = BlockFree::new(bsize);
            let ptr = addr as *mut BlockFree;
            let block = unsafe {
                ptr.write(block);
                UnsafeRef::from_raw(ptr)
            };
            Some(block)
        }
    }

    /// Free memory
    pub fn efree(&mut self, payload_addr: usize) {
        let block = self.payload_to_block(payload_addr);
        let block_addr = block.as_ref() as *const BlockFree as usize;
        let block_size = block.block_size();
        let block_end = block_addr + block_size;
        let res = self.find_chunk_with_block(block_addr, block_size);
        if res.is_none() {
            panic!();
        }

        // TODO: reconfigure the free block,
        // merging its dextral block into a large block
        let mut cursor = res.unwrap();
        let chunk = unsafe { cursor.get_mut().unwrap() };

        if block_end - chunk.base == chunk.used {
            chunk.used -= block.block_size();
            // TODO: Trigger merging the right-most block into this chunk,
            // if and only if the right-most block is in free large block list
            return;
        }

        self.put_free_block(block);
    }

    /// Adding the size of interior memory
    /// rsize: memory increment
    pub unsafe fn add_chunks(&mut self, rsize: usize) -> OsResult {
        // Here we alloc at least INIT_MEM_SIZE size,
        // but commit rsize memory, the remaining memory is COMMIT_ON_DEMAND
        let increment = self.incr_size.max(rsize);
        let mut vmmgr = VMMGR.get().unwrap().lock();

        let mut options = EmaOptions::new(None, increment + 2 * GUARD_SIZE, AllocFlags::RESERVED);

        options
            .info(PageInfo {
                typ: PageType::None,
                prot: ProtFlags::NONE,
            })
            .alloc(AllocType::Static);
        let base = vmmgr.alloc(&options, RangeType::User)?;

        let mut options = EmaOptions::new(
            Some(base + GUARD_SIZE),
            increment,
            AllocFlags::COMMIT_ON_DEMAND | AllocFlags::FIXED,
        );

        options.alloc(AllocType::Static);
        let base = vmmgr.alloc(&options, RangeType::User)?;

        vmmgr.commit(base, rsize)?;
        drop(vmmgr);

        unsafe {
            self.write_chunk(base, increment);
        }

        self.incr_size = (self.incr_size * 2).min(MAX_EMALLOC_SIZE);

        Ok(())
    }

    // Parsing the range of unmanaged memory. The function writes a chunk struct in the header of
    // unmanaged memory, the written chunk will be responsible for managing the remaining memory.
    unsafe fn write_chunk(&mut self, base: usize, size: usize) {
        let header_size = size_of::<Chunk>();
        let mem_base = base + header_size;
        let mem_size = size - header_size;

        let chunk: Chunk = Chunk::new(mem_base, mem_size);
        unsafe {
            core::ptr::write(base as *mut Chunk, chunk);
            let chunk_ref = UnsafeRef::from_raw(base as *const Chunk);
            self.chunks.push_front(chunk_ref);
        }
    }

    // Find the chunk including the specified block
    fn find_chunk_with_block(
        &mut self,
        block_addr: usize,
        block_size: usize,
    ) -> Option<CursorMut<'_, ChunkAda>> {
        if block_size == 0 {
            return None;
        }
        let mut cursor = self.chunks.front_mut();
        while !cursor.is_null() {
            let chunk = cursor.get().unwrap();
            if (block_addr >= chunk.base)
                && ((block_addr + block_size) <= (chunk.base + chunk.used))
            {
                return Some(cursor);
            }
            cursor.move_next();
        }

        None
    }
}
