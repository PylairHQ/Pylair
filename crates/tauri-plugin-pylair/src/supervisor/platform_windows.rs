//! Windows process containment: the Job Object, the suspended-spawn resume,
//! console creation flags and the Windows process monitor
//! (docs/protocol-v1.md section 6; docs/design/windows-job-object-race.md).

use std::io;

use tokio::process::Child;
use tokio::sync::{mpsc, oneshot};

#[cfg(windows)]
pub(super) async fn monitor_child(
    mut child: Child,
    mut kill_rx: mpsc::Receiver<()>,
    exit_tx: oneshot::Sender<io::Result<std::process::ExitStatus>>,
    // Holding this keeps the Job Object alive; it closes when the monitor
    // task ends (after the child exits), which also kills any survivors.
    job: Option<JobHandle>,
    tree_pid: Option<u32>,
) {
    tokio::select! {
        status = child.wait() => { let _ = exit_tx.send(status); }
        _ = kill_rx.recv() => {
            if job.is_none() {
                // No Job Object: taskkill /T is the only way to reach the tree.
                if let Some(pid) = tree_pid {
                    // taskkill is console-subsystem too — same suppression.
                    let _ = tokio::process::Command::new("taskkill")
                        .args(["/PID", &pid.to_string(), "/T", "/F"])
                        .creation_flags(child_creation_flags(
                            std::env::var_os("PYLAIR_CONSOLE").as_deref(),
                        ))
                        .output()
                        .await;
                }
            }
            let _ = child.start_kill(); // Job Object also reaps on handle close
            let status = child.wait().await;
            let _ = exit_tx.send(status);
        }
    }
}

/// Windows creation flags for console-subsystem children (the bundled
/// python.exe, taskkill). Without CREATE_NO_WINDOW a GUI-subsystem app
/// gets an empty console window per child. PYLAIR_CONSOLE (any value)
/// keeps the window for debugging; the bridge uses pipes, never the
/// console, so suppression is safe.
#[cfg(windows)]
pub(super) fn child_creation_flags(console_env: Option<&std::ffi::OsStr>) -> u32 {
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    if console_env.is_some() {
        0
    } else {
        CREATE_NO_WINDOW
    }
}

/// windows-sys HANDLE is *mut c_void, which is !Send — wrap it so it can be
/// held by the monitor task (keeps the Job Object alive for the child's life).
#[cfg(windows)]
pub(super) struct JobHandle(windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
unsafe impl Send for JobHandle {}

#[cfg(windows)]
impl Drop for JobHandle {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

/// Resume the single thread of a process created with `CREATE_SUSPENDED`.
/// std and tokio close the primary thread handle at spawn, so find the
/// thread by walking a Toolhelp snapshot for the one owned by `pid`. We hold
/// the child's process handle, so `pid` cannot have been recycled.
#[cfg(windows)]
pub(super) fn resume_main_thread(pid: u32) -> io::Result<()> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
    };
    use windows_sys::Win32::System::Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME};

    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        if snap == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let mut entry: THREADENTRY32 = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
        let mut tid = None;
        let mut more = Thread32First(snap, &mut entry);
        while more != 0 {
            if entry.th32OwnerProcessID == pid {
                tid = Some(entry.th32ThreadID);
                break;
            }
            more = Thread32Next(snap, &mut entry);
        }
        CloseHandle(snap);
        let tid = tid.ok_or_else(|| io::Error::other("no thread found for the new process"))?;
        let thread = OpenThread(THREAD_SUSPEND_RESUME, 0, tid);
        if thread.is_null() {
            return Err(io::Error::last_os_error());
        }
        let previous = ResumeThread(thread);
        let err = io::Error::last_os_error();
        CloseHandle(thread);
        if previous == u32::MAX {
            return Err(err);
        }
    }
    Ok(())
}

#[cfg(windows)]
pub(super) fn assign_to_job_object(pid: Option<u32>) -> Option<JobHandle> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::JobObjects::*;
    use windows_sys::Win32::System::Threading::*;

    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return None;
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const _,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        ) == 0
        {
            CloseHandle(job);
            return None;
        }
        let Some(pid) = pid else {
            CloseHandle(job);
            return None;
        };
        let proc = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid);
        if proc.is_null() || AssignProcessToJobObject(job, proc) == 0 {
            if !proc.is_null() {
                CloseHandle(proc);
            }
            CloseHandle(job);
            return None;
        }
        CloseHandle(proc);
        Some(JobHandle(job))
    }
}
