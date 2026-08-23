#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Ptt,
    Open,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeviceId(pub usize);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TransitionEpoch(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct EventTimestamp {
    pub seconds: i64,
    pub nanoseconds: u32,
}

impl EventTimestamp {
    pub const fn new(seconds: i64, nanoseconds: u32) -> Self {
        assert!(nanoseconds < 1_000_000_000);
        Self {
            seconds,
            nanoseconds,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlEvent {
    Startup,
    DeviceRecovered {
        device: DeviceId,
        ptt_held: bool,
        toggle_held: bool,
    },
    DeviceLost(DeviceId),
    SynchronizationLost(DeviceId),
    PttDown {
        device: DeviceId,
        timestamp: EventTimestamp,
    },
    PttUp {
        device: DeviceId,
        timestamp: EventTimestamp,
    },
    ToggleDown {
        device: DeviceId,
        timestamp: EventTimestamp,
    },
    ToggleUp {
        device: DeviceId,
        timestamp: EventTimestamp,
    },
    BeginBarrier(TransitionEpoch),
    KeySnapshot {
        device: DeviceId,
        epoch: TransitionEpoch,
        ptt_held: bool,
        toggle_held: bool,
    },
    CompleteBarrier(TransitionEpoch),
    Shutdown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DesiredAudio {
    PttIdle,
    PttTalking,
    Open,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransitionAction {
    StartTransitionBarrier,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PolicyOutcome {
    pub desired_audio: Option<DesiredAudio>,
    pub transition: Option<TransitionAction>,
}

impl PolicyOutcome {
    fn audio(desired_audio: DesiredAudio) -> Self {
        Self {
            desired_audio: Some(desired_audio),
            transition: None,
        }
    }

    fn transition(desired_audio: DesiredAudio) -> Self {
        Self {
            desired_audio: Some(desired_audio),
            transition: Some(TransitionAction::StartTransitionBarrier),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct DeviceState {
    recovered: bool,
    ptt_held: bool,
    toggle_held: bool,
    ptt_release_gate: bool,
    toggle_release_gate: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Policy {
    mode: Mode,
    devices: Vec<DeviceState>,
    pending_epoch: Option<TransitionEpoch>,
    mode_cutoff: Option<EventTimestamp>,
    startup_handled: bool,
}

impl Policy {
    pub fn new(device_count: usize) -> Self {
        Self {
            mode: Mode::Ptt,
            devices: vec![DeviceState::default(); device_count],
            pending_epoch: None,
            mode_cutoff: None,
            startup_handled: false,
        }
    }

    pub fn handle(&mut self, event: ControlEvent) -> PolicyOutcome {
        match event {
            ControlEvent::Startup => {
                if self.startup_handled {
                    PolicyOutcome::default()
                } else {
                    self.startup_handled = true;
                    PolicyOutcome::audio(DesiredAudio::PttIdle)
                }
            }
            ControlEvent::DeviceRecovered {
                device,
                ptt_held,
                toggle_held,
            } => self.recover(device, ptt_held, toggle_held),
            ControlEvent::DeviceLost(device) | ControlEvent::SynchronizationLost(device) => {
                self.lose(device)
            }
            ControlEvent::PttDown { device, timestamp } => self.ptt_down(device, timestamp),
            ControlEvent::PttUp { device, timestamp } => self.ptt_up(device, timestamp),
            ControlEvent::ToggleDown { device, timestamp } => self.toggle_down(device, timestamp),
            ControlEvent::ToggleUp { device, timestamp } => self.toggle_up(device, timestamp),
            ControlEvent::BeginBarrier(epoch) => {
                self.pending_epoch = Some(epoch);
                self.gate_held_ptt();
                PolicyOutcome::default()
            }
            ControlEvent::KeySnapshot {
                device,
                epoch,
                ptt_held,
                toggle_held,
            } => self.snapshot(device, epoch, ptt_held, toggle_held),
            ControlEvent::CompleteBarrier(epoch) => {
                if self.pending_epoch == Some(epoch) {
                    self.gate_held_ptt();
                    self.pending_epoch = None;
                }
                PolicyOutcome::default()
            }
            ControlEvent::Shutdown => {
                if self.mode == Mode::Ptt {
                    PolicyOutcome::audio(DesiredAudio::PttIdle)
                } else {
                    PolicyOutcome::default()
                }
            }
        }
    }

    fn recover(&mut self, device: DeviceId, ptt_held: bool, toggle_held: bool) -> PolicyOutcome {
        let had_valid_hold = self.has_valid_hold();
        let Some(device) = self.device_mut(device) else {
            return PolicyOutcome::default();
        };
        device.recovered = true;
        device.ptt_held = ptt_held;
        device.toggle_held = toggle_held;
        device.ptt_release_gate = ptt_held;
        device.toggle_release_gate = toggle_held;
        self.idle_if_final_hold_removed(had_valid_hold)
    }

    fn lose(&mut self, device: DeviceId) -> PolicyOutcome {
        let had_valid_hold = self.has_valid_hold();
        let Some(device) = self.device_mut(device) else {
            return PolicyOutcome::default();
        };
        *device = DeviceState::default();
        self.idle_if_final_hold_removed(had_valid_hold)
    }

    fn ptt_down(&mut self, device: DeviceId, timestamp: EventTimestamp) -> PolicyOutcome {
        if self.is_pretransition(timestamp) {
            return self.apply_old_edge(device, true, true);
        }
        let had_valid_hold = self.has_valid_hold();
        let barrier_pending = self.pending_epoch.is_some();
        let mode = self.mode;
        let Some(device) = self.device_mut(device) else {
            return PolicyOutcome::default();
        };
        if !device.recovered || device.ptt_held {
            return PolicyOutcome::default();
        }

        device.ptt_held = true;
        if barrier_pending {
            device.ptt_release_gate = true;
            return PolicyOutcome::default();
        }
        if mode == Mode::Ptt && !device.ptt_release_gate && !had_valid_hold {
            PolicyOutcome::audio(DesiredAudio::PttTalking)
        } else {
            PolicyOutcome::default()
        }
    }

    fn ptt_up(&mut self, device: DeviceId, timestamp: EventTimestamp) -> PolicyOutcome {
        if self.is_pretransition(timestamp) {
            return self.apply_old_edge(device, true, false);
        }
        let had_valid_hold = self.has_valid_hold();
        let Some(device) = self.device_mut(device) else {
            return PolicyOutcome::default();
        };
        device.ptt_held = false;
        device.ptt_release_gate = false;
        self.idle_if_final_hold_removed(had_valid_hold)
    }

    fn toggle_down(&mut self, device: DeviceId, timestamp: EventTimestamp) -> PolicyOutcome {
        if self.is_pretransition(timestamp) {
            return self.apply_old_edge(device, false, true);
        }
        let Some(device) = self.device_mut(device) else {
            return PolicyOutcome::default();
        };
        if !device.recovered || device.toggle_held {
            return PolicyOutcome::default();
        }
        device.toggle_held = true;
        if device.toggle_release_gate {
            return PolicyOutcome::default();
        }

        self.mode = match self.mode {
            Mode::Ptt => Mode::Open,
            Mode::Open => Mode::Ptt,
        };
        self.mode_cutoff = Some(timestamp);
        self.gate_held_ptt();
        let desired = match self.mode {
            Mode::Ptt => DesiredAudio::PttIdle,
            Mode::Open => DesiredAudio::Open,
        };
        PolicyOutcome::transition(desired)
    }

    fn toggle_up(&mut self, device: DeviceId, timestamp: EventTimestamp) -> PolicyOutcome {
        if self.is_pretransition(timestamp) {
            return self.apply_old_edge(device, false, false);
        }
        if let Some(device) = self.device_mut(device) {
            device.toggle_held = false;
            device.toggle_release_gate = false;
        }
        PolicyOutcome::default()
    }

    fn snapshot(
        &mut self,
        device: DeviceId,
        epoch: TransitionEpoch,
        ptt_held: bool,
        toggle_held: bool,
    ) -> PolicyOutcome {
        if self.pending_epoch != Some(epoch) {
            return PolicyOutcome::default();
        }
        let Some(device) = self.device_mut(device) else {
            return PolicyOutcome::default();
        };
        device.ptt_held = ptt_held;
        device.toggle_held = toggle_held;
        device.ptt_release_gate = ptt_held;
        device.toggle_release_gate = toggle_held;
        PolicyOutcome::default()
    }

    fn apply_old_edge(&mut self, device: DeviceId, ptt: bool, down: bool) -> PolicyOutcome {
        let Some(device) = self.device_mut(device) else {
            return PolicyOutcome::default();
        };
        match (ptt, down) {
            (true, true) => {
                device.ptt_held = true;
                device.ptt_release_gate = true;
            }
            (true, false) => {
                device.ptt_held = false;
                device.ptt_release_gate = false;
            }
            (false, true) => {
                device.toggle_held = true;
                device.toggle_release_gate = true;
            }
            (false, false) => {
                device.toggle_held = false;
                device.toggle_release_gate = false;
            }
        }
        PolicyOutcome::default()
    }

    fn is_pretransition(&self, timestamp: EventTimestamp) -> bool {
        self.mode_cutoff.is_some_and(|cutoff| timestamp <= cutoff)
    }

    fn idle_if_final_hold_removed(&self, had_valid_hold: bool) -> PolicyOutcome {
        if self.mode == Mode::Ptt && had_valid_hold && !self.has_valid_hold() {
            PolicyOutcome::audio(DesiredAudio::PttIdle)
        } else {
            PolicyOutcome::default()
        }
    }

    fn has_valid_hold(&self) -> bool {
        self.devices
            .iter()
            .any(|device| device.recovered && device.ptt_held && !device.ptt_release_gate)
    }

    fn gate_held_ptt(&mut self) {
        for device in &mut self.devices {
            if device.ptt_held {
                device.ptt_release_gate = true;
            }
        }
    }

    fn device_mut(&mut self, device: DeviceId) -> Option<&mut DeviceState> {
        self.devices.get_mut(device.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIRST: DeviceId = DeviceId(0);
    const SECOND: DeviceId = DeviceId(1);
    const EPOCH_1: TransitionEpoch = TransitionEpoch(1);
    const EPOCH_2: TransitionEpoch = TransitionEpoch(2);

    fn timestamp(value: i64) -> EventTimestamp {
        EventTimestamp::new(value, 0)
    }

    fn ptt_down(device: DeviceId) -> ControlEvent {
        ControlEvent::PttDown {
            device,
            timestamp: timestamp(30),
        }
    }

    fn ptt_up(device: DeviceId) -> ControlEvent {
        ControlEvent::PttUp {
            device,
            timestamp: timestamp(40),
        }
    }

    fn toggle_down_at(device: DeviceId, time: i64) -> ControlEvent {
        ControlEvent::ToggleDown {
            device,
            timestamp: timestamp(time),
        }
    }

    fn toggle_down(device: DeviceId) -> ControlEvent {
        toggle_down_at(device, 10)
    }

    fn toggle_up(device: DeviceId) -> ControlEvent {
        ControlEvent::ToggleUp {
            device,
            timestamp: timestamp(20),
        }
    }

    fn audio(desired: DesiredAudio) -> PolicyOutcome {
        PolicyOutcome::audio(desired)
    }

    fn transition(desired: DesiredAudio) -> PolicyOutcome {
        PolicyOutcome::transition(desired)
    }

    fn recover(policy: &mut Policy, device: DeviceId, ptt_held: bool, toggle_held: bool) {
        assert_eq!(
            policy.handle(ControlEvent::DeviceRecovered {
                device,
                ptt_held,
                toggle_held,
            }),
            PolicyOutcome::default()
        );
    }

    fn fresh(policy: &mut Policy, device: DeviceId) {
        recover(policy, device, false, false);
    }

    #[test]
    fn startup_emits_idle_once_and_invalid_devices_are_ignored() {
        let mut policy = Policy::new(1);
        assert_eq!(
            policy.handle(ControlEvent::Startup),
            audio(DesiredAudio::PttIdle)
        );
        assert_eq!(
            policy.handle(ControlEvent::Startup),
            PolicyOutcome::default()
        );
        assert_eq!(
            policy.handle(ptt_down(DeviceId(99))),
            PolicyOutcome::default()
        );
    }

    #[test]
    fn overlapping_holds_talk_once_and_only_final_release_idles() {
        let mut policy = Policy::new(2);
        fresh(&mut policy, FIRST);
        fresh(&mut policy, SECOND);
        assert_eq!(
            policy.handle(ptt_down(FIRST)),
            audio(DesiredAudio::PttTalking)
        );
        assert_eq!(policy.handle(ptt_down(SECOND)), PolicyOutcome::default());
        assert_eq!(policy.handle(ptt_up(FIRST)), PolicyOutcome::default());
        assert_eq!(policy.handle(ptt_up(SECOND)), audio(DesiredAudio::PttIdle));
    }

    #[test]
    fn only_final_loss_or_synchronization_loss_idles() {
        for final_loss in [
            ControlEvent::DeviceLost(SECOND),
            ControlEvent::SynchronizationLost(SECOND),
        ] {
            let mut policy = Policy::new(2);
            fresh(&mut policy, FIRST);
            fresh(&mut policy, SECOND);
            policy.handle(ptt_down(FIRST));
            policy.handle(ptt_down(SECOND));
            assert_eq!(
                policy.handle(ControlEvent::DeviceLost(FIRST)),
                PolicyOutcome::default()
            );
            assert_eq!(policy.handle(final_loss), audio(DesiredAudio::PttIdle));
        }
    }

    #[test]
    fn recovery_held_keys_require_release() {
        let mut policy = Policy::new(1);
        recover(&mut policy, FIRST, true, true);
        assert_eq!(policy.handle(ptt_down(FIRST)), PolicyOutcome::default());
        assert_eq!(policy.handle(toggle_down(FIRST)), PolicyOutcome::default());
        policy.handle(ptt_up(FIRST));
        policy.handle(toggle_up(FIRST));
        assert_eq!(
            policy.handle(ptt_down(FIRST)),
            audio(DesiredAudio::PttTalking)
        );
        assert_eq!(
            policy.handle(toggle_down_at(FIRST, 50)),
            transition(DesiredAudio::Open)
        );
    }

    #[test]
    fn held_recovery_post_cutoff_release_then_fresh_down_talks() {
        let mut policy = Policy::new(1);
        recover(&mut policy, FIRST, true, false);
        assert_eq!(
            policy.handle(ControlEvent::PttUp {
                device: FIRST,
                timestamp: timestamp(11),
            }),
            PolicyOutcome::default()
        );
        assert_eq!(
            policy.handle(ControlEvent::PttDown {
                device: FIRST,
                timestamp: timestamp(12),
            }),
            audio(DesiredAudio::PttTalking)
        );
    }

    #[test]
    fn delayed_ptt_down_during_barrier_cannot_talk() {
        let mut policy = Policy::new(2);
        fresh(&mut policy, FIRST);
        fresh(&mut policy, SECOND);
        assert_eq!(
            policy.handle(toggle_down(FIRST)),
            transition(DesiredAudio::Open)
        );
        policy.handle(ControlEvent::BeginBarrier(EPOCH_1));
        policy.handle(toggle_up(FIRST));
        assert_eq!(
            policy.handle(toggle_down_at(FIRST, 30)),
            transition(DesiredAudio::PttIdle)
        );
        assert_eq!(policy.handle(ptt_down(SECOND)), PolicyOutcome::default());
    }

    #[test]
    fn matching_completion_retains_gate_until_release_and_fresh_down() {
        let mut policy = Policy::new(1);
        fresh(&mut policy, FIRST);
        policy.handle(ControlEvent::BeginBarrier(EPOCH_1));
        policy.handle(ptt_down(FIRST));
        policy.handle(ControlEvent::CompleteBarrier(EPOCH_1));
        assert_eq!(policy.handle(ptt_down(FIRST)), PolicyOutcome::default());
        assert_eq!(policy.handle(ptt_up(FIRST)), PolicyOutcome::default());
        assert_eq!(
            policy.handle(ptt_down(FIRST)),
            audio(DesiredAudio::PttTalking)
        );
    }

    #[test]
    fn stale_snapshots_and_completion_do_not_change_current_barrier() {
        let mut policy = Policy::new(1);
        fresh(&mut policy, FIRST);
        policy.handle(ControlEvent::BeginBarrier(EPOCH_2));
        policy.handle(ControlEvent::KeySnapshot {
            device: FIRST,
            epoch: EPOCH_1,
            ptt_held: true,
            toggle_held: false,
        });
        policy.handle(ControlEvent::CompleteBarrier(EPOCH_1));
        assert_eq!(policy.pending_epoch, Some(EPOCH_2));
        assert!(!policy.devices[0].ptt_held);
    }

    #[test]
    fn current_snapshot_never_grants_talk() {
        let mut policy = Policy::new(1);
        fresh(&mut policy, FIRST);
        policy.handle(ControlEvent::BeginBarrier(EPOCH_1));
        policy.handle(ControlEvent::KeySnapshot {
            device: FIRST,
            epoch: EPOCH_1,
            ptt_held: true,
            toggle_held: false,
        });
        policy.handle(ControlEvent::CompleteBarrier(EPOCH_1));
        assert_eq!(policy.handle(ptt_down(FIRST)), PolicyOutcome::default());
        policy.handle(ptt_up(FIRST));
        assert_eq!(
            policy.handle(ptt_down(FIRST)),
            audio(DesiredAudio::PttTalking)
        );
    }

    #[test]
    fn pre_or_equal_cutoff_ptt_cannot_talk_and_post_cutoff_down_can() {
        let mut policy = Policy::new(1);
        fresh(&mut policy, FIRST);
        policy.handle(toggle_down_at(FIRST, 10));
        policy.handle(ControlEvent::ToggleUp {
            device: FIRST,
            timestamp: timestamp(11),
        });
        policy.handle(toggle_down_at(FIRST, 20));
        policy.handle(ControlEvent::BeginBarrier(EPOCH_1));
        policy.handle(ControlEvent::CompleteBarrier(EPOCH_1));
        assert_eq!(
            policy.handle(ControlEvent::PttDown {
                device: FIRST,
                timestamp: timestamp(19),
            }),
            PolicyOutcome::default()
        );
        assert_eq!(
            policy.handle(ControlEvent::PttDown {
                device: FIRST,
                timestamp: timestamp(20),
            }),
            PolicyOutcome::default()
        );
        policy.handle(ControlEvent::PttUp {
            device: FIRST,
            timestamp: timestamp(20),
        });
        assert_eq!(
            policy.handle(ControlEvent::PttDown {
                device: FIRST,
                timestamp: timestamp(21),
            }),
            audio(DesiredAudio::PttTalking)
        );
    }

    #[test]
    fn pre_or_equal_cutoff_toggle_cannot_retoggle() {
        let mut policy = Policy::new(1);
        fresh(&mut policy, FIRST);
        policy.handle(toggle_down_at(FIRST, 10));
        policy.handle(ControlEvent::ToggleUp {
            device: FIRST,
            timestamp: timestamp(11),
        });
        policy.handle(ControlEvent::BeginBarrier(EPOCH_1));
        policy.handle(ControlEvent::CompleteBarrier(EPOCH_1));
        assert_eq!(
            policy.handle(ControlEvent::ToggleDown {
                device: FIRST,
                timestamp: timestamp(10),
            }),
            PolicyOutcome::default()
        );
        assert_eq!(policy.mode, Mode::Open);
        assert!(policy.devices[0].toggle_held);
        assert!(policy.devices[0].toggle_release_gate);
    }

    #[test]
    fn second_toggle_replaces_epoch_and_stale_completion_cannot_clear_it() {
        let mut policy = Policy::new(1);
        fresh(&mut policy, FIRST);
        assert_eq!(
            policy.handle(toggle_down(FIRST)),
            transition(DesiredAudio::Open)
        );
        policy.handle(ControlEvent::BeginBarrier(EPOCH_1));
        policy.handle(toggle_up(FIRST));
        assert_eq!(
            policy.handle(toggle_down_at(FIRST, 30)),
            transition(DesiredAudio::PttIdle)
        );
        policy.handle(ControlEvent::BeginBarrier(EPOCH_2));
        policy.handle(ControlEvent::CompleteBarrier(EPOCH_1));
        assert_eq!(policy.pending_epoch, Some(EPOCH_2));
        policy.handle(ControlEvent::CompleteBarrier(EPOCH_2));
        assert_eq!(policy.pending_epoch, None);
    }

    #[test]
    fn open_tracks_ptt_without_audio_and_shutdown_respects_mode() {
        let mut policy = Policy::new(1);
        fresh(&mut policy, FIRST);
        policy.handle(toggle_down(FIRST));
        assert_eq!(policy.handle(ptt_down(FIRST)), PolicyOutcome::default());
        assert!(policy.devices[0].ptt_held);
        assert_eq!(
            policy.handle(ControlEvent::Shutdown),
            PolicyOutcome::default()
        );

        policy.handle(toggle_up(FIRST));
        policy.handle(toggle_down_at(FIRST, 30));
        assert_eq!(
            policy.handle(ControlEvent::Shutdown),
            audio(DesiredAudio::PttIdle)
        );
    }
}
