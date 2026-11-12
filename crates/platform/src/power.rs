// SPDX-License-Identifier: GPL-3.0-or-later
//! System power notifications. The audio callback never calls platform services.
#[cfg(target_os = "macos")]
mod native {
    use std::{
        ffi::c_void,
        io, ptr,
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering},
            mpsc,
        },
        thread,
    };
    type Ref = *mut c_void;
    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IORegisterForSystemPower(
            context: Ref,
            port: *mut Ref,
            callback: extern "C" fn(Ref, u32, u32, Ref),
            notifier: *mut u32,
        ) -> u32;
        fn IODeregisterForSystemPower(notifier: *mut u32) -> i32;
        fn IOAllowPowerChange(root: u32, notification: isize) -> i32;
        fn IOServiceClose(root: u32) -> i32;
        fn IONotificationPortGetRunLoopSource(port: Ref) -> Ref;
        fn IONotificationPortDestroy(port: Ref);
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        static kCFRunLoopDefaultMode: Ref;
        fn CFRunLoopGetCurrent() -> Ref;
        fn CFRunLoopAddSource(run_loop: Ref, source: Ref, mode: Ref);
        fn CFRunLoopRemoveSource(run_loop: Ref, source: Ref, mode: Ref);
        fn CFRunLoopRunInMode(mode: Ref, seconds: f64, return_after_source: bool) -> i32;
        fn CFRunLoopStop(run_loop: Ref);
        fn CFRunLoopWakeUp(run_loop: Ref);
    }
    struct Context {
        root: u32,
        changed: Arc<AtomicBool>,
        generation: Arc<AtomicU32>,
    }
    extern "C" fn notification(context: Ref, _: u32, message: u32, argument: Ref) {
        // SAFETY: the registering thread owns Context until after deregistration.
        let context = unsafe { &*(context as *const Context) };
        match message {
            0xe000_0270 | 0xe000_0280 => unsafe {
                IOAllowPowerChange(context.root, argument as isize);
            },
            0xe000_0300 => {
                context.generation.fetch_or(1, Ordering::AcqRel);
                context.changed.store(true, Ordering::Release);
            }
            _ => (),
        }
    }
    pub struct PowerMonitor {
        changed: Arc<AtomicBool>,
        run_loop: Arc<AtomicUsize>,
        worker: Option<thread::JoinHandle<()>>,
        stopping: Arc<AtomicBool>,
    }
    impl PowerMonitor {
        pub fn new(generation: Arc<AtomicU32>) -> io::Result<Self> {
            let stopping = Arc::new(AtomicBool::new(false));
            let stop = stopping.clone();
            let changed = Arc::new(AtomicBool::new(false));
            let run_loop = Arc::new(AtomicUsize::new(0));
            let signal = changed.clone();
            let loop_ptr = run_loop.clone();
            let (tx, rx) = mpsc::sync_channel(1);
            let worker = thread::Builder::new()
                .name("eg-power".into())
                .spawn(move || {
                    let mut context = Box::new(Context {
                        root: 0,
                        changed: signal,
                        generation,
                    });
                    let mut port = ptr::null_mut();
                    let mut notifier = 0;
                    // SAFETY: all pointers remain owned by this thread and callbacks run on its run loop.
                    unsafe {
                        context.root = IORegisterForSystemPower(
                            (&mut *context as *mut Context).cast(),
                            &mut port,
                            notification,
                            &mut notifier,
                        );
                        if context.root == 0 || port.is_null() {
                            let _ = tx.send(false);
                            return;
                        }
                        let source = IONotificationPortGetRunLoopSource(port);
                        if source.is_null() {
                            IODeregisterForSystemPower(&mut notifier);
                            IOServiceClose(context.root);
                            IONotificationPortDestroy(port);
                            let _ = tx.send(false);
                            return;
                        }
                        let current = CFRunLoopGetCurrent();
                        CFRunLoopAddSource(current, source, kCFRunLoopDefaultMode);
                        loop_ptr.store(current as usize, Ordering::Release);
                        let _ = tx.send(true);
                        while !stop.load(Ordering::Acquire) {
                            CFRunLoopRunInMode(kCFRunLoopDefaultMode, 1.0, false);
                        }
                        loop_ptr.store(0, Ordering::Release);
                        CFRunLoopRemoveSource(current, source, kCFRunLoopDefaultMode);
                        IODeregisterForSystemPower(&mut notifier);
                        IOServiceClose(context.root);
                        IONotificationPortDestroy(port);
                    }
                })?;
            if !rx.recv().unwrap_or(false) {
                let _ = worker.join();
                return Err(io::Error::other(
                    "не удалось зарегистрировать уведомления сна macOS",
                ));
            }
            Ok(Self {
                changed,
                run_loop,
                worker: Some(worker),
                stopping,
            })
        }
        pub fn take_wake(&self) -> bool {
            self.changed.swap(false, Ordering::AcqRel)
        }
    }
    impl Drop for PowerMonitor {
        fn drop(&mut self) {
            self.stopping.store(true, Ordering::Release);
            let run_loop = self.run_loop.load(Ordering::Acquire) as Ref;
            // SAFETY: the worker owns the run loop until this join; CF supports stopping it from another thread.
            if !run_loop.is_null() {
                unsafe {
                    CFRunLoopStop(run_loop);
                    CFRunLoopWakeUp(run_loop);
                }
            }
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        }
    }
}
#[cfg(target_os = "macos")]
pub use native::PowerMonitor;
#[cfg(not(target_os = "macos"))]
pub struct PowerMonitor;
#[cfg(not(target_os = "macos"))]
impl PowerMonitor {
    pub fn new(_: std::sync::Arc<std::sync::atomic::AtomicU32>) -> std::io::Result<Self> {
        Ok(Self)
    }
    pub fn take_wake(&self) -> bool {
        false
    }
}
