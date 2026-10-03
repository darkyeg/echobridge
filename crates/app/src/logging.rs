//! Bounded, asynchronous diagnostic logging. It records messages and numbers, never audio.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender, TrySendError, sync_channel};
use std::time::{Duration, Instant};

use crate::leak_test::Timestamp;

/// Rotate at 1 MiB and keep three backups so long calls retain their diagnostic history.
const MAX_BYTES: u64 = 1 << 20;
const BACKUPS: usize = 3;
const QUEUE_LINES: usize = 256;
const MAX_LINE_BYTES: usize = 4096;

enum Message {
    Line(String),
    Flush(Sender<()>),
}

struct FileLog {
    sender: SyncSender<Message>,
    skipped: Arc<AtomicU64>,
}

impl log::Log for FileLog {
    /// Ignore dependencies' per-frame messages, which would bury useful diagnostics.
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        // The app's own target is the binary name, `EchoBridge`; the libraries are `echobridge_*`.
        let ours = metadata.target().get(..10).is_some_and(|prefix| prefix.eq_ignore_ascii_case("echobridge"));
        metadata.level() <= log::Level::Info && ours
    }

    fn log(&self, record: &log::Record<'_>) {
        if self.enabled(record.metadata()) {
            let mut line = format!("{} {:<5} {}\n", Timestamp::now().iso(), record.level(), record.args());
            if line.len() > MAX_LINE_BYTES {
                const SUFFIX: &str = "… [truncated]\n";
                let mut end = MAX_LINE_BYTES - SUFFIX.len();
                while !line.is_char_boundary(end) {
                    end -= 1;
                }
                line.truncate(end);
                line.push_str(SUFFIX);
            }
            // Logging must never make an audio thread wait for a disk or a full queue.
            if self.sender.try_send(Message::Line(line)).is_err() {
                self.skipped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    fn flush(&self) {
        let (done, flushed) = std::sync::mpsc::channel();
        let deadline = Instant::now() + Duration::from_millis(250);
        let mut message = Message::Flush(done);
        // Shutdown can wait briefly for the barrier even when the queue is full.
        // Audio logging itself always uses try_send without waiting.
        loop {
            match self.sender.try_send(message) {
                Ok(()) => {
                    flushed.recv_timeout(deadline.saturating_duration_since(Instant::now())).ok();
                    return;
                }
                Err(TrySendError::Full(pending)) => {
                    message = pending;
                    if Instant::now() >= deadline {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(TrySendError::Disconnected(_)) => return,
            }
        }
    }
}

struct LogFile {
    path: PathBuf,
    file: Option<File>,
    bytes: u64,
}

impl LogFile {
    fn open(folder: &Path) -> io::Result<Self> {
        fs::create_dir_all(folder)?;
        let path = folder.join("echobridge.log");
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let bytes = file.metadata()?.len();
        let mut log = Self { path, file: Some(file), bytes };
        if bytes >= MAX_BYTES {
            log.rotate().ok();
        }
        Ok(log)
    }

    fn rotate(&mut self) -> io::Result<()> {
        // Windows needs the file handle closed before it can be renamed.
        self.file.take();
        let result = (|| {
            for index in (1..BACKUPS).rev() {
                let source = self.backup_path(index);
                match fs::metadata(&source) {
                    Ok(metadata) if metadata.is_file() => fs::rename(&source, self.backup_path(index + 1))?,
                    Ok(_) => return Err(io::Error::new(io::ErrorKind::AlreadyExists, "a log backup path is not a file")),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
            }
            fs::rename(&self.path, self.backup_path(1))
        })();
        // Even if rotation failed, preserve the active log and reopen it for append.
        let file = OpenOptions::new().create(true).append(true).open(&self.path)?;
        self.bytes = file.metadata()?.len();
        self.file = Some(file);
        result
    }

    fn backup_path(&self, index: usize) -> PathBuf {
        let name = if index == 1 { "echobridge.old.log".into() } else { format!("echobridge.old{index}.log") };
        self.path.with_file_name(name)
    }

    fn write(&mut self, line: &str) -> io::Result<()> {
        if self.bytes + line.len() as u64 > MAX_BYTES {
            self.rotate().ok();
        }
        if self.file.is_none() {
            let file = OpenOptions::new().create(true).append(true).open(&self.path)?;
            self.bytes = file.metadata()?.len();
            self.file = Some(file);
        }
        self.file.as_mut().expect("the log was reopened").write_all(line.as_bytes())?;
        self.bytes += line.len() as u64;
        Ok(())
    }

    fn flush(&mut self) {
        if let Some(file) = &mut self.file {
            file.flush().ok();
        }
    }
}

/// Open a log writer on a separate thread. A logging failure never stops audio.
pub fn init(folder: &Path) {
    let Ok(file) = LogFile::open(folder) else { return };
    let (sender, messages) = sync_channel(QUEUE_LINES);
    let skipped = Arc::new(AtomicU64::new(0));
    let lost = skipped.clone();
    let writer = std::thread::Builder::new()
        .name("EchoBridge logging".into())
        .spawn(move || write_messages(file, messages, lost));
    if writer.is_ok() && log::set_boxed_logger(Box::new(FileLog { sender, skipped })).is_ok() {
        log::set_max_level(log::LevelFilter::Info);
        log::info!("EchoBridge {} started", env!("CARGO_PKG_VERSION"));
    }
}

fn write_messages(mut file: LogFile, messages: Receiver<Message>, skipped: Arc<AtomicU64>) {
    while let Ok(message) = messages.recv() {
        let count = skipped.swap(0, Ordering::Relaxed);
        if count > 0 {
            file.write(&format!("{} WARN  logging queue full: skipped {count} messages\n", Timestamp::now().iso()))
                .ok();
        }
        match message {
            Message::Line(line) => {
                file.write(&line).ok();
            }
            Message::Flush(done) => {
                file.flush();
                done.send(()).ok();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Folder(PathBuf);

    impl Folder {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "echobridge-log-test-{}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Folder {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).ok();
        }
    }

    #[test]
    fn shutdown_flush_waits_for_a_full_queue_to_drain() {
        use log::Log;

        let folder = Folder::new();
        let file = LogFile::open(&folder.0).unwrap();
        let (sender, messages) = sync_channel(1);
        let skipped = Arc::new(AtomicU64::new(0));
        let lost = skipped.clone();
        let logger = FileLog { sender, skipped };
        logger.log(
            &log::Record::builder()
                .target("EchoBridge")
                .level(log::Level::Info)
                .args(format_args!("last event before quit"))
                .build(),
        );
        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(25));
            write_messages(file, messages, lost);
        });
        logger.flush();
        assert!(fs::read_to_string(folder.0.join("echobridge.log")).unwrap().ends_with("last event before quit\n"));
        drop(logger);
        writer.join().unwrap();
    }

    #[test]
    fn background_writer_flushes_events_in_order_and_reports_skipped_messages() {
        use log::Log;

        let folder = Folder::new();
        let file = LogFile::open(&folder.0).unwrap();
        let (sender, messages) = sync_channel(QUEUE_LINES);
        let skipped = Arc::new(AtomicU64::new(7));
        let lost = skipped.clone();
        let writer = std::thread::spawn(move || write_messages(file, messages, lost));
        let logger = FileLog { sender, skipped };
        for message in ["paused", "protecting", "device failed"] {
            logger.log(
                &log::Record::builder()
                    .target("EchoBridge")
                    .level(log::Level::Info)
                    .args(format_args!("{message}"))
                    .build(),
            );
        }
        logger.flush();
        let text = fs::read_to_string(folder.0.join("echobridge.log")).unwrap();
        assert!(text.contains("skipped 7 messages"));
        assert!(text.find("paused").unwrap() < text.find("protecting").unwrap());
        assert!(text.ends_with("device failed\n"));
        drop(logger);
        writer.join().unwrap();
    }

    #[test]
    fn retains_three_generations_when_a_long_session_crosses_multiple_limits() {
        let folder = Folder::new();
        let mut file = LogFile::open(&folder.0).unwrap();
        for mark in ['a', 'b', 'c', 'd', 'e'] {
            file.write(&mark.to_string().repeat(MAX_BYTES as usize)).unwrap();
        }
        assert!(fs::read_to_string(&file.path).unwrap().starts_with('e'));
        for (index, mark) in [(1, 'd'), (2, 'c'), (3, 'b')] {
            let path = file.backup_path(index);
            assert!(fs::read_to_string(&path).unwrap().starts_with(mark));
            assert_eq!(fs::metadata(path).unwrap().len(), MAX_BYTES);
        }
        assert_eq!(fs::read_dir(&folder.0).unwrap().count(), 4);
    }

    #[test]
    fn rotates_during_a_session_and_replaces_the_backup() {
        let folder = Folder::new();
        let active = folder.0.join("echobridge.log");
        let backup = folder.0.join("echobridge.old.log");
        let mut file = LogFile::open(&folder.0).unwrap();
        let full = "a".repeat(MAX_BYTES as usize);
        file.write(&full).unwrap();
        file.write("pause\n").unwrap();
        assert_eq!(fs::read_to_string(&backup).unwrap(), full);
        assert_eq!(fs::read_to_string(&active).unwrap(), "pause\n");
        file.write(&"b".repeat(MAX_BYTES as usize - 6)).unwrap();
        file.write("resume\n").unwrap();
        assert!(fs::read_to_string(&backup).unwrap().starts_with("pause\n"));
        assert_eq!(fs::read_to_string(&active).unwrap(), "resume\n");
        assert!(fs::metadata(&backup).unwrap().len() <= MAX_BYTES);
    }

    #[test]
    fn rotates_an_oversized_existing_log_on_startup() {
        let folder = Folder::new();
        fs::write(folder.0.join("echobridge.log"), "x".repeat(MAX_BYTES as usize + 1)).unwrap();
        let mut file = LogFile::open(&folder.0).unwrap();
        file.write("started\n").unwrap();
        assert_eq!(fs::read_to_string(folder.0.join("echobridge.log")).unwrap(), "started\n");
        assert_eq!(fs::metadata(folder.0.join("echobridge.old.log")).unwrap().len(), MAX_BYTES + 1);
    }

    #[test]
    fn failed_rotation_preserves_the_active_log_and_new_event() {
        let folder = Folder::new();
        fs::create_dir(folder.0.join("echobridge.old.log")).unwrap();
        let mut file = LogFile::open(&folder.0).unwrap();
        file.write(&"x".repeat(MAX_BYTES as usize)).unwrap();
        file.write("device failed\n").unwrap();
        assert!(fs::read_to_string(folder.0.join("echobridge.log")).unwrap().ends_with("device failed\n"));
    }

    #[test]
    fn a_full_queue_never_blocks_the_caller_and_counts_skipped_messages() {
        use log::Log;

        let (sender, messages) = sync_channel(1);
        let skipped = Arc::new(AtomicU64::new(0));
        let logger = FileLog { sender, skipped: skipped.clone() };
        let record = log::Record::builder()
            .target("echobridge_engine")
            .level(log::Level::Warn)
            .args(format_args!("reference missing"))
            .build();
        logger.log(&record);
        logger.log(&record);
        assert_eq!(skipped.load(Ordering::Relaxed), 1);
        assert!(matches!(messages.try_recv().unwrap(), Message::Line(_)));
    }

    #[test]
    fn oversized_unicode_messages_fit_the_queue_bound() {
        use log::Log;

        let (sender, messages) = sync_channel(1);
        let logger = FileLog { sender, skipped: Arc::default() };
        let text = "é".repeat(MAX_LINE_BYTES);
        logger.log(
            &log::Record::builder().target("EchoBridge").level(log::Level::Info).args(format_args!("{text}")).build(),
        );
        let Message::Line(line) = messages.try_recv().unwrap() else { panic!("expected a line") };
        assert!(line.len() <= MAX_LINE_BYTES);
        assert!(line.ends_with("[truncated]\n"));
    }
}
