//! Managed children have one reaper. poll(2) waits on pidfd + cancellation FD,
//! with an infinite deadline; neither waitpid(WNOHANG) nor timers drive exits.
use crate::{
    error::MixResult,
    native_events::{Queue, refusal},
};
use std::{process::Child, sync::Arc};

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use std::{
        os::{
            fd::{AsRawFd, FromRawFd, OwnedFd},
            unix::{net::UnixStream, process::ExitStatusExt},
        },
        thread,
    };

    pub(crate) struct ChildWatch {
        pub pid: u32,
        pidfd: Arc<OwnedFd>,
        cancel: UnixStream,
        completed: Arc<std::sync::atomic::AtomicBool>,
        worker: Option<thread::JoinHandle<()>>,
    }

    impl ChildWatch {
        pub fn new(mut child: Child, tag: String, queue: Arc<Queue>) -> MixResult<Self> {
            let pid = child.id() as i32;
            // The child is unreaped, so its PID cannot be reused before pidfd_open.
            let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) as i32 };
            if fd < 0 {
                let error = std::io::Error::last_os_error();
                let _ = child.kill();
                let _ = child.wait();
                return Err(refusal(
                    "PROC_UNSUPPORTED",
                    format!("managed spawn requires pidfd: {error}"),
                ));
            }
            let pidfd = Arc::new(unsafe { OwnedFd::from_raw_fd(fd) });
            let (cancel, wake) = match UnixStream::pair() {
                Ok(pair) => pair,
                Err(e) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(refusal("PROC_MONITOR", e.to_string()));
                }
            };
            crate::builtins::register_managed_pid(pid);
            // Keep ownership recoverable if thread creation itself fails.
            let slot = Arc::new(std::sync::Mutex::new(Some(child)));
            let worker_slot = slot.clone();
            let worker_fd = pidfd.clone();
            let completed = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let worker_completed = completed.clone();
            let worker = thread::Builder::new().name("mix-child-exit".into()).spawn(move || {
                let mut child = worker_slot.lock().unwrap().take().unwrap();
                let mut fds = [
                    libc::pollfd { fd: worker_fd.as_raw_fd(), events: libc::POLLIN, revents: 0 },
                    libc::pollfd { fd: wake.as_raw_fd(), events: libc::POLLIN, revents: 0 },
                ];
                let cancelled = loop {
                    let rc = unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) };
                    if rc < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted { continue }
                    // Cancellation wins simultaneous readiness. Still unreaped here:
                    // the group leader's PID is pinned, so -pid cannot name a reused group.
                    break rc < 0 || fds[1].revents != 0;
                };
                // End remaining descendants before reaping the leader, even on
                // a natural exit. The unreaped PID pins the process-group identity.
                unsafe { libc::kill(-pid, libc::SIGKILL); }
                // The leader may have left its original process group. Signal
                // its retained identity too before the blocking reap; a group
                // kill alone cannot bound cancellation by the child's death.
                if cancelled {
                    unsafe {
                        libc::syscall(libc::SYS_pidfd_send_signal, worker_fd.as_raw_fd(),
                            libc::SIGKILL, std::ptr::null::<libc::siginfo_t>(), 0);
                    }
                }
                let result = child.wait();
                crate::builtins::unregister_managed_pid(pid);
                // Publish completion before waking the consumer. Pruning joins
                // this worker, so its terminal record is queued before the
                // registry decides whether any sources remain.
                worker_completed.store(true, std::sync::atomic::Ordering::Release);
                if !cancelled {
                    match result {
                        Ok(status) => queue.child(serde_json::json!({
                            "pid": pid, "tag": tag, "exit_code": status.code(), "signal": status.signal()
                        })),
                        Err(e) => queue.child(serde_json::json!({
                            "pid": pid, "tag": tag, "exit_code": null, "signal": null,
                            "error_code": "PROC_REAP", "message": e.to_string()
                        })),
                    }
                }
            });
            match worker {
                Ok(worker) => Ok(Self {
                    pid: pid as u32,
                    pidfd,
                    cancel,
                    completed,
                    worker: Some(worker),
                }),
                Err(e) => {
                    if let Some(mut child) = slot.lock().unwrap().take() {
                        let _ = child.kill();
                        let _ = child.wait();
                    }
                    crate::builtins::unregister_managed_pid(pid);
                    Err(refusal("PROC_MONITOR", e.to_string()))
                }
            }
        }

        pub fn finished(&self) -> bool {
            self.completed.load(std::sync::atomic::Ordering::Acquire)
        }

        pub fn signal(&self, signal: i32) -> bool {
            unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    self.pidfd.as_raw_fd(),
                    signal,
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                ) == 0
            }
        }
    }

    impl Drop for ChildWatch {
        fn drop(&mut self) {
            let _ = self.cancel.shutdown(std::net::Shutdown::Write);
            if let Some(w) = self.worker.take() {
                let _ = w.join();
            }
        }
    }
}

#[cfg(target_os = "linux")]
pub(crate) use linux::ChildWatch;
#[cfg(not(target_os = "linux"))]
pub(crate) struct ChildWatch {
    pub pid: u32,
}
#[cfg(not(target_os = "linux"))]
impl ChildWatch {
    pub fn new(mut child: Child, _: String, _: Arc<Queue>) -> MixResult<Self> {
        let _ = child.kill();
        let _ = child.wait();
        Err(refusal(
            "PROC_UNSUPPORTED",
            "managed spawn requires Linux pidfd",
        ))
    }
    pub fn finished(&self) -> bool {
        true
    }
    pub fn signal(&self, _: i32) -> bool {
        false
    }
}
