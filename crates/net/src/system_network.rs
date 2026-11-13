// SPDX-License-Identifier: GPL-3.0-or-later
//! Event-driven system network information; unknown when unavailable.
use crate::policy::NetworkPolicy;
#[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos", test))]
use crate::policy::NetworkStatus;

/// Missing platform support and command failures are reported as unknown.
pub fn query(allow_metered: bool) -> NetworkPolicy {
    #[cfg(target_os = "macos")]
    {
        monitor::query(allow_metered)
    }
    #[cfg(target_os = "windows")]
    {
        windows_monitor::query(allow_metered)
    }
    #[cfg(target_os = "linux")]
    {
        if let Ok(output) = std::process::Command::new("nmcli")
            .env("LC_ALL", "C")
            .args([
                "--wait",
                "3",
                "--terse",
                "--fields",
                "CONNECTIVITY,METERED",
                "general",
            ])
            .output()
            && output.status.success()
            && let Ok(output) = std::str::from_utf8(&output.stdout)
        {
            return parse_nmcli(output, allow_metered);
        }
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    NetworkPolicy {
        allow_metered,
        ..NetworkPolicy::default()
    }
}

#[cfg(any(target_os = "linux", test))]
fn parse_nmcli(output: &str, allow_metered: bool) -> NetworkPolicy {
    let mut fields = output.trim().split(':');
    let online = match fields.next() {
        Some("none") => NetworkStatus::No,
        Some("full" | "limited" | "portal") => NetworkStatus::Yes,
        _ => NetworkStatus::Unknown,
    };
    let metered = match fields.next() {
        Some("yes" | "guess-yes") => NetworkStatus::Yes,
        Some("no") => NetworkStatus::No,
        _ => NetworkStatus::Unknown,
    };
    NetworkPolicy {
        online,
        metered,
        allow_metered,
    }
}

/// An event subscription. Dropping it unregisters callbacks and stops its worker.
pub struct NetworkMonitor {
    inner: monitor::Monitor,
}
impl NetworkMonitor {
    /// Callbacks carry observed network state with `allow_metered` false; the caller
    /// applies its current preference. Startup failures emit Unknown.
    pub fn start(callback: impl Fn(NetworkPolicy) + Send + Sync + 'static) -> Self {
        Self {
            inner: monitor::Monitor::start(std::sync::Arc::new(callback)),
        }
    }
}
impl Drop for NetworkMonitor {
    fn drop(&mut self) {
        self.inner.stop();
    }
}

type Callback = std::sync::Arc<dyn Fn(NetworkPolicy) + Send + Sync>;

#[cfg(target_os = "linux")]
mod monitor {
    use super::*;
    use std::{
        io::{BufRead, BufReader},
        process::{Child, Command, Stdio},
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        },
        thread,
    };
    pub struct Monitor {
        child: Option<Arc<Mutex<Child>>>,
        worker: Option<thread::JoinHandle<()>>,
        stopped: Arc<AtomicBool>,
    }
    impl Monitor {
        pub fn start(callback: Callback) -> Self {
            let stopped = Arc::new(AtomicBool::new(false));
            callback(query(false));
            let process = Command::new("nmcli")
                .env("LC_ALL", "C")
                .arg("monitor")
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn();
            let Ok(mut process) = process else {
                callback(NetworkPolicy::default());
                return Self {
                    child: None,
                    worker: None,
                    stopped,
                };
            };
            let Some(stdout) = process.stdout.take() else {
                let _ = process.kill();
                let _ = process.wait();
                callback(NetworkPolicy::default());
                return Self {
                    child: None,
                    worker: None,
                    stopped,
                };
            };
            let child = Arc::new(Mutex::new(process));
            let stop = stopped.clone();
            let worker = thread::Builder::new()
                .name("eg-net-monitor".into())
                .spawn(move || {
                    for line in BufReader::new(stdout).lines() {
                        if stop.load(Ordering::Acquire) {
                            break;
                        }
                        if line.is_err() {
                            break;
                        }
                        callback(query(false));
                    }
                    if !stop.load(Ordering::Acquire) {
                        callback(NetworkPolicy::default());
                    }
                })
                .ok();
            if worker.is_none() {
                let mut child = child.lock().unwrap_or_else(|p| p.into_inner());
                let _ = child.kill();
                let _ = child.wait();
            }
            Self {
                child: Some(child),
                worker,
                stopped,
            }
        }
        pub fn stop(&mut self) {
            self.stopped.store(true, Ordering::Release);
            if let Some(child) = &self.child {
                let _ = child.lock().unwrap_or_else(|p| p.into_inner()).kill();
            }
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
            if let Some(child) = self.child.take() {
                let _ = child.lock().unwrap_or_else(|p| p.into_inner()).wait();
            }
        }
    }
}

#[cfg(target_os = "windows")]
mod windows_monitor {
    use super::*;
    use windows::Networking::Connectivity::{
        NetworkConnectivityLevel as Level, NetworkCostType as Cost, NetworkInformation,
    };
    pub fn query(allow_metered: bool) -> NetworkPolicy {
        let Ok(profile) = NetworkInformation::GetInternetConnectionProfile() else {
            return NetworkPolicy {
                allow_metered,
                ..Default::default()
            };
        };
        let online = match profile.GetNetworkConnectivityLevel() {
            Ok(Level::None) => NetworkStatus::No,
            Ok(Level::LocalAccess | Level::ConstrainedInternetAccess | Level::InternetAccess) => {
                NetworkStatus::Yes
            }
            _ => NetworkStatus::Unknown,
        };
        let metered = match profile
            .GetConnectionCost()
            .and_then(|cost| cost.NetworkCostType())
        {
            Ok(Cost::Unrestricted) => NetworkStatus::No,
            Ok(Cost::Fixed | Cost::Variable) => NetworkStatus::Yes,
            _ => NetworkStatus::Unknown,
        };
        NetworkPolicy {
            online,
            metered,
            allow_metered,
        }
    }
}
#[cfg(target_os = "windows")]
mod monitor {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use windows::Networking::Connectivity::{NetworkInformation, NetworkStatusChangedEventHandler};
    pub struct Monitor {
        token: Option<i64>,
        stopped: Arc<AtomicBool>,
    }
    impl Monitor {
        pub fn start(callback: Callback) -> Self {
            let stopped = Arc::new(AtomicBool::new(false));
            let flag = stopped.clone();
            let notify = callback.clone();
            let handler = NetworkStatusChangedEventHandler::new(move |_| {
                if !flag.load(Ordering::Acquire) {
                    notify(windows_monitor::query(false));
                }
                Ok(())
            });
            let token = NetworkInformation::NetworkStatusChanged(&handler).ok();
            callback(windows_monitor::query(false));
            Self { token, stopped }
        }
        pub fn stop(&mut self) {
            self.stopped.store(true, Ordering::Release);
            if let Some(token) = self.token.take() {
                let _ = NetworkInformation::RemoveNetworkStatusChanged(token);
            }
        }
    }
}

#[cfg(target_os = "macos")]
mod monitor {
    use super::*;
    use block2::RcBlock;
    use dispatch2::{DispatchQueue, DispatchQueueAttr, DispatchRetained};
    use std::{
        ffi::c_void,
        sync::{
            Arc, Mutex, OnceLock,
            atomic::{AtomicBool, Ordering},
        },
    };
    static LATEST: OnceLock<Mutex<NetworkPolicy>> = OnceLock::new();
    pub fn query(allow_metered: bool) -> NetworkPolicy {
        let mut policy = *LATEST
            .get_or_init(|| Mutex::new(NetworkPolicy::default()))
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        policy.allow_metered = allow_metered;
        policy
    }
    #[link(name = "Network", kind = "framework")]
    unsafe extern "C" {
        fn nw_path_monitor_create() -> *mut c_void;
        fn nw_path_monitor_set_queue(monitor: *mut c_void, queue: *mut c_void);
        fn nw_path_monitor_set_update_handler(monitor: *mut c_void, handler: *mut c_void);
        fn nw_path_monitor_start(monitor: *mut c_void);
        fn nw_path_monitor_cancel(monitor: *mut c_void);
        fn nw_path_get_status(path: *mut c_void) -> u32;
        fn nw_path_is_expensive(path: *mut c_void) -> bool;
    }
    unsafe extern "C" {
        fn os_release(object: *mut c_void);
    }
    pub struct Monitor {
        monitor: *mut c_void,
        _queue: DispatchRetained<DispatchQueue>,
        stopped: Arc<AtomicBool>,
    }
    impl Monitor {
        pub fn start(callback: Callback) -> Self {
            let stopped = Arc::new(AtomicBool::new(false));
            let flag = stopped.clone();
            let queue = DispatchQueue::new("eagle.network-monitor", DispatchQueueAttr::SERIAL);
            let monitor = unsafe { nw_path_monitor_create() };
            if monitor.is_null() {
                callback(NetworkPolicy::default());
                return Self {
                    monitor,
                    _queue: queue,
                    stopped,
                };
            }
            let handler = RcBlock::new(move |path: *mut c_void| {
                if flag.load(Ordering::Acquire) || path.is_null() {
                    return;
                }
                let online = match unsafe { nw_path_get_status(path) } {
                    1 => NetworkStatus::Yes,
                    2 => NetworkStatus::No,
                    _ => NetworkStatus::Unknown,
                };
                let metered = if unsafe { nw_path_is_expensive(path) } {
                    NetworkStatus::Yes
                } else {
                    NetworkStatus::No
                };
                let policy = NetworkPolicy {
                    online,
                    metered,
                    allow_metered: false,
                };
                *LATEST
                    .get_or_init(|| Mutex::new(NetworkPolicy::default()))
                    .lock()
                    .unwrap_or_else(|p| p.into_inner()) = policy;
                callback(policy);
            });
            unsafe {
                nw_path_monitor_set_queue(
                    monitor,
                    DispatchRetained::as_ptr(&queue).as_ptr().cast(),
                );
                nw_path_monitor_set_update_handler(monitor, RcBlock::as_ptr(&handler).cast());
                nw_path_monitor_start(monitor);
            }
            Self {
                monitor,
                _queue: queue,
                stopped,
            }
        }
        pub fn stop(&mut self) {
            self.stopped.store(true, Ordering::Release);
            if !self.monitor.is_null() {
                unsafe {
                    nw_path_monitor_cancel(self.monitor);
                    os_release(self.monitor);
                }
                self.monitor = std::ptr::null_mut();
            }
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
mod monitor {
    use super::*;
    pub struct Monitor;
    impl Monitor {
        pub fn start(callback: Callback) -> Self {
            callback(NetworkPolicy::default());
            Self
        }
        pub fn stop(&mut self) {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn networkmanager_values_keep_unknown_and_treat_metered_guess_conservatively() {
        assert_eq!(parse_nmcli("full:yes\n", false).metered, NetworkStatus::Yes);
        assert_eq!(
            parse_nmcli("limited:guess-yes\n", false).online,
            NetworkStatus::Yes
        );
        assert_eq!(
            parse_nmcli("none:guess-no\n", false).metered,
            NetworkStatus::Unknown
        );
        assert_eq!(
            parse_nmcli("unavailable:unknown\n", false),
            NetworkPolicy::default()
        );
    }
}
