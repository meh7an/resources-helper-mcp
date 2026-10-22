//! Starting `rhd` as a detached background process.

use std::{
    env, io,
    path::PathBuf,
    process::{Command, Stdio},
    time::Duration,
};

const DAEMON_EXE: &str = "rhd.exe";
const DEFAULT_READY_TIMEOUT: Duration = Duration::from_secs(5);

// Win32 process creation flags. DETACHED_PROCESS gives rhd no console, so closing the
// terminal that started it does not stop it; a new process group keeps Ctrl+C in that
// terminal from reaching it.
#[cfg(windows)]
const DETACHED_PROCESS: u32 = 0x0000_0008;
#[cfg(windows)]
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

/// How the client starts the daemon when it is not running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnOptions {
    /// The `rhd` executable.
    pub daemon_exe: PathBuf,
    /// How long to wait for `/health` to answer after starting it.
    pub ready_timeout: Duration,
}

impl SpawnOptions {
    /// `rhd.exe` from the directory of the running executable, where the install puts both
    /// binaries; plain `rhd.exe` (found on `PATH`) when that directory has none.
    pub fn beside_current_exe() -> Self {
        let beside = env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(|dir| dir.join(DAEMON_EXE)))
            .filter(|path| path.is_file());
        Self::at(beside.unwrap_or_else(|| PathBuf::from(DAEMON_EXE)))
    }

    /// Starts `daemon_exe`, with the default ready timeout.
    pub fn at(daemon_exe: PathBuf) -> Self {
        Self {
            daemon_exe,
            ready_timeout: DEFAULT_READY_TIMEOUT,
        }
    }

    /// Starts the daemon without waiting for it; it outlives the caller.
    pub(crate) fn spawn_detached(&self) -> io::Result<()> {
        let mut command = Command::new(&self.daemon_exe);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
        }
        command.spawn().map(drop)
    }
}
