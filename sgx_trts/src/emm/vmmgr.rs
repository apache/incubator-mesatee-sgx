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

use crate::sync::Once;
use crate::{
    arch::{SE_PAGE_SHIFT, SE_PAGE_SIZE},
    emm::{PageType, ProtFlags},
    enclave::{is_within_enclave, is_within_rts_range, is_within_user_range, MmLayout},
    sync::SpinReentrantMutex,
};
use alloc::boxed::Box;
use intrusive_collections::{
    linked_list::{Cursor, CursorMut},
    LinkedList, UnsafeRef,
};
use sgx_tlibc_sys::{EEXIST, EINVAL, ENOMEM, EPERM};
use sgx_types::error::OsResult;

use super::{
    alloc::AllocType,
    ema::{Ema, EmaAda, EmaOptions},
    page::AllocFlags,
};

pub const ALLOC_FLAGS_SHIFT: usize = 0;
pub const ALLOC_FLAGS_MASK: usize = 0xFF << ALLOC_FLAGS_SHIFT;

pub const PAGE_TYPE_SHIFT: usize = 8;
pub const PAGE_TYPE_MASK: usize = 0xFF << PAGE_TYPE_SHIFT;

pub const ALLIGNMENT_SHIFT: usize = 24;
pub const ALLIGNMENT_MASK: usize = 0xFF << ALLIGNMENT_SHIFT;

pub const EMA_PROT_MASK: usize = 0x7;

pub(crate) static VMMGR: Once<SpinReentrantMutex<VmMgr>> = Once::new();

/// Initialize range management
pub fn init_vmmgr() {
    let _ = VMMGR.call_once(|| Ok(SpinReentrantMutex::new(VmMgr::new())));
}

pub fn mm_init_static_region(options: &EmaOptions) -> OsResult {
    let mut vmmgr = VMMGR.get().unwrap().lock();
    vmmgr.init_static_region(options)
}

pub fn mm_alloc_user(options: &EmaOptions) -> OsResult<usize> {
    let mut vmmgr = VMMGR.get().unwrap().lock();
    vmmgr.alloc(options, RangeType::User)
}

pub fn mm_alloc_rts(options: &EmaOptions) -> OsResult<usize> {
    let mut vmmgr = VMMGR.get().unwrap().lock();
    vmmgr.alloc(options, RangeType::Rts)
}

pub fn mm_dealloc(addr: usize, size: usize) -> OsResult {
    let mut vmmgr = VMMGR.get().unwrap().lock();
    vmmgr.dealloc(addr, size)
}

pub fn mm_commit(addr: usize, size: usize) -> OsResult {
    let mut vmmgr = VMMGR.get().unwrap().lock();
    vmmgr.commit(addr, size)
}

pub fn mm_uncommit(addr: usize, size: usize) -> OsResult {
    let mut vmmgr = VMMGR.get().unwrap().lock();
    vmmgr.uncommit(addr, size)
}

pub fn mm_modify_type(addr: usize, size: usize, new_page_typ: PageType) -> OsResult {
    let mut vmmgr = VMMGR.get().unwrap().lock();
    vmmgr.modify_type(addr, size, new_page_typ)
}

pub fn mm_modify_perms(addr: usize, size: usize, prot: ProtFlags) -> OsResult {
    let mut vmmgr = VMMGR.get().unwrap().lock();
    vmmgr.modify_perms(addr, size, prot)
}

pub fn check_addr(addr: usize, size: usize) -> OsResult<RangeType> {
    VmMgr::check(addr, size)
}

/// Virtual memory manager
pub(crate) struct VmMgr {
    user: LinkedList<EmaAda>,
    rts: LinkedList<EmaAda>,
}

/// RangeType specifies using Rts or User range
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeType {
    Rts,
    User,
}

impl VmMgr {
    pub fn new() -> Self {
        Self {
            user: LinkedList::new(EmaAda::new()),
            rts: LinkedList::new(EmaAda::new()),
        }
    }

    // Reserve memory range for allocations created
    // by the RTS enclave loader at fixed address ranges
    pub fn init_static_region(&mut self, options: &EmaOptions) -> OsResult {
        ensure!(options.addr.is_some(), EINVAL);
        EmaOptions::check(options)?;

        let mut next_ema = self
            .find_free_region_at(options.addr.unwrap(), options.length, RangeType::Rts)
            .ok_or(EINVAL)?;

        let mut new_ema = Ema::allocate(options, false)?;
        if !options.alloc_flags.contains(AllocFlags::RESERVED) {
            new_ema.set_eaccept_map_full()?;
        }
        next_ema.insert_before(new_ema);

        Ok(())
    }

    /// Allocate a new memory region in enclave address space (ELRANGE).
    pub fn alloc(&mut self, options: &EmaOptions, typ: RangeType) -> OsResult<usize> {
        EmaOptions::check(options)?;

        // let addr = options.addr.unwrap_or(0);
        let addr = options.addr;
        let size = options.length;
        // let end = addr + size;

        let mut alloc_addr: Option<usize> = None;
        let mut alloc_next_ema: Option<CursorMut<'_, EmaAda>> = None;

        if let Some(addr) = addr {
            let end = addr + options.length;
            let is_fixed_alloc = options.alloc_flags.contains(AllocFlags::FIXED);
            let range = self.search_ema_range(addr, end, typ, false, false);

            match range {
                // exist in emas list
                Some(_) => match self.clear_reserved_emas(addr, end, typ, options.alloc) {
                    Some(ema) => {
                        alloc_addr = Some(addr);
                        alloc_next_ema = Some(ema);
                    }
                    None => {
                        if is_fixed_alloc {
                            return Err(EEXIST);
                        }
                    }
                },
                // not exist in emas list
                None => {
                    let next_ema = self.find_free_region_at(addr, size, typ);
                    if next_ema.is_none() && is_fixed_alloc {
                        return Err(EPERM);
                    }
                }
            };
        };

        if alloc_addr.is_none() {
            let (free_addr, next_ema) = self
                .find_free_region(size, 1 << SE_PAGE_SHIFT, typ)
                .ok_or(ENOMEM)?;
            alloc_addr = Some(free_addr);
            alloc_next_ema = Some(next_ema);
        }

        let mut ema_options = *options;
        ema_options.addr(alloc_addr.unwrap());

        let new_ema = Ema::allocate(&ema_options, true)?;

        alloc_next_ema.unwrap().insert_before(new_ema);
        Ok(alloc_addr.unwrap())
    }

    /// Change permissions of an allocated region.
    pub fn modify_perms(&mut self, addr: usize, size: usize, prot: ProtFlags) -> OsResult {
        if prot.contains(ProtFlags::X) && !prot.contains(ProtFlags::R) {
            return Err(EINVAL);
        }
        self.apply_commands(
            addr,
            size,
            true,
            |cursor| cursor.get().unwrap().modify_perm_check(),
            |cursor| unsafe { cursor.get_mut().unwrap().modify_perm(prot) },
        )?;

        Ok(())
    }

    /// Commit a partial or full range of memory allocated previously with
    /// COMMIT_ON_DEMAND.
    ///
    /// TODO: don't split Emas when committing pages
    pub fn commit(&mut self, addr: usize, size: usize) -> OsResult {
        let end = addr + size;
        self.apply_commands(
            addr,
            size,
            false,
            |cursor| cursor.get().unwrap().commit_check(),
            |cursor| {
                let ema = unsafe { cursor.get_mut().unwrap() };
                let start = addr.max(ema.start());
                let end = end.min(ema.end());
                ema.commit(start, end - start)
            },
        )?;

        Ok(())
    }

    /// Uncommit (trim) physical EPC pages in a previously committed range.
    ///
    /// TODO: don't split Emas when trimming pages
    ///
    /// Question: There exist commit_now Emas with no pages if trimming,
    /// How should we treat those null commit_now Emas?
    pub fn uncommit(&mut self, addr: usize, size: usize) -> OsResult {
        let end = addr + size;
        self.apply_commands(
            addr,
            size,
            false,
            |cursor| cursor.get().unwrap().uncommit_check(),
            |cursor| {
                let ema = unsafe { cursor.get_mut().unwrap() };
                let start = addr.max(ema.start());
                let end = end.min(ema.end());
                ema.uncommit(start, end - start)
            },
        )?;

        Ok(())
    }

    /// Deallocate the address range.
    pub fn dealloc(&mut self, addr: usize, size: usize) -> OsResult {
        let typ = VmMgr::check(addr, size)?;
        let (mut cursor, mut ema_num) = self
            .search_ema_range(addr, addr + size, typ, false, true)
            .ok_or(EINVAL)?;
        while ema_num != 0 {
            // Calling remove() implicitly moves cursor pointing to next ema
            let mut ema = cursor.remove().unwrap();
            ema.dealloc()?;

            // Drop inner Ema inexplicitly
            let allocator = ema.allocator();
            let _ema_box = unsafe { Box::from_raw_in(UnsafeRef::into_raw(ema), allocator) };

            ema_num -= 1;
        }
        Ok(())
    }

    /// Change the page type of an allocated region.
    pub fn modify_type(&mut self, addr: usize, size: usize, new_page_typ: PageType) -> OsResult {
        let typ = VmMgr::check(addr, size)?;
        ensure!(new_page_typ == PageType::Tcs, EPERM);
        ensure!(size == SE_PAGE_SIZE, EINVAL);

        let (mut cursor, ema_num) = self
            .search_ema_range(addr, addr + size, typ, true, true)
            .ok_or(EINVAL)?;
        debug_assert!(ema_num == 1);
        unsafe { cursor.get_mut().unwrap().change_to_tcs()? };

        Ok(())
    }

    // Clear the reserved Emas in charging of [start, end) memory region,
    // return next ema cursor
    #[inline]
    fn clear_reserved_emas(
        &mut self,
        start: usize,
        end: usize,
        typ: RangeType,
        alloc: AllocType,
    ) -> Option<CursorMut<'_, EmaAda>> {
        let (mut cursor, ema_num) = self.search_ema_range(start, end, typ, true, true)?;
        let start_ema_ptr = cursor.get().unwrap() as *const Ema;
        let mut count = ema_num;
        while count != 0 {
            let ema = cursor.get().unwrap();
            // Ema must be reserved and can not manage internal memory region
            if !ema.flags().contains(AllocFlags::RESERVED) || ema.alloc_type() != alloc {
                return None;
            }
            cursor.move_next();
            count -= 1;
        }

        let mut cursor = unsafe { self.cursor_mut_from_ptr(start_ema_ptr, typ) };
        count = ema_num;
        while count != 0 {
            cursor.remove();
            count -= 1;
        }

        Some(cursor)
    }

    /// Search for a range of Emas containing addresses within [start, end).
    ///
    /// Return the mutable cursor of start ema and ema number.
    fn search_ema_range(
        &mut self,
        start: usize,
        end: usize,
        typ: RangeType,
        continuous: bool,
        split: bool,
    ) -> Option<(CursorMut<'_, EmaAda>, usize)> {
        let mut cursor = self.front(typ);

        while !cursor.is_null() && cursor.get().unwrap().lower_than_addr(start) {
            cursor.move_next();
        }

        if cursor.is_null() || cursor.get().unwrap().higher_than_addr(end) {
            return None;
        }

        let mut curr_ema = cursor.get().unwrap();
        let mut start_ema_ptr = curr_ema as *const Ema;
        let mut emas_num = 0;
        let mut prev_end = curr_ema.start();

        while !cursor.is_null() && !cursor.get().unwrap().higher_than_addr(end) {
            curr_ema = cursor.get().unwrap();
            // If continuity is required, there should
            // be no gaps in the specified range in the emas list.
            if continuous && prev_end != curr_ema.start() {
                return None;
            }

            emas_num += 1;
            prev_end = curr_ema.end();
            cursor.move_next();
        }

        // Spliting end ema
        if split {
            let mut end_ema_ptr = curr_ema as *const Ema;
            // Spliting start ema
            let mut start_cursor = unsafe { self.cursor_mut_from_ptr(start_ema_ptr, typ) };

            let curr_ema = unsafe { start_cursor.get_mut().unwrap() };
            let ema_start = curr_ema.start();

            if ema_start < start {
                let right_ema = curr_ema.split(start).unwrap();
                start_cursor.insert_after(right_ema);
                // start cursor moves next to refer real start ema
                start_cursor.move_next();
                // ptr points to the address of real start ema
                start_ema_ptr = start_cursor.get().unwrap() as *const Ema;
            }

            // Spliting end ema
            if emas_num == 1 {
                end_ema_ptr = start_ema_ptr;
            }

            let mut end_cursor = unsafe { self.cursor_mut_from_ptr(end_ema_ptr, typ) };

            let end_ema = unsafe { end_cursor.get_mut().unwrap() };
            let ema_end = end_ema.end();

            if ema_end > end {
                let right_ema = end_ema.split(end).unwrap();
                end_cursor.insert_after(right_ema);
            }
        }

        // Recover start ema and return it as range
        let start_cursor = unsafe { self.cursor_mut_from_ptr(start_ema_ptr, typ) };

        Some((start_cursor, emas_num))
    }

    // Search for a ema node whose memory range contains address
    pub fn search_ema(&mut self, addr: usize, typ: RangeType) -> Option<CursorMut<'_, EmaAda>> {
        let mut cursor = self.front_mut(typ);

        while !cursor.is_null() {
            let ema = cursor.get().unwrap();
            if ema.overlap_addr(addr) {
                return Some(cursor);
            }
            cursor.move_next();
        }

        None
    }

    // Find a free space at addr with 'len' bytes in reserve region,
    // the request space mustn't intersect with existed ema node.
    // If success, return the next ema cursor.
    fn find_free_region_at(
        &mut self,
        addr: usize,
        len: usize,
        typ: RangeType,
    ) -> Option<CursorMut<'_, EmaAda>> {
        let mut cursor = self.front_mut(typ);

        while !cursor.is_null() {
            let start_curr = cursor.get().map(|ema| ema.start()).unwrap();
            let end_curr = start_curr + cursor.get().map(|ema| ema.len()).unwrap();
            if start_curr >= addr + len {
                return Some(cursor);
            }

            if addr >= end_curr {
                cursor.move_next();
            } else {
                break;
            }
        }

        // Means addr is larger than the end of the last ema node
        if cursor.is_null() {
            return Some(cursor);
        }

        None
    }

    // Find a free space of size at least 'size' bytes in reserve region,
    // return the start address
    fn find_free_region(
        &mut self,
        len: usize,
        align: usize,
        typ: RangeType,
    ) -> Option<(usize, CursorMut<'_, EmaAda>)> {
        let user_base = MmLayout::user_region_mem_base();
        let user_end = user_base + MmLayout::user_region_mem_size();
        let mut addr;
        let mut cursor = self.front_mut(typ);

        // no ema in list
        if cursor.is_null() {
            match typ {
                RangeType::Rts => {
                    if user_base >= len {
                        addr = trim_to!(user_base - len, align);
                        if is_within_enclave(addr as *const u8, len) {
                            return Some((addr, cursor));
                        }
                    } else {
                        addr = round_to!(user_end, align);
                        // no integer overflow
                        if addr + len >= addr && is_within_enclave(addr as *const u8, len) {
                            return Some((addr, cursor));
                        }
                    }
                    return None;
                }
                RangeType::User => {
                    addr = round_to!(user_base, align);
                    if is_within_user_range(addr, len) {
                        return Some((addr, cursor));
                    }
                    return None;
                }
            }
        }

        let mut cursor_next = cursor.peek_next();

        // ema is_null means pointing to the Null object, not means this ema is empty
        while !cursor_next.is_null() {
            let curr_end = cursor.get().map(|ema| ema.aligned_end(align)).unwrap();

            let next_start = cursor_next.get().map(|ema| ema.start()).unwrap();

            if curr_end <= next_start {
                let free_size = next_start - curr_end;
                if free_size >= len
                    && (typ == RangeType::User || is_within_rts_range(curr_end, len))
                {
                    cursor.move_next();
                    return Some((curr_end, cursor));
                }
            }
            cursor.move_next();
            cursor_next = cursor.peek_next();
        }

        addr = cursor.get().map(|ema| ema.aligned_end(align)).unwrap();

        if is_within_enclave(addr as *const u8, len)
            && ((typ == RangeType::Rts && is_within_rts_range(addr, len))
                || (typ == RangeType::User && is_within_user_range(addr, len)))
        {
            cursor.move_next();
            return Some((addr, cursor));
        }

        // Cursor moves to emas->front_mut.
        // Firstly cursor moves to None, then moves to linkedlist head
        cursor.move_next();
        cursor.move_next();

        // Back to the first ema to check rts region before user region
        let start_first = cursor.get().map(|ema| ema.start()).unwrap();
        if start_first < len {
            return None;
        }

        addr = trim_to!(start_first, align);

        match typ {
            RangeType::User => {
                if is_within_user_range(addr, len) {
                    return Some((addr, cursor));
                }
            }
            RangeType::Rts => {
                if is_within_enclave(addr as *const u8, len) && is_within_rts_range(addr, len) {
                    return Some((addr, cursor));
                }
            }
        }

        None
    }

    fn front_mut(&mut self, typ: RangeType) -> CursorMut<'_, EmaAda> {
        match typ {
            RangeType::Rts => self.rts.front_mut(),
            RangeType::User => self.user.front_mut(),
        }
    }

    fn front(&self, typ: RangeType) -> Cursor<'_, EmaAda> {
        match typ {
            RangeType::Rts => self.rts.front(),
            RangeType::User => self.user.front(),
        }
    }

    unsafe fn cursor_mut_from_ptr(
        &mut self,
        ptr: *const Ema,
        typ: RangeType,
    ) -> CursorMut<'_, EmaAda> {
        match typ {
            RangeType::Rts => unsafe { self.rts.cursor_mut_from_ptr(ptr) },
            RangeType::User => unsafe { self.user.cursor_mut_from_ptr(ptr) },
        }
    }

    fn apply_commands<F1, F2>(
        &mut self,
        addr: usize,
        size: usize,
        split: bool,
        check: F1,
        commands: F2,
    ) -> OsResult
    where
        F1: Fn(&CursorMut<'_, EmaAda>) -> OsResult,
        F2: Fn(&mut CursorMut<'_, EmaAda>) -> OsResult,
    {
        let typ = VmMgr::check(addr, size)?;
        let (mut cursor, ema_num) = self
            .search_ema_range(addr, addr + size, typ, true, split)
            .ok_or(EINVAL)?;
        let start_ema_ptr = cursor.get().unwrap() as *const Ema;

        let mut count = ema_num;
        while count != 0 {
            check(&cursor)?;
            cursor.move_next();
            count -= 1;
        }

        let mut cursor = unsafe { self.cursor_mut_from_ptr(start_ema_ptr, typ) };
        count = ema_num;
        while count != 0 {
            commands(&mut cursor)?;
            cursor.move_next();
            count -= 1;
        }

        Ok(())
    }
}

// Utils
impl VmMgr {
    pub fn check(addr: usize, len: usize) -> OsResult<RangeType> {
        if addr > 0 {
            ensure!(
                is_page_aligned!(addr) && is_within_enclave(addr as *const u8, len),
                EINVAL
            );
        }
        ensure!(len != 0 && ((len % SE_PAGE_SIZE) == 0), EINVAL);

        if is_within_rts_range(addr, len) {
            Ok(RangeType::Rts)
        } else if is_within_user_range(addr, len) {
            Ok(RangeType::User)
        } else {
            Err(EINVAL)
        }
    }
}
