//! Own a test runtime and stop its workers on assertion failure.
use std::{
    io,
    thread::{self, JoinHandle},
};
use unquic::{
    config::Config,
    runtime::{self, Control},
};

pub struct Running {
    pub control: Control,
    thread: Option<JoinHandle<io::Result<()>>>,
}

impl Running {
    pub fn start(config: Config) -> Self {
        let control = Control::new();
        let worker_control = control.clone();
        let thread = Some(thread::spawn(move || {
            let result = runtime::run(config, worker_control);
            if let Err(error) = &result {
                eprintln!("runtime failed: {error:?}");
            }
            result
        }));
        Self { control, thread }
    }

    pub fn stop(&mut self) {
        self.control.shutdown();
        self.thread.take().unwrap().join().unwrap().unwrap();
        assert!(self.control.stopped());
        assert!(!self.control.ready());
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.control.shutdown();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
