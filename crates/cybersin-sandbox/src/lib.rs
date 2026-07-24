//! Sandboxed execution backends (spec §8.4).

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
use wait_timeout::ChildExt;

/// Whether a workspace is discarded after one call or retained for a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxScope {
    Call,
    Session,
}

/// How a file changed relative to a named snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DiffKind {
    Added,
    Modified,
    Deleted,
}

/// Owns call- and session-scoped copy-on-write workspaces plus snapshots.
#[derive(Debug, Clone)]
pub struct WorkspaceStore {
    root: PathBuf,
}

impl WorkspaceStore {
    pub fn new(root: impl Into<PathBuf>) -> io::Result<Self> {
        let root = root.into();
        fs::create_dir_all(root.join("workspaces"))?;
        fs::create_dir_all(root.join("snapshots"))?;
        Ok(Self { root })
    }

    pub fn open(
        &self,
        scope: SandboxScope,
        session_id: &str,
        call_id: &str,
    ) -> io::Result<Workspace> {
        validate_id(session_id)?;
        validate_id(call_id)?;
        let relative = match scope {
            SandboxScope::Call => PathBuf::from("calls").join(session_id).join(call_id),
            SandboxScope::Session => PathBuf::from("sessions").join(session_id),
        };
        let path = self.root.join("workspaces").join(&relative);
        if scope == SandboxScope::Call && path.exists() {
            fs::remove_dir_all(&path)?;
        }
        fs::create_dir_all(&path)?;
        Ok(Workspace {
            path,
            snapshot_root: self.root.join("snapshots").join(relative),
            discard_on_drop: scope == SandboxScope::Call,
        })
    }
}

/// A live sandbox workspace.
#[derive(Debug)]
pub struct Workspace {
    path: PathBuf,
    snapshot_root: PathBuf,
    discard_on_drop: bool,
}

impl Workspace {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn snapshot(&self, checkpoint_id: &str) -> io::Result<()> {
        validate_id(checkpoint_id)?;
        let destination = self.snapshot_root.join(checkpoint_id);
        if destination.exists() {
            fs::remove_dir_all(&destination)?;
        }
        fs::create_dir_all(&destination)?;
        copy_tree(&self.path, &destination)
    }

    pub fn diff(&self, checkpoint_id: &str) -> io::Result<Vec<(PathBuf, DiffKind)>> {
        validate_id(checkpoint_id)?;
        let snapshot = self.snapshot_root.join(checkpoint_id);
        if !snapshot.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("sandbox snapshot {checkpoint_id:?} does not exist"),
            ));
        }
        let before = read_tree(&snapshot)?;
        let after = read_tree(&self.path)?;
        let paths: BTreeSet<_> = before.keys().chain(after.keys()).cloned().collect();
        let mut changes = Vec::new();
        for path in paths {
            let kind = match (before.get(&path), after.get(&path)) {
                (None, Some(_)) => Some(DiffKind::Added),
                (Some(_), None) => Some(DiffKind::Deleted),
                (Some(left), Some(right)) if left != right => Some(DiffKind::Modified),
                _ => None,
            };
            if let Some(kind) = kind {
                changes.push((path, kind));
            }
        }
        Ok(changes)
    }

    pub fn restore(&self, checkpoint_id: &str) -> io::Result<()> {
        validate_id(checkpoint_id)?;
        let snapshot = self.snapshot_root.join(checkpoint_id);
        if !snapshot.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("sandbox snapshot {checkpoint_id:?} does not exist"),
            ));
        }
        clear_directory(&self.path)?;
        copy_tree(&snapshot, &self.path)
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        if self.discard_on_drop {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

fn validate_id(id: &str) -> io::Result<()> {
    if !id.is_empty()
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid sandbox identifier {id:?}"),
        ))
    }
}

fn clear_directory(path: &Path) -> io::Result<()> {
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            fs::remove_dir_all(entry.path())?;
        } else {
            fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

fn copy_tree(source: &Path, destination: &Path) -> io::Result<()> {
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let target = destination.join(entry.file_name());
        if file_type.is_dir() {
            fs::create_dir_all(&target)?;
            copy_tree(&entry.path(), &target)?;
        } else if file_type.is_file() {
            fs::copy(entry.path(), target)?;
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "sandbox workspaces may not contain symlinks or special files",
            ));
        }
    }
    Ok(())
}

fn read_tree(root: &Path) -> io::Result<BTreeMap<PathBuf, Vec<u8>>> {
    fn visit(
        root: &Path,
        current: &Path,
        files: &mut BTreeMap<PathBuf, Vec<u8>>,
    ) -> io::Result<()> {
        for entry in fs::read_dir(current)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                visit(root, &entry.path(), files)?;
            } else if file_type.is_file() {
                let relative = entry
                    .path()
                    .strip_prefix(root)
                    .map_err(io::Error::other)?
                    .to_path_buf();
                files.insert(relative, fs::read(entry.path())?);
            } else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "sandbox workspaces may not contain symlinks or special files",
                ));
            }
        }
        Ok(())
    }

    let mut files = BTreeMap::new();
    visit(root, root, &mut files)?;
    Ok(files)
}

/// Hard resource ceilings applied to one sandbox execution.
#[derive(Debug, Clone, PartialEq)]
pub struct ResourceLimits {
    pub cpus: f64,
    pub memory_mb: u64,
    pub pids: u32,
    pub wall_clock: Duration,
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            cpus: 1.0,
            memory_mb: 512,
            pids: 128,
            wall_clock: Duration::from_secs(30),
        }
    }
}

/// A single agent-generated command to execute.
#[derive(Debug, Clone, PartialEq)]
pub struct ExecRequest {
    pub image: String,
    pub command: Vec<String>,
    pub workspace: PathBuf,
    pub scope: SandboxScope,
    pub limits: ResourceLimits,
}

/// Observable result of a sandbox execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecOutcome {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub termination: Termination,
}

impl ExecOutcome {
    pub fn succeeded(&self) -> bool {
        self.termination == Termination::Exited && self.exit_code == Some(0)
    }
}

/// Resource ceiling responsible for terminating an execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitKind {
    WallClock,
    /// Docker reports an OOM, PID-limit, or comparable container-level kill.
    ContainerResource,
}

/// Why an execution stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Termination {
    Exited,
    KilledByLimit(LimitKind),
}

/// Public boundary implemented by each supported container backend.
pub trait SandboxBackend {
    fn exec(&self, request: ExecRequest) -> io::Result<ExecOutcome>;
}

/// Docker-backed development sandbox.
#[derive(Debug, Clone)]
pub struct DockerBackend {
    binary: PathBuf,
    runtime: Option<&'static str>,
}

impl DockerBackend {
    pub fn new() -> Self {
        Self::with_binary("docker")
    }

    /// Select an alternate Docker-compatible CLI.
    ///
    /// This is useful for remote/container runtimes and for exercising the
    /// public backend contract without requiring a daemon in unit tests.
    pub fn with_binary(binary: impl Into<PathBuf>) -> Self {
        Self {
            binary: binary.into(),
            runtime: None,
        }
    }
}

impl Default for DockerBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl SandboxBackend for DockerBackend {
    fn exec(&self, request: ExecRequest) -> io::Result<ExecOutcome> {
        execute_container(&self.binary, self.runtime, request)
    }
}

/// Docker using gVisor's `runsc` OCI runtime. This is the production default.
#[derive(Debug, Clone)]
pub struct GvisorBackend {
    binary: PathBuf,
}

impl GvisorBackend {
    pub fn new() -> Self {
        Self::with_binary("docker")
    }

    pub fn with_binary(binary: impl Into<PathBuf>) -> Self {
        Self {
            binary: binary.into(),
        }
    }
}

impl Default for GvisorBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl SandboxBackend for GvisorBackend {
    fn exec(&self, request: ExecRequest) -> io::Result<ExecOutcome> {
        execute_container(&self.binary, Some("runsc"), request)
    }
}

fn execute_container(
    binary: &PathBuf,
    runtime: Option<&str>,
    request: ExecRequest,
) -> io::Result<ExecOutcome> {
    let run_state = tempfile::tempdir()?;
    let cidfile = run_state.path().join("container-id");
    let mount = format!(
        "type=bind,src={},dst=/workspace",
        request.workspace.display()
    );
    let mut command = Command::new(binary);
    command.arg("run");
    if let Some(runtime) = runtime {
        command.arg(format!("--runtime={runtime}"));
    }
    command
        .arg("--network")
        .arg("none")
        .arg("--cpus")
        .arg(format_cpu_limit(request.limits.cpus))
        .arg("--memory")
        .arg(format!("{}m", request.limits.memory_mb))
        .arg("--pids-limit")
        .arg(request.limits.pids.to_string())
        .arg("--read-only")
        .arg("--rm")
        .arg("--cidfile")
        .arg(&cidfile)
        .arg("--tmpfs")
        .arg("/tmp:rw,noexec,nosuid,size=64m")
        .arg("--mount")
        .arg(mount)
        .arg("--workdir")
        .arg("/workspace")
        .arg(request.image)
        .args(request.command)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = command.spawn()?;
    let timed_out = child.wait_timeout(request.limits.wall_clock)?.is_none();
    let termination = if !timed_out {
        Termination::Exited
    } else {
        if let Ok(container_id) = fs::read_to_string(&cidfile) {
            let container_id = container_id.trim();
            if !container_id.is_empty() {
                let _ = Command::new(binary).arg("kill").arg(container_id).status();
            }
        }
        child.kill()?;
        Termination::KilledByLimit(LimitKind::WallClock)
    };
    let output = child.wait_with_output()?;
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let container_resource_limit = matches!(output.status.code(), None | Some(137))
        || stderr.contains("Resource temporarily unavailable")
        || stderr.contains("can't fork")
        || stderr.contains("Cannot fork");
    let termination = if termination == Termination::Exited && container_resource_limit {
        Termination::KilledByLimit(LimitKind::ContainerResource)
    } else {
        termination
    };

    Ok(ExecOutcome {
        exit_code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr,
        termination,
    })
}

fn format_cpu_limit(cpus: f64) -> String {
    if cpus.fract() == 0.0 {
        format!("{cpus:.0}")
    } else {
        cpus.to_string()
    }
}
