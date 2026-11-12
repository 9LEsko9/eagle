// SPDX-FileCopyrightText: 2026 eagle contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Bounded, demand-driven logging. Never pass titles or search queries at info.

use std::fmt::{self, Write as _};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use log::{Level, LevelFilter, Log, Metadata, Record};

const BUFFER_BYTES: usize = 64 * 1024;
const RECORD_BYTES: usize = 4096;
const FILE_BYTES: u64 = 1_000_000;
const FLUSH_DELAY: Duration = Duration::from_secs(1);

fn sensitive(key: &str) -> bool {
    [
        "token",
        "access_token",
        "refresh_token",
        "id_token",
        "sign",
        "sig",
        "signature",
        "key",
        "api_key",
        "apikey",
        "client_secret",
        "password",
        "authorization",
        "cookie",
        "auth",
        "oauth_token",
        "x-amz-signature",
        "x-amz-security-token",
        "x-amz-credential",
    ]
    .iter()
    .any(|candidate| key.eq_ignore_ascii_case(candidate))
}

/// Defence in depth for accidental raw parameters and headers; no regex.
/// Typed secrets remain mandatory: arbitrary unlabelled secrets cannot be detected.
pub fn redact(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut output = String::with_capacity(input.len());
    let mut cursor = 0;
    let mut copied = 0;
    while cursor < bytes.len() {
        if bytes[cursor].is_ascii_alphabetic()
            && (cursor == 0
                || !matches!(bytes[cursor - 1], b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'-'))
        {
            let start = cursor;
            while cursor < bytes.len()
                && (bytes[cursor].is_ascii_alphanumeric() || matches!(bytes[cursor], b'_' | b'-'))
            {
                cursor += 1;
            }
            let key = &input[start..cursor];
            let mut value = cursor;
            while value < bytes.len() && matches!(bytes[value], b' ' | b'\t' | b'"' | b'\'') {
                value += 1;
            }
            if sensitive(key) && value < bytes.len() && matches!(bytes[value], b'=' | b':') {
                value += 1;
                while value < bytes.len() && matches!(bytes[value], b' ' | b'\t') {
                    value += 1;
                }
                let quote = bytes
                    .get(value)
                    .copied()
                    .filter(|byte| matches!(byte, b'"' | b'\''));
                if quote.is_some() {
                    value += 1;
                }
                output.push_str(&input[copied..value]);
                let header =
                    key.eq_ignore_ascii_case("authorization") || key.eq_ignore_ascii_case("cookie");
                cursor = value;
                while cursor < bytes.len() {
                    let byte = bytes[cursor];
                    let end = if let Some(quote) = quote {
                        byte == quote || matches!(byte, b'\n' | b'\r')
                    } else {
                        matches!(byte, b'&' | b'#' | b'\n' | b'\r' | b'"' | b'\'')
                            || (!header && byte.is_ascii_whitespace())
                    };
                    if end {
                        break;
                    }
                    cursor += 1;
                }
                output.push_str("[REDACTED]");
                copied = cursor;
            }
        } else {
            cursor += 1;
        }
    }
    output.push_str(&input[copied..]);
    output
}

// Formatting is capped before allocation can grow with a message or kv field.
struct Line(String);
impl fmt::Write for Line {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let mut end = text.len().min(RECORD_BYTES.saturating_sub(self.0.len()));
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        self.0.push_str(&text[..end]);
        if end < text.len() {
            Err(fmt::Error)
        } else {
            Ok(())
        }
    }
}

impl<'kvs> log::kv::VisitSource<'kvs> for Line {
    fn visit_pair(
        &mut self,
        key: log::kv::Key<'kvs>,
        value: log::kv::Value<'kvs>,
    ) -> Result<(), log::kv::Error> {
        let _ = write!(self, " {key}={value}");
        Ok(())
    }
}

fn format_record(record: &Record<'_>) -> String {
    let mut line = Line(String::with_capacity(RECORD_BYTES));
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let _ = write!(
        line,
        "{timestamp} {} {} {}",
        record.level(),
        record.target(),
        record.args()
    );
    let _ = record.key_values().visit(&mut line);
    // Bound again after replacement expansion and escape newlines against log injection.
    let clean = redact(&line.0).replace(['\n', '\r', '\0'], " ");
    let mut line = Line(String::with_capacity(RECORD_BYTES + 1));
    let _ = line.write_str(&clean);
    line.0.push('\n');
    line.0
}

#[derive(Debug, thiserror::Error)]
pub enum InitError {
    #[error("invalid log filter (use info,eagle_net=debug)")]
    Filter,
    #[error("a global logger is already installed")]
    Installed,
    #[error(transparent)]
    Io(#[from] io::Error),
}

struct Filter(Vec<(String, LevelFilter)>);
impl Filter {
    fn parse(spec: &str) -> Result<Self, InitError> {
        let mut directives = vec![(String::new(), LevelFilter::Info)];
        for item in spec.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            let (target, level) = item.split_once('=').unwrap_or(("", item));
            if target.len() > 256 || directives.len() >= 128 {
                return Err(InitError::Filter);
            }
            directives.push((
                target.trim().to_owned(),
                level.trim().parse().map_err(|_| InitError::Filter)?,
            ));
        }
        directives.sort_by_key(|(name, _)| name.len());
        Ok(Self(directives))
    }

    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        self.0
            .iter()
            .rev()
            .find(|(name, _)| {
                name.is_empty()
                    || metadata.target() == name
                    || metadata
                        .target()
                        .strip_prefix(name)
                        .is_some_and(|suffix| suffix.starts_with("::"))
            })
            .is_some_and(|(_, level)| metadata.level() <= *level)
    }
}

struct Files {
    dir: PathBuf,
    file: Option<File>,
    size: u64,
}
impl Files {
    fn open(dir: &Path) -> io::Result<Self> {
        fs::create_dir_all(dir)?;
        let file = Self::open_file(&dir.join("eagle.log"))?;
        let size = file.metadata()?.len();
        Ok(Self {
            dir: dir.to_owned(),
            file: Some(file),
            size,
        })
    }

    fn open_file(path: &Path) -> io::Result<File> {
        let mut options = OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options.open(path)
    }

    fn write(&mut self, line: &str) -> io::Result<()> {
        if self.size + line.len() as u64 > FILE_BYTES {
            self.file.take(); // Close before renaming on Windows.
            let previous = self.dir.join("eagle.previous.log");
            match fs::remove_file(&previous) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            fs::rename(self.dir.join("eagle.log"), previous)?;
            self.file = Some(Self::open_file(&self.dir.join("eagle.log"))?);
            self.size = 0;
        }
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| io::Error::other("log file unavailable"))?;
        file.write_all(line.as_bytes())?;
        self.size += line.len() as u64;
        Ok(())
    }

    fn sync(&self) -> io::Result<()> {
        self.file
            .as_ref()
            .ok_or_else(|| io::Error::other("log file unavailable"))?
            .sync_data()
    }
}

struct State {
    buffer: String,
    dropped: u64,
    deadline: Option<Instant>,
    urgent: bool,
    stopping: bool,
    files: Files,
    error: Option<io::Error>,
}

impl State {
    fn enqueue(&mut self, line: &str) -> bool {
        if self.buffer.len() + line.len() <= BUFFER_BYTES {
            self.buffer.push_str(line);
            if self.buffer.len() >= BUFFER_BYTES - RECORD_BYTES {
                self.urgent = true;
            }
            if self.deadline.is_none() {
                self.deadline = Some(Instant::now() + FLUSH_DELAY);
            }
            true
        } else {
            self.dropped = self.dropped.saturating_add(1);
            self.urgent = true;
            false
        }
    }

    fn drain(&mut self, durable: bool) -> io::Result<()> {
        // Keep whole records together at the rotation boundary.
        let result = (|| {
            for line in self.buffer.split_inclusive('\n') {
                self.files.write(line)?;
            }
            if self.dropped > 0 {
                self.files
                    .write(&format!("WARN пропущено {} записей\n", self.dropped))?;
            }
            if durable {
                self.files.sync()?;
            }
            Ok(())
        })();
        self.buffer.clear();
        self.dropped = 0;
        self.deadline = None;
        self.urgent = false;
        result
    }
}

struct Backend {
    state: Mutex<State>,
    ready: Condvar,
    filter: RwLock<Filter>,
    #[cfg(target_os = "android")]
    system: android_logger::AndroidLogger,
    #[cfg(target_os = "ios")]
    system: oslog::OsLog,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

impl Backend {
    fn new(dir: &Path, spec: &str) -> Result<Arc<Self>, InitError> {
        Ok(Arc::new(Self {
            state: Mutex::new(State {
                buffer: String::with_capacity(BUFFER_BYTES),
                dropped: 0,
                deadline: None,
                urgent: false,
                stopping: false,
                files: Files::open(dir)?,
                error: None,
            }),
            ready: Condvar::new(),
            filter: RwLock::new(Filter::parse(spec)?),
            #[cfg(target_os = "android")]
            system: android_logger::AndroidLogger::new(
                android_logger::Config::default()
                    .with_tag("eagle")
                    .with_max_level(LevelFilter::Trace),
            ),
            #[cfg(target_os = "ios")]
            system: oslog::OsLog::new("eagle", "app"),
        }))
    }

    fn writer(&self) {
        let mut state = lock(&self.state);
        loop {
            if state.stopping {
                break;
            }
            if state.urgent
                || state
                    .deadline
                    .is_some_and(|deadline| Instant::now() >= deadline)
            {
                if let Err(error) = state.drain(true) {
                    state.error = Some(error);
                }
                self.ready.notify_all();
            } else if let Some(deadline) = state.deadline {
                state = self
                    .ready
                    .wait_timeout(state, deadline.saturating_duration_since(Instant::now()))
                    .unwrap_or_else(|poison| poison.into_inner())
                    .0;
            } else {
                state = self
                    .ready
                    .wait(state)
                    .unwrap_or_else(|poison| poison.into_inner());
            }
        }
    }

    fn flush_file(&self) -> io::Result<()> {
        let mut state = lock(&self.state);
        let result = state.drain(true);
        self.ready.notify_all();
        result?;
        state.error.take().map_or(Ok(()), Err)
    }

    fn panic(&self, message: fmt::Arguments<'_>) {
        // Do not recurse through log or depend on the writer thread.
        let mut line = Line(String::with_capacity(RECORD_BYTES));
        let _ = write!(line, "PANIC {message}");
        let clean = redact(&line.0).replace(['\n', '\r', '\0'], " ");
        let mut state = lock(&self.state);
        let _ = state.drain(false);
        let _ = state.files.write(&(clean + "\n"));
        let backtrace = std::backtrace::Backtrace::force_capture();
        let mut line = Line(String::with_capacity(RECORD_BYTES));
        let _ = write!(line, "{backtrace}");
        for line in redact(&line.0).lines() {
            let _ = state.files.write(&(line.to_owned() + "\n"));
        }
        let _ = state.files.sync();
        self.ready.notify_all();
    }

    fn output(&self, level: Level, line: &str) {
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        {
            let _ = level;
            let _ = io::stderr().lock().write_all(line.as_bytes());
        }
        #[cfg(target_os = "android")]
        self.system.log(
            &Record::builder()
                .level(level)
                .target("eagle")
                .args(format_args!("{line}"))
                .build(),
        );
        #[cfg(target_os = "ios")]
        self.system.with_level(
            match level {
                Level::Error => oslog::Level::Fault,
                Level::Warn => oslog::Level::Error,
                Level::Info => oslog::Level::Default,
                _ => oslog::Level::Debug,
            },
            line,
        );
    }
}

struct Adapter(Arc<Backend>);
impl Log for Adapter {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        self.0
            .filter
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .enabled(metadata)
    }

    fn log(&self, record: &Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let line = format_record(record);
        let mut state = lock(&self.0.state);
        if state.stopping {
            return;
        }
        // Preserve urgent records even when a burst has filled the info buffer.
        while record.level() <= Level::Warn && state.buffer.len() + line.len() > BUFFER_BYTES {
            state.urgent = true;
            self.0.ready.notify_all();
            state = self.0.ready.wait(state).unwrap_or_else(|p| p.into_inner());
            if state.stopping {
                return;
            }
        }
        let was_empty = state.deadline.is_none();
        state.enqueue(&line);
        if record.level() <= Level::Warn {
            state.urgent = true;
        }
        if was_empty || state.urgent {
            self.0.ready.notify_all();
        }
        // Warn/error return only once flushed and sync_data has completed.
        while state.urgent && !state.stopping {
            state = self.0.ready.wait(state).unwrap_or_else(|p| p.into_inner());
        }
        drop(state);
        self.0.output(record.level(), &line);
    }

    fn flush(&self) {
        let _ = self.0.flush_file();
    }
}

/// Keep alive until application shutdown. Settings can call `set_filter`.
pub struct Logger {
    backend: Arc<Backend>,
    worker: Option<JoinHandle<()>>,
}

impl Logger {
    /// `directory` must be application-private; mobile shells supply their sandbox data path.
    pub fn init(directory: &Path) -> Result<Self, InitError> {
        let spec = std::env::var("EAGLE_LOG").unwrap_or_else(|_| "info".into());
        let backend = Backend::new(directory, &spec)?;
        let writer = Arc::clone(&backend);
        let worker = thread::Builder::new()
            .name("eg-log".into())
            .spawn(move || writer.writer())?;
        let mut logger = Self {
            backend: Arc::clone(&backend),
            worker: Some(worker),
        };
        if log::set_boxed_logger(Box::new(Adapter(Arc::clone(&backend)))).is_err() {
            logger.stop();
            return Err(InitError::Installed);
        }
        log::set_max_level(log::STATIC_MAX_LEVEL);
        std::panic::set_hook(Box::new(move |info| backend.panic(format_args!("{info}"))));
        Ok(logger)
    }

    pub fn set_filter(&self, spec: &str) -> Result<(), InitError> {
        *self
            .backend
            .filter
            .write()
            .unwrap_or_else(|p| p.into_inner()) = Filter::parse(spec)?;
        Ok(())
    }

    pub fn flush(&self) -> io::Result<()> {
        self.backend.flush_file()
    }

    /// Snapshot for “Copy report”, including buffered records, oldest file first.
    pub fn report(&self) -> io::Result<String> {
        let mut state = lock(&self.backend.state);
        state.drain(true)?;
        self.backend.ready.notify_all();
        let mut report = String::new();
        for name in ["eagle.previous.log", "eagle.log"] {
            match fs::read_to_string(state.files.dir.join(name)) {
                Ok(text) => report.push_str(&text),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        Ok(report)
    }

    fn stop(&mut self) {
        let _ = self.flush();
        lock(&self.backend.state).stopping = true;
        self.backend.ready.notify_all();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
impl Drop for Logger {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Desktop defaults; mobile entry points should pass their native data directory to init.
pub fn default_directory() -> io::Result<PathBuf> {
    if let Some(path) = std::env::var_os("EAGLE_DATA_DIR") {
        return Ok(PathBuf::from(path).join("logs"));
    }
    let base = if cfg!(target_os = "windows") {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
    } else if cfg!(target_vendor = "apple") {
        std::env::var_os("HOME").map(|home| PathBuf::from(home).join("Library/Application Support"))
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share"))
            })
    };
    base.map(|base| base.join("eagle/logs"))
        .ok_or_else(|| io::Error::other("application data directory is unavailable"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use eagle_core::{SignedUrl, Token};
    use std::sync::atomic::{AtomicU64, Ordering};

    static SERIAL: AtomicU64 = AtomicU64::new(0);
    fn directory() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "eagle-log-test-{}-{}",
            std::process::id(),
            SERIAL.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn secrets_are_safe_in_display_debug_and_backend() {
        let token = Token::new("very-secret".into());
        let url = SignedUrl::new("https://user:pass@host/track?sign=hidden#fragment".into());
        assert_eq!(format!("{token} {token:?}"), "[REDACTED] [REDACTED]");
        assert_eq!(
            format!("{url} {url:?}"),
            "https://host/track https://host/track"
        );
        assert_eq!(token.expose(), "very-secret");
        assert!(url.expose().contains("hidden"));
        for input in [
            "token=LEAKME&track=42",
            "sign=LEAKME",
            "SIGNATURE=LEAKME",
            "{\"access_token\":\"LEAKME\"}",
            "Authorization: Bearer LEAKME",
            "Cookie: sid=LEAKME; other=LEAKME",
            "client_secret = 'LEAKME with spaces&moreLEAKME'",
            "https://host/path?X-Amz-Signature=LEAKME&track=42",
            "https://host/track?sig=LEAKME&key=LEAKME",
        ] {
            assert!(!redact(input).contains("LEAKME"), "{input}");
        }
        assert_eq!(redact("track_id=42 provider=vk"), "track_id=42 provider=vk");
        let line = format_record(
            &Record::builder()
                .args(format_args!("token=secret\nforged line"))
                .key_values(&[("sign", "secret")])
                .level(Level::Info)
                .build(),
        );
        assert!(!line.contains("secret"));
        assert_eq!(line.lines().count(), 1);
        let long = "é".repeat(RECORD_BYTES);
        let line = format_record(&Record::builder().args(format_args!("{long}")).build());
        assert!(line.len() <= RECORD_BYTES + 1);
    }

    #[test]
    fn filter_selects_longest_crate_boundary_and_can_turn_off() {
        let filter = Filter::parse("info,eagle_net=debug,eagle_net::private=off").unwrap();
        let enabled = |target, level| {
            filter.enabled(&Metadata::builder().target(target).level(level).build())
        };
        assert!(enabled("eagle_net::request", Level::Debug));
        assert!(!enabled("eagle_network", Level::Debug));
        assert!(!enabled("eagle_net::private", Level::Error));
        assert!(enabled("other", Level::Info));
        assert!(!enabled("other", Level::Trace));
        assert!(Filter::parse("invalid").is_err());
    }

    #[test]
    fn overflow_is_bounded_and_has_loss_marker() {
        let dir = directory();
        let backend = Backend::new(&dir, "info").unwrap();
        let mut state = lock(&backend.state);
        let line = "x".repeat(RECORD_BYTES - 1) + "\n";
        for _ in 0..BUFFER_BYTES / RECORD_BYTES {
            assert!(state.enqueue(&line));
        }
        assert!(!state.enqueue("dropped\n"));
        assert!(!state.enqueue("also dropped\n"));
        assert_eq!(state.buffer.len(), BUFFER_BYTES);
        state.drain(true).unwrap();
        assert!(state.deadline.is_none());
        assert!(
            fs::read_to_string(dir.join("eagle.log"))
                .unwrap()
                .contains("пропущено 2 записей")
        );
        drop(state);
        drop(backend);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rotation_keeps_exactly_two_bounded_files() {
        let dir = directory();
        let mut files = Files::open(&dir).unwrap();
        let line = "x".repeat(999) + "\n";
        for _ in 0..2501 {
            files.write(&line).unwrap();
        }
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 2);
        assert_eq!(
            fs::metadata(dir.join("eagle.previous.log")).unwrap().len(),
            FILE_BYTES
        );
        assert_eq!(fs::metadata(dir.join("eagle.log")).unwrap().len(), 501_000);
        drop(files);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn timer_flushes_info_warn_is_durable_and_panic_drains_itself() {
        let dir = directory();
        let backend = Backend::new(&dir, "info").unwrap();
        let writer = Arc::clone(&backend);
        let worker = thread::spawn(move || writer.writer());
        let adapter = Adapter(Arc::clone(&backend));
        adapter.log(
            &Record::builder()
                .args(format_args!("buffered info"))
                .level(Level::Info)
                .build(),
        );
        let limit = Instant::now() + Duration::from_secs(3);
        while fs::metadata(dir.join("eagle.log")).unwrap().len() == 0 {
            assert!(Instant::now() < limit);
            thread::sleep(Duration::from_millis(10));
        }
        assert!(lock(&backend.state).deadline.is_none());
        lock(&backend.state).enqueue(&("x".repeat(BUFFER_BYTES - 1) + "\n"));
        adapter.log(
            &Record::builder()
                .args(format_args!("urgent warning"))
                .level(Level::Warn)
                .build(),
        );
        assert!(
            fs::read_to_string(dir.join("eagle.log"))
                .unwrap()
                .contains("urgent warning")
        );
        lock(&backend.state).stopping = true;
        backend.ready.notify_all();
        worker.join().unwrap();
        lock(&backend.state).enqueue("info before panic\n");
        backend.panic(format_args!("token=hidden"));
        let report = fs::read_to_string(dir.join("eagle.log")).unwrap();
        assert!(report.contains("info before panic"));
        assert!(report.contains("PANIC token=[REDACTED]"));
        assert!(!report.contains("hidden"));
        assert!(report.contains("logging")); // Backtrace exists even without RUST_BACKTRACE.
        drop(adapter);
        drop(backend);
        fs::remove_dir_all(dir).unwrap();
    }
}
