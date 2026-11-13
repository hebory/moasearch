//! A bounded pool keeps disk/model commands off the native window thread.
use anyhow::{Result, anyhow};
use std::{
    sync::{
        Arc, Mutex,
        mpsc::{self, SyncSender},
    },
    thread::{self, JoinHandle},
};

type Job = Box<dyn FnOnce() + Send + 'static>;
pub struct Workers {
    sender: Option<SyncSender<Job>>,
    threads: Vec<JoinHandle<()>>,
}
impl Workers {
    pub fn new(count: usize, capacity: usize) -> Self {
        let (sender, receiver) = mpsc::sync_channel::<Job>(capacity);
        let receiver = Arc::new(Mutex::new(receiver));
        let threads = (0..count)
            .map(|_| {
                let receiver = receiver.clone();
                thread::spawn(move || {
                    loop {
                        let job = receiver.lock().unwrap().recv();
                        match job {
                            Ok(job) => job(),
                            Err(_) => break,
                        }
                    }
                })
            })
            .collect();
        Self {
            sender: Some(sender),
            threads,
        }
    }
    pub fn submit(&self, job: impl FnOnce() + Send + 'static) -> Result<()> {
        self.sender
            .as_ref()
            .unwrap()
            .try_send(Box::new(job))
            .map_err(|_| anyhow!("처리 중인 작업이 많습니다. 잠시 후 다시 시도하세요."))
    }
}
impl Drop for Workers {
    fn drop(&mut self) {
        self.sender.take();
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn model_work_does_not_block_another_command_and_shutdown_drains_jobs() -> Result<()> {
        let pool = Workers::new(2, 4);
        let (release, wait) = mpsc::channel();
        let (started, ready) = mpsc::channel();
        let (finished, done) = mpsc::channel();
        let caller = thread::current().id();
        pool.submit(move || {
            started.send(thread::current().id()).unwrap();
            let _ = wait.recv();
            finished.send(()).unwrap();
        })?;
        assert_ne!(
            ready.recv_timeout(std::time::Duration::from_secs(2))?,
            caller
        );
        let (ping, pong) = mpsc::channel();
        pool.submit(move || {
            ping.send(true).unwrap();
        })?;
        let responsive = pong.recv_timeout(std::time::Duration::from_secs(2));
        release.send(())?;
        assert!(responsive?);
        drop(pool);
        done.try_recv()?;
        Ok(())
    }
}
