// SPDX-License-Identifier: GPL-3.0-or-later
//! A stable HWND for Windows media controls, independent of the visible window.
use std::ptr;
use windows_sys::Win32::{
    System::LibraryLoader::GetModuleHandleW,
    UI::WindowsAndMessaging::{CreateWindowExW, DestroyWindow},
};

pub struct NativeWindow(windows_sys::Win32::Foundation::HWND);

impl NativeWindow {
    pub fn new() -> std::io::Result<Self> {
        let class: Vec<u16> = "STATIC\0".encode_utf16().collect();
        // The built-in static class needs no custom window procedure. It remains hidden.
        let hwnd = unsafe {
            CreateWindowExW(
                0,
                class.as_ptr(),
                ptr::null(),
                0,
                0,
                0,
                0,
                0,
                ptr::null_mut(),
                ptr::null_mut(),
                GetModuleHandleW(ptr::null()),
                ptr::null(),
            )
        };
        if hwnd.is_null() {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(Self(hwnd))
        }
    }
    pub fn handle(&self) -> usize {
        self.0 as usize
    }
}
impl Drop for NativeWindow {
    fn drop(&mut self) {
        unsafe {
            DestroyWindow(self.0);
        }
    }
}
