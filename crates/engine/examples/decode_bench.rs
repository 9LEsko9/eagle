// SPDX-License-Identifier: GPL-3.0-or-later
//! Offline decode throughput and Rust allocation traffic (excluding native libraries).
use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::Instant,
};

static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);
static ALLOCATED_BYTES: AtomicU64 = AtomicU64::new(0);

#[cfg(not(debug_assertions))]
struct CountingAllocator;

#[cfg(not(debug_assertions))]
unsafe impl std::alloc::GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        ALLOCATED_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        unsafe { std::alloc::System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: std::alloc::Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        ALLOCATED_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        unsafe { std::alloc::System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: std::alloc::Layout) {
        unsafe { std::alloc::System.dealloc(pointer, layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: std::alloc::Layout, size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        ALLOCATED_BYTES.fetch_add(size as u64, Ordering::Relaxed);
        unsafe { std::alloc::System.realloc(pointer, layout, size) }
    }
}

#[cfg(not(debug_assertions))]
#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if cfg!(debug_assertions) {
        return Err("decode_bench requires --release".into());
    }
    let mut args = std::env::args().skip(1);
    let path = args
        .next()
        .ok_or("usage: decode_bench FILE ITERATIONS [OUTPUT_RATE]")?;
    let iterations: u64 = args.next().ok_or("missing iteration count")?.parse()?;
    let output_rate = args.next().map(|arg| arg.parse::<u32>()).transpose()?;
    if iterations == 0 || output_rate == Some(0) || args.next().is_some() {
        return Err(
            "iterations and output rate must be positive; unexpected extra argument".into(),
        );
    }
    let source = eagle_engine::Source::file(&path);
    let reference = eagle_engine::inspect(&source, output_rate)?;
    ALLOCATIONS.store(0, Ordering::Relaxed);
    ALLOCATED_BYTES.store(0, Ordering::Relaxed);
    let started = Instant::now();
    let mut frames = 0;
    for _ in 0..iterations {
        let decoded = eagle_engine::inspect(&source, output_rate)?;
        if decoded.frames != reference.frames
            || decoded.damaged_packets != reference.damaged_packets
        {
            return Err("decode result changed between iterations".into());
        }
        frames += std::hint::black_box(decoded.frames);
    }
    let elapsed = started.elapsed().as_secs_f64();
    let allocations = ALLOCATIONS.load(Ordering::Relaxed);
    let bytes = ALLOCATED_BYTES.load(Ordering::Relaxed);
    println!("iterations,frames,seconds,allocations,allocated_bytes,input_rate,output_rate");
    println!(
        "{iterations},{frames},{elapsed:.9},{allocations},{bytes},{},{}",
        reference.input_rate, reference.output_rate
    );
    Ok(())
}
