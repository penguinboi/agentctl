#![cfg_attr(windows, allow(unsafe_code))]

#[cfg(unix)]
use nix::{
    sys::signal::{Signal, kill, killpg},
    unistd::Pid,
};

use crate::{Result, WorkspaceError};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProcessGroupId(u32);

impl ProcessGroupId {
    pub fn from_child_id(id: u32) -> Self {
        Self(id)
    }

    pub fn get(self) -> u32 {
        self.0
    }
}

/// An owned operating-system process tree.
///
/// Unix children are placed in a dedicated process group before `spawn`.
/// Windows children are assigned immediately after `spawn` to a Job Object
/// configured with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`. The value must remain
/// alive for as long as the child can run.
#[derive(Debug)]
pub struct ProcessTree {
    id: ProcessGroupId,
    #[cfg(windows)]
    job: windows_job::JobObject,
}

impl ProcessTree {
    /// Attaches a spawned Tokio child to its platform process-tree primitive.
    pub fn attach(child: &tokio::process::Child) -> Result<Self> {
        let id = child
            .id()
            .map(ProcessGroupId::from_child_id)
            .ok_or_else(|| WorkspaceError::ProcessTree("child has no process id".to_owned()))?;

        #[cfg(windows)]
        let job = windows_job::JobObject::attach(child)?;

        Ok(Self {
            id,
            #[cfg(windows)]
            job,
        })
    }

    pub fn id(&self) -> ProcessGroupId {
        self.id
    }

    /// Force-terminates the child and every descendant currently in the tree.
    pub fn terminate(&self) -> Result<()> {
        terminate_tree(self)
    }
}

#[cfg(unix)]
fn terminate_tree(tree: &ProcessTree) -> Result<()> {
    signal_group(tree.id, Signal::SIGKILL)
}

#[cfg(windows)]
fn terminate_tree(tree: &ProcessTree) -> Result<()> {
    tree.job.terminate()
}

#[cfg(not(any(unix, windows)))]
fn terminate_tree(_tree: &ProcessTree) -> Result<()> {
    Err(WorkspaceError::ProcessTree(
        "process-tree termination is unsupported on this platform".to_owned(),
    ))
}

#[cfg(unix)]
pub fn configure_tokio_process_group(command: &mut tokio::process::Command) {
    use std::os::unix::process::CommandExt;
    command.as_std_mut().process_group(0);
}

#[cfg(not(unix))]
pub fn configure_tokio_process_group(_command: &mut tokio::process::Command) {}

#[cfg(unix)]
pub fn terminate_process_group(group: ProcessGroupId) -> Result<()> {
    signal_group(group, Signal::SIGTERM)
}

#[cfg(not(unix))]
pub fn terminate_process_group(_group: ProcessGroupId) -> Result<()> {
    Err(WorkspaceError::ProcessTree(
        "PID-only process-group termination is unsupported on this platform; retain ProcessTree"
            .to_owned(),
    ))
}

#[cfg(unix)]
pub fn kill_process_group(group: ProcessGroupId) -> Result<()> {
    signal_group(group, Signal::SIGKILL)
}

#[cfg(not(unix))]
pub fn kill_process_group(_group: ProcessGroupId) -> Result<()> {
    Err(WorkspaceError::ProcessTree(
        "PID-only process-group termination is unsupported on this platform; retain ProcessTree"
            .to_owned(),
    ))
}

#[cfg(unix)]
pub fn process_group_exists(group: ProcessGroupId) -> Result<bool> {
    let pid = platform_pid(group)?;
    match kill(Pid::from_raw(-pid.as_raw()), None) {
        Ok(()) | Err(nix::errno::Errno::EPERM) => Ok(true),
        Err(nix::errno::Errno::ESRCH) => Ok(false),
        Err(error) => Err(error.into()),
    }
}

#[cfg(not(unix))]
pub fn process_group_exists(_group: ProcessGroupId) -> Result<bool> {
    Err(WorkspaceError::ProcessTree(
        "PID-only process-group queries are unsupported on this platform".to_owned(),
    ))
}

/// Checks whether the journaled process identity is still live. On Unix the
/// child PID is also its dedicated process-group id; on Windows this checks the
/// root process after the wrapper-owned Job Object has been closed.
#[cfg(unix)]
pub fn process_root_exists(id: ProcessGroupId) -> Result<bool> {
    process_group_exists(id)
}

#[cfg(windows)]
pub fn process_root_exists(id: ProcessGroupId) -> Result<bool> {
    windows_job::process_exists(id)
}

#[cfg(not(any(unix, windows)))]
pub fn process_root_exists(_id: ProcessGroupId) -> Result<bool> {
    Err(WorkspaceError::ProcessTree(
        "process identity queries are unsupported on this platform".to_owned(),
    ))
}

#[cfg(unix)]
fn signal_group(group: ProcessGroupId, signal: Signal) -> Result<()> {
    match killpg(platform_pid(group)?, signal) {
        Ok(()) | Err(nix::errno::Errno::ESRCH) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[cfg(unix)]
fn platform_pid(group: ProcessGroupId) -> Result<Pid> {
    let raw = i32::try_from(group.0).map_err(|_| WorkspaceError::InvalidProcessGroup(group.0))?;
    Ok(Pid::from_raw(raw))
}

#[cfg(windows)]
mod windows_job {
    use std::{
        io,
        mem::size_of,
        os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
        ptr,
    };

    use tokio::process::Child;
    use windows_sys::Win32::{
        Foundation::{ERROR_ACCESS_DENIED, ERROR_INVALID_PARAMETER, HANDLE, STILL_ACTIVE},
        System::{
            JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
                SetInformationJobObject, TerminateJobObject,
            },
            Threading::{GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
        },
    };

    use super::ProcessGroupId;
    use crate::{Result, WorkspaceError};

    #[derive(Debug)]
    pub(super) struct JobObject {
        handle: OwnedHandle,
    }

    impl JobObject {
        pub(super) fn attach(child: &Child) -> Result<Self> {
            let raw_process = child.raw_handle().ok_or_else(|| {
                WorkspaceError::ProcessTree("child has no Windows process handle".to_owned())
            })?;

            // SAFETY: null security/name pointers request an unnamed Job Object
            // with default security. The returned owned handle is closed exactly
            // once by `OwnedHandle`.
            let raw_job = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
            if raw_job.is_null() {
                return Err(last_error("CreateJobObjectW"));
            }
            // SAFETY: `raw_job` is a newly-created, non-null owned HANDLE.
            let handle = unsafe { OwnedHandle::from_raw_handle(raw_job.cast()) };
            let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let size =
                u32::try_from(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>()).map_err(|_| {
                    WorkspaceError::ProcessTree("Job Object limits overflow".to_owned())
                })?;

            // SAFETY: both handles are valid for the duration of the calls and
            // `limits` points to the exact structure/size required by the info
            // class. No pointer is retained by Windows.
            let configured = unsafe {
                SetInformationJobObject(
                    handle.as_raw_handle().cast::<core::ffi::c_void>() as HANDLE,
                    JobObjectExtendedLimitInformation,
                    (&raw const limits).cast(),
                    size,
                )
            };
            if configured == 0 {
                return Err(last_error("SetInformationJobObject"));
            }
            // SAFETY: Tokio owns `raw_process` until the Child is dropped; the
            // Job Object handle is valid and remains owned by this value.
            let assigned = unsafe {
                AssignProcessToJobObject(
                    handle.as_raw_handle().cast::<core::ffi::c_void>() as HANDLE,
                    raw_process.cast::<core::ffi::c_void>() as HANDLE,
                )
            };
            if assigned == 0 {
                return Err(last_error("AssignProcessToJobObject"));
            }

            Ok(Self { handle })
        }

        pub(super) fn terminate(&self) -> Result<()> {
            // SAFETY: `handle` owns a live Job Object handle.
            let terminated = unsafe {
                TerminateJobObject(
                    self.handle.as_raw_handle().cast::<core::ffi::c_void>() as HANDLE,
                    1,
                )
            };
            if terminated == 0 {
                return Err(last_error("TerminateJobObject"));
            }
            Ok(())
        }
    }

    pub(super) fn process_exists(id: ProcessGroupId) -> Result<bool> {
        // SAFETY: the call only opens a query handle for the numeric PID. The
        // returned handle is either null or converted immediately to one owned
        // handle which closes exactly once.
        let raw = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, id.get()) };
        if raw.is_null() {
            let error = io::Error::last_os_error();
            return match error.raw_os_error().map(|code| code as u32) {
                Some(ERROR_INVALID_PARAMETER) => Ok(false),
                Some(ERROR_ACCESS_DENIED) => Ok(true),
                _ => Err(WorkspaceError::ProcessTree(format!(
                    "OpenProcess failed: {error}"
                ))),
            };
        }
        // SAFETY: `raw` is a newly-opened, non-null owned process handle.
        let handle = unsafe { OwnedHandle::from_raw_handle(raw.cast()) };
        let mut exit_code = 0_u32;
        // SAFETY: the process handle remains valid and `exit_code` points to a
        // writable u32 for the duration of the call.
        let queried = unsafe {
            GetExitCodeProcess(
                handle.as_raw_handle().cast::<core::ffi::c_void>() as HANDLE,
                &mut exit_code,
            )
        };
        if queried == 0 {
            return Err(last_error("GetExitCodeProcess"));
        }
        Ok(exit_code == STILL_ACTIVE as u32)
    }

    fn last_error(operation: &str) -> WorkspaceError {
        WorkspaceError::ProcessTree(format!(
            "{operation} failed: {}",
            io::Error::last_os_error()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_group_id_round_trips_child_id() {
        let id = ProcessGroupId::from_child_id(42);
        assert_eq!(id.get(), 42);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn owned_tree_terminates_spawned_process_group() {
        let mut command = tokio::process::Command::new("/bin/sh");
        command.arg("-c").arg("sleep 30");
        configure_tokio_process_group(&mut command);
        let mut child = command.spawn().unwrap();
        let tree = ProcessTree::attach(&child).unwrap();

        tree.terminate().unwrap();
        let status = tokio::time::timeout(std::time::Duration::from_secs(2), child.wait())
            .await
            .unwrap()
            .unwrap();
        assert!(!status.success());
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_job_terminates_attached_child() {
        let mut command = tokio::process::Command::new("cmd.exe");
        command
            .args(["/C", "ping -n 30 127.0.0.1 >NUL"])
            .kill_on_drop(true);
        configure_tokio_process_group(&mut command);
        let mut child = command.spawn().unwrap();
        let tree = ProcessTree::attach(&child).unwrap();

        tree.terminate().unwrap();
        let status = tokio::time::timeout(std::time::Duration::from_secs(5), child.wait())
            .await
            .unwrap()
            .unwrap();
        assert!(!status.success());
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_job_close_terminates_attached_child() {
        let mut command = tokio::process::Command::new("cmd.exe");
        command
            .args(["/C", "ping -n 30 127.0.0.1 >NUL"])
            .kill_on_drop(true);
        let mut child = command.spawn().unwrap();
        let tree = ProcessTree::attach(&child).unwrap();

        drop(tree);
        let status = tokio::time::timeout(std::time::Duration::from_secs(5), child.wait())
            .await
            .unwrap()
            .unwrap();
        assert!(!status.success());
    }
}
