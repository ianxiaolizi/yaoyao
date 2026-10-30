//! A job object that takes its processes down when this one closes.
//!
//! A child started with `kill_on_drop` dies when the *provider* is dropped, which
//! covers every ordinary ending. What it does not cover is this process ending
//! without running destructors: `taskkill /F`, a crash, a machine that pulls the
//! plug on the parent's thread. The child keeps its pipes, its handle to the
//! event log and its turn, and goes on writing into a session whose parent no
//! longer exists.
//!
//! Windows answers that with a job object: every process assigned to one is
//! terminated by the kernel when the job's last handle closes, and this process
//! is the one holding it (CONTRACT §7 #136). The job owns the *lifetime* of a
//! delegated child, and nothing about its authority — what the child may do is
//! fixed on its command line and by its own guard.
//!
//! Assignment is by process id rather than by handle because the spawn belongs
//! to `tokio::process`, which hands back an id and not the handle it created.
//! Opening the process a second time costs a handle and widens the window in
//! which the child is unguarded — the call that opened the job and the call that
//! assigned it are both in the parent's spawn path, so that window is one
//! `OpenProcess`, not a lifetime.

use std::ffi::c_void;
use std::io;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject,
};
use windows_sys::Win32::System::Threading::{
    OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
};

/// A job whose processes die with this one.
pub struct KillOnCloseJob(HANDLE);

// SAFETY: the value owns one kernel handle and nothing thread-affine. Methods
// take `&self` and only pass the handle back to APIs that document it as valid
// for the lifetime of an open handle.
unsafe impl Send for KillOnCloseJob {}
// SAFETY: as above; the handle is not mutated, and Windows job APIs accept the
// same handle from any thread.
unsafe impl Sync for KillOnCloseJob {}

impl KillOnCloseJob {
    /// Create the job, with the one limit this exists for.
    pub fn new() -> io::Result<Self> {
        // SAFETY: no job attributes and no name, which is what the API
        // documents as "a fresh anonymous job".
        let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if job.is_null() || job == INVALID_HANDLE_VALUE {
            return Err(last_error("could not create the job object"));
        }
        let job = Self(job);

        let mut information: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = {
            // SAFETY: a plain C struct, zeroed before the one field used is set.
            unsafe { std::mem::zeroed() }
        };
        information.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: the handle is open and owned by `job`; `information` has the
        // layout the class expects and its size is passed with it.
        let ok = unsafe {
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                &information as *const _ as *const c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if ok == 0 {
            return Err(last_error("could not set KILL_ON_JOB_CLOSE"));
        }
        Ok(job)
    }

    /// Put the process with this id in the job, so it dies with this one.
    ///
    /// A process already in a job is assigned only where Windows allows
    /// nesting (Windows 8 and later, which this build requires for the
    /// AppContainer backend anyway); anywhere else the refusal is returned
    /// rather than swallowed, because the caller promised the child a guard.
    pub fn attach(&self, pid: u32) -> io::Result<()> {
        // SAFETY: the flags are the minimum `AssignProcessToJobObject`
        // documents as required for assignment, and `pid` is the caller's.
        let process = unsafe {
            OpenProcess(
                PROCESS_SET_QUOTA | PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION,
                0,
                pid,
            )
        };
        if process.is_null() {
            return Err(last_error("the child could not be opened to assign it to the job"));
        }
        let process = OwnedHandle(process);
        // SAFETY: both handles are open and owned by values still alive here.
        if unsafe { AssignProcessToJobObject(self.0, process.0) } == 0 {
            return Err(last_error("the child could not be assigned to the job"));
        }
        Ok(())
    }
}

impl Drop for KillOnCloseJob {
    fn drop(&mut self) {
        // Closing the last handle is what terminates what is left in the job.
        // SAFETY: the handle is owned by this value and closed exactly once.
        unsafe { CloseHandle(self.0) };
    }
}

/// A process handle closed once, on drop.
struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: the handle is owned by this value and closed exactly once.
        unsafe { CloseHandle(self.0) };
    }
}

fn last_error(context: &str) -> io::Error {
    // SAFETY: reading the calling thread's last-error value has no preconditions.
    let code = unsafe { windows_sys::Win32::Foundation::GetLastError() };
    let os = io::Error::from_raw_os_error(code as i32);
    io::Error::new(os.kind(), format!("{context}: {os}"))
}
