//! A stream that runs on its own audio thread, shared by the system backends.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::sync_channel;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use crate::{Error, Stream};

/// A stream thread, stopped and joined on drop.
#[derive(Debug)]
pub(crate) struct ThreadStream {
    stop: Arc<AtomicBool>,
    failure: Arc<Mutex<Option<String>>>,
    thread: Option<JoinHandle<()>>,
}

impl ThreadStream {
    /// Run `body` on a new audio thread. `body` opens the device, reports what it opened
    /// through its second argument, then runs until the first argument is set.
    pub fn spawn<T, F>(name: &str, body: F) -> Result<(Self, T), Error>
    where
        T: Send + 'static,
        F: FnOnce(&Arc<AtomicBool>, &mut dyn FnMut(T)) -> Result<(), Error> + Send + 'static,
    {
        let stop = Arc::new(AtomicBool::new(false));
        let failure = Arc::new(Mutex::new(None));
        let (sender, receiver) = sync_channel::<Result<T, Error>>(1);
        let thread = {
            let (stop, failure) = (stop.clone(), failure.clone());
            std::thread::Builder::new()
                .name(name.into())
                .spawn(move || {
                    let _priority = crate::AudioThreadPriority::raise();
                    let mut sender = Some(sender);
                    let mut report = |value: T| {
                        if let Some(sender) = sender.take() {
                            sender.send(Ok(value)).ok();
                        }
                    };
                    if let Err(error) = body(&stop, &mut report) {
                        log::warn!("audio stream {:?} stopped: {error}", std::thread::current().name());
                        *failure.lock().unwrap() = Some(error.to_string());
                        // A caller still waiting for the device to open gets the error.
                        if let Some(sender) = sender.take() {
                            sender.send(Err(error)).ok();
                        }
                    }
                })
                .map_err(|e| Error::System(e.to_string()))?
        };
        let mut stream = Self { stop, failure, thread: Some(thread) };
        match receiver.recv() {
            Ok(Ok(value)) => Ok((stream, value)),
            Ok(Err(error)) => {
                stream.join();
                Err(error)
            }
            Err(_) => {
                stream.join();
                Err(Error::System("The audio device could not be opened.".into()))
            }
        }
    }

    fn join(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            thread.join().ok();
        }
    }
}

impl Stream for ThreadStream {
    fn failure(&self) -> Option<String> {
        self.failure.lock().unwrap().clone()
    }
}

impl Drop for ThreadStream {
    fn drop(&mut self) {
        self.join();
    }
}
