use crate::types::node_data::EventSource;
use crossbeam_channel::{Sender, unbounded};
use notify::{RecursiveMode, Watcher, recommended_watcher};
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering as AtomicOrdering},
    },
    thread,
    time::{Duration, Instant},
};

#[derive(Debug)]
pub(crate) enum FileEvent {
    OrderStatus(String),
    OrderDiff(String),
    Fill(String),
}

struct FileReader {
    current_path: Option<PathBuf>,
    file_position: u64,
    partial_line: String,
    base_dir: PathBuf,
    read_buf: String,
}

impl FileReader {
    fn new(base_dir: PathBuf) -> Self {
        Self {
            current_path: None,
            file_position: 0,
            partial_line: String::with_capacity(4096),
            base_dir,
            read_buf: String::with_capacity(65536),
        }
    }

    fn find_latest_file(&self) -> Option<PathBuf> {
        let hourly_dir = self.base_dir.join("hourly");
        if !hourly_dir.exists() {
            return None;
        }

        let latest_day = std::fs::read_dir(&hourly_dir)
            .ok()?
            .flatten()
            .filter(|e| e.path().is_dir())
            .max_by_key(|e| e.file_name())?
            .path();

        std::fs::read_dir(&latest_day)
            .ok()?
            .flatten()
            .filter(|e| e.path().is_file())
            .max_by_key(|e| e.metadata().and_then(|m| m.modified()).ok())
            .map(|e| e.path())
    }

    fn check_for_newer_file(&mut self) -> Option<PathBuf> {
        let latest = self.find_latest_file()?;
        if let Some(ref current) = self.current_path {
            if latest != *current {
                let latest_mtime = latest.metadata().and_then(|m| m.modified()).ok()?;
                let current_mtime = current.metadata().and_then(|m| m.modified()).ok()?;
                if latest_mtime > current_mtime {
                    return Some(latest);
                }
            }
            None
        } else {
            Some(latest)
        }
    }

    fn on_modify(&mut self) -> Vec<String> {
        let mut lines = Vec::with_capacity(16);
        let path = match &self.current_path {
            Some(p) => p,
            None => return lines,
        };

        let mut file = match File::open(path) {
            Ok(f) => f,
            Err(_) => return lines,
        };

        let file_size = file.metadata().map(|m| m.len()).unwrap_or(0);
        if file_size <= self.file_position {
            return lines;
        }

        if file.seek(SeekFrom::Start(self.file_position)).is_ok() {
            self.read_buf.clear();
            if let Ok(bytes_read) = file.read_to_string(&mut self.read_buf) {
                if bytes_read > 0 {
                    self.file_position += bytes_read as u64;

                    let combined = if self.partial_line.is_empty() {
                        &self.read_buf
                    } else {
                        self.partial_line.push_str(&self.read_buf);
                        &self.partial_line
                    };

                    let mut last_end = 0;
                    for (start, part) in combined.match_indices('\n') {
                        let line = &combined[last_end..start];
                        let trimmed = line.trim();
                        if !trimmed.is_empty() && trimmed.starts_with('{') && trimmed.ends_with('}') {
                            lines.push(trimmed.to_string());
                        }
                        last_end = start + part.len();
                    }

                    let remainder = &combined[last_end..];
                    if last_end < combined.len() {
                        let new_partial = remainder.to_string();
                        self.partial_line = new_partial;
                    } else {
                        self.partial_line.clear();
                    }
                }
            }
        }
        lines
    }

    fn on_create(&mut self, path: &PathBuf) -> Vec<String> {
        let old_lines = self.on_modify();
        self.current_path = Some(path.clone());
        self.file_position = 0;
        self.partial_line.clear();
        old_lines
    }
}

pub(super) fn spawn_file_watcher(
    source: EventSource,
    dir: PathBuf,
    tx: Sender<FileEvent>,
    last_event: Arc<AtomicU64>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut reader = FileReader::new(dir.clone());
        let (event_tx, event_rx) = std::sync::mpsc::channel();

        let mut watcher = recommended_watcher(move |res| {
            let _ = event_tx.send(res);
        })
        .expect("Failed to create watcher");

        watcher.watch(&dir, RecursiveMode::Recursive).expect("Failed to watch dir");

        let poll_interval = Duration::from_millis(1);
        let mut poll_count = 0u64;

        loop {
            poll_count += 1;

            match event_rx.recv_timeout(poll_interval) {
                Ok(Ok(event)) => {
                    if event.kind.is_create() {
                        if let Some(path) = event.paths.first() {
                            if path.is_file() {
                                let lines = reader.on_create(path);
                                send_lines(&tx, lines, &source, &last_event);
                            }
                        }
                    }
                }
                _ => {}
            }

            let lines = reader.on_modify();
            send_lines(&tx, lines, &source, &last_event);

            if poll_count % 5000 == 0 {
                if let Some(newer_file) = reader.check_for_newer_file() {
                    let lines = reader.on_create(&newer_file);
                    send_lines(&tx, lines, &source, &last_event);
                }
            }
        }
    })
}

#[inline(always)]
fn send_lines(tx: &Sender<FileEvent>, lines: Vec<String>, source: &EventSource, last_event: &Arc<AtomicU64>) {
    if lines.is_empty() {
        return;
    }

    let now = Instant::now().elapsed().as_millis() as u64;
    for line in lines {
        let event = match source {
            EventSource::OrderStatuses => FileEvent::OrderStatus(line),
            EventSource::OrderDiffs => FileEvent::OrderDiff(line),
            EventSource::Fills => FileEvent::Fill(line),
        };
        if tx.send(event).is_err() {
            return;
        }
    }
    last_event.store(now, AtomicOrdering::Relaxed);
}

pub(crate) fn start_parallel_file_watchers(
    data_dir: PathBuf,
) -> (crossbeam_channel::Receiver<FileEvent>, Vec<thread::JoinHandle<()>>, Arc<AtomicU64>, Arc<AtomicU64>, Arc<AtomicU64>)
{
    let (tx, rx) = unbounded();
    let mut handles = Vec::new();
    let last_order_status = Arc::new(AtomicU64::new(0));
    let last_fills = Arc::new(AtomicU64::new(0));
    let last_order_diffs = Arc::new(AtomicU64::new(0));

    let sources = [
        (EventSource::OrderStatuses, last_order_status.clone()),
        (EventSource::Fills, last_fills.clone()),
        (EventSource::OrderDiffs, last_order_diffs.clone()),
    ];

    for (src, health) in sources {
        let path = src.event_source_dir_streaming(&data_dir);
        handles.push(spawn_file_watcher(src, path, tx.clone(), health));
    }

    (rx, handles, last_order_status, last_fills, last_order_diffs)
}
