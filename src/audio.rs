use std::fmt;
use std::future::Future;
use std::io;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, watch};
use tokio::time::{Instant, MissedTickBehavior};

use crate::state::DesiredAudio;

pub const WPCTL_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceId(pub u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DesiredRequest {
    pub revision: u64,
    pub state: DesiredAudio,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AckOutcome {
    Applied,
    HandledFailure,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioAck {
    pub revision: u64,
    pub outcome: AckOutcome,
}

pub struct AudioWorker {
    worker: Worker<TokioLauncher>,
}

impl AudioWorker {
    pub fn new(
        desired: watch::Receiver<Option<DesiredRequest>>,
        acknowledgements: mpsc::UnboundedSender<AudioAck>,
        stop: watch::Receiver<bool>,
    ) -> Self {
        Self {
            worker: Worker::new(TokioLauncher, desired, acknowledgements, stop),
        }
    }

    pub async fn run(self) -> Result<(), WorkerError> {
        self.worker.run().await
    }
}

#[derive(Debug)]
pub enum WorkerError {
    Audio(AudioError),
    AcknowledgementChannelClosed,
}

impl fmt::Display for WorkerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Audio(error) => error.fmt(formatter),
            Self::AcknowledgementChannelClosed => {
                write!(formatter, "audio acknowledgement channel closed")
            }
        }
    }
}

impl std::error::Error for WorkerError {}

#[derive(Debug)]
pub enum AudioError {
    Spawn {
        operation: &'static str,
        source: io::Error,
    },
    Wait {
        operation: &'static str,
        source: io::Error,
    },
    NonzeroExit {
        operation: &'static str,
        status: Option<i32>,
    },
    Parse {
        operation: &'static str,
        message: String,
    },
    Timeout {
        operation: &'static str,
    },
    Kill {
        operation: &'static str,
        source: io::Error,
    },
    Reap {
        operation: &'static str,
        source: io::Error,
    },
    Poisoned {
        operation: &'static str,
    },
}

impl fmt::Display for AudioError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spawn { operation, source } => {
                write!(formatter, "failed to start {operation}: {source}")
            }
            Self::Wait { operation, source } => {
                write!(formatter, "failed while waiting for {operation}: {source}")
            }
            Self::NonzeroExit { operation, status } => {
                write!(formatter, "{operation} exited unsuccessfully ({status:?})")
            }
            Self::Parse { operation, message } => {
                write!(formatter, "could not parse {operation} output: {message}")
            }
            Self::Timeout { operation } => write!(formatter, "{operation} timed out"),
            Self::Kill { operation, source } => {
                write!(formatter, "failed to kill timed-out {operation}: {source}")
            }
            Self::Reap { operation, source } => write!(
                formatter,
                "failed to reap after incomplete {operation}: {source}"
            ),
            Self::Poisoned { operation } => write!(
                formatter,
                "refusing {operation} because child cleanup is uncertain"
            ),
        }
    }
}

impl std::error::Error for AudioError {}

struct Worker<L> {
    commands: CommandRunner<L>,
    desired: watch::Receiver<Option<DesiredRequest>>,
    desired_open: bool,
    acknowledgements: mpsc::UnboundedSender<AudioAck>,
    stop: watch::Receiver<bool>,
    remembered: Option<SourceId>,
    last_acknowledged: Option<u64>,
    handled: Option<DesiredRequest>,
    idle_retry_pending: bool,
}

impl<L: Launcher> Worker<L> {
    fn new(
        launcher: L,
        desired: watch::Receiver<Option<DesiredRequest>>,
        acknowledgements: mpsc::UnboundedSender<AudioAck>,
        stop: watch::Receiver<bool>,
    ) -> Self {
        Self {
            commands: CommandRunner::new(launcher),
            desired,
            desired_open: true,
            acknowledgements,
            stop,
            remembered: None,
            last_acknowledged: None,
            handled: None,
            idle_retry_pending: false,
        }
    }

    async fn run(mut self) -> Result<(), WorkerError> {
        let mut pending = *self.desired.borrow_and_update();
        let mut ticks = tokio::time::interval_at(
            Instant::now() + Duration::from_secs(1),
            Duration::from_secs(1),
        );
        ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            if let Some(request) = pending.take() {
                if self
                    .last_acknowledged
                    .is_some_and(|revision| request.revision <= revision)
                {
                    continue;
                }
                match self.apply(request).await? {
                    ApplyResult::Completed(outcome) => {
                        if let Some(newer) = self.take_newer(request.revision) {
                            pending = Some(newer);
                            continue;
                        }
                        self.acknowledge(request.revision, outcome)?;
                        self.handled = Some(request);
                    }
                    ApplyResult::Superseded(newer) => {
                        pending = Some(newer);
                        continue;
                    }
                }
                continue;
            }

            if *self.stop.borrow() {
                return Ok(());
            }
            let idle = self
                .handled
                .is_some_and(|request| request.state == DesiredAudio::PttIdle);
            tokio::select! {
                biased;
                result = self.stop.changed() => {
                    if result.is_err() || *self.stop.borrow_and_update() {
                        return Ok(());
                    }
                }
                result = self.desired.changed(), if self.desired_open => {
                    match result {
                        Ok(()) => pending = *self.desired.borrow_and_update(),
                        Err(_) => self.desired_open = false,
                    }
                }
                _ = ticks.tick(), if idle => {
                    if let Some(newer) = self.reconcile_idle().await? {
                        pending = Some(newer);
                    }
                }
            }
        }
    }

    async fn apply(&mut self, request: DesiredRequest) -> Result<ApplyResult, WorkerError> {
        match request.state {
            DesiredAudio::PttTalking => self.apply_talking(request).await,
            DesiredAudio::Open => self.apply_open(request).await,
            DesiredAudio::PttIdle => self.apply_idle(request).await,
        }
    }

    async fn apply_talking(&mut self, request: DesiredRequest) -> Result<ApplyResult, WorkerError> {
        let output = match self
            .commands
            .execute_interruptible(
                "resolve default source",
                &["inspect", "@DEFAULT_AUDIO_SOURCE@"],
                request.revision,
                &mut self.desired,
                &mut self.desired_open,
            )
            .await
        {
            Ok(Execution::Output(output)) => output,
            Ok(Execution::Superseded(newer)) => return Ok(ApplyResult::Superseded(newer)),
            Err(error) => return self.handled_error(error),
        };
        let source = match parse_source_id(&output) {
            Ok(source) => source,
            Err(message) => {
                return self.handled_error(AudioError::Parse {
                    operation: "resolve default source",
                    message,
                });
            }
        };
        self.remembered = Some(source);
        if let Some(newer) = self.take_newer(request.revision) {
            return Ok(ApplyResult::Superseded(newer));
        }

        let source_arg = source.0.to_string();
        match self
            .commands
            .execute_interruptible(
                "unmute concrete source",
                &["set-mute", &source_arg, "0"],
                request.revision,
                &mut self.desired,
                &mut self.desired_open,
            )
            .await
        {
            Ok(Execution::Output(_)) => Ok(ApplyResult::Completed(AckOutcome::Applied)),
            Ok(Execution::Superseded(newer)) => Ok(ApplyResult::Superseded(newer)),
            Err(error) => self.handled_error(error),
        }
    }

    async fn apply_open(&mut self, request: DesiredRequest) -> Result<ApplyResult, WorkerError> {
        match self
            .commands
            .execute_interruptible(
                "unmute default source",
                &["set-mute", "@DEFAULT_AUDIO_SOURCE@", "0"],
                request.revision,
                &mut self.desired,
                &mut self.desired_open,
            )
            .await
        {
            Ok(Execution::Output(_)) => Ok(ApplyResult::Completed(AckOutcome::Applied)),
            Ok(Execution::Superseded(newer)) => Ok(ApplyResult::Superseded(newer)),
            Err(error) => self.handled_error(error),
        }
    }

    async fn apply_idle(&mut self, request: DesiredRequest) -> Result<ApplyResult, WorkerError> {
        let succeeded = self.mute_idle_targets().await?;
        self.idle_retry_pending = !succeeded;
        if let Some(newer) = self.take_newer(request.revision) {
            return Ok(ApplyResult::Superseded(newer));
        }
        Ok(ApplyResult::Completed(if succeeded {
            AckOutcome::Applied
        } else {
            AckOutcome::HandledFailure
        }))
    }

    async fn mute_idle_targets(&mut self) -> Result<bool, WorkerError> {
        let mut succeeded = true;
        if let Some(source) = self.remembered {
            let source_arg = source.0.to_string();
            if let Err(error) = self
                .commands
                .execute("mute remembered source", &["set-mute", &source_arg, "1"])
                .await
            {
                self.note_nonfatal(error)?;
                succeeded = false;
            }
        }
        if let Err(error) = self
            .commands
            .execute(
                "mute default source",
                &["set-mute", "@DEFAULT_AUDIO_SOURCE@", "1"],
            )
            .await
        {
            self.note_nonfatal(error)?;
            succeeded = false;
        }
        Ok(succeeded)
    }

    async fn reconcile_idle(&mut self) -> Result<Option<DesiredRequest>, WorkerError> {
        let revision = self
            .handled
            .expect("idle reconciliation requires handled state")
            .revision;
        if self.idle_retry_pending {
            self.idle_retry_pending = !self.mute_idle_targets().await?;
            return Ok(self.take_newer(revision));
        }

        let output = match self
            .commands
            .execute_interruptible(
                "query default source mute",
                &["get-volume", "@DEFAULT_AUDIO_SOURCE@"],
                revision,
                &mut self.desired,
                &mut self.desired_open,
            )
            .await
        {
            Ok(Execution::Output(output)) => output,
            Ok(Execution::Superseded(newer)) => return Ok(Some(newer)),
            Err(error) => {
                self.note_nonfatal(error)?;
                return Ok(self.take_newer(revision));
            }
        };
        let muted = match parse_default_mute(&output) {
            Ok(muted) => muted,
            Err(message) => {
                self.note_nonfatal(AudioError::Parse {
                    operation: "query default source mute",
                    message,
                })?;
                return Ok(self.take_newer(revision));
            }
        };
        if muted {
            return Ok(self.take_newer(revision));
        }
        if let Some(newer) = self.take_newer(revision) {
            return Ok(Some(newer));
        }
        if let Err(error) = self
            .commands
            .execute(
                "reconcile default source mute",
                &["set-mute", "@DEFAULT_AUDIO_SOURCE@", "1"],
            )
            .await
        {
            self.note_nonfatal(error)?;
        }
        Ok(self.take_newer(revision))
    }

    fn take_newer(&mut self, revision: u64) -> Option<DesiredRequest> {
        loop {
            match self.desired.has_changed() {
                Ok(true) => {
                    let request = *self.desired.borrow_and_update();
                    if let Some(request) = request
                        && request.revision > revision
                    {
                        return Some(request);
                    }
                }
                Ok(false) => return None,
                Err(_) => {
                    self.desired_open = false;
                    return None;
                }
            }
        }
    }

    fn acknowledge(&mut self, revision: u64, outcome: AckOutcome) -> Result<(), WorkerError> {
        if self
            .last_acknowledged
            .is_some_and(|previous| revision <= previous)
        {
            return Ok(());
        }
        self.acknowledgements
            .send(AudioAck { revision, outcome })
            .map_err(|_| WorkerError::AcknowledgementChannelClosed)?;
        self.last_acknowledged = Some(revision);
        Ok(())
    }

    fn handled_error(&self, error: AudioError) -> Result<ApplyResult, WorkerError> {
        if is_fatal(&error) {
            Err(WorkerError::Audio(error))
        } else {
            tracing::error!(%error, "audio operation failed");
            Ok(ApplyResult::Completed(AckOutcome::HandledFailure))
        }
    }

    fn note_nonfatal(&self, error: AudioError) -> Result<(), WorkerError> {
        if is_fatal(&error) {
            Err(WorkerError::Audio(error))
        } else {
            tracing::error!(%error, "audio operation failed");
            Ok(())
        }
    }
}

fn is_fatal(error: &AudioError) -> bool {
    matches!(error, AudioError::Reap { .. } | AudioError::Poisoned { .. })
}

enum ApplyResult {
    Completed(AckOutcome),
    Superseded(DesiredRequest),
}

enum Execution {
    Output(Vec<u8>),
    Superseded(DesiredRequest),
}

fn parse_source_id(output: &[u8]) -> Result<SourceId, String> {
    let output = std::str::from_utf8(output).map_err(|error| error.to_string())?;
    let first_line = output
        .lines()
        .next()
        .ok_or_else(|| "output is empty".to_owned())?;
    let (id, node_type) = first_line
        .strip_prefix("id ")
        .and_then(|line| line.split_once(", type "))
        .ok_or_else(|| "missing source identity header".to_owned())?;
    if node_type != "PipeWire:Interface:Node" {
        return Err("default source is not a PipeWire node".to_owned());
    }
    if id.is_empty() || !id.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("source ID is not numeric".to_owned());
    }
    let id: u32 = id
        .parse()
        .map_err(|_| "source ID is out of range".to_owned())?;
    if id == 0 {
        return Err("source ID must be positive".to_owned());
    }
    Ok(SourceId(id))
}

fn parse_default_mute(output: &[u8]) -> Result<bool, String> {
    let output = std::str::from_utf8(output).map_err(|error| error.to_string())?;
    let line = output.strip_suffix('\n').unwrap_or(output);
    if line.contains(['\n', '\r']) {
        return Err("unexpected trailing output".to_owned());
    }
    let value = line
        .strip_prefix("Volume: ")
        .ok_or_else(|| "missing volume prefix".to_owned())?;
    let (number, muted) = if let Some(number) = value.strip_suffix(" [MUTED]") {
        (number, true)
    } else {
        (value, false)
    };
    let volume: f64 = number
        .parse()
        .map_err(|_| "volume is not numeric".to_owned())?;
    if !volume.is_finite() {
        return Err("volume is not finite".to_owned());
    }
    Ok(muted)
}

struct CommandOutput {
    success: bool,
    status: Option<i32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

trait Launcher {
    type Child: ManagedChild;
    fn spawn(&mut self, args: &[&str]) -> io::Result<Self::Child>;
}

trait ManagedChild {
    fn wait(&mut self) -> impl Future<Output = io::Result<CommandOutput>>;
    fn start_kill(&mut self) -> io::Result<()>;
    fn reap(&mut self) -> impl Future<Output = io::Result<()>>;
}

struct CommandRunner<L> {
    launcher: L,
    poisoned: bool,
}

impl<L: Launcher> CommandRunner<L> {
    fn new(launcher: L) -> Self {
        Self {
            launcher,
            poisoned: false,
        }
    }

    async fn execute(
        &mut self,
        operation: &'static str,
        args: &[&str],
    ) -> Result<Vec<u8>, AudioError> {
        if self.poisoned {
            tracing::error!(
                operation,
                ?args,
                "refusing wpctl spawn after uncertain child cleanup"
            );
            return Err(AudioError::Poisoned { operation });
        }
        let mut child = self.spawn(operation, args)?;
        match tokio::time::timeout(WPCTL_TIMEOUT, child.wait()).await {
            Ok(result) => self.finish_wait(operation, args, result, &mut child).await,
            Err(_) => {
                tracing::error!(operation, ?args, "wpctl timed out");
                let kill_error = self.kill_and_reap(&mut child, operation, args).await?;
                if let Some(source) = kill_error {
                    Err(AudioError::Kill { operation, source })
                } else {
                    Err(AudioError::Timeout { operation })
                }
            }
        }
    }

    async fn execute_interruptible(
        &mut self,
        operation: &'static str,
        args: &[&str],
        revision: u64,
        desired: &mut watch::Receiver<Option<DesiredRequest>>,
        desired_open: &mut bool,
    ) -> Result<Execution, AudioError> {
        if self.poisoned {
            tracing::error!(
                operation,
                ?args,
                "refusing wpctl spawn after uncertain child cleanup"
            );
            return Err(AudioError::Poisoned { operation });
        }
        let mut child = self.spawn(operation, args)?;
        let outcome = {
            let wait = child.wait();
            tokio::pin!(wait);
            let deadline = tokio::time::sleep(WPCTL_TIMEOUT);
            tokio::pin!(deadline);
            loop {
                tokio::select! {
                    result = &mut wait => break InterruptOutcome::Finished(result),
                    _ = &mut deadline => break InterruptOutcome::Timeout,
                    changed = desired.changed(), if *desired_open => {
                        match changed {
                            Ok(()) => {
                                let latest = *desired.borrow_and_update();
                                if let Some(latest) = latest
                                    && latest.revision > revision
                                {
                                    break InterruptOutcome::Superseded(latest);
                                }
                            }
                            Err(_) => *desired_open = false,
                        }
                    }
                }
            }
        };
        match outcome {
            InterruptOutcome::Finished(result) => self
                .finish_wait(operation, args, result, &mut child)
                .await
                .map(Execution::Output),
            InterruptOutcome::Timeout => {
                tracing::error!(operation, ?args, "wpctl timed out");
                let kill_error = self.kill_and_reap(&mut child, operation, args).await?;
                if let Some(source) = kill_error {
                    Err(AudioError::Kill { operation, source })
                } else {
                    Err(AudioError::Timeout { operation })
                }
            }
            InterruptOutcome::Superseded(latest) => {
                self.kill_and_reap(&mut child, operation, args).await?;
                Ok(Execution::Superseded(latest))
            }
        }
    }

    fn spawn(&mut self, operation: &'static str, args: &[&str]) -> Result<L::Child, AudioError> {
        self.launcher.spawn(args).map_err(|source| {
            tracing::error!(operation, ?args, %source, "failed to spawn wpctl");
            AudioError::Spawn { operation, source }
        })
    }

    async fn finish_wait(
        &mut self,
        operation: &'static str,
        args: &[&str],
        result: io::Result<CommandOutput>,
        child: &mut L::Child,
    ) -> Result<Vec<u8>, AudioError> {
        match result {
            Ok(output) if output.success => Ok(output.stdout),
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                tracing::error!(operation, ?args, status = ?output.status, %stderr, "wpctl exited unsuccessfully");
                Err(AudioError::NonzeroExit {
                    operation,
                    status: output.status,
                })
            }
            Err(source) => {
                tracing::error!(operation, ?args, %source, "failed while waiting for wpctl");
                self.kill_and_reap(child, operation, args).await?;
                Err(AudioError::Wait { operation, source })
            }
        }
    }

    async fn kill_and_reap(
        &mut self,
        child: &mut L::Child,
        operation: &'static str,
        args: &[&str],
    ) -> Result<Option<io::Error>, AudioError> {
        let kill_error = child.start_kill().err();
        if let Some(source) = &kill_error {
            tracing::error!(operation, ?args, %source, "failed to kill wpctl after incomplete wait");
        }
        if let Err(source) = child.reap().await {
            self.poisoned = true;
            tracing::error!(operation, ?args, %source, "failed to reap wpctl; runner is now unsafe");
            return Err(AudioError::Reap { operation, source });
        }
        Ok(kill_error)
    }
}

enum InterruptOutcome {
    Finished(io::Result<CommandOutput>),
    Timeout,
    Superseded(DesiredRequest),
}

struct TokioLauncher;

impl Launcher for TokioLauncher {
    type Child = TokioManagedChild;
    fn spawn(&mut self, args: &[&str]) -> io::Result<Self::Child> {
        let mut child = Command::new("wpctl")
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        Ok(TokioManagedChild {
            stdout: child.stdout.take(),
            stderr: child.stderr.take(),
            child,
        })
    }
}

struct TokioManagedChild {
    child: Child,
    stdout: Option<tokio::process::ChildStdout>,
    stderr: Option<tokio::process::ChildStderr>,
}

impl ManagedChild for TokioManagedChild {
    async fn wait(&mut self) -> io::Result<CommandOutput> {
        let stdout = self
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("wpctl stdout unavailable"))?;
        let stderr = self
            .stderr
            .take()
            .ok_or_else(|| io::Error::other("wpctl stderr unavailable"))?;
        let (status, stdout, stderr) =
            tokio::try_join!(self.child.wait(), read_all(stdout), read_all(stderr))?;
        Ok(CommandOutput {
            success: status.success(),
            status: status.code(),
            stdout,
            stderr,
        })
    }
    fn start_kill(&mut self) -> io::Result<()> {
        self.child.start_kill()
    }
    async fn reap(&mut self) -> io::Result<()> {
        self.child.wait().await.map(|_| ())
    }
}

async fn read_all(mut stream: impl AsyncRead + Unpin) -> io::Result<Vec<u8>> {
    let mut contents = Vec::new();
    stream.read_to_end(&mut contents).await?;
    Ok(contents)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use tokio::sync::oneshot;

    use super::*;

    enum Behavior {
        Ready {
            success: bool,
            stdout: Vec<u8>,
        },
        ReadyAndPublish {
            success: bool,
            stdout: Vec<u8>,
            desired: watch::Sender<Option<DesiredRequest>>,
            request: DesiredRequest,
        },
        Pending(Option<oneshot::Sender<()>>),
        Blocked {
            started: Option<oneshot::Sender<()>>,
            release: Option<oneshot::Receiver<()>>,
        },
        BlockedResult {
            started: Option<oneshot::Sender<()>>,
            release: Option<oneshot::Receiver<()>>,
            success: bool,
            stdout: Vec<u8>,
        },
        WaitError {
            reap_fails: bool,
        },
    }

    struct FakeLauncher {
        behaviors: VecDeque<Behavior>,
        argv: Arc<Mutex<Vec<Vec<String>>>>,
        events: Arc<Mutex<Vec<&'static str>>>,
        live: Arc<Mutex<bool>>,
    }

    struct FakeChild {
        behavior: Behavior,
        events: Arc<Mutex<Vec<&'static str>>>,
        live: Arc<Mutex<bool>>,
    }

    impl Launcher for FakeLauncher {
        type Child = FakeChild;
        fn spawn(&mut self, args: &[&str]) -> io::Result<Self::Child> {
            let mut live = self.live.lock().unwrap();
            if *live {
                return Err(io::Error::other("spawn before reap"));
            }
            *live = true;
            self.argv
                .lock()
                .unwrap()
                .push(args.iter().map(ToString::to_string).collect());
            self.events.lock().unwrap().push("spawn");
            Ok(FakeChild {
                behavior: self.behaviors.pop_front().expect("missing fake behavior"),
                events: Arc::clone(&self.events),
                live: Arc::clone(&self.live),
            })
        }
    }

    impl ManagedChild for FakeChild {
        async fn wait(&mut self) -> io::Result<CommandOutput> {
            self.events.lock().unwrap().push("wait");
            match &mut self.behavior {
                Behavior::Ready { success, stdout } => {
                    *self.live.lock().unwrap() = false;
                    Ok(CommandOutput {
                        success: *success,
                        status: success.then_some(0).or(Some(1)),
                        stdout: stdout.clone(),
                        stderr: if *success {
                            Vec::new()
                        } else {
                            b"failed".to_vec()
                        },
                    })
                }
                Behavior::ReadyAndPublish {
                    success,
                    stdout,
                    desired,
                    request,
                } => {
                    desired.send(Some(*request)).unwrap();
                    *self.live.lock().unwrap() = false;
                    Ok(CommandOutput {
                        success: *success,
                        status: success.then_some(0).or(Some(1)),
                        stdout: stdout.clone(),
                        stderr: if *success {
                            Vec::new()
                        } else {
                            b"failed".to_vec()
                        },
                    })
                }
                Behavior::Pending(signal) => {
                    if let Some(signal) = signal.take() {
                        signal.send(()).unwrap();
                    }
                    std::future::pending().await
                }
                Behavior::Blocked { started, release } => {
                    if let Some(started) = started.take() {
                        started.send(()).unwrap();
                    }
                    release.take().unwrap().await.unwrap();
                    *self.live.lock().unwrap() = false;
                    Ok(CommandOutput {
                        success: true,
                        status: Some(0),
                        stdout: Vec::new(),
                        stderr: Vec::new(),
                    })
                }
                Behavior::BlockedResult {
                    started,
                    release,
                    success,
                    stdout,
                } => {
                    started.take().unwrap().send(()).unwrap();
                    release.take().unwrap().await.unwrap();
                    *self.live.lock().unwrap() = false;
                    Ok(CommandOutput {
                        success: *success,
                        status: success.then_some(0).or(Some(1)),
                        stdout: stdout.clone(),
                        stderr: if *success {
                            Vec::new()
                        } else {
                            b"failed".to_vec()
                        },
                    })
                }
                Behavior::WaitError { .. } => Err(io::Error::other("wait failed")),
            }
        }
        fn start_kill(&mut self) -> io::Result<()> {
            self.events.lock().unwrap().push("kill");
            Ok(())
        }
        async fn reap(&mut self) -> io::Result<()> {
            self.events.lock().unwrap().push("reap");
            if matches!(self.behavior, Behavior::WaitError { reap_fails: true }) {
                return Err(io::Error::other("reap failed"));
            }
            *self.live.lock().unwrap() = false;
            Ok(())
        }
    }

    type FakeParts = (
        FakeLauncher,
        Arc<Mutex<Vec<Vec<String>>>>,
        Arc<Mutex<Vec<&'static str>>>,
    );

    fn fake(behaviors: impl IntoIterator<Item = Behavior>) -> FakeParts {
        let argv = Arc::new(Mutex::new(Vec::new()));
        let events = Arc::new(Mutex::new(Vec::new()));
        (
            FakeLauncher {
                behaviors: behaviors.into_iter().collect(),
                argv: Arc::clone(&argv),
                events: Arc::clone(&events),
                live: Arc::new(Mutex::new(false)),
            },
            argv,
            events,
        )
    }

    fn ready(stdout: &[u8]) -> Behavior {
        Behavior::Ready {
            success: true,
            stdout: stdout.to_vec(),
        }
    }

    fn failure() -> Behavior {
        Behavior::Ready {
            success: false,
            stdout: Vec::new(),
        }
    }

    fn worker(
        launcher: FakeLauncher,
        initial: Option<DesiredRequest>,
    ) -> (
        Worker<FakeLauncher>,
        watch::Sender<Option<DesiredRequest>>,
        mpsc::UnboundedReceiver<AudioAck>,
        watch::Sender<bool>,
    ) {
        let (desired_tx, desired_rx) = watch::channel(initial);
        let (ack_tx, ack_rx) = mpsc::unbounded_channel();
        let (stop_tx, stop_rx) = watch::channel(false);
        (
            Worker::new(launcher, desired_rx, ack_tx, stop_rx),
            desired_tx,
            ack_rx,
            stop_tx,
        )
    }

    #[test]
    fn strict_parsers_are_retained() {
        assert_eq!(
            parse_source_id(b"id 57, type PipeWire:Interface:Node\n"),
            Ok(SourceId(57))
        );
        assert!(parse_source_id(b"id 0, type PipeWire:Interface:Node").is_err());
        assert_eq!(parse_default_mute(b"Volume: 0.4 [MUTED]\n"), Ok(true));
        assert_eq!(parse_default_mute(b"Volume: 0.4\n"), Ok(false));
        assert!(parse_default_mute(b"Volume: 0.4\nextra").is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn timeout_kills_and_reaps_and_reap_failure_poisons() {
        let (signal_tx, signal_rx) = oneshot::channel();
        let (launcher, _, events) = fake([Behavior::Pending(Some(signal_tx))]);
        let mut runner = CommandRunner::new(launcher);
        let task = tokio::spawn(async move {
            let result = runner.execute("timeout", &["arg"]).await;
            (runner, result)
        });
        signal_rx.await.unwrap();
        tokio::time::advance(WPCTL_TIMEOUT).await;
        let (_, result) = task.await.unwrap();
        assert!(matches!(result, Err(AudioError::Timeout { .. })));
        assert_eq!(*events.lock().unwrap(), ["spawn", "wait", "kill", "reap"]);

        let (launcher, _, _) = fake([Behavior::WaitError { reap_fails: true }]);
        let mut runner = CommandRunner::new(launcher);
        assert!(matches!(
            runner.execute("bad", &["arg"]).await,
            Err(AudioError::Reap { .. })
        ));
        assert!(matches!(
            runner.execute("later", &["arg"]).await,
            Err(AudioError::Poisoned { .. })
        ));
    }

    #[tokio::test]
    async fn supersession_during_resolution_kills_reaps_and_suppresses_old_ack() {
        let (signal_tx, signal_rx) = oneshot::channel();
        let (launcher, argv, events) = fake([Behavior::Pending(Some(signal_tx)), ready(b"")]);
        let (worker, desired, mut acks, stop) = worker(
            launcher,
            Some(DesiredRequest {
                revision: 1,
                state: DesiredAudio::PttTalking,
            }),
        );
        let task = tokio::spawn(worker.run());
        signal_rx.await.unwrap();
        desired
            .send(Some(DesiredRequest {
                revision: 2,
                state: DesiredAudio::Open,
            }))
            .unwrap();
        assert_eq!(
            acks.recv().await.unwrap(),
            AudioAck {
                revision: 2,
                outcome: AckOutcome::Applied
            }
        );
        stop.send(true).unwrap();
        task.await.unwrap().unwrap();
        assert_eq!(
            *events.lock().unwrap(),
            ["spawn", "wait", "kill", "reap", "spawn", "wait"]
        );
        assert_eq!(argv.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn supersession_during_unmute_and_ptt_reentry_supersede_open() {
        let (unmute_tx, unmute_rx) = oneshot::channel();
        let (open_tx, open_rx) = oneshot::channel();
        let (launcher, argv, events) = fake([
            ready(b"id 42, type PipeWire:Interface:Node\n"),
            Behavior::Pending(Some(unmute_tx)),
            Behavior::Pending(Some(open_tx)),
            ready(b"id 42, type PipeWire:Interface:Node\n"),
            ready(b""),
        ]);
        let (worker, desired, mut acks, stop) = worker(
            launcher,
            Some(DesiredRequest {
                revision: 1,
                state: DesiredAudio::PttTalking,
            }),
        );
        let task = tokio::spawn(worker.run());
        unmute_rx.await.unwrap();
        desired
            .send(Some(DesiredRequest {
                revision: 2,
                state: DesiredAudio::Open,
            }))
            .unwrap();
        open_rx.await.unwrap();
        desired
            .send(Some(DesiredRequest {
                revision: 3,
                state: DesiredAudio::PttTalking,
            }))
            .unwrap();
        assert_eq!(
            acks.recv().await.unwrap(),
            AudioAck {
                revision: 3,
                outcome: AckOutcome::Applied,
            }
        );
        assert!(acks.try_recv().is_err());
        stop.send(true).unwrap();
        task.await.unwrap().unwrap();
        assert_eq!(
            events
                .lock()
                .unwrap()
                .iter()
                .filter(|event| **event == "kill")
                .count(),
            2
        );
        assert_eq!(
            &argv.lock().unwrap()[3..],
            [
                vec!["inspect".to_owned(), "@DEFAULT_AUDIO_SOURCE@".to_owned(),],
                vec!["set-mute".to_owned(), "42".to_owned(), "0".to_owned()],
            ]
        );
    }

    #[tokio::test]
    async fn source_is_remembered_before_failed_unmute_and_idle_mutes_both_targets() {
        let (launcher, argv, _) = fake([
            ready(b"id 42, type PipeWire:Interface:Node\n"),
            failure(),
            failure(),
            ready(b""),
        ]);
        let (worker, desired, mut acks, stop) = worker(
            launcher,
            Some(DesiredRequest {
                revision: 1,
                state: DesiredAudio::PttTalking,
            }),
        );
        let task = tokio::spawn(worker.run());
        assert_eq!(
            acks.recv().await.unwrap().outcome,
            AckOutcome::HandledFailure
        );
        desired
            .send(Some(DesiredRequest {
                revision: 2,
                state: DesiredAudio::PttIdle,
            }))
            .unwrap();
        assert_eq!(acks.recv().await.unwrap().revision, 2);
        stop.send(true).unwrap();
        task.await.unwrap().unwrap();
        assert_eq!(
            &argv.lock().unwrap()[2..4],
            [
                vec!["set-mute".to_owned(), "42".to_owned(), "1".to_owned()],
                vec![
                    "set-mute".to_owned(),
                    "@DEFAULT_AUDIO_SOURCE@".to_owned(),
                    "1".to_owned()
                ],
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn idle_failure_is_acknowledged_then_retried_on_tick() {
        let (started_tx, started_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let (launcher, argv, _) = fake([
            failure(),
            Behavior::BlockedResult {
                started: Some(started_tx),
                release: Some(release_rx),
                success: true,
                stdout: Vec::new(),
            },
        ]);
        let (worker, _, mut acks, stop) = worker(
            launcher,
            Some(DesiredRequest {
                revision: 1,
                state: DesiredAudio::PttIdle,
            }),
        );
        let task = tokio::spawn(worker.run());
        assert_eq!(
            acks.recv().await.unwrap().outcome,
            AckOutcome::HandledFailure
        );
        tokio::time::advance(Duration::from_secs(1)).await;
        started_rx.await.unwrap();
        assert_eq!(argv.lock().unwrap().len(), 2);
        release_tx.send(()).unwrap();
        stop.send(true).unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn reconcile_query_and_mute_failures_retry_on_later_ticks() {
        fn blocked_result(
            success: bool,
            stdout: &[u8],
        ) -> (Behavior, oneshot::Receiver<()>, oneshot::Sender<()>) {
            let (started_tx, started_rx) = oneshot::channel();
            let (release_tx, release_rx) = oneshot::channel();
            (
                Behavior::BlockedResult {
                    started: Some(started_tx),
                    release: Some(release_rx),
                    success,
                    stdout: stdout.to_vec(),
                },
                started_rx,
                release_tx,
            )
        }
        let (query_failure, query_failure_started, query_failure_release) =
            blocked_result(false, b"");
        let (unmuted_query, unmuted_query_started, unmuted_query_release) =
            blocked_result(true, b"Volume: 1.0\n");
        let (mute_failure, mute_failure_started, mute_failure_release) = blocked_result(false, b"");
        let (muted_query, muted_query_started, muted_query_release) =
            blocked_result(true, b"Volume: 1.0 [MUTED]\n");
        let (launcher, argv, _) = fake([
            ready(b""),
            query_failure,
            unmuted_query,
            mute_failure,
            muted_query,
        ]);
        let (worker, _, mut acks, stop) = worker(
            launcher,
            Some(DesiredRequest {
                revision: 1,
                state: DesiredAudio::PttIdle,
            }),
        );
        let task = tokio::spawn(worker.run());
        acks.recv().await.unwrap();
        tokio::time::advance(Duration::from_secs(1)).await;
        query_failure_started.await.unwrap();
        query_failure_release.send(()).unwrap();
        tokio::time::advance(Duration::from_secs(1)).await;
        unmuted_query_started.await.unwrap();
        unmuted_query_release.send(()).unwrap();
        mute_failure_started.await.unwrap();
        mute_failure_release.send(()).unwrap();
        tokio::time::advance(Duration::from_secs(1)).await;
        muted_query_started.await.unwrap();
        muted_query_release.send(()).unwrap();
        assert_eq!(argv.lock().unwrap().len(), 5);
        stop.send(true).unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn stale_query_to_mute_is_suppressed() {
        let (desired, desired_rx) = watch::channel(Some(DesiredRequest {
            revision: 1,
            state: DesiredAudio::PttIdle,
        }));
        let (launcher, argv, _) = fake([Behavior::ReadyAndPublish {
            success: true,
            stdout: b"Volume: 1.0\n".to_vec(),
            desired: desired.clone(),
            request: DesiredRequest {
                revision: 2,
                state: DesiredAudio::Open,
            },
        }]);
        let (ack_tx, _acks) = mpsc::unbounded_channel();
        let (_stop_tx, stop_rx) = watch::channel(false);
        let mut worker = Worker::new(launcher, desired_rx, ack_tx, stop_rx);
        worker.handled = Some(DesiredRequest {
            revision: 1,
            state: DesiredAudio::PttIdle,
        });
        worker.desired.borrow_and_update();
        let newer = worker.reconcile_idle().await.unwrap();
        assert_eq!(newer.unwrap().revision, 2);
        assert_eq!(argv.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn stale_success_and_failure_acknowledgements_are_suppressed_and_ordered() {
        let (desired, desired_rx) = watch::channel(Some(DesiredRequest {
            revision: 1,
            state: DesiredAudio::Open,
        }));
        let (launcher, _, _) = fake([
            Behavior::ReadyAndPublish {
                success: false,
                stdout: Vec::new(),
                desired: desired.clone(),
                request: DesiredRequest {
                    revision: 2,
                    state: DesiredAudio::Open,
                },
            },
            Behavior::ReadyAndPublish {
                success: true,
                stdout: Vec::new(),
                desired: desired.clone(),
                request: DesiredRequest {
                    revision: 3,
                    state: DesiredAudio::PttIdle,
                },
            },
            ready(b""),
        ]);
        let (ack_tx, mut acks) = mpsc::unbounded_channel();
        let (stop, stop_rx) = watch::channel(false);
        let worker = Worker::new(launcher, desired_rx, ack_tx, stop_rx);
        let task = tokio::spawn(worker.run());
        assert_eq!(
            acks.recv().await.unwrap(),
            AudioAck {
                revision: 3,
                outcome: AckOutcome::Applied,
            }
        );
        assert!(acks.try_recv().is_err());
        stop.send(true).unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn graceful_stop_and_desired_closure_do_not_cancel_active_work() {
        let (started_tx, started_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let (launcher, _, events) = fake([Behavior::Blocked {
            started: Some(started_tx),
            release: Some(release_rx),
        }]);
        let (worker, desired, mut acks, stop) = worker(
            launcher,
            Some(DesiredRequest {
                revision: 1,
                state: DesiredAudio::Open,
            }),
        );
        let task = tokio::spawn(worker.run());
        started_rx.await.unwrap();
        drop(desired);
        stop.send(true).unwrap();
        assert_eq!(*events.lock().unwrap(), ["spawn", "wait"]);
        release_tx.send(()).unwrap();
        assert_eq!(acks.recv().await.unwrap().revision, 1);
        task.await.unwrap().unwrap();
        assert_eq!(*events.lock().unwrap(), ["spawn", "wait"]);
    }

    #[tokio::test]
    async fn talking_and_open_failures_ack_once_without_automatic_retry_and_stop_waits() {
        let (launcher, argv, _) = fake([failure(), failure()]);
        let (worker, desired, mut acks, stop) = worker(
            launcher,
            Some(DesiredRequest {
                revision: 1,
                state: DesiredAudio::PttTalking,
            }),
        );
        let task = tokio::spawn(worker.run());
        assert_eq!(
            acks.recv().await.unwrap(),
            AudioAck {
                revision: 1,
                outcome: AckOutcome::HandledFailure
            }
        );
        assert_eq!(argv.lock().unwrap().len(), 1);
        desired
            .send(Some(DesiredRequest {
                revision: 2,
                state: DesiredAudio::Open,
            }))
            .unwrap();
        assert_eq!(
            acks.recv().await.unwrap(),
            AudioAck {
                revision: 2,
                outcome: AckOutcome::HandledFailure
            }
        );
        stop.send(true).unwrap();
        task.await.unwrap().unwrap();
        assert_eq!(argv.lock().unwrap().len(), 2);
    }
}
