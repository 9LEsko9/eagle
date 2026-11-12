// SPDX-License-Identifier: GPL-3.0-or-later
//! A process lock owns a loopback activation endpoint; no polling in the primary process.
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    net::{TcpListener, TcpStream},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};
pub enum Instance {
    Primary(Primary),
    Activated,
}
pub struct Primary {
    _lock: File,
    listener: TcpListener,
}
impl Instance {
    pub fn claim(root: &Path) -> io::Result<Self> {
        fs::create_dir_all(root)?;
        let mut lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(root.join("desktop.lock"))?;
        match lock.try_lock() {
            Ok(()) => {
                let listener = TcpListener::bind("127.0.0.1:0")?;
                lock.set_len(0)?;
                writeln!(lock, "{}", listener.local_addr()?.port())?;
                lock.sync_all()?;
                Ok(Self::Primary(Primary {
                    _lock: lock,
                    listener,
                }))
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                // The lock may be acquired just before the primary publishes its port.
                for _ in 0..20 {
                    lock.seek(SeekFrom::Start(0))?;
                    let mut port = String::new();
                    lock.read_to_string(&mut port)?;
                    if let Ok(port) = port.trim().parse::<u16>()
                        && let Ok(mut stream) = TcpStream::connect_timeout(
                            &([127, 0, 0, 1], port).into(),
                            Duration::from_millis(100),
                        )
                    {
                        stream.set_write_timeout(Some(Duration::from_secs(1)))?;
                        stream.write_all(b"eagle-activate\n")?;
                        return Ok(Self::Activated);
                    }
                    thread::sleep(Duration::from_millis(50));
                }
                Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "не удалось активировать открытое окно",
                ))
            }
            Err(std::fs::TryLockError::Error(e)) => Err(e),
        }
    }
}
pub struct Activation {
    stopping: Arc<AtomicBool>,
    address: std::net::SocketAddr,
    worker: Option<thread::JoinHandle<()>>,
    _lock: File,
}
impl Primary {
    pub fn listen(self, activate: impl Fn() + Send + 'static) -> io::Result<Activation> {
        let address = self.listener.local_addr()?;
        let stopping = Arc::new(AtomicBool::new(false));
        let stopped = stopping.clone();
        let worker = thread::Builder::new()
            .name("eg-activation".into())
            .spawn(move || {
                for stream in self.listener.incoming() {
                    if stopped.load(Ordering::Acquire) {
                        break;
                    }
                    let Ok(mut stream) = stream else {
                        break;
                    };
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
                    let mut message = [0; 15];
                    if stream.read_exact(&mut message).is_ok() && &message == b"eagle-activate\n" {
                        activate();
                    }
                }
            })?;
        Ok(Activation {
            stopping,
            address,
            worker: Some(worker),
            _lock: self._lock,
        })
    }
}
impl Drop for Activation {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        if let Ok(mut stream) = TcpStream::connect(self.address) {
            let _ = stream.write_all(b"eagle-shutdown\n");
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn second_launch_activates_and_primary_drop_releases_lock() {
        let root = std::env::temp_dir().join(format!("eagle-instance-{}", std::process::id()));
        let Instance::Primary(primary) = Instance::claim(&root).unwrap() else {
            panic!("unexpected secondary");
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let activation = primary
            .listen(move || {
                let _ = tx.send(());
            })
            .unwrap();
        assert!(matches!(
            Instance::claim(&root).unwrap(),
            Instance::Activated
        ));
        rx.recv_timeout(Duration::from_secs(2)).unwrap();
        drop(activation);
        assert!(matches!(
            Instance::claim(&root).unwrap(),
            Instance::Primary(_)
        ));
        fs::remove_dir_all(root).unwrap();
    }
}
