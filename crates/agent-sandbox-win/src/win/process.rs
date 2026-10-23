//! Starting one process inside the AppContainer, and watching or killing it.
//!
//! This is the part the B0 spike measured piece by piece. Each choice below is
//! the one that made the launcher work, and the comment says which failure the
//! choice avoids:
//!
//! * pipes are created inheritable (`bInheritHandle`) — `HANDLE_LIST` refuses
//!   handles that a child could not inherit, and the error is
//!   `ERROR_INVALID_PARAMETER`, which reads like an argument mistake;
//! * the read ends are made non-inheritable after creation, so a grandchild
//!   does not keep the pipe open past the command;
//! * `PROC_THREAD_ATTRIBUTE_JOB_LIST` puts the process in a job with
//!   `KILL_ON_JOB_CLOSE` at creation time, so the tree dies with this agent even
//!   on a path where no destructor runs;
//! * stdin is a pipe whose write end is closed immediately: opening `NUL` is
//!   refused inside the container, and a child that reads stdin must see EOF
//!   rather than block on an invalid handle;
//! * the handle and job arrays live in `build_and_create`'s frame until after
//!   `CreateProcessW` returns — the attribute list keeps the pointers it was
//!   given rather than copying the values, so an array that dies with its
//!   block is a stack slot an optimized build reuses before the call reads it.

use std::ffi::c_void;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::windows::io::{FromRawHandle, RawHandle};
use std::path::Path;
use std::ptr::{null, null_mut};

use agent_ports::process_launcher::{LaunchError, RunningProcess};
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE,
    SetHandleInformation, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::{
    PSID, SECURITY_ATTRIBUTES, SECURITY_CAPABILITIES, SID_AND_ATTRIBUTES,
};
use windows_sys::Win32::System::JobObjects::{
    CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectExtendedLimitInformation, SetInformationJobObject, TerminateJobObject,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DeleteProcThreadAttributeList,
    EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess, INFINITE, InitializeProcThreadAttributeList,
    PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROC_THREAD_ATTRIBUTE_JOB_LIST,
    PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES, PROCESS_INFORMATION, STARTF_USESTDHANDLES,
    STARTUPINFOEXW, TerminateProcess, UpdateProcThreadAttribute, WaitForSingleObject,
};

use super::wide;

/// Start one command under the package SID and return the handle to wait on.
///
/// `keep_stdin` decides what happens to the write end of the stdin pipe: a
/// shell command gets EOF immediately, while a helper the caller talks to over
/// the pipe keeps the end open and takes it from
/// [`AppContainerProcess::take_stdin`].
pub fn spawn(
    sid: PSID,
    capabilities: Option<&[SID_AND_ATTRIBUTES]>,
    environment: &[u16],
    command_line: &str,
    cwd: &Path,
    keep_stdin: bool,
) -> Result<AppContainerProcess, LaunchError> {
    // All six pipe ends are owned until this function returns; every early
    // return below drops them and closes the handles.
    let (stdin_read, stdin_write) = create_pipe()?;
    let (stdout_read, stdout_write) = create_pipe()?;
    let (stderr_read, stderr_write) = create_pipe()?;
    // The parent must not pass its read ends on; only the child's three ends are
    // in the handle list.
    for read_end in [stdout_read.get(), stderr_read.get()] {
        // SAFETY: `read_end` is an open pipe handle owned by this function.
        if unsafe { SetHandleInformation(read_end, HANDLE_FLAG_INHERIT, 0) } == 0 {
            return Err(last_error("could not make a pipe read end non-inheritable"));
        }
    }

    let job = create_kill_on_close_job()?;

    // The attribute list is a blob the API lays out itself; a `usize` buffer is
    // pointer-aligned, which the API requires and a `Vec<u8>` does not promise.
    let mut size = 0usize;
    // SAFETY: with a null list the call reports the required size in `size` and
    // is expected to fail with ERROR_INSUFFICIENT_BUFFER.
    unsafe { InitializeProcThreadAttributeList(null_mut(), 3, 0, &mut size) };
    if size == 0 {
        return Err(last_error("could not size the process attribute list"));
    }
    let words = size.div_ceil(std::mem::size_of::<usize>());
    let mut buffer = vec![0usize; words];
    let attributes = buffer.as_mut_ptr() as *mut c_void;
    // SAFETY: the buffer is at least `size` bytes and aligned; the count matches
    // the three attributes set below.
    if unsafe { InitializeProcThreadAttributeList(attributes, 3, 0, &mut size) } == 0 {
        return Err(last_error("could not initialize the process attribute list"));
    }
    // From here on the list must be deleted on every path.
    let result = build_and_create(
        sid,
        capabilities,
        environment,
        command_line,
        cwd,
        attributes,
        &job,
        stdin_read.get(),
        stdout_write.get(),
        stderr_write.get(),
    );
    // SAFETY: `attributes` was initialized just above and is deleted once.
    unsafe { DeleteProcThreadAttributeList(attributes) };
    // Closing the write end of stdin gives the child EOF, and closing the two
    // write ends of the output pipes leaves the reader threads their read ends.
    // This happens on the failure path too; the child, if any, never started.
    // A kept stdin stays open: the caller writes requests to it.
    let stdin = keep_stdin.then_some(stdin_write);
    drop(stdout_write);
    drop(stderr_write);
    // The child's copy of the stdin read end belongs to the child now.
    drop(stdin_read);

    let process = result?;
    Ok(AppContainerProcess {
        process,
        job,
        stdin,
        stdout: Some(stdout_read),
        stderr: Some(stderr_read),
    })
}

#[allow(clippy::too_many_arguments)]
fn build_and_create(
    sid: PSID,
    capabilities: Option<&[SID_AND_ATTRIBUTES]>,
    environment: &[u16],
    command_line: &str,
    cwd: &Path,
    attributes: *mut c_void,
    job: &OwnedHandle,
    stdin_read: HANDLE,
    stdout_write: HANDLE,
    stderr_write: HANDLE,
) -> Result<OwnedHandle, LaunchError> {
    let mut security = SECURITY_CAPABILITIES {
        AppContainerSid: sid,
        Capabilities: capabilities.map_or(null_mut(), |entries| entries.as_ptr() as *mut _),
        CapabilityCount: capabilities.map_or(0, |entries| entries.len() as u32),
        Reserved: 0,
    };

    // `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` and `PROC_THREAD_ATTRIBUTE_JOB_LIST`
    // keep the pointers they are handed rather than copying what they point
    // at, and `CreateProcessW` reads through them. Both arrays therefore live
    // in this frame until after the call, and the `black_box` past it is what
    // guarantees that: when they were declared inside the block below, the
    // release build reused their stack slots for the `CreateProcessW` arguments
    // and the kernel read garbage handles (`ERROR_INVALID_HANDLE` in the
    // AppContainer path, `ERROR_INVALID_PARAMETER` without it), while the debug
    // build happened to leave the slots untouched.
    let handles: [HANDLE; 3] = [stdin_read, stdout_write, stderr_write];
    let jobs: [HANDLE; 1] = [job.get()];

    // SAFETY: every value pointer below outlives the `CreateProcessW` call:
    // `attributes` is the initialized list, `security` and the capability
    // slice live in this frame, and `handles` and `jobs` do too.
    unsafe {
        if UpdateProcThreadAttribute(
            attributes,
            0,
            PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES as usize,
            &mut security as *mut _ as *const c_void,
            std::mem::size_of::<SECURITY_CAPABILITIES>(),
            null_mut(),
            null(),
        ) == 0
        {
            return Err(last_error("could not set the AppContainer security capability"));
        }

        if UpdateProcThreadAttribute(
            attributes,
            0,
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
            handles.as_ptr() as *const c_void,
            std::mem::size_of::<HANDLE>() * handles.len(),
            null_mut(),
            null(),
        ) == 0
        {
            return Err(last_error("could not set the inherited handle list"));
        }

        if UpdateProcThreadAttribute(
            attributes,
            0,
            PROC_THREAD_ATTRIBUTE_JOB_LIST as usize,
            jobs.as_ptr() as *const c_void,
            std::mem::size_of::<HANDLE>() * jobs.len(),
            null_mut(),
            null(),
        ) == 0
        {
            return Err(last_error("could not attach the job object"));
        }
    }

    let mut startup: STARTUPINFOEXW = {
        // SAFETY: plain C struct; zero is the documented empty value and every
        // field used is assigned below.
        unsafe { std::mem::zeroed() }
    };
    startup.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup.StartupInfo.hStdInput = stdin_read;
    startup.StartupInfo.hStdOutput = stdout_write;
    startup.StartupInfo.hStdError = stderr_write;
    startup.lpAttributeList = attributes;

    // `CreateProcessW` may write to the command-line buffer, which is why it is
    // a mutable vector rather than a borrowed string.
    let mut command_line: Vec<u16> =
        command_line.encode_utf16().chain(std::iter::once(0)).collect();
    let cwd = wide(&cwd.to_string_lossy());
    let mut information: PROCESS_INFORMATION = {
        // SAFETY: plain C struct; zeroed output parameter.
        unsafe { std::mem::zeroed() }
    };

    // SAFETY: the environment block is UTF-16 and double-NUL terminated (built
    // by `env_block::build`), the startup structure is a `STARTUPINFOEXW` with
    // its size set, and the attribute list is initialized and non-null.
    let ok = unsafe {
        CreateProcessW(
            null(),
            command_line.as_mut_ptr(),
            null(),
            null(),
            1,
            EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW,
            environment.as_ptr() as *const c_void,
            cwd.as_ptr(),
            &startup.StartupInfo,
            &mut information,
        )
    };
    // Reading the arrays here is what keeps them live across the call above;
    // see the comment where they are declared.
    std::hint::black_box((&handles, &jobs));
    if ok == 0 {
        return Err(last_error("could not start the process inside the AppContainer"));
    }

    // SAFETY: on success both handles are owned by the caller. The thread handle
    // is not needed; the process handle is.
    unsafe { CloseHandle(information.hThread) };
    Ok(OwnedHandle::new(information.hProcess))
}

fn create_pipe() -> Result<(OwnedHandle, OwnedHandle), LaunchError> {
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: null_mut(),
        // Inheritable: `HANDLE_LIST` rejects a handle a child cannot inherit.
        bInheritHandle: 1,
    };
    let mut read: HANDLE = null_mut();
    let mut write: HANDLE = null_mut();
    // SAFETY: both out-pointers are valid and the attributes describe an
    // inheritable pipe with the default buffer size.
    if unsafe { CreatePipe(&mut read, &mut write, &attributes, 0) } == 0 {
        return Err(last_error("could not create a pipe"));
    }
    Ok((OwnedHandle::new(read), OwnedHandle::new(write)))
}

fn create_kill_on_close_job() -> Result<OwnedHandle, LaunchError> {
    // SAFETY: no job attributes and no name.
    let job = unsafe { CreateJobObjectW(null(), null()) };
    if job.is_null() {
        return Err(last_error("could not create the job object"));
    }
    let job = OwnedHandle::new(job);
    let mut information: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = {
        // SAFETY: plain C struct; zeroed before assigning the one limit used.
        unsafe { std::mem::zeroed() }
    };
    information.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    // SAFETY: the job handle is open and `information` has the layout the class
    // expects, with its size passed.
    let ok = unsafe {
        SetInformationJobObject(
            job.get(),
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

fn last_error(context: &str) -> LaunchError {
    // SAFETY: reading the calling thread's last-error value has no preconditions.
    let code = unsafe { GetLastError() };
    let os = io::Error::from_raw_os_error(code as i32);
    LaunchError::Io(io::Error::new(os.kind(), format!("{context}: {os}")))
}

/// A handle closed exactly once, on drop or when handed over.
struct OwnedHandle(HANDLE);

impl OwnedHandle {
    fn new(handle: HANDLE) -> Self {
        Self(handle)
    }

    fn get(&self) -> HANDLE {
        self.0
    }

    /// Give up ownership: the caller becomes responsible for closing it.
    fn into_raw(mut self) -> HANDLE {
        let handle = self.0;
        self.0 = null_mut();
        handle
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            // SAFETY: the handle is owned by this value and closed once.
            unsafe { CloseHandle(self.0) };
        }
    }
}

/// One running confined process.
pub struct AppContainerProcess {
    process: OwnedHandle,
    job: OwnedHandle,
    /// The write end of the child's stdin, kept only when the spawn asked for
    /// a pipe. Dropping it early would give the child EOF.
    stdin: Option<OwnedHandle>,
    stdout: Option<OwnedHandle>,
    stderr: Option<OwnedHandle>,
}

// SAFETY: the struct owns nothing thread-affine — raw handles belong to the
// process, not to a thread, and every method that touches them takes `&mut self`.
unsafe impl Send for AppContainerProcess {}

impl RunningProcess for AppContainerProcess {
    fn try_wait(&mut self) -> io::Result<Option<i32>> {
        // SAFETY: the process handle is open and owned by this value.
        match unsafe { WaitForSingleObject(self.process.get(), 0) } {
            WAIT_OBJECT_0 => self.exit_code().map(Some),
            WAIT_TIMEOUT => Ok(None),
            _ => Err(io::Error::last_os_error()),
        }
    }

    fn wait(&mut self) -> io::Result<Option<i32>> {
        // SAFETY: the process handle is open and owned by this value.
        match unsafe { WaitForSingleObject(self.process.get(), INFINITE) } {
            WAIT_OBJECT_0 => self.exit_code().map(Some),
            _ => Err(io::Error::last_os_error()),
        }
    }

    fn kill_tree(&mut self) -> io::Result<()> {
        // Terminating the job takes every process in it, which is what the job
        // was for. If the job call fails, the direct process is still killed so
        // the caller is never left waiting on a process nobody can stop.
        // SAFETY: the job and process handles are open and owned by this value.
        let job_ok = unsafe { TerminateJobObject(self.job.get(), 1) } != 0;
        if !job_ok {
            // SAFETY: as above.
            unsafe { TerminateProcess(self.process.get(), 1) };
        }
        Ok(())
    }

    fn take_stdout(&mut self) -> Option<Box<dyn Read + Send>> {
        self.stdout.take().map(take_as_reader)
    }

    fn take_stderr(&mut self) -> Option<Box<dyn Read + Send>> {
        self.stderr.take().map(take_as_reader)
    }

    fn take_stdin(&mut self) -> Option<Box<dyn Write + Send>> {
        self.stdin.take().map(take_as_writer)
    }
}

fn take_as_reader(handle: OwnedHandle) -> Box<dyn Read + Send> {
    let raw = handle.into_raw();
    // SAFETY: the handle is a pipe read end owned by this process; ownership is
    // transferred to `File` exactly once, and `into_raw` already gave it up.
    Box::new(unsafe { File::from_raw_handle(raw as RawHandle) })
}

fn take_as_writer(handle: OwnedHandle) -> Box<dyn Write + Send> {
    let raw = handle.into_raw();
    // SAFETY: the handle is a pipe write end owned by this process; ownership
    // is transferred to `File` exactly once, and `into_raw` already gave it up.
    Box::new(unsafe { File::from_raw_handle(raw as RawHandle) })
}

impl AppContainerProcess {
    fn exit_code(&mut self) -> io::Result<i32> {
        let mut code: u32 = 0;
        // SAFETY: the process handle is open and signaled, so querying its exit
        // code is valid.
        if unsafe { GetExitCodeProcess(self.process.get(), &mut code) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(code as i32)
    }
}

// The fields close themselves on drop (declaration order). Closing the job
// handle is the last line of defense: with KILL_ON_JOB_CLOSE the kernel kills
// anything still running, including a tree whose `ReapOnDrop` never ran.
