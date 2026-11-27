use std::collections::HashMap;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread::{self, JoinHandle};

use crate::app::{App, Status, login_status};
use crate::config::{Entry, LoginCheck};

#[derive(Default)]
pub(super) struct Readiness<'a> {
    results: HashMap<&'a str, EntryStatus>,
    worker: Option<(Sender<Request>, JoinHandle<()>)>,
}

enum EntryStatus {
    Checking(Pending),
    Ready(Status),
}

struct Pending {
    cancelled: Arc<AtomicBool>,
    result: Receiver<Status>,
}

impl Drop for Pending {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
}

struct Request {
    checks: Vec<LoginCheck>,
    cancelled: Arc<AtomicBool>,
    result: Sender<Status>,
}

impl<'a> Readiness<'a> {
    pub fn start(app: &'a App) -> io::Result<Self> {
        let mut readiness = Self::default();
        for entry in app.entries() {
            readiness.refresh(entry)?;
        }
        Ok(readiness)
    }

    pub fn refresh(&mut self, entry: &'a Entry) -> io::Result<()> {
        let Some(checks) = entry.command_readiness() else {
            return Ok(());
        };
        if self.worker.is_none() {
            let (sender, requests) = mpsc::channel::<Request>();
            let worker = thread::Builder::new()
                .name("readiness".into())
                .spawn(move || {
                    for request in requests {
                        let cancelled = || request.cancelled.load(Ordering::Relaxed);
                        if !cancelled() {
                            let _ = request
                                .result
                                .send(login_status(&request.checks, cancelled));
                        }
                    }
                })?;
            self.worker = Some((sender, worker));
        }

        let cancelled = Arc::new(AtomicBool::new(false));
        let (sender, result) = mpsc::channel();
        self.results.insert(
            &entry.id,
            EntryStatus::Checking(Pending {
                cancelled: Arc::clone(&cancelled),
                result,
            }),
        );
        self.worker
            .as_ref()
            .unwrap()
            .0
            .send(Request {
                checks: checks.to_vec(),
                cancelled,
                result: sender,
            })
            .map_err(|_| io::Error::other("readiness worker stopped"))
    }

    pub fn collect(&mut self) -> bool {
        let mut waiting = false;
        for status in self.results.values_mut() {
            let EntryStatus::Checking(pending) = status else {
                continue;
            };
            *status = EntryStatus::Ready(match pending.result.try_recv() {
                Ok(result) => result,
                Err(TryRecvError::Empty) => {
                    waiting = true;
                    continue;
                }
                Err(TryRecvError::Disconnected) => {
                    Status::CheckError("readiness worker stopped".into())
                }
            });
        }
        waiting
    }

    pub fn status(&self, entry: &Entry) -> Option<&Status> {
        self.results
            .get(entry.id.as_str())
            .map(|status| match status {
                EntryStatus::Checking(_) => &Status::Checking,
                EntryStatus::Ready(status) => status,
            })
    }
}

impl Drop for Readiness<'_> {
    fn drop(&mut self) {
        // Cancel current and queued work before waiting for the worker to exit.
        self.results.clear();
        if let Some((sender, worker)) = self.worker.take() {
            drop(sender);
            let _ = worker.join();
        }
    }
}
