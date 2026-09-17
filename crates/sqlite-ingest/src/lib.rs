pub mod reminder_scheduler;
pub mod worker;

use model_router::ModelRouter;
use reminder_scheduler::{DesktopNotifier, IDLE_SLEEP_SECONDS, ReminderScheduler};
use sqlite_memory::SqliteMemoryDb;
use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};
use worker::MemoryExtractionWorker;

pub struct BackgroundWorkers {
    stop: Arc<AtomicBool>,
    handles: Vec<JoinHandle<()>>,
}

impl BackgroundWorkers {
    pub fn start(
        memory_root: impl AsRef<Path>,
        db_path: impl AsRef<Path>,
        qwen_url: impl Into<String>,
        qwen_model: impl Into<String>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let memory_root = memory_root.as_ref().to_path_buf();
        let db_path = db_path.as_ref().to_path_buf();
        std::fs::create_dir_all(&memory_root)?;

        let db = SqliteMemoryDb::open(&db_path)?;
        db.initialize_schema()?;

        let qwen_url = qwen_url.into();
        let qwen_model = qwen_model.into();
        let stop = Arc::new(AtomicBool::new(false));
        let mut handles = Vec::with_capacity(2);

        let extraction_handle = thread::Builder::new()
            .name("assistant-memory-extraction".to_string())
            .spawn({
                let stop = Arc::clone(&stop);
                let db_path = db_path.clone();
                let qwen_url = qwen_url.clone();
                let qwen_model = qwen_model.clone();
                move || {
                    let db = match SqliteMemoryDb::open(&db_path) {
                        Ok(db) => db,
                        Err(error) => {
                            eprintln!("Memory extraction worker startup failed: {error}");
                            return;
                        }
                    };
                    if let Err(error) = db.initialize_schema() {
                        eprintln!("Memory extraction worker schema initialization failed: {error}");
                        return;
                    }
                    let worker =
                        MemoryExtractionWorker::new(db, ModelRouter::new(qwen_url, qwen_model));
                    while !stop.load(Ordering::Relaxed) {
                        if let Err(error) = worker.run_once() {
                            eprintln!("Memory extraction worker: {error}");
                        }
                        wait_or_stop(&stop, Duration::from_secs(2));
                    }
                }
            })?;

        let reminder_handle = match thread::Builder::new()
            .name("assistant-reminder-scheduler".to_string())
            .spawn({
                let stop = Arc::clone(&stop);
                let memory_root = memory_root.clone();
                let db_path = db_path.clone();
                move || {
                    let mut scheduler =
                        match ReminderScheduler::new(memory_root, db_path, DesktopNotifier) {
                            Ok(scheduler) => scheduler,
                            Err(error) => {
                                eprintln!("Reminder scheduler startup failed: {error}");
                                return;
                            }
                        };
                    if let Err(error) = scheduler.ensure_scheduled() {
                        eprintln!("Reminder scheduler setup failed: {error}");
                        return;
                    }
                    while !stop.load(Ordering::Relaxed) {
                        match scheduler.run_once() {
                            Ok(true) => {}
                            Ok(false) => {
                                wait_or_stop(&stop, Duration::from_secs(IDLE_SLEEP_SECONDS))
                            }
                            Err(error) => {
                                eprintln!("Reminder scheduler: {error}");
                                wait_or_stop(&stop, Duration::from_secs(IDLE_SLEEP_SECONDS));
                            }
                        }
                    }
                }
            }) {
            Ok(handle) => handle,
            Err(error) => {
                stop.store(true, Ordering::Relaxed);
                let _ = extraction_handle.join();
                return Err(error.into());
            }
        };

        handles.push(extraction_handle);
        handles.push(reminder_handle);

        Ok(Self { stop, handles })
    }

    pub fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.handles.clear();
    }
}

impl Drop for BackgroundWorkers {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn wait_or_stop(stop: &AtomicBool, duration: Duration) {
    let slices = duration.as_millis().div_ceil(100).max(1);
    for _ in 0..slices {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        thread::sleep(Duration::from_millis(100));
    }
}
