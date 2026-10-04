//! Real-time scheduling for audio threads.

/// Audio scheduling for the current thread while this value lives.
///
/// On Windows this is the "Pro Audio" multimedia class (MMCSS), as Chrome uses for audio.
/// On a busy PC a normal-priority thread was preempted for 30-70 ms and the output ran dry.
/// On Linux it is round-robin real-time scheduling, which needs an `rtprio` limit; systems
/// with PipeWire usually grant one to desktop users, and without it the thread keeps its
/// normal priority.
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
