//! The module's allocator: the system one, zeroing what it frees while the
//! kernel says so.
//!
//! A snapshot of a container is its memory, and memory the kernel has
//! freed still holds what it held — a flushed block cache is tens of
//! megabytes of stale decoded instructions that a snapshot would keep for
//! nothing. Zeroing on every free would cost every free; zeroing only while
//! [`kernel::SCRUB_FREED`] is set costs the one moment a snapshot tool asks
//! the kernel to throw its caches away, so what is thrown away is zero, and
//! a page of zeros is a page the snapshot leaves out.

use core::alloc::{GlobalAlloc, Layout};
use core::sync::atomic::Ordering;
use std::alloc::System;

struct Scrubbing;

// SAFETY: forwards every operation to `System`, and touches only memory
// that is being freed, before it is freed.
unsafe impl GlobalAlloc for Scrubbing {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if kernel::SCRUB_FREED.load(Ordering::Relaxed) {
            unsafe { core::ptr::write_bytes(ptr, 0, layout.size()) };
        }
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if kernel::SCRUB_FREED.load(Ordering::Relaxed) {
            // Move by hand so that the old bytes are zeroed on the way out;
            // `System::realloc` would leave them.
            let new_layout = unsafe { Layout::from_size_align_unchecked(new_size, layout.align()) };
            let moved = unsafe { System.alloc(new_layout) };
            if !moved.is_null() {
                unsafe {
                    core::ptr::copy_nonoverlapping(ptr, moved, layout.size().min(new_size));
                    self.dealloc(ptr, layout);
                }
            }
            return moved;
        }
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Scrubbing = Scrubbing;
