// SPDX-License-Identifier: GPL-3.0-or-later
//! A process lock owns a loopback activation endpoint; no polling in the primary process.
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    net::{TcpListener, TcpStream},
    path::Path,
    sync::{
        Arc, Mutex,
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
    activation: Activation,
}
#[derive(Default)]
struct Handler {
    callback: Option<Box<dyn Fn() + Send>>,
    pending: bool,
}
impl Instance {
    pub fn claim(root: &Path) -> io::Result<Self> {
        fs::create_dir_all(root)?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(root.join("desktop.lock"))?;
        match lock.try_lock() {
            Ok(()) => {
                let port_path = root.join("desktop.port");
                match fs::remove_file(&port_path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
                let listener = TcpListener::bind("127.0.0.1:0")?;
                let activation = Activation::start(listener, Lock::Instance(lock))?;
                let temporary = root.join("desktop.port.tmp");
                fs::write(&temporary, activation.address.port().to_string())?;
                fs::rename(temporary, port_path)?;
                Ok(Self::Primary(Primary { activation }))
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                // The lock may be acquired just before the primary publishes its port.
                for _ in 0..20 {
                    if let Ok(port) = fs::read_to_string(root.join("desktop.port"))
                        && let Ok(port) = port.trim().parse::<u16>()
                        && let Ok(mut stream) = TcpStream::connect_timeout(
                            &([127, 0, 0, 1], port).into(),
                            Duration::from_millis(100),
                        )
                    {
                        stream.set_write_timeout(Some(Duration::from_secs(1)))?;
                        stream.set_read_timeout(Some(Duration::from_millis(100)))?;
                        let mut reply = [0; 16];
                        if stream.write_all(b"eagle-activate\n").is_ok()
                            && stream.read_exact(&mut reply).is_ok()
                            && &reply == b"eagle-activated\n"
                        {
                            return Ok(Self::Activated);
                        }
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
#[allow(dead_code)]
enum Lock {
    Instance(File),
    Storage(Arc<eagle_store::relocation::StorageLease>),
}
impl Primary {
    pub fn claim_empty(
        root: &Path,
    ) -> io::Result<(Self, Arc<eagle_store::relocation::StorageLease>)> {
        let lease = Arc::new(eagle_store::relocation::StorageLease::claim_empty(root)?);
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let activation = Activation::start(listener, Lock::Storage(lease.clone()))?;
        fs::write(
            root.join("desktop.port"),
            activation.address.port().to_string(),
        )?;
        Ok((Self { activation }, lease))
    }
}
pub struct Activation {
    stopping: Arc<AtomicBool>,
    address: std::net::SocketAddr,
    worker: Option<thread::JoinHandle<()>>,
    handler: Arc<Mutex<Handler>>,
    _lock: Lock,
}
impl Primary {
    pub fn listen(self, activate: impl Fn() + Send + 'static) -> io::Result<Activation> {
        {
            let mut handler = self
                .activation
                .handler
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if handler.pending {
                activate();
                handler.pending = false;
            }
            handler.callback = Some(Box::new(activate));
        }
        Ok(self.activation)
    }
}
impl Activation {
    fn start(listener: TcpListener, lock: Lock) -> io::Result<Self> {
        let address = listener.local_addr()?;
        let stopping = Arc::new(AtomicBool::new(false));
        let stopped = stopping.clone();
        let handler = Arc::new(Mutex::new(Handler::default()));
        let worker_handler = handler.clone();
        let worker = thread::Builder::new()
            .name("eg-activation".into())
            .spawn(move || {
                for stream in listener.incoming() {
                    if stopped.load(Ordering::Acquire) {
                        break;
                    }
                    let Ok(mut stream) = stream else {
                        break;
                    };
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
                    let mut message = [0; 15];
                    if stream.read_exact(&mut message).is_ok() && &message == b"eagle-activate\n" {
                        let mut handler = worker_handler.lock().unwrap_or_else(|e| e.into_inner());
                        if let Some(callback) = &handler.callback {
                            callback();
                        } else {
                            handler.pending = true;
                        }
                        drop(handler);
                        let _ = stream.set_write_timeout(Some(Duration::from_secs(1)));
                        let _ = stream.write_all(b"eagle-activated\n");
                    }
                }
            })?;
        Ok(Activation {
            stopping,
            address,
            worker: Some(worker),
            handler,
            _lock: lock,
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
    fn activation_is_accepted_before_delayed_listener_binding() {
        let root =
            std::env::temp_dir().join(format!("eagle-delayed-instance-{}", std::process::id()));
        let Instance::Primary(primary) = Instance::claim(&root).unwrap() else {
            panic!("unexpected secondary");
        };
        thread::sleep(Duration::from_millis(3100));
        for _ in 0..2 {
            assert!(matches!(
                Instance::claim(&root).unwrap(),
                Instance::Activated
            ));
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let activation = primary
            .listen(move || {
                let _ = tx.send(());
            })
            .unwrap();
        rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(rx.try_recv().is_err());
        assert!(matches!(
            Instance::claim(&root).unwrap(),
            Instance::Activated
        ));
        rx.recv_timeout(Duration::from_secs(2)).unwrap();
        drop(activation);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stale_port_does_not_activate_an_unrelated_listener() {
        let root =
            std::env::temp_dir().join(format!("eagle-stale-instance-{}", std::process::id()));
        let Instance::Primary(primary) = Instance::claim(&root).unwrap() else {
            panic!("unexpected secondary");
        };
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        fs::write(
            root.join("desktop.port"),
            listener.local_addr().unwrap().port().to_string(),
        )
        .unwrap();
        let stopping = Arc::new(AtomicBool::new(false));
        let stopped = stopping.clone();
        let foreign = thread::spawn(move || {
            let mut accepted = 0;
            while !stopped.load(Ordering::Acquire) {
                if let Ok((mut stream, _)) = listener.accept() {
                    accepted += 1;
                    if accepted == 1 {
                        thread::sleep(Duration::from_millis(150));
                    } else {
                        let _ = stream.write_all(b"wrong\n");
                    }
                } else {
                    thread::sleep(Duration::from_millis(1));
                }
            }
            accepted
        });
        let result = Instance::claim(&root);
        stopping.store(true, Ordering::Release);
        assert!(foreign.join().unwrap() > 1);
        assert!(matches!(result, Err(error) if error.kind() == io::ErrorKind::TimedOut));
        drop(primary);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn second_launch_activates_and_primary_drop_releases_lock() {
        let root = std::env::temp_dir().join(format!("eagle-instance-{}", std::process::id()));
        let Instance::Primary(primary) = Instance::claim(&root).unwrap() else {
            panic!("unexpected secondary");
        };
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .open(root.join("desktop.lock"))
            .unwrap();
        assert!(matches!(
            lock.try_lock(),
            Err(std::fs::TryLockError::WouldBlock)
        ));
        assert_eq!(
            fs::read_to_string(root.join("desktop.port"))
                .unwrap()
                .parse::<u16>()
                .unwrap(),
            primary.activation.address.port()
        );
        drop(lock);
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
