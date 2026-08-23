use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::sync::Arc;

use clap::Parser;
use pttd::audio::{AudioAck, AudioWorker, DesiredRequest, WorkerError};
use pttd::config;
use pttd::input::{self, NodeClaims, ReaderConfig, ReaderError};
use pttd::state::{
    ControlEvent, DesiredAudio, DeviceId, Policy, TransitionAction, TransitionEpoch,
};
use tokio::sync::{mpsc, watch};
use tokio::task::{Id, JoinError, JoinHandle, JoinSet};

const INPUT_CHANNEL_CAPACITY: usize = 64;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let config_path = config::Args::parse().config_path()?;
    let config = config::load(&config_path)?;

    // Creating Unix signal streams installs both handlers before startup audio is published.
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;

    let (desired_tx, desired_rx) = watch::channel(None);
    let (ack_tx, mut ack_rx) = mpsc::unbounded_channel();
    let (audio_stop_tx, audio_stop_rx) = watch::channel(false);
    let mut audio_task = tokio::spawn(AudioWorker::new(desired_rx, ack_tx, audio_stop_rx).run());

    let device_count = config.input.devices.len();
    let mut epoch_senders = Vec::with_capacity(device_count);
    let mut epoch_receivers = Vec::with_capacity(device_count);
    for _ in 0..device_count {
        let (sender, receiver) = watch::channel(None);
        epoch_senders.push(sender);
        epoch_receivers.push(receiver);
    }
    let mut controller = Controller::new(device_count, desired_tx, epoch_senders);
    let startup = controller.handle(ControlEvent::Startup)?;
    let startup_revision = startup
        .published
        .ok_or_else(|| AppError::new("policy did not request startup idle"))?;

    let mut startup_signalled = false;
    let mut startup_failure = None;
    let mut acknowledgements_open = true;
    while !controller.target_complete(startup_revision) {
        tokio::select! {
            biased;
            signal = sigint.recv(), if !startup_signalled => {
                startup_signalled = true;
                if signal.is_none() {
                    startup_failure = Some(AppError::new("SIGINT listener closed"));
                }
            }
            signal = sigterm.recv(), if !startup_signalled => {
                startup_signalled = true;
                if signal.is_none() {
                    startup_failure = Some(AppError::new("SIGTERM listener closed"));
                }
            }
            result = &mut audio_task => {
                return Err(audio_exit_before("startup idle acknowledgement", result).into());
            }
            acknowledgement = ack_rx.recv(), if acknowledgements_open => match acknowledgement {
                Some(acknowledgement) => controller.record_ack(acknowledgement),
                None => acknowledgements_open = false,
            },
        }
    }
    if startup_signalled {
        request_audio_stop(&audio_stop_tx);
        join_audio(audio_task).await?;
        return match startup_failure {
            Some(error) => Err(error.into()),
            None => Ok(()),
        };
    }

    let (events_tx, mut events_rx) = mpsc::channel(INPUT_CHANNEL_CAPACITY);
    let (reader_stop_tx, reader_stop_rx) = watch::channel(false);
    let claims = Arc::new(NodeClaims::new());
    let mut readers = ReaderSupervisor::new();
    for (index, (path, epochs)) in config
        .input
        .devices
        .into_iter()
        .zip(epoch_receivers)
        .enumerate()
    {
        let device = DeviceId(index);
        let reader = ReaderConfig {
            device,
            path,
            ptt_key: config.input.ptt_key,
            toggle_key: config.input.toggle_key,
        };
        readers.spawn(
            device,
            input::run_reader(
                reader,
                Arc::clone(&claims),
                events_tx.clone(),
                epochs,
                reader_stop_rx.clone(),
            ),
        );
    }
    drop(events_tx);

    let exit = loop {
        tokio::select! {
            event = events_rx.recv() => match event {
                Some(event) => {
                    if let Err(error) = controller.handle(event) {
                        break RuntimeExit::Fatal(AppError::new(error.to_string()));
                    }
                }
                None => break RuntimeExit::Fatal(AppError::new("input event channel closed unexpectedly")),
            },
            acknowledgement = ack_rx.recv(), if acknowledgements_open => match acknowledgement {
                Some(acknowledgement) => controller.record_ack(acknowledgement),
                None => acknowledgements_open = false,
            },
            result = &mut audio_task => {
                break RuntimeExit::Audio(audio_exit_before("controller shutdown", result));
            }
            signal = sigint.recv() => {
                break match signal {
                    Some(()) => RuntimeExit::Signal,
                    None => RuntimeExit::Fatal(AppError::new("SIGINT listener closed")),
                };
            }
            signal = sigterm.recv() => {
                break match signal {
                    Some(()) => RuntimeExit::Signal,
                    None => RuntimeExit::Fatal(AppError::new("SIGTERM listener closed")),
                };
            }
            exit = readers.next(), if !readers.is_empty() => {
                let Some(exit) = exit else {
                    break RuntimeExit::Fatal(AppError::new("reader supervisor lost task identity"));
                };
                let device = exit.device();
                let loss = controller.handle(ControlEvent::DeviceLost(device));
                let loss_revision = match loss {
                    Ok(effect) => effect.published,
                    Err(error) => return Err(error.into()),
                };
                break RuntimeExit::Reader { exit, loss_revision };
            }
        }
    };

    match exit {
        RuntimeExit::Audio(error) => {
            stop_and_join_readers(&reader_stop_tx, &mut readers).await;
            Err(error.into())
        }
        RuntimeExit::Signal if controller.mode() == ControllerMode::Open => {
            let target = controller
                .latest
                .filter(|request| request.state == DesiredAudio::Open)
                .ok_or_else(|| AppError::new("OPEN shutdown has no published OPEN request"))?
                .revision;
            stop_and_join_readers(&reader_stop_tx, &mut readers).await;
            if let Err(error) = wait_for_audio_target(
                target,
                &mut controller,
                &mut ack_rx,
                &mut audio_task,
                None,
                &mut events_rx,
                &mut readers,
            )
            .await
            {
                return Err(error.into());
            }
            request_audio_stop(&audio_stop_tx);
            join_audio(audio_task).await
        }
        RuntimeExit::Signal => {
            let target = controller.publish(DesiredAudio::PttIdle)?;
            if let Err(error) = wait_for_audio_target(
                target,
                &mut controller,
                &mut ack_rx,
                &mut audio_task,
                Some(&mut acknowledgements_open),
                &mut events_rx,
                &mut readers,
            )
            .await
            {
                stop_and_join_readers(&reader_stop_tx, &mut readers).await;
                return Err(error.into());
            }
            stop_and_join_readers(&reader_stop_tx, &mut readers).await;
            request_audio_stop(&audio_stop_tx);
            join_audio(audio_task).await
        }
        RuntimeExit::Reader {
            exit,
            loss_revision,
        } => {
            let reason = AppError::new(exit.describe());
            if let Err(error) = finish_fatal_cleanup(
                &mut controller,
                loss_revision,
                FatalAudio {
                    acknowledgements: &mut ack_rx,
                    task: audio_task,
                    stop: &audio_stop_tx,
                    acknowledgements_open: &mut acknowledgements_open,
                },
                FatalReaders {
                    events: &mut events_rx,
                    tasks: &mut readers,
                    stop: &reader_stop_tx,
                },
            )
            .await
            {
                tracing::error!(%error, "fatal cleanup failed");
            }
            Err(reason.into())
        }
        RuntimeExit::Fatal(error) => {
            if let Err(cleanup_error) = finish_fatal_cleanup(
                &mut controller,
                None,
                FatalAudio {
                    acknowledgements: &mut ack_rx,
                    task: audio_task,
                    stop: &audio_stop_tx,
                    acknowledgements_open: &mut acknowledgements_open,
                },
                FatalReaders {
                    events: &mut events_rx,
                    tasks: &mut readers,
                    stop: &reader_stop_tx,
                },
            )
            .await
            {
                tracing::error!(error = %cleanup_error, "fatal cleanup failed");
            }
            Err(error.into())
        }
    }
}

struct FatalAudio<'a> {
    acknowledgements: &'a mut mpsc::UnboundedReceiver<AudioAck>,
    task: JoinHandle<Result<(), WorkerError>>,
    stop: &'a watch::Sender<bool>,
    acknowledgements_open: &'a mut bool,
}

struct FatalReaders<'a> {
    events: &'a mut mpsc::Receiver<ControlEvent>,
    tasks: &'a mut ReaderSupervisor,
    stop: &'a watch::Sender<bool>,
}

async fn finish_fatal_cleanup(
    controller: &mut Controller,
    loss_revision: Option<u64>,
    audio: FatalAudio<'_>,
    readers: FatalReaders<'_>,
) -> Result<(), AppError> {
    let (mode, target) = controller.fatal_cleanup_target(loss_revision)?;
    if mode == ControllerMode::Open {
        stop_and_join_readers(readers.stop, readers.tasks).await;
    }
    let mut audio_task = audio.task;
    if let Err(error) = wait_for_audio_target(
        target,
        controller,
        audio.acknowledgements,
        &mut audio_task,
        Some(audio.acknowledgements_open),
        readers.events,
        readers.tasks,
    )
    .await
    {
        if mode == ControllerMode::Ptt {
            stop_and_join_readers(readers.stop, readers.tasks).await;
        }
        return Err(error);
    }
    if mode == ControllerMode::Ptt {
        stop_and_join_readers(readers.stop, readers.tasks).await;
    }
    request_audio_stop(audio.stop);
    join_audio(audio_task).await.map_err(|error| {
        AppError::new(format!("audio worker failed during fatal cleanup: {error}"))
    })
}

async fn wait_for_audio_target(
    target: u64,
    controller: &mut Controller,
    acknowledgements: &mut mpsc::UnboundedReceiver<AudioAck>,
    audio_task: &mut JoinHandle<Result<(), WorkerError>>,
    acknowledgements_open: Option<&mut bool>,
    events: &mut mpsc::Receiver<ControlEvent>,
    readers: &mut ReaderSupervisor,
) -> Result<(), AppError> {
    let mut ack_open = acknowledgements_open.as_ref().is_none_or(|open| **open);
    let mut events_open = true;
    while !controller.target_complete(target) {
        tokio::select! {
            acknowledgement = acknowledgements.recv(), if ack_open => match acknowledgement {
                Some(acknowledgement) => controller.record_ack(acknowledgement),
                None => ack_open = false,
            },
            result = &mut *audio_task => {
                return Err(audio_exit_before("required audio acknowledgement", result));
            }
            event = events.recv(), if events_open => {
                match event {
                    Some(event) => controller.observe_frozen(event),
                    None => events_open = false,
                }
            }
            exit = readers.next(), if !readers.is_empty() => {
                if let Some(exit) = exit {
                    controller.observe_frozen(ControlEvent::DeviceLost(exit.device()));
                    tracing::error!(error = %exit.describe(), "reader terminated during audio shutdown hold");
                }
            }
        }
    }
    if let Some(open) = acknowledgements_open {
        *open = ack_open;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ControllerMode {
    Ptt,
    Open,
}

#[derive(Debug)]
struct PendingBarrier {
    epoch: TransitionEpoch,
    awaiting: HashSet<usize>,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct EventEffect {
    published: Option<u64>,
    began_epoch: Option<TransitionEpoch>,
    completed_epoch: Option<TransitionEpoch>,
}

struct Controller {
    policy: Policy,
    desired: watch::Sender<Option<DesiredRequest>>,
    epochs: Vec<watch::Sender<Option<TransitionEpoch>>>,
    recovered: HashSet<usize>,
    pending: Option<PendingBarrier>,
    next_revision: u64,
    epoch: u64,
    latest: Option<DesiredRequest>,
    last_acknowledged: Option<u64>,
}

impl Controller {
    fn new(
        device_count: usize,
        desired: watch::Sender<Option<DesiredRequest>>,
        epochs: Vec<watch::Sender<Option<TransitionEpoch>>>,
    ) -> Self {
        Self {
            policy: Policy::new(device_count),
            desired,
            epochs,
            recovered: HashSet::new(),
            pending: None,
            next_revision: 0,
            epoch: 0,
            latest: None,
            last_acknowledged: None,
        }
    }

    fn handle(&mut self, event: ControlEvent) -> Result<EventEffect, AppError> {
        let outcome = self.policy.handle(event);
        match event {
            ControlEvent::DeviceRecovered { device, .. } => {
                self.recovered.insert(device.0);
            }
            ControlEvent::DeviceLost(device) | ControlEvent::SynchronizationLost(device) => {
                self.recovered.remove(&device.0);
            }
            _ => {}
        }

        let mut effect = EventEffect::default();
        if let Some(desired) = outcome.desired_audio {
            effect.published = Some(self.publish(desired)?);
        }
        if outcome.transition == Some(TransitionAction::StartTransitionBarrier) {
            let epoch = self.begin_barrier()?;
            effect.began_epoch = Some(epoch);
            if self.pending.is_none() {
                effect.completed_epoch = Some(epoch);
            }
            return Ok(effect);
        }

        let participant = match event {
            ControlEvent::KeySnapshot { device, epoch, .. }
                if self
                    .pending
                    .as_ref()
                    .is_some_and(|pending| pending.epoch == epoch) =>
            {
                Some(device)
            }
            ControlEvent::DeviceLost(device) | ControlEvent::SynchronizationLost(device) => {
                Some(device)
            }
            _ => None,
        };
        if let Some(device) = participant
            && let Some(pending) = &mut self.pending
        {
            pending.awaiting.remove(&device.0);
            if pending.awaiting.is_empty() {
                let epoch = pending.epoch;
                self.policy.handle(ControlEvent::CompleteBarrier(epoch));
                self.pending = None;
                effect.completed_epoch = Some(epoch);
            }
        }
        Ok(effect)
    }

    fn observe_frozen(&mut self, event: ControlEvent) {
        let _ = self.policy.handle(event);
        match event {
            ControlEvent::DeviceRecovered { device, .. } => {
                self.recovered.insert(device.0);
            }
            ControlEvent::DeviceLost(device) | ControlEvent::SynchronizationLost(device) => {
                self.recovered.remove(&device.0);
                if let Some(pending) = &mut self.pending {
                    pending.awaiting.remove(&device.0);
                }
            }
            _ => {}
        }
    }

    fn publish(&mut self, state: DesiredAudio) -> Result<u64, AppError> {
        self.next_revision = self
            .next_revision
            .checked_add(1)
            .ok_or_else(|| AppError::new("audio revision overflow"))?;
        let request = DesiredRequest {
            revision: self.next_revision,
            state,
        };
        self.desired.send_replace(Some(request));
        self.latest = Some(request);
        Ok(request.revision)
    }

    fn begin_barrier(&mut self) -> Result<TransitionEpoch, AppError> {
        self.epoch = self
            .epoch
            .checked_add(1)
            .ok_or_else(|| AppError::new("transition epoch overflow"))?;
        let epoch = TransitionEpoch(self.epoch);
        let awaiting = self.recovered.clone();
        self.policy.handle(ControlEvent::BeginBarrier(epoch));
        for device in &awaiting {
            if let Some(sender) = self.epochs.get(*device) {
                sender.send_replace(Some(epoch));
            }
        }
        if awaiting.is_empty() {
            self.policy.handle(ControlEvent::CompleteBarrier(epoch));
            self.pending = None;
        } else {
            self.pending = Some(PendingBarrier { epoch, awaiting });
        }
        Ok(epoch)
    }

    fn record_ack(&mut self, acknowledgement: AudioAck) {
        if self
            .last_acknowledged
            .is_none_or(|revision| acknowledgement.revision > revision)
        {
            self.last_acknowledged = Some(acknowledgement.revision);
        }
    }

    fn target_complete(&self, target: u64) -> bool {
        self.last_acknowledged
            .is_some_and(|revision| revision >= target)
    }

    fn mode(&self) -> ControllerMode {
        match self.latest.map(|request| request.state) {
            Some(DesiredAudio::Open) => ControllerMode::Open,
            _ => ControllerMode::Ptt,
        }
    }

    fn fatal_cleanup_target(
        &mut self,
        loss_revision: Option<u64>,
    ) -> Result<(ControllerMode, u64), AppError> {
        let mode = self.mode();
        let target = match mode {
            ControllerMode::Open => {
                self.latest
                    .filter(|request| request.state == DesiredAudio::Open)
                    .ok_or_else(|| {
                        AppError::new("OPEN fatal cleanup has no published OPEN request")
                    })?
                    .revision
            }
            ControllerMode::Ptt => match loss_revision {
                Some(revision)
                    if self.latest.is_some_and(|request| {
                        request.revision == revision && request.state == DesiredAudio::PttIdle
                    }) =>
                {
                    revision
                }
                _ => self.publish(DesiredAudio::PttIdle)?,
            },
        };
        Ok((mode, target))
    }
}

struct ReaderSupervisor {
    tasks: JoinSet<Result<DeviceId, ReaderError>>,
    identities: HashMap<Id, DeviceId>,
}

impl ReaderSupervisor {
    fn new() -> Self {
        Self {
            tasks: JoinSet::new(),
            identities: HashMap::new(),
        }
    }

    fn spawn<F>(&mut self, device: DeviceId, task: F)
    where
        F: Future<Output = Result<DeviceId, ReaderError>> + Send + 'static,
    {
        let handle = self.tasks.spawn(task);
        self.identities.insert(handle.id(), device);
    }

    fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    async fn next(&mut self) -> Option<ReaderExit> {
        match self.tasks.join_next_with_id().await? {
            Ok((id, result)) => {
                let expected = self.identities.remove(&id).unwrap_or(DeviceId(usize::MAX));
                Some(ReaderExit::Returned { expected, result })
            }
            Err(error) => {
                let device = self
                    .identities
                    .remove(&error.id())
                    .unwrap_or(DeviceId(usize::MAX));
                Some(ReaderExit::Join { device, error })
            }
        }
    }

    async fn join_requested(&mut self) {
        while let Some(exit) = self.next().await {
            if let ReaderExit::Returned {
                result: Err(error), ..
            } = exit
            {
                tracing::error!(%error, "reader failed during requested shutdown");
            } else if let ReaderExit::Join { error, .. } = exit {
                tracing::error!(%error, "reader task failed during requested shutdown");
            }
        }
    }
}

enum ReaderExit {
    Returned {
        expected: DeviceId,
        result: Result<DeviceId, ReaderError>,
    },
    Join {
        device: DeviceId,
        error: JoinError,
    },
}

impl ReaderExit {
    fn device(&self) -> DeviceId {
        match self {
            Self::Returned { expected, .. } => *expected,
            Self::Join { device, .. } => *device,
        }
    }

    fn describe(&self) -> String {
        match self {
            Self::Returned {
                expected,
                result: Ok(returned),
            } => format!(
                "reader {} terminated unexpectedly (returned identity {})",
                expected.0, returned.0
            ),
            Self::Returned {
                expected,
                result: Err(error),
            } => format!("reader {} terminated unexpectedly: {error}", expected.0),
            Self::Join { device, error } => {
                format!("reader {} task terminated unexpectedly: {error}", device.0)
            }
        }
    }
}

enum RuntimeExit {
    Signal,
    Reader {
        exit: ReaderExit,
        loss_revision: Option<u64>,
    },
    Audio(AppError),
    Fatal(AppError),
}

#[derive(Debug)]
struct AppError(String);

impl AppError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for AppError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for AppError {}

fn request_reader_stop(stop: &watch::Sender<bool>) {
    stop.send_replace(true);
}

async fn stop_and_join_readers(stop: &watch::Sender<bool>, readers: &mut ReaderSupervisor) {
    request_reader_stop(stop);
    readers.join_requested().await;
}

fn request_audio_stop(stop: &watch::Sender<bool>) {
    stop.send_replace(true);
}

fn audio_exit_before(target: &str, result: Result<Result<(), WorkerError>, JoinError>) -> AppError {
    match result {
        Ok(Ok(())) => AppError::new(format!("audio worker stopped before {target}")),
        Ok(Err(error)) => AppError::new(format!("audio worker failed before {target}: {error}")),
        Err(error) => AppError::new(format!("audio worker task failed before {target}: {error}")),
    }
}

async fn join_audio(task: JoinHandle<Result<(), WorkerError>>) -> Result<(), Box<dyn Error>> {
    match task.await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(error.into()),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pttd::audio::AckOutcome;
    use pttd::state::EventTimestamp;
    use tokio::sync::oneshot;

    fn timestamp(value: i64) -> EventTimestamp {
        EventTimestamp::new(value, 0)
    }

    type TestController = (
        Controller,
        watch::Receiver<Option<DesiredRequest>>,
        Vec<watch::Receiver<Option<TransitionEpoch>>>,
    );

    fn controller(devices: usize) -> TestController {
        let (desired_tx, desired_rx) = watch::channel(None);
        let mut senders = Vec::new();
        let mut receivers = Vec::new();
        for _ in 0..devices {
            let (sender, receiver) = watch::channel(None);
            senders.push(sender);
            receivers.push(receiver);
        }
        (
            Controller::new(devices, desired_tx, senders),
            desired_rx,
            receivers,
        )
    }

    fn recover(controller: &mut Controller, device: DeviceId) {
        controller
            .handle(ControlEvent::DeviceRecovered {
                device,
                ptt_held: false,
                toggle_held: false,
            })
            .unwrap();
    }

    #[test]
    fn barrier_excludes_unavailable_ignores_stale_and_replaces_without_blocking() {
        let (mut controller, mut desired, mut epochs) = controller(2);
        recover(&mut controller, DeviceId(0));
        let first = controller
            .handle(ControlEvent::ToggleDown {
                device: DeviceId(0),
                timestamp: timestamp(10),
            })
            .unwrap();
        assert_eq!(first.published, Some(1));
        assert_eq!(
            desired.borrow_and_update().unwrap().state,
            DesiredAudio::Open
        );
        assert_eq!(first.began_epoch, Some(TransitionEpoch(1)));
        assert_eq!(*epochs[0].borrow_and_update(), Some(TransitionEpoch(1)));
        assert_eq!(*epochs[1].borrow_and_update(), None);
        assert_eq!(controller.pending.as_ref().unwrap().awaiting.len(), 1);

        controller
            .handle(ControlEvent::KeySnapshot {
                device: DeviceId(0),
                epoch: TransitionEpoch(0),
                ptt_held: false,
                toggle_held: false,
            })
            .unwrap();
        assert_eq!(
            controller.pending.as_ref().unwrap().epoch,
            TransitionEpoch(1)
        );

        controller
            .handle(ControlEvent::ToggleUp {
                device: DeviceId(0),
                timestamp: timestamp(11),
            })
            .unwrap();
        let second = controller
            .handle(ControlEvent::ToggleDown {
                device: DeviceId(0),
                timestamp: timestamp(20),
            })
            .unwrap();
        assert_eq!(second.published, Some(2));
        assert_eq!(second.began_epoch, Some(TransitionEpoch(2)));
        assert_eq!(*epochs[0].borrow_and_update(), Some(TransitionEpoch(2)));
        assert_eq!(
            controller.pending.as_ref().unwrap().epoch,
            TransitionEpoch(2)
        );
        controller
            .handle(ControlEvent::KeySnapshot {
                device: DeviceId(0),
                epoch: TransitionEpoch(1),
                ptt_held: true,
                toggle_held: false,
            })
            .unwrap();
        assert!(controller.pending.is_some());
        let completion = controller
            .handle(ControlEvent::KeySnapshot {
                device: DeviceId(0),
                epoch: TransitionEpoch(2),
                ptt_held: false,
                toggle_held: false,
            })
            .unwrap();
        assert_eq!(completion.completed_epoch, Some(TransitionEpoch(2)));
    }

    #[test]
    fn timestamped_event_is_forwarded_unchanged_to_policy() {
        let (mut controller, mut desired, _) = controller(1);
        recover(&mut controller, DeviceId(0));
        let event = ControlEvent::PttDown {
            device: DeviceId(0),
            timestamp: timestamp(37),
        };
        assert_eq!(controller.handle(event).unwrap().published, Some(1));
        assert_eq!(
            desired.borrow_and_update().unwrap().state,
            DesiredAudio::PttTalking
        );
    }

    #[tokio::test]
    async fn startup_gate_does_not_start_readers_before_idle_completion() {
        let (mut controller, mut desired, _) = controller(1);
        let target = controller
            .handle(ControlEvent::Startup)
            .unwrap()
            .published
            .unwrap();
        assert_eq!(
            desired.borrow_and_update().unwrap().state,
            DesiredAudio::PttIdle
        );
        let (ack_tx, mut ack_rx) = mpsc::unbounded_channel();
        let (reader_started_tx, mut reader_started_rx) = mpsc::unbounded_channel();
        let gate = tokio::spawn(async move {
            while !controller.target_complete(target) {
                controller.record_ack(ack_rx.recv().await.unwrap());
            }
            reader_started_tx.send(()).unwrap();
        });
        assert!(reader_started_rx.try_recv().is_err());
        ack_tx
            .send(AudioAck {
                revision: target,
                outcome: AckOutcome::Applied,
            })
            .unwrap();
        reader_started_rx.recv().await.unwrap();
        gate.await.unwrap();
    }

    #[tokio::test]
    async fn reader_join_error_retains_identity_and_all_readers_join_on_shutdown() {
        let mut readers = ReaderSupervisor::new();
        readers.spawn(DeviceId(4), async { panic!("reader panic") });
        let exit = readers.next().await.unwrap();
        assert_eq!(exit.device(), DeviceId(4));

        let (stop_tx, stop_rx) = watch::channel(false);
        let (joined_tx, mut joined_rx) = mpsc::unbounded_channel();
        for device in [DeviceId(0), DeviceId(1)] {
            let mut stop = stop_rx.clone();
            let joined = joined_tx.clone();
            readers.spawn(device, async move {
                stop.changed().await.unwrap();
                joined.send(device).unwrap();
                Ok(device)
            });
        }
        stop_and_join_readers(&stop_tx, &mut readers).await;
        let mut joined = [
            joined_rx.recv().await.unwrap(),
            joined_rx.recv().await.unwrap(),
        ];
        joined.sort_by_key(|device| device.0);
        assert_eq!(joined, [DeviceId(0), DeviceId(1)]);
    }

    #[test]
    fn final_hold_loss_publishes_idle_before_fatal_cleanup() {
        let (mut controller, mut desired, _) = controller(1);
        recover(&mut controller, DeviceId(0));
        controller
            .handle(ControlEvent::PttDown {
                device: DeviceId(0),
                timestamp: timestamp(1),
            })
            .unwrap();
        desired.borrow_and_update();
        let effect = controller
            .handle(ControlEvent::DeviceLost(DeviceId(0)))
            .unwrap();
        assert_eq!(effect.published, Some(2));
        assert_eq!(
            desired.borrow_and_update().unwrap().state,
            DesiredAudio::PttIdle
        );
        assert_eq!(
            controller.fatal_cleanup_target(effect.published).unwrap(),
            (ControllerMode::Ptt, 2)
        );
        assert!(!desired.has_changed().unwrap());
    }

    #[test]
    fn open_reader_and_generic_fatal_targets_do_not_supersede_open() {
        let (mut open_controller, mut open_desired, _) = controller(1);
        let open = open_controller.publish(DesiredAudio::Open).unwrap();
        open_desired.borrow_and_update();
        let loss_revision = open_controller
            .handle(ControlEvent::DeviceLost(DeviceId(0)))
            .unwrap()
            .published;
        assert_eq!(
            open_controller.fatal_cleanup_target(loss_revision).unwrap(),
            (ControllerMode::Open, open)
        );
        assert_eq!(
            open_controller.fatal_cleanup_target(None).unwrap(),
            (ControllerMode::Open, open)
        );
        assert_eq!(open_controller.latest.unwrap().revision, open);
        assert!(!open_desired.has_changed().unwrap());
    }

    #[test]
    fn ptt_generic_fatal_target_publishes_idle() {
        let (mut ptt_controller, mut ptt_desired, _) = controller(1);
        let talking = ptt_controller.publish(DesiredAudio::PttTalking).unwrap();
        ptt_desired.borrow_and_update();
        assert_eq!(
            ptt_controller.fatal_cleanup_target(None).unwrap(),
            (ControllerMode::Ptt, talking + 1)
        );
        assert_eq!(
            ptt_desired.borrow_and_update().unwrap().state,
            DesiredAudio::PttIdle
        );
    }

    #[test]
    fn acknowledgement_tracking_proves_open_without_mute_and_rejects_stale_ack() {
        let (mut controller, mut desired, _) = controller(1);
        let open = controller.publish(DesiredAudio::Open).unwrap();
        controller.record_ack(AudioAck {
            revision: open,
            outcome: AckOutcome::Applied,
        });
        controller.record_ack(AudioAck {
            revision: open - 1,
            outcome: AckOutcome::Applied,
        });
        assert!(controller.target_complete(open));
        assert_eq!(controller.mode(), ControllerMode::Open);
        assert_eq!(
            desired.borrow_and_update().unwrap().state,
            DesiredAudio::Open
        );
    }

    #[tokio::test]
    async fn ptt_idle_ack_precedes_reader_stop_and_pending_open_ack_precedes_audio_stop() {
        let (mut controller, mut desired, _) = controller(1);
        let idle = controller.publish(DesiredAudio::PttIdle).unwrap();
        let (reader_stop_tx, mut reader_stop_rx) = watch::channel(false);
        assert!(!*reader_stop_rx.borrow());
        controller.record_ack(AudioAck {
            revision: idle,
            outcome: AckOutcome::Applied,
        });
        assert!(controller.target_complete(idle));
        request_reader_stop(&reader_stop_tx);
        reader_stop_rx.changed().await.unwrap();

        let open = controller.publish(DesiredAudio::Open).unwrap();
        let (audio_stop_tx, mut audio_stop_rx) = watch::channel(false);
        assert!(!controller.target_complete(open));
        assert!(!*audio_stop_rx.borrow());
        controller.record_ack(AudioAck {
            revision: open,
            outcome: AckOutcome::HandledFailure,
        });
        request_audio_stop(&audio_stop_tx);
        audio_stop_rx.changed().await.unwrap();
        assert_eq!(
            desired.borrow_and_update().unwrap().state,
            DesiredAudio::Open
        );
    }

    #[tokio::test]
    async fn worker_fatal_is_observed_before_required_ack() {
        let (stop_tx, stop_rx) = watch::channel(false);
        let (joined_tx, joined_rx) = oneshot::channel();
        let mut readers = ReaderSupervisor::new();
        readers.spawn(DeviceId(7), async move {
            let mut stop = stop_rx;
            stop.changed().await.unwrap();
            joined_tx.send(()).unwrap();
            Ok(DeviceId(7))
        });
        let worker =
            tokio::spawn(async { Err::<(), _>(WorkerError::AcknowledgementChannelClosed) });
        let error = audio_exit_before("required audio acknowledgement", worker.await);
        assert!(error.to_string().contains("acknowledgement channel closed"));
        stop_and_join_readers(&stop_tx, &mut readers).await;
        joined_rx.await.unwrap();
    }

    #[tokio::test]
    async fn startup_signal_waits_for_idle_and_never_starts_reader() {
        let (mut controller, _, _) = controller(1);
        let target = controller
            .handle(ControlEvent::Startup)
            .unwrap()
            .published
            .unwrap();
        let (signal_tx, signal_rx) = oneshot::channel();
        let (ack_tx, mut ack_rx) = mpsc::unbounded_channel();
        let (reader_tx, mut reader_rx) = mpsc::unbounded_channel::<()>();
        let task = tokio::spawn(async move {
            signal_rx.await.unwrap();
            while !controller.target_complete(target) {
                controller.record_ack(ack_rx.recv().await.unwrap());
            }
            drop(reader_tx);
        });
        signal_tx.send(()).unwrap();
        assert!(reader_rx.try_recv().is_err());
        ack_tx
            .send(AudioAck {
                revision: target,
                outcome: AckOutcome::Applied,
            })
            .unwrap();
        task.await.unwrap();
        assert_eq!(reader_rx.recv().await, None);
    }
}
