// SPDX-License-Identifier: GPL-3.0-or-later
//! Event-driven system network information; unknown when unavailable.
use crate::policy::NetworkPolicy;
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
        Some("none" | "limited" | "portal") => NetworkStatus::No,
        Some("full") => NetworkStatus::Yes,
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

#[cfg_attr(not(any(target_os = "windows", test)), allow(dead_code))]
fn windows_status(level: i32) -> NetworkStatus {
    match level {
        0 | 1 => NetworkStatus::No,
        3 => NetworkStatus::Yes,
        _ => NetworkStatus::Unknown,
    }
}

#[cfg(any(target_os = "linux", test))]
fn relevant_monitor_line(line: &str) -> bool {
    line.starts_with("Connectivity is now")
        || line.contains("is now the primary connection")
        || line.ends_with(": connection profile changed")
}

#[cfg(any(target_os = "linux", test))]
fn handle_monitor_line(
    line: &str,
    previous: &mut NetworkPolicy,
    query: impl FnOnce() -> NetworkPolicy,
    callback: impl FnOnce(NetworkPolicy),
) {
    if relevant_monitor_line(line) {
        let policy = query();
        if policy != *previous {
            callback(policy);
            *previous = policy;
        }
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
            let initial = query(false);
            callback(initial);
            let process = Command::new("nmcli")
                .env("LC_ALL", "C")
                .arg("monitor")
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn();
            let Ok(mut process) = process else {
                if initial != NetworkPolicy::default() {
                    callback(NetworkPolicy::default());
                }
                return Self {
                    child: None,
                    worker: None,
                    stopped,
                };
            };
            let Some(stdout) = process.stdout.take() else {
                let _ = process.kill();
                let _ = process.wait();
                if initial != NetworkPolicy::default() {
                    callback(NetworkPolicy::default());
                }
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
                    let mut previous = initial;
                    for line in BufReader::new(stdout).lines() {
                        if stop.load(Ordering::Acquire) {
                            break;
                        }
                        let Ok(line) = line else { break };
                        handle_monitor_line(
                            &line,
                            &mut previous,
                            || query(false),
                            |policy| callback(policy),
                        );
                    }
                    if !stop.load(Ordering::Acquire) && previous != NetworkPolicy::default() {
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
    use windows::Networking::Connectivity::{NetworkCostType as Cost, NetworkInformation};
    pub fn query(allow_metered: bool) -> NetworkPolicy {
        let Ok(profile) = NetworkInformation::GetInternetConnectionProfile() else {
            return NetworkPolicy {
                allow_metered,
                ..Default::default()
            };
        };
        let online = match profile.GetNetworkConnectivityLevel() {
            Ok(level) => windows_status(level.0),
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
            NetworkStatus::No
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

    #[test]
    fn local_or_captive_networks_are_not_online() {
        assert_eq!(windows_status(0), NetworkStatus::No);
        assert_eq!(windows_status(1), NetworkStatus::No);
        assert_eq!(windows_status(2), NetworkStatus::Unknown);
        assert_eq!(windows_status(3), NetworkStatus::Yes);
        assert_eq!(windows_status(4), NetworkStatus::Unknown);
        assert_eq!(parse_nmcli("limited:no\n", false).online, NetworkStatus::No);
        assert_eq!(parse_nmcli("portal:no\n", false).online, NetworkStatus::No);
    }

    #[test]
    fn monitor_lines_trigger_only_relevant_changes() {
        assert!(relevant_monitor_line("Connectivity is now 'full'"));
        assert!(relevant_monitor_line(
            "'Home' is now the primary connection"
        ));
        assert!(relevant_monitor_line("Home: connection profile changed"));
        assert!(!relevant_monitor_line("wlp2s0: using connection 'Home'"));
        assert!(!relevant_monitor_line(
            "Networkmanager is now in the 'connected' state"
        ));
    }

    #[test]
    fn monitor_queries_profile_changes_and_notifies_only_on_policy_changes() {
        let mut previous = parse_nmcli("full:no", false);
        let metered = parse_nmcli("full:yes", false);
        let mut queries = 0;
        let mut notifications = Vec::new();
        for line in [
            "wlp2s0: using connection 'Home'",
            "Home: connection profile changed",
            "Home: connection profile changed",
        ] {
            handle_monitor_line(
                line,
                &mut previous,
                || {
                    queries += 1;
                    metered
                },
                |policy| notifications.push(policy),
            );
        }
        assert_eq!(queries, 2);
        assert_eq!(notifications, vec![metered]);
        assert_eq!(previous, metered);
    }
}
