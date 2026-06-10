//! Channel-scoped hardware leases and cancellation-safe operation lifetimes.

use std::collections::HashMap;
use std::sync::Arc;

use dut_ctrl::{Button, ButtonChannels, PowerControl, PowerControlConfig, PowerError};
use tokio::sync::Mutex;
use tokio_util::task::TaskTracker;
use tokio_util::task::task_tracker::TaskTrackerToken;

use crate::lock_manager::{self, EndpointLease, LockError};

#[derive(Clone, Default)]
pub struct PowerRuntime {
    tasks: TaskTracker,
}

impl PowerRuntime {
    /// Refuse new operations and drain workers, held buttons, and recovery.
    pub async fn shutdown(&self) {
        self.tasks.close();
        self.tasks.wait().await;
    }

    pub(crate) fn is_idle(&self) -> bool {
        self.tasks.is_empty()
    }

    pub fn is_closed(&self) -> bool {
        self.tasks.is_closed()
    }
}

struct ControlState {
    backend: Box<dyn PowerControl>,
    held: HashMap<Button, EndpointLease>,
    host: String,
    port: String,
    channels: ButtonChannels,
    lock_dir: String,
}

impl ControlState {
    fn claim(&mut self, button: Button) -> Result<(), PowerError> {
        if !self.backend.has_button(button) {
            return Err(PowerError::NotConfigured);
        }
        if self.held.contains_key(&button) {
            return Ok(());
        }
        let host = format!("relay:{}:channel:{}", self.host, self.channels.get(button));
        let lease =
            EndpointLease::acquire(&host, &self.port, &self.lock_dir).map_err(
                |error| match error {
                    LockError::Busy(_) => PowerError::RelayBusy,
                    LockError::Io(error) => PowerError::Io(error),
                },
            )?;
        self.held.insert(button, lease);
        Ok(())
    }
}

/// A control session retains its channel lease from press through release.
/// Workers survive caller cancellation; dropping a session restores any button
/// whose release was not confirmed. Shutdown waits for these workers.
pub struct ManagedPower {
    state: Arc<Mutex<ControlState>>,
    runtime: PowerRuntime,
    _lifetime: TaskTrackerToken,
    name: &'static str,
    buttons: [bool; 4],
}

enum Operation {
    Press(Button, bool),
    Release(Button, bool),
    Pulse(Button, u64),
    Verify,
}

impl ManagedPower {
    pub fn new(mut config: PowerControlConfig, lock_dir: String, runtime: PowerRuntime) -> Self {
        config.host = lock_manager::canonical_host(&config.host);
        let backend = dut_ctrl::build_backend(&config);
        let name = backend.name();
        let buttons = [
            Button::Reset,
            Button::Download,
            Button::Power,
            Button::PowerKey,
        ]
        .map(|button| backend.has_button(button));
        Self {
            state: Arc::new(Mutex::new(ControlState {
                backend,
                held: HashMap::new(),
                host: lock_manager::canonical_host(&config.host),
                port: config.port.to_string(),
                channels: config.channels,
                lock_dir,
            })),
            _lifetime: runtime.tasks.token(),
            runtime,
            name,
            buttons,
        }
    }

    async fn run(&mut self, operation: Operation) -> Result<bool, PowerError> {
        if self.runtime.is_closed() {
            return Err(PowerError::Io(std::io::Error::other("DUT is stopping")));
        }
        // Acquire before spawning. Drop recovery must queue behind even a
        // worker that has been registered but has not been polled yet.
        let mut state = self.state.clone().lock_owned().await;
        self.runtime
            .tasks
            .spawn(async move {
                match operation {
                    Operation::Press(button, verified) => {
                        // Arm recovery before the first write: cancellation or a
                        // lost response does not prove that the press was rejected.
                        state.claim(button)?;
                        if verified {
                            state.backend.press_verified(button).await?;
                        } else {
                            state.backend.press(button).await?;
                        }
                    }
                    Operation::Release(button, verified) => {
                        state.claim(button)?;
                        if verified {
                            state.backend.release_verified(button).await?;
                        } else {
                            state.backend.release(button).await?;
                        }
                        state.held.remove(&button);
                    }
                    Operation::Pulse(button, delay) => {
                        state.claim(button)?;
                        state.backend.pulse(button, delay).await?;
                        state.held.remove(&button);
                    }
                    Operation::Verify => return state.backend.verify().await,
                }
                Ok(true)
            })
            .await
            .map_err(|error| PowerError::Io(std::io::Error::other(error)))?
    }

    /// An explicit user press intentionally persists beyond this request.
    pub async fn keep_pressed(&mut self) {
        self.state.lock().await.held.clear();
    }
}

impl Drop for ManagedPower {
    fn drop(&mut self) {
        if self
            .state
            .try_lock()
            .is_ok_and(|state| state.held.is_empty())
        {
            return;
        }
        let state = self.state.clone();
        // The session token is still alive until after this task is registered,
        // so shutdown cannot observe an empty tracker between the two.
        self.runtime.tasks.spawn(async move {
            let mut state = state.lock().await;
            let buttons: Vec<_> = state.held.keys().copied().collect();
            for button in buttons {
                let mut failure = None;
                for _ in 0..3 {
                    match state.backend.release_verified(button).await {
                        Ok(()) => { failure = None; break; }
                        Err(error) => failure = Some(error),
                    }
                }
                if let Some(error) = failure {
                    tracing::error!(?button, %error, "Hardware recovery failed; button state is unknown");
                }
                state.held.remove(&button);
            }
        });
    }
}

#[dut_ctrl::async_trait]
impl PowerControl for ManagedPower {
    async fn press(&mut self, button: Button) -> Result<(), PowerError> {
        self.run(Operation::Press(button, false)).await.map(|_| ())
    }
    async fn release(&mut self, button: Button) -> Result<(), PowerError> {
        self.run(Operation::Release(button, false))
            .await
            .map(|_| ())
    }
    async fn press_verified(&mut self, button: Button) -> Result<(), PowerError> {
        self.run(Operation::Press(button, true)).await.map(|_| ())
    }
    async fn release_verified(&mut self, button: Button) -> Result<(), PowerError> {
        self.run(Operation::Release(button, true)).await.map(|_| ())
    }
    async fn pulse(&mut self, button: Button, delay_ms: u64) -> Result<(), PowerError> {
        self.run(Operation::Pulse(button, delay_ms))
            .await
            .map(|_| ())
    }
    async fn verify(&mut self) -> Result<bool, PowerError> {
        self.run(Operation::Verify).await
    }
    fn name(&self) -> &'static str {
        self.name
    }
    fn has_button(&self, button: Button) -> bool {
        self.buttons[match button {
            Button::Reset => 0,
            Button::Download => 1,
            Button::Power => 2,
            Button::PowerKey => 3,
        }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::LazyLock;
    use std::sync::atomic::{AtomicU8, Ordering};
    use std::time::Duration;
    use tokio::sync::Semaphore;

    struct Probe {
        pressed: AtomicU8,
        started: [Semaphore; 4],
        finish: [Semaphore; 4],
    }

    impl Default for Probe {
        fn default() -> Self {
            Self {
                pressed: AtomicU8::new(0),
                started: std::array::from_fn(|_| Semaphore::new(0)),
                finish: std::array::from_fn(|_| Semaphore::new(0)),
            }
        }
    }

    static PROBES: LazyLock<std::sync::Mutex<HashMap<String, Arc<Probe>>>> =
        LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));

    struct Mock {
        probe: Arc<Probe>,
        channels: ButtonChannels,
    }

    fn index(button: Button) -> usize {
        match button {
            Button::Reset => 0,
            Button::Download => 1,
            Button::Power => 2,
            Button::PowerKey => 3,
        }
    }

    #[dut_ctrl::async_trait]
    impl PowerControl for Mock {
        async fn press(&mut self, button: Button) -> Result<(), PowerError> {
            self.probe
                .pressed
                .fetch_or(1 << self.channels.get(button), Ordering::SeqCst);
            Ok(())
        }
        async fn release(&mut self, button: Button) -> Result<(), PowerError> {
            self.probe
                .pressed
                .fetch_and(!(1 << self.channels.get(button)), Ordering::SeqCst);
            Ok(())
        }
        async fn pulse(&mut self, button: Button, _: u64) -> Result<(), PowerError> {
            self.press(button).await?;
            self.probe.started[index(button)].add_permits(1);
            self.probe.finish[index(button)]
                .acquire()
                .await
                .unwrap()
                .forget();
            self.release(button).await
        }
        async fn verify(&mut self) -> Result<bool, PowerError> {
            Ok(true)
        }
        fn name(&self) -> &'static str {
            "managed-test"
        }
        fn has_button(&self, button: Button) -> bool {
            self.channels.get(button) != 0
        }
    }

    fn factory(config: &PowerControlConfig) -> Box<dyn PowerControl> {
        Box::new(Mock {
            probe: PROBES.lock().unwrap()[&config.dut_name].clone(),
            channels: config.channels.clone(),
        })
    }

    struct Fixture {
        dir: tempfile::TempDir,
        probe: Arc<Probe>,
        runtime: PowerRuntime,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let probe = Arc::new(Probe::default());
            PROBES
                .lock()
                .unwrap()
                .insert(dir.path().to_string_lossy().into_owned(), probe.clone());
            dut_ctrl::register_backend("managed-test", factory);
            Self {
                dir,
                probe,
                runtime: PowerRuntime::default(),
            }
        }
        fn control(&self, reset: u8) -> ManagedPower {
            ManagedPower::new(
                PowerControlConfig {
                    backend: "managed-test".into(),
                    host: "relay.invalid".into(),
                    port: 41000,
                    channels: ButtonChannels {
                        reset,
                        power: 2,
                        ..Default::default()
                    },
                    reset_time_ms: 1,
                    power_off_time_ms: 1,
                    dut_name: self.dir.path().to_string_lossy().into_owned(),
                },
                self.dir.path().to_string_lossy().into_owned(),
                self.runtime.clone(),
            )
        }
        async fn started(&self, button: Button) {
            tokio::time::timeout(
                Duration::from_secs(2),
                self.probe.started[index(button)].acquire(),
            )
            .await
            .unwrap()
            .unwrap()
            .forget();
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            PROBES
                .lock()
                .unwrap()
                .remove(self.dir.path().to_str().unwrap());
        }
    }

    #[tokio::test]
    async fn separate_channels_and_duts_do_not_block_each_other() {
        let f = Fixture::new();
        let mut reset = f.control(1);
        let mut power = f.control(1);
        let mut other_dut = f.control(3);
        reset.press(Button::Reset).await.unwrap();
        power.press(Button::Power).await.unwrap();
        other_dut.press(Button::Reset).await.unwrap();
        let mut conflicting = f.control(1);
        assert!(matches!(
            conflicting.press(Button::Reset).await,
            Err(PowerError::RelayBusy)
        ));
        drop(conflicting);
        reset.release(Button::Reset).await.unwrap();
        let mut still_busy = f.control(1);
        assert!(matches!(
            still_busy.press(Button::Power).await,
            Err(PowerError::RelayBusy)
        ));
        assert_eq!(f.probe.pressed.load(Ordering::SeqCst), (1 << 2) | (1 << 3));
        drop((reset, power, other_dut, still_busy));
        f.runtime.shutdown().await;
        assert_eq!(f.probe.pressed.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn cancelled_pulse_keeps_lease_until_hardware_released_and_shutdown_waits() {
        let f = Fixture::new();
        let mut control = f.control(1);
        let caller = tokio::spawn(async move { control.pulse(Button::Reset, 500).await });
        f.started(Button::Reset).await;
        caller.abort();
        assert!(caller.await.unwrap_err().is_cancelled());
        let mut contender = f.control(1);
        assert!(matches!(
            contender.press(Button::Reset).await,
            Err(PowerError::RelayBusy)
        ));
        drop(contender);
        let shutdown = f.runtime.shutdown();
        tokio::pin!(shutdown);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut shutdown)
                .await
                .is_err()
        );
        f.probe.finish[index(Button::Reset)].add_permits(1);
        tokio::time::timeout(Duration::from_secs(2), &mut shutdown)
            .await
            .unwrap();
        assert_eq!(f.probe.pressed.load(Ordering::SeqCst), 0);
        let _lease = EndpointLease::acquire(
            "relay:relay.invalid:channel:1",
            "41000",
            f.dir.path().to_str().unwrap(),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn abandoned_press_is_restored_but_explicit_press_persists() {
        let f = Fixture::new();
        let mut abandoned = f.control(1);
        abandoned.press(Button::Reset).await.unwrap();
        drop(abandoned);
        let mut explicit = f.control(1);
        explicit.press(Button::Power).await.unwrap();
        explicit.keep_pressed().await;
        drop(explicit);
        f.runtime.shutdown().await;
        assert_eq!(f.probe.pressed.load(Ordering::SeqCst), 1 << 2);
    }

    #[tokio::test]
    async fn cancellation_before_worker_first_poll_still_restores_press() {
        use std::future::Future;
        use std::task::Poll;

        let f = Fixture::new();
        let mut control = f.control(1);
        let mut press = Box::pin(control.press(Button::Reset));
        std::future::poll_fn(|cx| {
            assert!(press.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        // The current-thread runtime has not polled the worker yet.
        drop(press);
        drop(control);
        f.runtime.shutdown().await;
        assert_eq!(f.probe.pressed.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn same_dut_reset_and_power_run_concurrently_and_stop_drains_both() {
        use crate::serial_engine::{new_shared_engine, pulse_shared};
        let f = Fixture::new();
        let path = f.dir.path();
        let mut config = crate::config::test_config(
            path,
            "41001",
            "41000",
            None,
            ".dut-serial",
            path.to_str().unwrap(),
        );
        for (key, value) in [
            ("RELAY_TYPE", "managed-test"),
            ("RELAY_IP", "relay.invalid"),
            ("RESET_CHANNEL", "1"),
            ("POWER_CHANNEL", "2"),
            ("DUT_NAME", path.to_str().unwrap()),
        ] {
            config.values.insert(key.into(), value.into());
        }
        let engine = new_shared_engine(config);
        let reset_engine = engine.clone();
        let reset = tokio::spawn(async move { pulse_shared(&reset_engine, Button::Reset).await });
        f.started(Button::Reset).await;
        let power_engine = engine.clone();
        let power = tokio::spawn(async move { pulse_shared(&power_engine, Button::Power).await });
        f.started(Button::Power).await;
        assert_eq!(f.probe.pressed.load(Ordering::SeqCst), (1 << 1) | (1 << 2));
        reset.abort();
        reset.await.unwrap_err();
        let stop_engine = engine.clone();
        let mut stop = tokio::spawn(async move {
            stop_engine.lock().await.stop().await;
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut stop)
                .await
                .is_err()
        );
        f.probe.finish[index(Button::Reset)].add_permits(1);
        f.probe.finish[index(Button::Power)].add_permits(1);
        tokio::time::timeout(Duration::from_secs(2), stop)
            .await
            .unwrap()
            .unwrap();
        power.await.unwrap();
        assert_eq!(f.probe.pressed.load(Ordering::SeqCst), 0);
        assert_eq!(
            pulse_shared(&engine, Button::Reset).await["code"],
            "engine_stopped"
        );
    }
}
