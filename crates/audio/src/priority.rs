//! Real-time scheduling for audio threads.

/// Audio scheduling for the current thread while this value lives.
///
/// On Windows this is the "Pro Audio" multimedia class (MMCSS), as Chrome uses for audio.
/// On a busy PC a normal-priority thread was preempted for 30-70 ms and the output ran dry.
/// On Linux it is round-robin real-time scheduling, which needs an `rtprio` limit; systems
/// without that allowance, RealtimeKit can grant it through the system bus. If both
/// requests fail, the thread keeps its normal priority.
#[derive(Debug)]
pub struct AudioThreadPriority {
    #[cfg(windows)]
    task: Option<windows::Win32::Foundation::HANDLE>,
    #[cfg(target_os = "linux")]
    granted: bool,
}

impl AudioThreadPriority {
    pub fn raise() -> Self {
        #[cfg(windows)]
        {
            use windows::Win32::System::Threading::AvSetMmThreadCharacteristicsW;
            let mut index = 0u32;
            // SAFETY: the task name is a static NUL-terminated string.
            let task = unsafe { AvSetMmThreadCharacteristicsW(windows::core::w!("Pro Audio"), &mut index) }.ok();
            Self { task }
        }
        #[cfg(target_os = "linux")]
        {
            let parameter = libc::sched_param { sched_priority: 20 };
            // SAFETY: `parameter` is a valid structure; pid 0 is the calling thread.
            let granted =
                unsafe { libc::sched_setscheduler(0, libc::SCHED_RR | libc::SCHED_RESET_ON_FORK, &parameter) } == 0;
            let granted = granted
                || match zbus::connection::Builder::system().and_then(request_realtimekit) {
                    Ok(()) => true,
                    Err(error) => {
                        log::debug!("audio scheduling remains at normal priority: {error}");
                        false
                    }
                };
            Self { granted }
        }
        #[cfg(not(any(windows, target_os = "linux")))]
        {
            Self {}
        }
    }

    /// Whether the system granted audio scheduling.
    pub fn granted(&self) -> bool {
        #[cfg(windows)]
        {
            self.task.is_some()
        }
        #[cfg(target_os = "linux")]
        {
            self.granted
        }
        #[cfg(not(any(windows, target_os = "linux")))]
        {
            false
        }
    }
}

#[cfg(target_os = "linux")]
fn request_realtimekit(builder: zbus::connection::Builder<'_>) -> zbus::Result<()> {
    let deadline = async {
        async_io::Timer::after(std::time::Duration::from_secs(1)).await;
        Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "RealtimeKit startup request timed out").into())
    };
    futures_lite::future::block_on(futures_lite::future::race(promote_with_realtimekit(builder), deadline))
}

#[cfg(target_os = "linux")]
async fn promote_with_realtimekit(builder: zbus::connection::Builder<'_>) -> zbus::Result<()> {
    let connection = builder.build().await?;
    let proxy = zbus::Proxy::new(
        &connection,
        "org.freedesktop.RealtimeKit1",
        "/org/freedesktop/RealtimeKit1",
        "org.freedesktop.RealtimeKit1",
    )
    .await?;
    let budget: i64 = proxy.get_property("RTTimeUSecMax").await?;
    if budget <= 0 {
        return Err(zbus::Error::Failure(format!("RealtimeKit returned an invalid time budget: {budget}")));
    }
    let priority: i32 = proxy.get_property("MaxRealtimePriority").await?;
    if priority <= 0 {
        return Err(zbus::Error::Failure(format!("RealtimeKit returned an invalid priority: {priority}")));
    }
    let mut limit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    // SAFETY: `limit` is writable storage for the requested process limit.
    if unsafe { libc::getrlimit(libc::RLIMIT_RTTIME, &mut limit) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // RealtimeKit checks the hard budget; retain any stricter existing limit.
    limit.rlim_max = limit.rlim_max.min(budget as libc::rlim_t);
    limit.rlim_cur = limit.rlim_cur.min(limit.rlim_max);
    // SAFETY: `limit` is valid and only lowers the current process limits.
    if unsafe { libc::setrlimit(libc::RLIMIT_RTTIME, &limit) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: gettid returns the calling Linux thread's positive identifier.
    let thread = unsafe { libc::gettid() } as u64;
    proxy.call("MakeThreadRealtime", &(thread, priority.min(20) as u32)).await
}

impl Drop for AudioThreadPriority {
    fn drop(&mut self) {
        #[cfg(windows)]
        if let Some(task) = self.task {
            // SAFETY: `task` came from AvSetMmThreadCharacteristicsW on this thread, which
            // this value never leaves (it is not Send).
            unsafe { windows::Win32::System::Threading::AvRevertMmThreadCharacteristics(task) }.ok();
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::AudioThreadPriority;

    #[test]
    #[ignore = "requires the system RealtimeKit service and an unprivileged desktop session"]
    fn realtimekit_grants_priority_when_direct_scheduling_is_denied() {
        let mut limit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
        // SAFETY: `limit` is writable storage for the requested process limit.
        assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_RTPRIO, &mut limit) }, 0);
        assert_eq!(limit.rlim_cur, 0, "this regression requires direct scheduling to be denied");
        std::thread::spawn(|| {
            let priority = AudioThreadPriority::raise();
            assert!(priority.granted(), "RealtimeKit must grant priority without an rtprio allowance");
            // SAFETY: pid 0 queries the calling thread's scheduling policy.
            assert_eq!(unsafe { libc::sched_getscheduler(0) } & !libc::SCHED_RESET_ON_FORK, libc::SCHED_RR);
        })
        .join()
        .unwrap();
    }

    #[test]
    fn a_stalled_bus_cannot_block_audio_startup() {
        let (client, server) = std::os::unix::net::UnixStream::pair().unwrap();
        let (sent, received) = std::sync::mpsc::sync_channel(1);
        let worker = std::thread::spawn(move || {
            let builder = zbus::connection::Builder::async_io_unix_stream(client);
            sent.send(super::request_realtimekit(builder)).ok();
        });
        let result = received.recv_timeout(std::time::Duration::from_secs(3));
        drop(server);
        worker.join().unwrap();
        let result = result.expect("connection authentication must finish within the startup deadline");
        assert!(matches!(result, Err(zbus::Error::InputOutput(error))
            if error.kind() == std::io::ErrorKind::TimedOut));
    }

    #[test]
    fn an_unavailable_bus_does_not_change_thread_scheduling() {
        let path = std::env::temp_dir().join(format!("echobridge-missing-bus-{}", std::process::id()));
        assert!(!path.exists(), "this regression requires an absent bus socket");
        let address = format!("unix:path={}", path.display());
        let builder = zbus::connection::Builder::address(address.as_str()).unwrap();
        // SAFETY: pid 0 queries the calling thread's scheduling policy.
        let policy = unsafe { libc::sched_getscheduler(0) };
        assert!(super::request_realtimekit(builder).is_err());
        // SAFETY: pid 0 queries the same calling thread after the failed request.
        assert_eq!(unsafe { libc::sched_getscheduler(0) }, policy);
    }
}
