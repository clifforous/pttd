use std::collections::HashSet;
use std::fmt;
use std::future::Future;
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use evdev::raw_stream::{EventStream, RawDevice};
use evdev::{EventType, InputEvent, KeyCode, SynchronizationCode};
use tokio::sync::{mpsc, watch};

use crate::state::{ControlEvent, DeviceId, EventTimestamp, TransitionEpoch};

nix::ioctl_write_ptr!(eviocsclockid, b'E', 0xa0, libc::c_int);

pub const RECONNECT_DELAY: Duration = Duration::from_secs(1);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReaderConfig {
    pub device: DeviceId,
    pub path: PathBuf,
    pub ptt_key: KeyCode,
    pub toggle_key: KeyCode,
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub struct NodeIdentity(libc::dev_t);

#[derive(Debug, Default)]
pub struct NodeClaims {
    claimed: Mutex<HashSet<NodeIdentity>>,
}

impl NodeClaims {
    pub fn new() -> Self {
        Self::default()
    }

    fn try_acquire(self: &Arc<Self>, identity: NodeIdentity) -> Option<NodeLease> {
        let mut claimed = self.claimed.lock().unwrap();
        if !claimed.insert(identity) {
            return None;
        }
        Some(NodeLease {
            claims: Arc::clone(self),
            identity,
        })
    }
}

struct NodeLease {
    claims: Arc<NodeClaims>,
    identity: NodeIdentity,
}

impl Drop for NodeLease {
    fn drop(&mut self) {
        self.claims.claimed.lock().unwrap().remove(&self.identity);
    }
}

#[derive(Debug)]
pub enum ReaderError {
    EventChannelClosed(DeviceId),
    EpochChannelClosed(DeviceId),
}

impl ReaderError {
    pub fn device(&self) -> DeviceId {
        match self {
            Self::EventChannelClosed(device) | Self::EpochChannelClosed(device) => *device,
        }
    }
}

impl fmt::Display for ReaderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EventChannelClosed(device) => {
                write!(formatter, "event channel closed for reader {}", device.0)
            }
            Self::EpochChannelClosed(device) => {
                write!(formatter, "epoch channel closed for reader {}", device.0)
            }
        }
    }
}

impl std::error::Error for ReaderError {}

/// Runs one configured reader. Graceful shutdown returns its `DeviceId`; errors also retain it.
/// A controller can therefore identify normal task completion directly and panic/cancellation via
/// the task ID associated with its join handle.
pub async fn run_reader(
    config: ReaderConfig,
    claims: Arc<NodeClaims>,
    events: mpsc::Sender<ControlEvent>,
    mut epochs: watch::Receiver<Option<TransitionEpoch>>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<DeviceId, ReaderError> {
    let mut latest_epoch = *epochs.borrow_and_update();

    loop {
        if *shutdown.borrow() {
            return Ok(config.device);
        }
        if epochs.has_changed().is_err() {
            return Err(ReaderError::EpochChannelClosed(config.device));
        }

        let mut device = match RawDevice::open(&config.path) {
            Ok(device) => device,
            Err(error) => {
                tracing::warn!(device = config.device.0, path = %config.path.display(), %error, "failed to open input device");
                if wait_to_reconnect(config.device, &mut epochs, &mut shutdown, &mut latest_epoch)
                    .await?
                {
                    return Ok(config.device);
                }
                continue;
            }
        };
        if let Err(error) = set_monotonic_clock(&device) {
            tracing::warn!(device = config.device.0, path = %config.path.display(), %error, "failed to select monotonic input clock");
            drop(device);
            if wait_to_reconnect(config.device, &mut epochs, &mut shutdown, &mut latest_epoch)
                .await?
            {
                return Ok(config.device);
            }
            continue;
        }

        let identity = match node_identity(&device) {
            Ok(identity) => identity,
            Err(error) => {
                tracing::warn!(device = config.device.0, path = %config.path.display(), %error, "failed to identify opened input device");
                drop(device);
                if wait_to_reconnect(config.device, &mut epochs, &mut shutdown, &mut latest_epoch)
                    .await?
                {
                    return Ok(config.device);
                }
                continue;
            }
        };
        let Some(lease) = claims.try_acquire(identity) else {
            tracing::warn!(device = config.device.0, path = %config.path.display(), "opened input node is already claimed");
            drop(device);
            if wait_to_reconnect(config.device, &mut epochs, &mut shutdown, &mut latest_epoch)
                .await?
            {
                return Ok(config.device);
            }
            continue;
        };

        if let Err(error) = set_nonblocking(&device).and_then(|()| drain_raw_events(&mut device)) {
            tracing::warn!(device = config.device.0, path = %config.path.display(), %error, "failed to clear initial input events");
            drop(lease);
            drop(device);
            if wait_to_reconnect(config.device, &mut epochs, &mut shutdown, &mut latest_epoch)
                .await?
            {
                return Ok(config.device);
            }
            continue;
        }

        let held = match query_held(&device, &config) {
            Ok(held) => held,
            Err(error) => {
                tracing::warn!(device = config.device.0, path = %config.path.display(), %error, "failed to query input key state");
                drop(lease);
                drop(device);
                if wait_to_reconnect(config.device, &mut epochs, &mut shutdown, &mut latest_epoch)
                    .await?
                {
                    return Ok(config.device);
                }
                continue;
            }
        };
        let mut recovery_cutoff = match monotonic_now() {
            Ok(timestamp) => timestamp,
            Err(error) => {
                tracing::warn!(device = config.device.0, path = %config.path.display(), %error, "failed to capture input recovery cutoff");
                drop(lease);
                drop(device);
                if wait_to_reconnect(config.device, &mut epochs, &mut shutdown, &mut latest_epoch)
                    .await?
                {
                    return Ok(config.device);
                }
                continue;
            }
        };
        let mut stream = match device.into_event_stream() {
            Ok(stream) => stream,
            Err(error) => {
                tracing::warn!(device = config.device.0, path = %config.path.display(), %error, "failed to start input event stream");
                drop(lease);
                if wait_to_reconnect(config.device, &mut epochs, &mut shutdown, &mut latest_epoch)
                    .await?
                {
                    return Ok(config.device);
                }
                continue;
            }
        };

        if !send_event(
            &events,
            &mut shutdown,
            config.device,
            recovered_event(config.device, held),
        )
        .await?
        {
            return Ok(config.device);
        }
        if let Some(epoch) = latest_epoch
            && !send_event(
                &events,
                &mut shutdown,
                config.device,
                snapshot_event(config.device, epoch, held),
            )
            .await?
        {
            return Ok(config.device);
        }
        let end = active_stream(
            &config,
            &events,
            &mut epochs,
            &mut latest_epoch,
            &mut recovery_cutoff,
            &mut shutdown,
            &mut stream,
        )
        .await?;

        drop(stream);
        drop(lease);
        match end {
            ActiveEnd::Shutdown => return Ok(config.device),
            ActiveEnd::EpochClosed => {
                return Err(ReaderError::EpochChannelClosed(config.device));
            }
            ActiveEnd::Disconnected => {
                for step in disconnect_plan(config.device) {
                    match step {
                        DisconnectStep::EmitLoss(event) => {
                            if !send_event(&events, &mut shutdown, config.device, event).await? {
                                return Ok(config.device);
                            }
                        }
                        DisconnectStep::Retry => {
                            if wait_to_reconnect(
                                config.device,
                                &mut epochs,
                                &mut shutdown,
                                &mut latest_epoch,
                            )
                            .await?
                            {
                                return Ok(config.device);
                            }
                        }
                    }
                }
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ActiveEnd {
    Shutdown,
    EpochClosed,
    Disconnected,
}

async fn active_stream(
    config: &ReaderConfig,
    events: &mpsc::Sender<ControlEvent>,
    epochs: &mut watch::Receiver<Option<TransitionEpoch>>,
    latest_epoch: &mut Option<TransitionEpoch>,
    recovery_cutoff: &mut EventTimestamp,
    shutdown: &mut watch::Receiver<bool>,
    stream: &mut EventStream,
) -> Result<ActiveEnd, ReaderError> {
    loop {
        match next_active(epochs, shutdown, stream.next_event()).await {
            ActiveWake::Shutdown => return Ok(ActiveEnd::Shutdown),
            ActiveWake::Epoch(Err(_)) => return Ok(ActiveEnd::EpochClosed),
            ActiveWake::Epoch(Ok(())) => {
                *latest_epoch = *epochs.borrow_and_update();
                let Some(epoch) = *latest_epoch else {
                    continue;
                };
                let held = query_held(stream.device(), config);
                if let Err(error) = &held {
                    tracing::warn!(device = config.device.0, path = %config.path.display(), %error, "failed to query transition snapshot");
                }
                match snapshot_decision(config, epoch, held.map_err(|_| ())) {
                    SnapshotDecision::Emit(event) => {
                        if !send_event(events, shutdown, config.device, event).await? {
                            return Ok(ActiveEnd::Shutdown);
                        }
                    }
                    SnapshotDecision::Disconnect => return Ok(ActiveEnd::Disconnected),
                }
            }
            ActiveWake::Event(Err(error)) => {
                tracing::warn!(device = config.device.0, path = %config.path.display(), %error, "failed to read input event");
                return Ok(ActiveEnd::Disconnected);
            }
            ActiveWake::Event(Ok(event)) => match decide(event, config, false, *recovery_cutoff) {
                Err(error) => {
                    tracing::warn!(device = config.device.0, path = %config.path.display(), %error, "invalid input event timestamp");
                    return Ok(ActiveEnd::Disconnected);
                }
                Ok(Decision::Emit(event)) => {
                    if !send_event(events, shutdown, config.device, event).await? {
                        return Ok(ActiveEnd::Shutdown);
                    }
                }
                Ok(Decision::BeginSynchronization) => {
                    match synchronize_stream(config, events, shutdown, stream).await? {
                        SynchronizationEnd::Recovered(cutoff) => *recovery_cutoff = cutoff,
                        SynchronizationEnd::Shutdown => return Ok(ActiveEnd::Shutdown),
                        SynchronizationEnd::Disconnected => return Ok(ActiveEnd::Disconnected),
                    }
                }
                Ok(Decision::CompleteSynchronization | Decision::Ignore) => {}
            },
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SynchronizationEnd {
    Recovered(EventTimestamp),
    Shutdown,
    Disconnected,
}

async fn synchronize_stream(
    config: &ReaderConfig,
    events: &mpsc::Sender<ControlEvent>,
    shutdown: &mut watch::Receiver<bool>,
    stream: &mut EventStream,
) -> Result<SynchronizationEnd, ReaderError> {
    if !send_event(
        events,
        shutdown,
        config.device,
        synchronization_lost_event(config.device),
    )
    .await?
    {
        return Ok(SynchronizationEnd::Shutdown);
    }
    match discard_to_report(stream, shutdown).await {
        Ok(true) => {}
        Ok(false) => return Ok(SynchronizationEnd::Shutdown),
        Err(error) => {
            tracing::warn!(device = config.device.0, path = %config.path.display(), %error, "failed while discarding dropped input events");
            return Ok(SynchronizationEnd::Disconnected);
        }
    }
    let held = match query_held(stream.device(), config) {
        Ok(held) => held,
        Err(error) => {
            tracing::warn!(device = config.device.0, path = %config.path.display(), %error, "failed to recover input key state");
            return Ok(SynchronizationEnd::Disconnected);
        }
    };
    let cutoff = match monotonic_now() {
        Ok(cutoff) => cutoff,
        Err(error) => {
            tracing::warn!(device = config.device.0, path = %config.path.display(), %error, "failed to capture synchronization recovery cutoff");
            return Ok(SynchronizationEnd::Disconnected);
        }
    };
    if send_event(
        events,
        shutdown,
        config.device,
        recovered_event(config.device, held),
    )
    .await?
    {
        Ok(SynchronizationEnd::Recovered(cutoff))
    } else {
        Ok(SynchronizationEnd::Shutdown)
    }
}

fn set_monotonic_clock(device: &RawDevice) -> io::Result<()> {
    let clock_id = libc::CLOCK_MONOTONIC;
    unsafe { eviocsclockid(device.as_raw_fd(), &clock_id) }
        .map(|_| ())
        .map_err(io::Error::from)
}

fn set_nonblocking(device: &RawDevice) -> io::Result<()> {
    let fd = device.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

enum RawDrainStep {
    Continue,
    Complete,
    Error(io::Error),
}

fn raw_drain_step(result: io::Result<usize>) -> RawDrainStep {
    match result {
        Ok(0) => RawDrainStep::Error(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "input device returned an empty event batch",
        )),
        Ok(_) => RawDrainStep::Continue,
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => RawDrainStep::Complete,
        Err(error) => RawDrainStep::Error(error),
    }
}

fn drain_raw_events(device: &mut RawDevice) -> io::Result<()> {
    loop {
        let result = device.fetch_events().map(|events| events.count());
        match raw_drain_step(result) {
            RawDrainStep::Continue => {}
            RawDrainStep::Complete => return Ok(()),
            RawDrainStep::Error(error) => return Err(error),
        }
    }
}

fn monotonic_now() -> io::Result<EventTimestamp> {
    let mut timestamp = MaybeUninit::<libc::timespec>::uninit();
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, timestamp.as_mut_ptr()) } == -1 {
        return Err(io::Error::last_os_error());
    }
    timestamp_from_timespec(unsafe { timestamp.assume_init() })
}

fn timestamp_from_timespec(timestamp: libc::timespec) -> io::Result<EventTimestamp> {
    let nanoseconds = u32::try_from(timestamp.tv_nsec)
        .ok()
        .filter(|nanoseconds| *nanoseconds < 1_000_000_000)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid nanosecond value"))?;
    Ok(EventTimestamp::new(timestamp.tv_sec, nanoseconds))
}

fn timestamp_from_system_time(timestamp: SystemTime) -> io::Result<EventTimestamp> {
    let duration = timestamp.duration_since(UNIX_EPOCH).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidData, "negative input event timestamp")
    })?;
    let seconds = i64::try_from(duration.as_secs())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "input timestamp overflow"))?;
    Ok(EventTimestamp::new(seconds, duration.subsec_nanos()))
}

fn event_timestamp(event: &InputEvent) -> io::Result<EventTimestamp> {
    timestamp_from_system_time(event.timestamp())
}

fn node_identity(device: &RawDevice) -> io::Result<NodeIdentity> {
    let mut stat = MaybeUninit::<libc::stat>::uninit();
    let result = unsafe { libc::fstat(device.as_raw_fd(), stat.as_mut_ptr()) };
    if result == -1 {
        return Err(io::Error::last_os_error());
    }
    let stat = unsafe { stat.assume_init() };
    Ok(NodeIdentity(stat.st_rdev))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct HeldKeys {
    ptt: bool,
    toggle: bool,
}

fn query_held(device: &RawDevice, config: &ReaderConfig) -> io::Result<HeldKeys> {
    let keys = device.get_key_state()?;
    Ok(HeldKeys {
        ptt: keys.contains(config.ptt_key),
        toggle: keys.contains(config.toggle_key),
    })
}

fn recovered_event(device: DeviceId, held: HeldKeys) -> ControlEvent {
    ControlEvent::DeviceRecovered {
        device,
        ptt_held: held.ptt,
        toggle_held: held.toggle,
    }
}

fn synchronization_lost_event(device: DeviceId) -> ControlEvent {
    ControlEvent::SynchronizationLost(device)
}

fn lost_event(device: DeviceId) -> ControlEvent {
    ControlEvent::DeviceLost(device)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DisconnectStep {
    EmitLoss(ControlEvent),
    Retry,
}

fn disconnect_plan(device: DeviceId) -> [DisconnectStep; 2] {
    [
        DisconnectStep::EmitLoss(lost_event(device)),
        DisconnectStep::Retry,
    ]
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SnapshotDecision {
    Emit(ControlEvent),
    Disconnect,
}

fn snapshot_event(device: DeviceId, epoch: TransitionEpoch, held: HeldKeys) -> ControlEvent {
    ControlEvent::KeySnapshot {
        device,
        epoch,
        ptt_held: held.ptt,
        toggle_held: held.toggle,
    }
}

fn snapshot_decision(
    config: &ReaderConfig,
    epoch: TransitionEpoch,
    held: Result<HeldKeys, ()>,
) -> SnapshotDecision {
    match held {
        Ok(held) => SnapshotDecision::Emit(snapshot_event(config.device, epoch, held)),
        Err(()) => SnapshotDecision::Disconnect,
    }
}

#[derive(Debug)]
enum ActiveWake {
    Shutdown,
    Epoch(Result<(), watch::error::RecvError>),
    Event(io::Result<InputEvent>),
}

async fn next_active<F>(
    epochs: &mut watch::Receiver<Option<TransitionEpoch>>,
    shutdown: &mut watch::Receiver<bool>,
    event: F,
) -> ActiveWake
where
    F: Future<Output = io::Result<InputEvent>>,
{
    tokio::select! {
        biased;
        _ = shutdown.changed() => ActiveWake::Shutdown,
        result = epochs.changed() => ActiveWake::Epoch(result),
        result = event => ActiveWake::Event(result),
    }
}

async fn discard_to_report(
    stream: &mut EventStream,
    shutdown: &mut watch::Receiver<bool>,
) -> io::Result<bool> {
    loop {
        let event = tokio::select! {
            biased;
            _ = shutdown.changed() => return Ok(false),
            result = stream.next_event() => result?,
        };
        if decide_synchronizing(event) == Decision::CompleteSynchronization {
            return Ok(true);
        }
    }
}

async fn send_event(
    events: &mpsc::Sender<ControlEvent>,
    shutdown: &mut watch::Receiver<bool>,
    device: DeviceId,
    event: ControlEvent,
) -> Result<bool, ReaderError> {
    if *shutdown.borrow() {
        return Ok(false);
    }
    tokio::select! {
        biased;
        _ = shutdown.changed() => Ok(false),
        result = events.send(event) => result
            .map(|_| true)
            .map_err(|_| ReaderError::EventChannelClosed(device)),
    }
}

async fn wait_to_reconnect(
    device: DeviceId,
    epochs: &mut watch::Receiver<Option<TransitionEpoch>>,
    shutdown: &mut watch::Receiver<bool>,
    latest_epoch: &mut Option<TransitionEpoch>,
) -> Result<bool, ReaderError> {
    if *shutdown.borrow() {
        return Ok(true);
    }
    let delay = tokio::time::sleep(RECONNECT_DELAY);
    tokio::pin!(delay);
    loop {
        tokio::select! {
            biased;
            _ = shutdown.changed() => return Ok(true),
            result = epochs.changed() => match result {
                Ok(()) => *latest_epoch = *epochs.borrow_and_update(),
                Err(_) => return Err(ReaderError::EpochChannelClosed(device)),
            },
            _ = &mut delay => return Ok(false),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Decision {
    Emit(ControlEvent),
    BeginSynchronization,
    CompleteSynchronization,
    Ignore,
}

fn decide(
    event: InputEvent,
    config: &ReaderConfig,
    synchronizing: bool,
    recovery_cutoff: EventTimestamp,
) -> io::Result<Decision> {
    if synchronizing {
        return Ok(decide_synchronizing(event));
    }
    if event.event_type() == EventType::SYNCHRONIZATION
        && event.code() == SynchronizationCode::SYN_DROPPED.0
    {
        return Ok(Decision::BeginSynchronization);
    }
    if event.event_type() != EventType::KEY {
        return Ok(Decision::Ignore);
    }
    let key = KeyCode::new(event.code());
    if key != config.ptt_key && key != config.toggle_key || !matches!(event.value(), 0 | 1) {
        return Ok(Decision::Ignore);
    }
    normalize_key(
        key,
        event.value(),
        event_timestamp(&event)?,
        config,
        recovery_cutoff,
    )
}

fn normalize_key(
    key: KeyCode,
    value: i32,
    timestamp: EventTimestamp,
    config: &ReaderConfig,
    recovery_cutoff: EventTimestamp,
) -> io::Result<Decision> {
    // evdev timestamps have microsecond precision, so a same-microsecond tie is conservatively
    // treated as represented by the nanosecond-precision recovery cutoff.
    if timestamp <= recovery_cutoff {
        return Ok(Decision::Ignore);
    }
    let event = match (key == config.ptt_key, key == config.toggle_key, value) {
        (true, _, 1) => ControlEvent::PttDown {
            device: config.device,
            timestamp,
        },
        (true, _, 0) => ControlEvent::PttUp {
            device: config.device,
            timestamp,
        },
        (_, true, 1) => ControlEvent::ToggleDown {
            device: config.device,
            timestamp,
        },
        (_, true, 0) => ControlEvent::ToggleUp {
            device: config.device,
            timestamp,
        },
        _ => return Ok(Decision::Ignore),
    };
    Ok(Decision::Emit(event))
}

fn decide_synchronizing(event: InputEvent) -> Decision {
    if event.event_type() == EventType::SYNCHRONIZATION
        && event.code() == SynchronizationCode::SYN_REPORT.0
    {
        Decision::CompleteSynchronization
    } else {
        Decision::Ignore
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(device: usize) -> ReaderConfig {
        ReaderConfig {
            device: DeviceId(device),
            path: PathBuf::from("/unused"),
            ptt_key: KeyCode::KEY_F9,
            toggle_key: KeyCode::KEY_F10,
        }
    }

    fn event(event_type: EventType, code: u16, value: i32) -> InputEvent {
        InputEvent::new(event_type.0, code, value)
    }

    #[test]
    fn normalizes_tagged_keys_and_filters_repeats_and_movement() {
        let config = config(3);
        let cutoff = EventTimestamp::new(10, 0);
        let timestamp = EventTimestamp::new(11, 0);
        assert_eq!(
            normalize_key(KeyCode::KEY_F9, 1, timestamp, &config, cutoff).unwrap(),
            Decision::Emit(ControlEvent::PttDown {
                device: DeviceId(3),
                timestamp,
            })
        );
        assert_eq!(
            normalize_key(KeyCode::KEY_F10, 0, timestamp, &config, cutoff).unwrap(),
            Decision::Emit(ControlEvent::ToggleUp {
                device: DeviceId(3),
                timestamp,
            })
        );
        assert_eq!(
            decide(
                event(EventType::KEY, KeyCode::KEY_F9.code(), 2),
                &config,
                false,
                cutoff,
            )
            .unwrap(),
            Decision::Ignore
        );
        assert_eq!(
            decide(event(EventType::RELATIVE, 0, 4), &config, false, cutoff,).unwrap(),
            Decision::Ignore
        );
    }

    #[test]
    fn recovery_cutoff_discards_down_and_up_at_or_before_it() {
        let config = config(0);
        let cutoff = EventTimestamp::new(10, 500);
        for (value, timestamp) in [(1, EventTimestamp::new(9, 999_999_999)), (0, cutoff)] {
            assert_eq!(
                normalize_key(KeyCode::KEY_F9, value, timestamp, &config, cutoff).unwrap(),
                Decision::Ignore
            );
        }
        assert!(matches!(
            normalize_key(
                KeyCode::KEY_F9,
                1,
                EventTimestamp::new(10, 501),
                &config,
                cutoff,
            )
            .unwrap(),
            Decision::Emit(ControlEvent::PttDown { .. })
        ));
    }

    #[test]
    fn conservative_same_microsecond_recovery_tie_is_discarded_until_next_microsecond() {
        let config = config(0);
        let cutoff = EventTimestamp::new(10, 123_456_789);
        assert_eq!(
            normalize_key(
                KeyCode::KEY_F9,
                1,
                EventTimestamp::new(10, 123_456_000),
                &config,
                cutoff,
            )
            .unwrap(),
            Decision::Ignore
        );
        assert_eq!(
            normalize_key(
                KeyCode::KEY_F9,
                1,
                EventTimestamp::new(10, 123_457_000),
                &config,
                cutoff,
            )
            .unwrap(),
            Decision::Emit(ControlEvent::PttDown {
                device: DeviceId(0),
                timestamp: EventTimestamp::new(10, 123_457_000),
            })
        );
    }

    #[test]
    fn synchronization_boundary_discards_through_report() {
        let config = config(0);
        assert_eq!(
            decide(
                event(
                    EventType::SYNCHRONIZATION,
                    SynchronizationCode::SYN_DROPPED.0,
                    0,
                ),
                &config,
                false,
                EventTimestamp::new(10, 0),
            )
            .unwrap(),
            Decision::BeginSynchronization
        );
        assert_eq!(
            decide(
                event(EventType::KEY, KeyCode::KEY_F9.code(), 1),
                &config,
                true,
                EventTimestamp::new(10, 0),
            )
            .unwrap(),
            Decision::Ignore
        );
        assert_eq!(
            decide(
                event(
                    EventType::SYNCHRONIZATION,
                    SynchronizationCode::SYN_REPORT.0,
                    0,
                ),
                &config,
                true,
                EventTimestamp::new(10, 0),
            )
            .unwrap(),
            Decision::CompleteSynchronization
        );
        assert_eq!(
            [
                synchronization_lost_event(DeviceId(0)),
                recovered_event(
                    DeviceId(0),
                    HeldKeys {
                        ptt: true,
                        toggle: false,
                    },
                ),
                snapshot_event(
                    DeviceId(0),
                    TransitionEpoch(4),
                    HeldKeys {
                        ptt: true,
                        toggle: false,
                    },
                ),
            ],
            [
                ControlEvent::SynchronizationLost(DeviceId(0)),
                ControlEvent::DeviceRecovered {
                    device: DeviceId(0),
                    ptt_held: true,
                    toggle_held: false,
                },
                ControlEvent::KeySnapshot {
                    device: DeviceId(0),
                    epoch: TransitionEpoch(4),
                    ptt_held: true,
                    toggle_held: false,
                },
            ]
        );
    }

    #[test]
    fn same_rdev_claim_is_exclusive() {
        let claims = Arc::new(NodeClaims::new());
        let identity = NodeIdentity(123);
        let first = claims.try_acquire(identity);
        assert!(first.is_some());
        assert!(claims.try_acquire(identity).is_none());
        assert!(claims.try_acquire(NodeIdentity(124)).is_some());
    }

    #[test]
    fn lease_release_allows_reacquisition() {
        let claims = Arc::new(NodeClaims::new());
        let identity = NodeIdentity(123);
        let lease = claims.try_acquire(identity).unwrap();
        drop(lease);
        assert!(claims.try_acquire(identity).is_some());
    }

    #[tokio::test]
    async fn watch_coalesces_to_latest_epoch() {
        let (sender, mut receiver) = watch::channel(None);
        sender.send(Some(TransitionEpoch(1))).unwrap();
        sender.send(Some(TransitionEpoch(2))).unwrap();
        receiver.changed().await.unwrap();
        assert_eq!(*receiver.borrow_and_update(), Some(TransitionEpoch(2)));
    }

    #[test]
    fn timestamp_conversion_and_comparison_share_one_representation() {
        let timespec = timestamp_from_timespec(libc::timespec {
            tv_sec: 12,
            tv_nsec: 345_678_000,
        })
        .unwrap();
        let system =
            timestamp_from_system_time(UNIX_EPOCH + Duration::new(12, 345_678_000)).unwrap();
        assert_eq!(timespec, system);
        assert!(EventTimestamp::new(12, 345_677_999) < timespec);
        assert_eq!(
            event_timestamp(&InputEvent::new(EventType::KEY.0, 1, 1)).unwrap(),
            EventTimestamp::new(0, 0)
        );
    }

    #[test]
    fn raw_drain_continues_through_batches_and_stops_only_at_would_block() {
        assert!(matches!(raw_drain_step(Ok(3)), RawDrainStep::Continue));
        assert!(matches!(raw_drain_step(Ok(1)), RawDrainStep::Continue));
        assert!(matches!(
            raw_drain_step(Err(io::Error::from(io::ErrorKind::WouldBlock))),
            RawDrainStep::Complete
        ));
        let RawDrainStep::Error(error) =
            raw_drain_step(Err(io::Error::from(io::ErrorKind::PermissionDenied)))
        else {
            panic!("non-WouldBlock error was not propagated");
        };
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn snapshot_query_failure_disconnects_for_loss_and_retry() {
        assert_eq!(
            snapshot_decision(&config(0), TransitionEpoch(1), Err(())),
            SnapshotDecision::Disconnect
        );
        assert_eq!(
            disconnect_plan(DeviceId(0)),
            [
                DisconnectStep::EmitLoss(ControlEvent::DeviceLost(DeviceId(0))),
                DisconnectStep::Retry,
            ]
        );
    }
}
