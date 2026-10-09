//! The deterministic dual-board system (DESIGN.md sections 5 and 10).
//!
//! # Scheduling
//!
//! One thread, no wall clock. Virtual time advances in quanta of 100 us (`emu_core::QUANTUM`, aligned to
//! time 0): the main board runs to the boundary, then the handset board runs to the boundary (each board
//! keeps its own clock source and, like Renode, its own chunk lag), then at the boundary
//!
//! 1. `CanLink::pump` drains both CAN controllers and delivers the frames to the other board, ordered by
//!    `(sender stamp, main before handset, transmit order)` - Renode's end-of-quantum synchronization
//!    phase; the receiving CPU sees the interrupt from its first instruction of the next quantum;
//! 2. host inputs that are due (`schedule_input`) are applied - after the pump, so a link control change
//!    never reclassifies a frame that was already transmitted;
//! 3. the handset CPU is released when its deferred start is due;
//! 4. at every multiple of 50 virtual ms the runner fixtures poll (`poll_fixtures`): standby request
//!    detection and the inferred main PE3 handset supply gate.
//!
//! The result is a pure function of the firmware images, the configuration and the time-stamped inputs:
//! how the host chunks its `run_*` calls does not change it.
//!
//! # Fixtures (runner parity, `emulation/run_emulator.py`)
//!
//! * **Handset gate**: the handset CPU is held halted (`cpu IsHalted true`) until main `GPIOE_ODR` bit 3 is
//!   seen set by a poll. The release only takes effect one quantum later (Renode `DeferredEnabled`): after a
//!   poll at 1.05 s the first handset instruction runs at 1.0501 s (`ExecutedInstructions` 44 990 000 at
//!   1.5 s). The handset's clock and peripherals run while its CPU is held. `simultaneous_start` bypasses
//!   the gate.
//! * **Standby**: main `PWR_CR1 & 7 == 3` with `SCB.SCR` bit 2 set halts both CPUs; `standby` becomes true
//!   and the system stops until the host calls `wake`/`restart`.
//! * **Error stop**: the handset executing at `0x0800598e` (interrupts-disabled error loop) stops the run
//!   with an error text, like the viewer.
//! * **Boot modes**: handset-wake writes main `RTC.BKP1R = 0x32F0`, `PWR.SR1 = 0x104`, `RCC.CSR = 0` through
//!   the bus before the first instruction (after the host restored its RTC checkpoint, if any); cold leaves
//!   the flags zero.
//!
//! Full electrical power, standby and reset behavior and CAN wire timing are not modeled.

use crate::board::{Board, RunReport};
use crate::bus;
use crate::firmware::{self, Firmware, Release};
use crate::fixtures::{self, Inputs, POLL_INTERVAL};
use crate::handset::{self, HandsetBoard, HandsetOptions};
use crate::main_board::{self, MainBoard};
use crate::models::adc_main::NgcMainAdc;
use crate::models::buttons::{Buttons, ButtonsError};
use crate::models::can_link::{CanLink, CanPorts, EndpointId};
use crate::models::lcd::NgcParallelLcd;
use crate::models::telemetry::{self, Telemetry};
use crate::models::ms5837::NgcMs5837;
use crate::models::qspi::NgcQuadSpi;
use crate::models::uart_capture::UartCapture;
use crate::sha256::Sha256;
use armv7m::{Cpu, FastForwardStats, TraceEntry};
pub use armv7m::{RoutineAccelMode, RoutineAccelStats};
use emu_core::{from_secs_f64, to_secs_f64, Json, Time, Width, QUANTUM, TICKS_PER_MILLISECOND};
use stm32::can::{CanFrame, StmCan, TxFrame};
use stm32::i2c::Stm32F7I2c;
use stm32::iwdg::Iwdg;
use stm32::rtc::{Rtc, RtcCheckpoint};
use stm32::usart::Usart;

/// What the system contains.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Real main 5.8 and handset 65.3 firmware linked over CAN (`run_emulator.py --dual`).
    Dual,
    /// Handset only, no CAN peer (`run_emulator.py` without `--dual`).
    HandsetOnly,
}

/// Dual hardware boot fixture (`--boot-mode`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BootMode {
    /// Existing RTC backup plus standby exit from the handset wake input (the default).
    HandsetWake,
    /// Zero wake flags: the unchanged firmware selects cause 0 and requests standby.
    Cold,
}

impl BootMode {
    pub fn name(self) -> &'static str {
        match self {
            BootMode::HandsetWake => "handset-wake",
            BootMode::Cold => "cold",
        }
    }

    pub fn parse(text: &str) -> Option<BootMode> {
        match text {
            "handset-wake" => Some(BootMode::HandsetWake),
            "cold" => Some(BootMode::Cold),
            _ => None,
        }
    }
}

/// Which board of the system.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Which {
    Main,
    Handset,
}

impl Which {
    pub fn name(self) -> &'static str {
        match self {
            Which::Main => "main",
            Which::Handset => "handset",
        }
    }

    pub fn parse(text: &str) -> Option<Which> {
        match text {
            "main" => Some(Which::Main),
            "handset" => Some(Which::Handset),
            _ => None,
        }
    }
}

/// Where a board's CPU starts after a machine reset (IWDG expiry or `AIRCR.SYSRESETREQ`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResetStart {
    /// The application vector table: `VTOR = 0x08004000`, `SP`/`PC` read from its first two words, and the `.resc`
    /// register fixture (CAN `MCR = 0x10000`) written again. This stands in for the manufacturer bootloader that the
    /// supplied images omit, which on the physical device hands over to the application after every reset.
    ApplicationVectors,
    /// What the stock Renode platform does (measured with the pinned 1.17.0 runtime: `reference` experiment of the
    /// FEATURES work package): `CortexM.Reset` clears `VTOR`, `InitPCAndSP` reads SP and PC from address 0 where nothing
    /// is mapped (both 0, Thumb bit clear) and the first fetch raises `UsageFault.INVSTATE` and locks the core up
    /// (`PC = 0xEFFFFFFE`, `CFSR = 0x20000`, no instruction executed). Here the registers are set to `VTOR = SP = PC = 0`
    /// and the core is held (halted) instead of locking up; peripherals and time keep running. The firmware does not
    /// run again, exactly like in Renode.
    RenodeLiteral,
}

/// Why a machine reset happened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResetCause {
    /// The firmware wrote `AIRCR.SYSRESETREQ` (`NVIC.cs`: `machine.RequestReset()`).
    SystemResetRequest,
    /// The independent watchdog expired or a window was violated (`STM32_IndependentWatchdog.cs`: `machine.RequestReset()`).
    Watchdog,
}

impl ResetCause {
    pub fn name(self) -> &'static str {
        match self {
            ResetCause::SystemResetRequest => "sysresetreq",
            ResetCause::Watchdog => "iwdg",
        }
    }
}

/// One machine reset: requested during a quantum, applied at its end (Renode `RequestReset` runs `Reset()` at
/// the nearest synchronized state).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResetEvent {
    pub board: Which,
    pub cause: ResetCause,
    /// Virtual time at which the request was made.
    pub requested_at: Time,
    /// Virtual time of the quantum boundary at which the reset ran.
    pub applied_at: Time,
}

/// Platform-script choices that are not part of the runner's command line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BuildOptions {
    /// The functional I2C idle-high fixture of the main board (`main.resc` of the analysis workspace: `gpioB OnGPIO 6|7|10|11 true`,
    /// added to the Renode runner after the alert-output investigation): the input state of PB6/PB7/PB10/PB11 is high
    /// before the first instruction, standing for the external pull-ups; the modeled I2C transactions do not drive
    /// physical open-drain lines and the GPIO pull configuration alone does not raise the input data register. **On by
    /// default** (the fixture of the analysis workspace's current runner): without it the main firmware's start-up checks PB6/PB7 low,
    /// waits for a 1 s I2C-idle timeout, loads its settings late and lets the HUD initialization consume zero brightness.
    /// The recordings that older Renode runs made (the scenario comparisons, the micro vectors) predate it and pin this
    /// to `false`. This is a functional idle-line fixture, not electrical I2C modeling.
    pub main_i2c_idle_high: bool,
}

impl Default for BuildOptions {
    fn default() -> Self {
        Self { main_i2c_idle_high: true }
    }
}

impl BuildOptions {
    /// The fixture as the state document names it.
    pub fn i2c_fixture_text(&self) -> &'static str {
        if self.main_i2c_idle_high {
            "Main I2C idle inputs PB6/PB7/PB10/PB11 driven high before the first instruction (functional idle-line fixture, not electrical I2C modeling)"
        } else {
            "Main I2C idle inputs PB6/PB7/PB10/PB11 left at their reset state (low): the fixture is off"
        }
    }
}

/// Static configuration of a system.
#[derive(Clone, Debug)]
pub struct SystemConfig {
    pub mode: Mode,
    pub boot_mode: BootMode,
    /// Start both CPUs together, bypassing the inferred main PE3 handset power gate.
    pub simultaneous_start: bool,
    /// Exact idle-loop fast-forward of both cores (results are identical either way; only host speed differs).
    pub idle_fast_forward: bool,
    /// Exact acceleration of the runtime-library routines both cores call (memoized calls; results are identical in every
    /// mode, only host speed differs; `Shadow` replays and interprets every hit and compares).
    pub routine_accel: RoutineAccelMode,
    /// Handset ADC board-ID sample (`--adc-sample`).
    pub adc_sample: u32,
    /// Sensor inputs of the dual run (applied to the main board's models).
    pub inputs: Inputs,
}

impl Default for SystemConfig {
    fn default() -> Self {
        Self {
            mode: Mode::Dual,
            boot_mode: BootMode::HandsetWake,
            simultaneous_start: false,
            idle_fast_forward: true,
            routine_accel: RoutineAccelMode::On,
            adc_sample: handset::DEFAULT_ADC_SAMPLE,
            inputs: Inputs::defaults(),
        }
    }
}

impl SystemConfig {
    pub fn dual() -> Self {
        Self::default()
    }

    pub fn handset_only() -> Self {
        Self { mode: Mode::HandsetOnly, ..Self::default() }
    }
}

/// A host input applied at a quantum boundary (`System::schedule_input`, `System::apply_input`).
#[derive(Clone, Debug, PartialEq)]
pub enum Input {
    /// Physical Up (`true`) / Down (`false`) navigation button: a 204.8 ms low pulse on the PE3 / PE5 input
    /// selected through the handset orientation byte (runner `up` / `down`).
    Navigate { up: bool },
    /// Physical confirm: two overlapping pulses staggered by 50 virtual ms (runner `confirm`).
    Confirm,
    /// `buttons Press mask`: 204.8 ms pulse on PE3 (1), PE5 (2) or both exactly simultaneously (3).
    Press { mask: u32 },
    /// `buttons Pulse mask duration`: low pulse of the given width (microseconds, 1..=2 000 000).
    Pulse { mask: u32, duration_us: u32 },
    /// `ngcCAN Connected`.
    CanConnected(bool),
    /// `ngcCAN DropId` (-1 forwards every identifier).
    CanDropId(i32),
    /// Sensor control updates (`apply_inputs`): a JSON object with the runner's input names.
    Sensors(Json),
}

/// Application variables of the main firmware that the benchmark and the reference checkpoints read. A variable is
/// `None` when the release's address table has no proven address for it (`ReleaseAddresses`); the value is never
/// taken from another release.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MainApplication {
    pub wake_cause: Option<u32>,
    pub screen_mode: Option<u32>,
    pub main_mode: Option<u32>,
    pub battery_ready: Option<u32>,
    pub hal_tick: Option<u32>,
    pub pressure: Option<u32>,
    pub temperature: Option<u32>,
}

impl MainApplication {
    pub fn to_json(&self) -> Json {
        let number = |value: Option<u32>| value.map(u64::from);
        Json::object()
            .with("wakeCause", number(self.wake_cause))
            .with("screenMode", number(self.screen_mode))
            .with("mainMode", number(self.main_mode))
            .with("batteryReady", number(self.battery_ready))
            .with("halTick", number(self.hal_tick))
            .with("pressure", number(self.pressure))
            .with("temperature", number(self.temperature))
            .with("batteryReadyFlag", self.battery_ready.map(|value| value != 0))
    }
}

/// Counters of the system loop.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SystemStats {
    /// Quanta executed.
    pub quanta: u64,
    /// Fixture polls (every 50 virtual ms).
    pub polls: u64,
    /// CAN frames delivered to the other board.
    pub can_deliveries: u64,
    /// Host inputs applied.
    pub inputs_applied: u64,
}

/// Instruction and idle-skip statistics of one board.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BoardCounters {
    pub instructions: u64,
    pub slices: u64,
    pub events_fired: u64,
    pub idle_jumps: u64,
    pub stop_requests: u64,
    pub fast_forward: FastForwardStats,
}

/// Adapter that lets the CAN link reach the controllers behind its endpoints.
struct BoardPorts<'a> {
    main: Option<&'a mut MainBoard>,
    handset: &'a mut HandsetBoard,
    main_endpoint: Option<EndpointId>,
    handset_endpoint: Option<EndpointId>,
}

impl CanPorts for BoardPorts<'_> {
    fn take_tx_frames(&mut self, endpoint: EndpointId) -> Vec<TxFrame> {
        if Some(endpoint) == self.handset_endpoint {
            let id = self.handset.ids.can1;
            return self.handset.board.get_mut::<StmCan>(id).map(StmCan::take_tx_frames).unwrap_or_default();
        }
        if Some(endpoint) == self.main_endpoint {
            if let Some(main) = self.main.as_deref_mut() {
                let id = main.ids.can1;
                return main.board.get_mut::<StmCan>(id).map(StmCan::take_tx_frames).unwrap_or_default();
            }
        }
        Vec::new()
    }

    fn deliver(&mut self, endpoint: EndpointId, frame: &CanFrame) {
        if Some(endpoint) == self.handset_endpoint {
            let id = self.handset.ids.can1;
            self.handset.board.with_peripheral::<StmCan, _>(id, |can, ctx| can.deliver_frame(ctx, frame));
        } else if Some(endpoint) == self.main_endpoint {
            if let Some(main) = self.main.as_deref_mut() {
                let id = main.ids.can1;
                main.board.with_peripheral::<StmCan, _>(id, |can, ctx| can.deliver_frame(ctx, frame));
            }
        }
    }
}

/// Tags of `bus::access_trace` records: which board the access belongs to.
pub const TAG_MAIN: u8 = 0;
pub const TAG_HANDSET: u8 = 1;

/// Rounds a time up to a whole number of quanta.
pub fn round_up_to_quantum(time: Time) -> Time {
    time.div_ceil(QUANTUM) * QUANTUM
}

/// `main.resc`: `gpioB OnGPIO 6 true` (and 7, 10, 11): the functional I2C idle-high fixture of the main board.
pub const MAIN_I2C_IDLE_HIGH_PINS: [u32; 4] = [6, 7, 10, 11];

/// Runs one board to the quantum boundary `next`. A firmware system reset request (`AIRCR.SYSRESETREQ`) makes
/// `Board::run_until` return early; the board keeps executing until the boundary (Renode runs the queued reset in
/// the sync phase) and the request time is returned.
fn run_board_to(board: &mut Board<Cpu>, next: Time) -> Option<Time> {
    let mut requested = None;
    loop {
        let report: RunReport = board.run_until(next);
        if !report.reset_requested {
            return requested;
        }
        requested.get_or_insert(board.now());
        board.clear_reset_request();
        if board.now() >= next {
            return requested;
        }
    }
}

/// The system: one or two boards, the CAN link between them and the runner fixtures.
pub struct System {
    config: SystemConfig,
    pub main: Option<MainBoard>,
    pub handset: HandsetBoard,
    pub link: CanLink,
    pub uart: UartCapture,
    main_endpoint: Option<EndpointId>,
    handset_endpoint: Option<EndpointId>,
    main_firmware: Option<Firmware>,
    handset_firmware: Firmware,
    /// The firmware release of the images (both come from the same release): selects the firmware-specific addresses.
    release: &'static Release,
    time: Time,
    /// The handset CPU has been (or is being) released: `handsetPowered` of the runner state.
    handset_released: bool,
    /// Time of the poll that released the handset (`handsetReleaseTime`).
    handset_release_time: Option<Time>,
    /// Boundary at which the released handset CPU is un-halted (one quantum after the release poll).
    handset_start_at: Option<Time>,
    standby_time: Option<Time>,
    error: Option<String>,
    inputs: Inputs,
    pending_inputs: Vec<(Time, Input)>,
    input_errors: Vec<(Time, String)>,
    fixtures_applied: bool,
    stats: SystemStats,
    /// Called between assembly and the wake fixture (`build` / `restart`): the persistence layer restores
    /// the RTC checkpoint, the NOR image and the EEPROM here (runner order: storage, inputs, RTC, fixture).
    restore_hook: Option<Box<dyn FnMut(&mut System) -> Result<(), String>>>,
    /// Called with the system that `restart` is about to replace.
    shutdown_hook: Option<Box<dyn FnMut(&System) -> Result<(), String>>>,
    /// The boot mode a plain restart uses (`--boot-mode`); Cold / Wake override it for one start only.
    default_boot_mode: BootMode,
    /// Machine resets that ran (IWDG expiry, `SYSRESETREQ`), oldest first, bounded.
    reset_log: Vec<ResetEvent>,
    /// Machine resets since this system was built (not bounded): each one clears the output histories.
    machine_reset_count: u64,
    /// `cpu.instructions()` at the last machine reset of each board (main, handset): Renode's `tlib_reset` clears the
    /// executed-instruction counter, so [`System::instructions`] counts from there.
    instruction_base: [u64; 2],
    /// CPU start state after a machine reset (default: the application vectors).
    reset_start: ResetStart,
    /// A machine reset keeps the RTC calendar (default). The pinned Renode `STM32F4_RTC.Reset()` clears the calendar
    /// to 2020-01-01 while the backup registers survive; the runner's checkpoint policy retains the calendar across
    /// Restart, so the machine reset restores the same whole-second checkpoint instead of losing it.
    reset_keeps_rtc: bool,
    options: BuildOptions,
}

/// Number of machine resets kept in the log (the newest are kept).
pub const RESET_LOG_CAPACITY: usize = 32;

/// Sampling grid of the main HUD command history (`NGCBoardTelemetry`: 20 Hz) and of the vibrator PWM command history (50 Hz).
const HUD_SAMPLE_INTERVAL: Time = 50 * TICKS_PER_MILLISECOND;
const VIBRATOR_SAMPLE_INTERVAL: Time = 20 * TICKS_PER_MILLISECOND;

impl System {
    /// Assembles the boards, attaches the UART capture and the CAN link and boots them (firmware loaded,
    /// `.resc` registers set), applies the sensor inputs and the boot fixtures. No instruction has run.
    pub fn new(config: SystemConfig, main_firmware: Option<&Firmware>, handset_firmware: &Firmware) -> Result<System, String> {
        System::new_with(config, main_firmware, handset_firmware, BuildOptions::default())
    }

    /// [`System::new`] with platform-script options (see [`BuildOptions`]).
    pub fn new_with(config: SystemConfig, main_firmware: Option<&Firmware>, handset_firmware: &Firmware, options: BuildOptions) -> Result<System, String> {
        let mut system = System::build_with(config, main_firmware, handset_firmware, options)?;
        system.apply_boot_fixtures()?;
        Ok(system)
    }

    /// Everything of `new` except the wake fixture and the handset hold, so that a persistence layer can
    /// restore the RTC checkpoint (and the NOR/EEPROM images) first. Call `apply_boot_fixtures` afterwards;
    /// running before that gives a system without the wake fixture.
    pub fn build(config: SystemConfig, main_firmware: Option<&Firmware>, handset_firmware: &Firmware) -> Result<System, String> {
        System::build_with(config, main_firmware, handset_firmware, BuildOptions::default())
    }

    /// [`System::build`] with platform-script options (see [`BuildOptions`]).
    pub fn build_with(config: SystemConfig, main_firmware: Option<&Firmware>, handset_firmware: &Firmware, options: BuildOptions) -> Result<System, String> {
        let dual = config.mode == Mode::Dual;
        if dual && main_firmware.is_none() {
            return Err("the dual system needs the main firmware".to_string());
        }
        // Both images must come from the same release (mixed pairs are refused with a clear message).
        let release = match main_firmware.filter(|_| dual) {
            Some(main) => firmware::common_release(main, handset_firmware)?,
            None => handset_firmware.release,
        };
        if dual && config.boot_mode == BootMode::Cold {
            if let Some(reason) = release.cold_boot_refusal {
                return Err(reason.to_string());
            }
        }
        let handset_board = HandsetBoard::new(handset_firmware, HandsetOptions { adc_sample: config.adc_sample, can_link: dual })?;
        let main_board = if dual { Some(MainBoard::new(main_firmware.expect("checked above"))?) } else { None };

        let mut link = CanLink::new();
        let (mut main_endpoint, mut handset_endpoint) = (None, None);
        if dual {
            // dual.resc: `connector Connect can1 ngcCAN` on the handset first, then on main.
            handset_endpoint = Some(link.attach(format!("{}.can1", handset::NAME)));
            main_endpoint = Some(link.attach(format!("{}.can1", main_board::NAME)));
        }
        let mut system = System {
            inputs: config.inputs.clone(),
            default_boot_mode: config.boot_mode,
            config,
            main: main_board,
            handset: handset_board,
            link,
            uart: UartCapture::new(),
            main_endpoint,
            handset_endpoint,
            main_firmware: main_firmware.cloned(),
            handset_firmware: handset_firmware.clone(),
            release,
            time: 0,
            handset_released: true,
            handset_release_time: None,
            handset_start_at: None,
            standby_time: None,
            error: None,
            pending_inputs: Vec::new(),
            input_errors: Vec::new(),
            fixtures_applied: false,
            stats: SystemStats::default(),
            restore_hook: None,
            shutdown_hook: None,
            reset_log: Vec::new(),
            machine_reset_count: 0,
            instruction_base: [0, 0],
            reset_start: ResetStart::ApplicationVectors,
            reset_keeps_rtc: true,
            options,
        };
        system.attach_uart_capture()?;
        system.set_idle_fast_forward(system.config.idle_fast_forward);
        system.set_routine_accel(system.config.routine_accel);
        system.apply_sensor_inputs()?;
        if options.main_i2c_idle_high {
            system.apply_main_input_fixtures();
        }
        Ok(system)
    }

    /// The runner's UART capture: five channels on a dual run (main uart4, usart1, usart2, uart5, then
    /// handset usart3), the handset's only otherwise.
    fn attach_uart_capture(&mut self) -> Result<(), String> {
        let dual = self.main.is_some();
        for (board, peripheral, _) in fixtures::UART_CHANNELS {
            if board == "main" && !dual {
                continue;
            }
            let channel = format!("ngc-{board}.{peripheral}");
            let hook = self.uart.attach(channel);
            let ok = if board == "main" {
                let main = self.main.as_mut().expect("dual");
                let id = match peripheral {
                    "uart4" => main.ids.uart4,
                    "usart1" => main.ids.usart1,
                    "usart2" => main.ids.usart2,
                    _ => main.ids.uart5,
                };
                main.board.get_mut::<Usart>(id).map(|usart| usart.set_tx_hook(Some(hook))).is_some()
            } else {
                let id = self.handset.ids.usart3;
                self.handset.board.get_mut::<Usart>(id).map(|usart| usart.set_tx_hook(Some(hook))).is_some()
            };
            if !ok {
                return Err(format!("cannot attach the UART capture to {board}.{peripheral}"));
            }
        }
        Ok(())
    }

    /// Applies the boot fixtures after the restore hook: the wake flags of a dual handset-wake boot and
    /// the handset hold (`cpu IsHalted true`) unless both CPUs start together.
    pub fn apply_boot_fixtures(&mut self) -> Result<(), String> {
        if let Some(mut hook) = self.restore_hook.take() {
            let result = hook(self);
            self.restore_hook = Some(hook);
            result?;
        }
        if let Some(main) = self.main.as_mut() {
            if self.config.boot_mode == BootMode::HandsetWake {
                for (address, value) in fixtures::WAKE_FIXTURE {
                    main.board.bus_write(address, Width::Word, value);
                }
            }
            self.handset_released = self.config.simultaneous_start;
            if !self.handset_released {
                self.handset.board.set_halted(true);
            }
        } else {
            self.handset_released = true;
        }
        self.handset_release_time = None;
        self.handset_start_at = None;
        self.standby_time = None;
        self.error = None;
        self.fixtures_applied = true;
        Ok(())
    }

    /// Installs the restore hook (see the field documentation); it runs inside `apply_boot_fixtures`.
    pub fn set_restore_hook(&mut self, hook: Option<Box<dyn FnMut(&mut System) -> Result<(), String>>>) {
        self.restore_hook = hook;
    }

    /// Installs the shutdown hook: it sees the system that is being replaced by `restart` (the persistence
    /// layer saves the RTC checkpoint and the storage images there, like the runner's `shutdown_process`).
    pub fn set_shutdown_hook(&mut self, hook: Option<Box<dyn FnMut(&System) -> Result<(), String>>>) {
        self.shutdown_hook = hook;
    }

    /// The viewer's Restart / Cold / Wake: the boards are recreated from the original firmware (RAM, CPU and
    /// peripheral state start over, the CAN link controls return to their defaults) while the nonvolatile
    /// state survives: the EEPROM and NOR images are carried over in memory, the RTC checkpoint through the
    /// shutdown / restore hooks of the persistence layer. `boot_mode` selects the boot fixture for this start
    /// only (`None` uses the configured default, like the runner's `reset`). The sensor inputs persist.
    pub fn restart(&mut self, boot_mode: Option<BootMode>) -> Result<(), String> {
        if let Some(mut hook) = self.shutdown_hook.take() {
            let result = hook(self);
            self.shutdown_hook = Some(hook);
            result?;
        }
        let eeprom = self.main.as_ref().map(|main| main.eeprom.image());
        let nor = self.main.as_ref().and_then(|main| main.board.get::<NgcQuadSpi>(main.ids.qspi)).map(NgcQuadSpi::serialize_backing);
        let mut config = self.config.clone();
        config.boot_mode = boot_mode.unwrap_or(self.default_boot_mode);
        config.inputs = self.inputs.clone();
        let mut fresh = System::build_with(config, self.main_firmware.as_ref(), &self.handset_firmware, self.options)?;
        fresh.default_boot_mode = self.default_boot_mode;
        if let (Some(image), Some(main)) = (eeprom.as_ref(), fresh.main.as_ref()) {
            main.eeprom.load_image(image).map_err(|e| e.to_string())?;
        }
        if let (Some(image), Some(main)) = (nor.as_ref(), fresh.main.as_mut()) {
            let id = main.ids.qspi;
            if let Some(qspi) = main.board.get_mut::<NgcQuadSpi>(id) {
                qspi.load_image(image).map_err(|e| e.to_string())?;
            }
        }
        fresh.restore_hook = self.restore_hook.take();
        fresh.shutdown_hook = self.shutdown_hook.take();
        fresh.apply_boot_fixtures()?;
        *self = fresh;
        Ok(())
    }

    /// The viewer's "Wake system": restart with the handset-wake boot fixture.
    pub fn wake(&mut self) -> Result<(), String> {
        self.restart(Some(BootMode::HandsetWake))
    }

    /// The viewer's "Cold": restart with zero wake flags.
    pub fn cold(&mut self) -> Result<(), String> {
        self.restart(Some(BootMode::Cold))
    }

    /// The runner's serial-number fixture: writes the 32-bit serial at EEPROM offset 0 (little endian, a
    /// synthetic fixture: it does not identify a physical unit) and restarts. The EEPROM must have been
    /// initialized by a first boot (validity marker `0xA3` at offset 254).
    pub fn set_serial_number(&mut self, serial: u32) -> Result<(), String> {
        let main = self.main.as_ref().ok_or("Serial fixture requires the dual system")?;
        let marker = main.eeprom.get_byte(fixtures::EEPROM_VALIDITY_OFFSET).map_err(|e| e.to_string())?;
        if marker != fixtures::EEPROM_VALIDITY_MARKER {
            return Err("Wait for the first boot to initialize EEPROM before changing the emulated serial".to_string());
        }
        main.eeprom.set_double_word(0, serial).map_err(|e| e.to_string())?;
        main.eeprom.flush();
        self.restart(None)
    }

    /// The RTC state `emulation/rtc_persistence.py` keeps (TR, DR, PRER, CR.FMT and the 20 backup words) of
    /// a board, read without side effects.
    pub fn rtc_checkpoint(&self, which: Which) -> Option<RtcCheckpoint> {
        let id = match which {
            Which::Main => self.main.as_ref()?.ids.rtc,
            Which::Handset => self.handset.ids.rtc,
        };
        self.board(which)?.get::<Rtc>(id).map(Rtc::checkpoint)
    }

    /// Restores an RTC checkpoint through the protected register sequence (call it from the restore hook,
    /// before the wake fixture).
    pub fn restore_rtc_checkpoint(&mut self, which: Which, checkpoint: &RtcCheckpoint) -> Result<(), String> {
        let id = match which {
            Which::Main => self.main.as_ref().ok_or("no main board")?.ids.rtc,
            Which::Handset => self.handset.ids.rtc,
        };
        let board = self.board_mut(which).ok_or("no such board")?;
        board.with_peripheral::<Rtc, _>(id, |rtc, ctx| rtc.restore_checkpoint(ctx, checkpoint)).ok_or("RTC missing")?
    }

    /// The emulated serial number: the EEPROM double word at offset 0 (`serialNumber` of the runner state).
    pub fn serial_number(&self) -> Option<u32> {
        self.main.as_ref().and_then(|main| main.eeprom.get_double_word(0).ok())
    }

    pub fn config(&self) -> &SystemConfig {
        &self.config
    }

    pub fn mode(&self) -> Mode {
        self.config.mode
    }

    pub fn boot_mode(&self) -> BootMode {
        self.config.boot_mode
    }

    /// Virtual time of the system (a whole number of quanta).
    pub fn time(&self) -> Time {
        self.time
    }

    pub fn seconds(&self) -> f64 {
        to_secs_f64(self.time)
    }

    pub fn stats(&self) -> SystemStats {
        self.stats
    }

    /// True once the firmware requested standby (both CPUs halted).
    pub fn is_standby(&self) -> bool {
        self.standby_time.is_some()
    }

    pub fn standby_time(&self) -> Option<Time> {
        self.standby_time
    }

    /// The error that stopped the run, if any (terminal handler, system reset request).
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Clears an error stop so that running can continue (the viewer's Resume).
    pub fn clear_error(&mut self) {
        self.error = None;
    }

    /// True when `run_*` makes progress: not in standby and no error stop.
    pub fn can_run(&self) -> bool {
        self.standby_time.is_none() && self.error.is_none()
    }

    /// `handsetPowered` of the runner state: the main board has enabled the handset supply (or both
    /// CPUs start together).
    pub fn handset_powered(&self) -> bool {
        self.handset_released
    }

    /// Time of the poll that released the handset CPU (`handsetReleaseTime`).
    pub fn handset_release_time(&self) -> Option<Time> {
        self.handset_release_time
    }

    pub fn inputs(&self) -> &Inputs {
        &self.inputs
    }

    // ---- running ----------------------------------------------------------------------------

    /// Runs until virtual time `target` (rounded up to a quantum boundary). Stops early on standby or
    /// an error stop. Returns the time reached.
    pub fn run_until(&mut self, target: Time) -> Time {
        let target = round_up_to_quantum(target);
        while self.time < target && self.can_run() {
            self.step_quantum();
        }
        self.time
    }

    /// Like [`run_until`](Self::run_until), but an error stop (terminal handler) does not end the call: the runner's
    /// Step and Advance execute even after the firmware stopped in its error loop. Standby still ends it.
    pub fn run_until_ignoring_error(&mut self, target: Time) -> Time {
        let target = round_up_to_quantum(target);
        while self.time < target && self.standby_time.is_none() {
            self.step_quantum();
        }
        self.time
    }

    /// Runs for `duration` of virtual time (rounded up to whole quanta).
    pub fn run_for(&mut self, duration: Time) -> Time {
        self.run_until(self.time + duration)
    }

    /// Runs for `seconds` of virtual time.
    pub fn run_for_secs(&mut self, seconds: f64) -> Time {
        self.run_for(from_secs_f64(seconds))
    }

    /// [`run_for_secs`](Self::run_for_secs) that ignores an error stop (see
    /// [`run_until_ignoring_error`](Self::run_until_ignoring_error)).
    pub fn run_for_secs_ignoring_error(&mut self, seconds: f64) -> Time {
        self.run_until_ignoring_error(self.time + from_secs_f64(seconds))
    }

    /// One 100 us quantum: main, handset, CAN pump, due inputs, deferred handset release, polls.
    pub fn step_quantum(&mut self) {
        let next = self.time + QUANTUM;
        // Renode `RequestReset` (AIRCR.SYSRESETREQ, IWDG expiry) is queued and executed at the nearest synchronized
        // state: the CPU keeps running until the end of its quantum, then `Machine.Reset()` runs in the sync phase.
        let mut requests: [Option<(ResetCause, Time)>; 2] = [None, None];
        if let Some(main) = self.main.as_mut() {
            bus::access_trace::set_tag(TAG_MAIN);
            if let Some(at) = run_board_to(&mut main.board, next) {
                requests[0] = Some((ResetCause::SystemResetRequest, at));
            }
        }
        bus::access_trace::set_tag(TAG_HANDSET);
        if let Some(at) = run_board_to(&mut self.handset.board, next) {
            requests[1] = Some((ResetCause::SystemResetRequest, at));
        }
        self.time = next;
        self.stats.quanta += 1;
        self.sample_outputs();
        self.collect_watchdog_requests(&mut requests);
        if self.main.is_some() {
            self.pump_can();
        }
        if !self.pending_inputs.is_empty() {
            self.apply_due_inputs();
        }
        if let Some(at) = self.handset_start_at {
            if self.time >= at {
                self.handset_start_at = None;
                self.handset.board.set_halted(false);
            }
        }
        for (index, request) in requests.into_iter().enumerate() {
            if let Some((cause, requested_at)) = request {
                self.machine_reset(if index == 0 { Which::Main } else { Which::Handset }, cause, requested_at);
            }
        }
        if self.time % POLL_INTERVAL == 0 {
            self.poll_fixtures();
        }
    }

    /// Output activity sampling at the quantum boundary: the HUD command history every 50 virtual ms (main), the vibrator PWM
    /// command history every 20 virtual ms (handset). `main` samples with Renode managed threads; here the registers are
    /// read through side-effect-free peeks at the boundary, so no clock entry, timer event or chunk split is added to a
    /// board and the guest is untouched (the histories are a function of the guest state at the boundaries).
    fn sample_outputs(&mut self) {
        let hud_due = self.time % HUD_SAMPLE_INTERVAL == 0;
        let vibrator_due = self.time % VIBRATOR_SAMPLE_INTERVAL == 0;
        if !hud_due && !vibrator_due {
            return;
        }
        let seconds = to_secs_f64(self.time);
        if hud_due {
            if let Some(main) = self.main.as_mut() {
                let commands = telemetry::read_hud_commands(&|address| main.board.peek32(address).unwrap_or(0));
                let id = main.ids.telemetry;
                if let Some(telemetry) = main.board.get_mut::<Telemetry>(id) {
                    telemetry.record_hud(commands, seconds);
                }
            }
        }
        if vibrator_due {
            let board = &mut self.handset.board;
            let command = telemetry::read_vibrator_command(&|address| board.peek32(address).unwrap_or(0));
            if let Some(telemetry) = board.get_mut::<Telemetry>(self.handset.ids.telemetry) {
                telemetry.record_vibrator(command, seconds);
            }
        }
    }

    /// Takes the watchdog reset requests recorded during the quantum (the first request of a board wins, like the
    /// idempotent `RequestReset`).
    fn collect_watchdog_requests(&mut self, requests: &mut [Option<(ResetCause, Time)>; 2]) {
        if let Some(main) = self.main.as_mut() {
            let id = main.ids.iwdg;
            if let Some(at) = main.board.get_mut::<Iwdg>(id).and_then(Iwdg::take_reset_request) {
                requests[0].get_or_insert((ResetCause::Watchdog, at));
            }
        }
        let id = self.handset.ids.iwdg;
        if let Some(at) = self.handset.board.get_mut::<Iwdg>(id).and_then(Iwdg::take_reset_request) {
            requests[1].get_or_insert((ResetCause::Watchdog, at));
        }
    }

    /// Renode `Machine.Reset()` of one board (`Machine.cs`: `RequestReset` -> `LocalTimeSource.ExecuteInNearestSyncedState`
    /// -> `Reset`), applied at the quantum boundary:
    ///
    /// * **reset**: the CPU (`CortexM.Reset`: registers, NVIC, SysTick, SCB; the executed-instruction counter restarts)
    ///   and, in registration order, every peripheral through its `reset` (timers, USART/CAN/I2C controllers, GPIO,
    ///   EXTI, LCD, ADCs, the RTC without its backup registers, the IWDG...);
    /// * **kept**: all of flash and SRAM (`MappedMemory.Reset()` does nothing), the `ArrayMemory` register stores
    ///   (PWR, flash control, FMC, SYSCFG: `ArrayMemory.Reset()` does nothing), the EEPROM and NOR contents, the RTC
    ///   backup registers, the CAN link and its trace, the UART capture, the virtual time;
    /// * **not re-run**: the `.resc` script; instead [`ResetStart`] decides the CPU start state.
    ///
    /// The RTC calendar is restored from a checkpoint (`SystemConfig::reset_keeps_rtc`, see there).
    fn machine_reset(&mut self, which: Which, cause: ResetCause, requested_at: Time) {
        let checkpoint = if self.reset_keeps_rtc { self.rtc_checkpoint(which) } else { None };
        let start = self.reset_start;
        let index = if which == Which::Main { 0 } else { 1 };
        let Some(board) = self.board_mut(which) else { return };
        // The nvic and cpu are declared first in the .repl, so they reset before the other peripherals.
        board.cpu.reset();
        board.clear_lockup();
        board.clear_reset_request();
        board.core.reset_all();
        board.sync_irqs();
        let base = board.cpu.instructions();
        match start {
            ResetStart::ApplicationVectors => {
                let vtor = handset::VECTOR_TABLE;
                let sp = board.peek(vtor, Width::Word).unwrap_or(handset::INITIAL_SP);
                let pc = board.peek(vtor + 4, Width::Word).unwrap_or(handset::RESET_PC | 1);
                // The .resc fixtures that Machine.Reset() does not repeat: the CAN controller starts awake, and the main
                // board's I2C idle lines are pulled high.
                board.bus_write(handset::CAN_MCR_ADDRESS, Width::Word, handset::CAN_MCR_BOOT_VALUE);
                board.cpu.set_vtor(vtor);
                board.cpu.set_sp(sp);
                board.cpu.set_pc(pc);
            }
            ResetStart::RenodeLiteral => {
                // CortexM.InitPCAndSP with VTOR 0: both words read 0 from the unmapped address; the core then locks up.
                board.cpu.set_vtor(0);
                board.cpu.set_sp(0);
                board.cpu.set_pc(0);
                board.set_halted(true);
            }
        }
        self.instruction_base[index] = base;
        if which == Which::Main && self.options.main_i2c_idle_high {
            self.apply_main_input_fixtures();
        }
        if let Some(checkpoint) = checkpoint {
            let _ = self.restore_rtc_checkpoint(which, &checkpoint);
        }
        if self.reset_log.len() == RESET_LOG_CAPACITY {
            self.reset_log.remove(0);
        }
        self.reset_log.push(ResetEvent { board: which, cause, requested_at, applied_at: self.time });
        self.machine_reset_count += 1;
    }

    /// The machine resets that ran, oldest first (the newest [`RESET_LOG_CAPACITY`]).
    pub fn reset_log(&self) -> &[ResetEvent] {
        &self.reset_log
    }

    /// The platform-script options this system was built with.
    pub fn options(&self) -> BuildOptions {
        self.options
    }

    /// Where the CPU starts after a machine reset (default [`ResetStart::ApplicationVectors`]).
    pub fn set_reset_start(&mut self, start: ResetStart) {
        self.reset_start = start;
    }

    pub fn reset_start(&self) -> ResetStart {
        self.reset_start
    }

    /// Whether a machine reset restores the RTC calendar from a checkpoint (default true, see the field).
    pub fn set_reset_keeps_rtc(&mut self, keep: bool) {
        self.reset_keeps_rtc = keep;
    }

    fn pump_can(&mut self) {
        // Main before handset: the tie-break of frames with equal stamps (no allocation per quantum).
        let (Some(main), Some(handset)) = (self.main_endpoint, self.handset_endpoint) else { return };
        let order = [main, handset];
        let mut ports = BoardPorts {
            main: self.main.as_mut(),
            handset: &mut self.handset,
            main_endpoint: self.main_endpoint,
            handset_endpoint: self.handset_endpoint,
        };
        let delivered = self.link.pump(&order, &mut ports);
        self.stats.can_deliveries += delivered as u64;
    }

    /// `run_interval` after `RunFor`: the standby check, then the PE3 power-gate release; also the viewer's
    /// error stop for the handset terminal handler.
    fn poll_fixtures(&mut self) {
        self.stats.polls += 1;
        if let Some(main) = self.main.as_mut() {
            if fixtures::standby_requested(&main.board) {
                main.board.set_halted(true);
                self.handset.board.set_halted(true);
                self.handset_released = false;
                self.handset_start_at = None;
                self.standby_time = Some(self.time);
                return;
            }
            if !self.handset_released && fixtures::handset_supply_enabled(&main.board) {
                // `cpu IsHalted false` only takes effect at the next unlatch: the handset executes its first
                // instruction one quantum after the poll.
                self.handset_released = true;
                self.handset_release_time = Some(self.time);
                self.handset_start_at = Some(self.time + QUANTUM);
            }
        }
        if let Some(pc) = self.terminal_handler_pc().filter(|pc| self.handset.pc() == *pc) {
            self.error = Some(format!("Firmware stopped in a terminal handler at 0x{pc:08x}; inspect the log."));
        }
    }

    /// The release this system runs (the firmware-specific addresses and fixtures).
    pub fn release(&self) -> &'static Release {
        self.release
    }

    /// The handset PC of the interrupts-disabled error loop of this release (`None` when the release has no proven one).
    pub fn terminal_handler_pc(&self) -> Option<u32> {
        self.release.addresses.handset_error_loop.address()
    }

    /// The firmware-specific fields the state document cannot give for this release, with the reason: `(field, reason)`.
    pub fn unavailable_fields(&self) -> Vec<(&'static str, &'static str)> {
        let addresses = &self.release.addresses;
        let mut fields = Vec::new();
        if addresses.main_battery_ready.address().is_none() && self.main.is_some() {
            fields.push(("mainBatteryReady", addresses.main_battery_ready.reason().unwrap_or("")));
        }
        if addresses.handset_error_loop.address().is_none() {
            fields.push(("terminalHandlerDetection", addresses.handset_error_loop.reason().unwrap_or("")));
        }
        if addresses.handset_orientation.address().is_none() {
            fields.push(("navigationOrientation", addresses.handset_orientation.reason().unwrap_or("")));
        }
        fields
    }

    /// Machine resets since this system was built (each clears the output activity histories).
    pub fn machine_reset_count(&self) -> u64 {
        self.machine_reset_count
    }

    /// The viewer's Advance: at most 20 virtual seconds.
    pub fn advance(&mut self, seconds: f64) -> Result<Time, String> {
        if !seconds.is_finite() || seconds <= 0.0 || seconds > 20.0 {
            return Err("Advance must be between zero and 20 virtual seconds".to_string());
        }
        Ok(self.run_for_secs(seconds))
    }

    /// The viewer's Step: two 50 ms intervals.
    pub fn step(&mut self) -> Time {
        self.run_for(2 * POLL_INTERVAL)
    }

    /// Idle fast-forward on or off for both cores.
    pub fn set_idle_fast_forward(&mut self, enabled: bool) {
        self.config.idle_fast_forward = enabled;
        self.handset.board.cpu.set_idle_fast_forward(enabled);
        if let Some(main) = self.main.as_mut() {
            main.board.cpu.set_idle_fast_forward(enabled);
        }
    }

    /// Routine acceleration mode for both cores (results are identical in every mode; see `armv7m::accel`).
    pub fn set_routine_accel(&mut self, mode: RoutineAccelMode) {
        self.config.routine_accel = mode;
        self.handset.board.cpu.set_routine_accel(mode);
        if let Some(main) = self.main.as_mut() {
            main.board.cpu.set_routine_accel(mode);
        }
    }

    pub fn routine_accel_mode(&self) -> RoutineAccelMode {
        self.config.routine_accel
    }

    /// Counters of the routine acceleration of one core.
    pub fn routine_accel_stats(&self, which: Which) -> Option<RoutineAccelStats> {
        self.board(which).map(|board| board.cpu.routine_accel_stats())
    }

    /// Digest of everything routine acceleration must leave exactly as interpretation would (registers, flags, FPSCR, VFP
    /// registers, retire counts, the predecode cache and the cut-block history) for one core: `Cpu::exactness_digest`.
    pub fn exactness_digest(&self, which: Which) -> Option<u64> {
        self.board(which).map(|board| board.cpu.exactness_digest())
    }

    // ---- inputs -----------------------------------------------------------------------------

    /// Queues an input to be applied at the first quantum boundary at or after `at` (after the CAN pump).
    pub fn schedule_input(&mut self, at: Time, input: Input) {
        let index = self.pending_inputs.partition_point(|(time, _)| *time <= at);
        self.pending_inputs.insert(index, (at, input));
    }

    /// Applies an input now. Between `run_*` calls the system is at a quantum boundary, so this is the
    /// same as scheduling it for the current time.
    pub fn apply_input(&mut self, input: &Input) -> Result<(), String> {
        let result = self.apply_input_inner(input);
        if result.is_ok() {
            self.stats.inputs_applied += 1;
        }
        result
    }

    fn apply_due_inputs(&mut self) {
        while let Some((at, _)) = self.pending_inputs.first() {
            if *at > self.time {
                break;
            }
            let (at, input) = self.pending_inputs.remove(0);
            if let Err(error) = self.apply_input(&input) {
                self.input_errors.push((at, error));
            }
        }
    }

    /// Errors of queued inputs that failed when they became due `(scheduled time, message)`.
    pub fn input_errors(&self) -> &[(Time, String)] {
        &self.input_errors
    }

    fn apply_input_inner(&mut self, input: &Input) -> Result<(), String> {
        match input {
            Input::Navigate { up } => {
                self.require_handset_powered()?;
                let entry = &self.release.addresses.handset_orientation;
                let Some(address) = entry.address() else {
                    return Err(format!("Up/Down are unavailable for {}: {}", self.release.id, entry.reason().unwrap_or("the display orientation byte is unknown")));
                };
                let orientation = self.handset.board.peek(address, Width::Byte).unwrap_or(0) as u8;
                let mask = fixtures::navigation_mask(*up, orientation);
                self.press_buttons(mask)
            }
            Input::Confirm => {
                self.require_handset_powered()?;
                self.confirm_buttons()
            }
            Input::Press { mask } => {
                self.require_handset_powered()?;
                self.press_buttons(*mask)
            }
            Input::Pulse { mask, duration_us } => {
                self.require_handset_powered()?;
                self.pulse_buttons(*mask, *duration_us)
            }
            Input::CanConnected(connected) => {
                if self.main.is_none() {
                    return Err("CAN controls require the dual system".to_string());
                }
                self.link.set_connected(*connected);
                Ok(())
            }
            Input::CanDropId(id) => {
                if self.main.is_none() {
                    return Err("CAN controls require the dual system".to_string());
                }
                if !(-1..=2047).contains(id) {
                    return Err("dropId must be -1 or a standard CAN ID (0..2047)".to_string());
                }
                self.link.set_drop_id(*id);
                Ok(())
            }
            Input::Sensors(updates) => {
                if self.main.is_none() {
                    return Err("Sensor controls require the dual system".to_string());
                }
                let mut values = self.inputs.clone();
                values.apply_json(updates)?;
                self.inputs = values;
                self.apply_sensor_inputs()
            }
        }
    }

    fn require_handset_powered(&self) -> Result<(), String> {
        if self.main.is_some() && !self.handset_released {
            return Err("The main board has not enabled the handset supply yet".to_string());
        }
        Ok(())
    }

    /// Runs `f` on the handset button model (`NGCHandsetButtons`); its errors are the C# exceptions.
    fn with_buttons<R>(&mut self, f: impl FnOnce(&mut Buttons) -> Result<R, ButtonsError>) -> Result<R, String> {
        let id = self.handset.ids.buttons;
        let buttons = self.handset.board.get_mut::<Buttons>(id).ok_or("the handset button model is missing")?;
        f(buttons).map_err(|e| e.to_string())
    }

    /// `buttons Press mask`.
    fn press_buttons(&mut self, mask: u32) -> Result<(), String> {
        self.with_buttons(|buttons| buttons.press(mask))
    }

    /// `buttons Confirm`: two overlapping 204.8 ms pulses staggered by 50 virtual ms.
    fn confirm_buttons(&mut self) -> Result<(), String> {
        self.with_buttons(Buttons::confirm)
    }

    /// `buttons Pulse mask duration`.
    fn pulse_buttons(&mut self, mask: u32, duration_us: u32) -> Result<(), String> {
        self.with_buttons(|buttons| buttons.pulse(mask, duration_us))
    }

    /// `buttons Summary` (the runner's `buttonSummary`).
    pub fn button_summary(&self) -> String {
        self.handset.board.get::<Buttons>(self.handset.ids.buttons).map(Buttons::summary_text).unwrap_or_default()
    }

    /// The runner's `hardwareOutputs`: the handset's backlight and vibrator, then (dual) the three main HUD
    /// channels, as computed by the telemetry models from side-effect-free register reads. The viewer adds
    /// the LED color labels (`color`) itself.
    pub fn hardware_outputs(&self) -> Json {
        let mut outputs = Vec::new();
        let boards = [Some((&self.handset.board, self.handset.ids.telemetry)), self.main.as_ref().map(|m| (&m.board, m.ids.telemetry))];
        for (board, id) in boards.into_iter().flatten() {
            if let Some(telemetry) = board.get::<Telemetry>(id) {
                if let Ok(Json::Array(items)) = Json::parse(&telemetry.outputs_json_for_board(board)) {
                    outputs.extend(items);
                }
            }
        }
        Json::Array(outputs)
    }

    // The sensor inputs of the main board (`NGCMainADC`, `NGCMS5837`) are applied by `apply_sensor_inputs`.

    /// Pushes `self.inputs` into the main board's ADC and pressure sensors (runner `apply_inputs`, same
    /// order: battery banks, oxygen cells, pressure sensors, then the ADC acquisition settings).
    fn apply_sensor_inputs(&mut self) -> Result<(), String> {
        let Some(main) = self.main.as_mut() else { return Ok(()) };
        let inputs = &self.inputs;
        let (adc_id, i2c1, i2c2) = (main.ids.adc, main.ids.i2c1, main.ids.i2c2);
        // Renode's monitor parsed the runner's double arguments through f32, so the C# models received
        // quantized values; round the same way to stay bit identical at rounding boundaries.
        let quantized = |value: f64| f64::from(value as f32);
        {
            let adc = main.board.get_mut::<NgcMainAdc>(adc_id).ok_or("main adc missing")?;
            for bank in 0..2usize {
                adc.set_battery_millivolts(bank as u32, quantized(inputs.battery_mv[bank])).map_err(|e| e.to_string())?;
            }
            for cell in 0..3usize {
                adc.set_oxygen_millivolts(cell as u32, quantized(inputs.oxygen_mv[cell])).map_err(|e| e.to_string())?;
            }
        }
        for (index, controller) in [(0usize, i2c1), (1usize, i2c2)] {
            let i2c = main.board.get_mut::<Stm32F7I2c>(controller).ok_or("main i2c missing")?;
            let sensor = i2c
                .target_mut::<NgcMs5837>(main_board::PRESSURE_SENSOR_ADDRESS)
                .ok_or_else(|| format!("main pressure sensor {} missing", index + 1))?;
            sensor.set_pressure_mbar(quantized(inputs.pressure_mbar[index])).map_err(|e| e.to_string())?;
            sensor.set_temperature_celsius(quantized(inputs.temperature_c[index])).map_err(|e| e.to_string())?;
            sensor.set_use_maximum_conversion_time(inputs.pressure_maximum_timing);
        }
        let adc = main.board.get_mut::<NgcMainAdc>(adc_id).ok_or("main adc missing")?;
        adc.set_acquisition_enabled(inputs.acquisition_enabled);
        adc.set_initial_acquisition_delay_us(inputs.acquisition_delay_us);
        adc.set_noise_amplitude_raw(inputs.noise_amplitude_raw);
        adc.set_noise_seed(inputs.noise_seed);
        Ok(())
    }

    // ---- observation ------------------------------------------------------------------------

    /// The board of `which` (the main board only exists in the dual system).
    pub fn board(&self, which: Which) -> Option<&Board<Cpu>> {
        match which {
            Which::Main => self.main.as_ref().map(|m| &m.board),
            Which::Handset => Some(&self.handset.board),
        }
    }

    pub fn board_mut(&mut self, which: Which) -> Option<&mut Board<Cpu>> {
        match which {
            Which::Main => self.main.as_mut().map(|m| &mut m.board),
            Which::Handset => Some(&mut self.handset.board),
        }
    }

    /// Executed instructions of a board (`cpu ExecutedInstructions`): since the start or the last machine reset,
    /// whose `tlib_reset` clears the counter.
    pub fn instructions(&self, which: Which) -> Option<u64> {
        self.board(which).map(|b| b.cpu.instructions() - self.instruction_base[which as usize])
    }

    /// Sends a classical CAN frame from the handset's controller the way the `NGCCANStimulus` diagnostic pump does
    /// (`oxygen-air-test/NGCCANStimulus.cs`): `TDT0R`, `TDL0R`, `TDH0R` and `TI0R` (with `TXRQ`) of the idle transmit
    /// mailbox 0 are written, so the frame passes through the controller, the CAN link (trace, `Connected`, `DropId`)
    /// and the main controller's filters and FIFO exactly like firmware traffic. Done at the current quantum boundary.
    pub fn inject_can_from_handset(&mut self, id: u32, data: &[u8]) -> Result<(), String> {
        if id > 0x7FF || data.len() > 8 {
            return Err("Expected standard classical CAN ID and up to eight payload bytes".to_string());
        }
        let mut padded = [0u8; 8];
        padded[..data.len()].copy_from_slice(data);
        let base = handset::CAN_MCR_ADDRESS;
        let board = &mut self.handset.board;
        board.bus_write(base + 0x184, Width::Word, data.len() as u32);
        board.bus_write(base + 0x188, Width::Word, u32::from_le_bytes([padded[0], padded[1], padded[2], padded[3]]));
        board.bus_write(base + 0x18C, Width::Word, u32::from_le_bytes([padded[4], padded[5], padded[6], padded[7]]));
        board.bus_write(base + 0x180, Width::Word, (id << 21) | 1);
        Ok(())
    }

    /// The main firmware this system was built from (dual system only).
    pub fn main_firmware(&self) -> Option<&Firmware> {
        self.main_firmware.as_ref()
    }

    pub fn handset_firmware(&self) -> &Firmware {
        &self.handset_firmware
    }

    /// `adc Summary` of the main board.
    pub fn adc_summary(&self) -> Option<String> {
        let main = self.main.as_ref()?;
        main.board.get::<NgcMainAdc>(main.ids.adc).map(NgcMainAdc::describe)
    }

    /// `qspi Summary` of the main board.
    pub fn flash_summary(&self) -> Option<String> {
        let main = self.main.as_ref()?;
        main.board.get::<NgcQuadSpi>(main.ids.qspi).map(NgcQuadSpi::describe)
    }

    /// `gpioB OnGPIO 6|7|10|11 true` of `main.resc`.
    fn apply_main_input_fixtures(&mut self) {
        if let Some(main) = self.main.as_mut() {
            let gpio_b = main.ids.gpio[1];
            for pin in MAIN_I2C_IDLE_HIGH_PINS {
                main.board.set_input(gpio_b, pin, true);
            }
        }
    }

    pub fn pc(&self, which: Which) -> Option<u32> {
        self.board(which).map(|b| b.cpu.pc())
    }

    /// Instruction, slice, event and idle-skip counters of one board.
    pub fn counters(&self, which: Which) -> Option<BoardCounters> {
        let board = self.board(which)?;
        let stats = board.stats();
        Some(BoardCounters {
            instructions: board.cpu.instructions() - self.instruction_base[which as usize],
            slices: stats.slices,
            events_fired: stats.events_fired,
            idle_jumps: stats.idle_jumps,
            stop_requests: stats.stop_requests,
            fast_forward: board.cpu.fast_forward_stats(),
        })
    }

    /// Runs `f` on the handset LCD model (frame buffer for the browser, geometry, counters).
    pub fn with_lcd<R>(&mut self, f: impl FnOnce(&mut NgcParallelLcd) -> R) -> Option<R> {
        let id = self.handset.ids.lcd;
        self.handset.board.get_mut::<NgcParallelLcd>(id).map(f)
    }

    /// The handset LCD as a binary PPM: the byte-identical equivalent of the Renode model's `SavePPM`
    /// (it first brings the visible buffer up to date, which has no guest-visible effect).
    pub fn lcd_ppm(&mut self) -> Option<Vec<u8>> {
        self.with_lcd(|lcd| lcd.export_ppm())
    }

    /// The LCD model's summary line (`lcd Summary`).
    pub fn lcd_summary(&self) -> String {
        self.handset.board.get::<NgcParallelLcd>(self.handset.ids.lcd).map(NgcParallelLcd::describe).unwrap_or_default()
    }

    /// Application variables of the main firmware (side-effect-free reads of main RAM).
    pub fn main_application(&self) -> Option<MainApplication> {
        let board = &self.main.as_ref()?.board;
        let addresses = &self.release.addresses;
        let byte = |entry: &firmware::AddressEntry| entry.address().map(|address| board.peek(address, Width::Byte).unwrap_or(0));
        let word = |entry: &firmware::AddressEntry| entry.address().map(|address| board.peek(address, Width::Word).unwrap_or(0));
        Some(MainApplication {
            wake_cause: byte(&addresses.main_wake_cause),
            screen_mode: byte(&addresses.main_screen_mode),
            main_mode: byte(&addresses.main_mode),
            battery_ready: byte(&addresses.main_battery_ready),
            hal_tick: word(&addresses.main_hal_tick),
            pressure: word(&addresses.main_pressure),
            temperature: word(&addresses.main_temperature),
        })
    }

    /// `mainBatteryReady` of the runner state: `None` when this release has no proven address for the readiness byte.
    pub fn main_battery_ready_flag(&self) -> Option<bool> {
        let address = self.release.addresses.main_battery_ready.address()?;
        self.main.as_ref().map(|m| m.board.peek(address, Width::Byte).is_some_and(|value| value != 0))
    }

    /// [`main_battery_ready_flag`](Self::main_battery_ready_flag) with an unavailable flag read as `false`.
    pub fn main_battery_ready(&self) -> bool {
        self.main_battery_ready_flag().unwrap_or(false)
    }

    /// `ICSR`, `CFSR`, `HFSR` of a board.
    pub fn fault_registers(&self, which: Which) -> Option<[(&'static str, u32); 3]> {
        let board = self.board(which)?;
        let mut out = fixtures::FAULT_REGISTERS;
        for entry in out.iter_mut() {
            entry.1 = board.peek(entry.1, Width::Word).unwrap_or(0);
        }
        Some(out)
    }

    /// Records the next `capacity` executed instruction addresses of a board (4 bytes each). While a
    /// trace is armed the idle fast-forward of that board is off so the trace has every instruction;
    /// `take_pc_trace` stops tracing and re-arms it.
    pub fn trace_pcs(&mut self, which: Which, capacity: usize) -> bool {
        match self.board_mut(which) {
            Some(board) => {
                board.cpu.trace_pcs(capacity);
                true
            }
            None => false,
        }
    }

    /// Records full trace entries (retire count, address, encoding) of the next `capacity` instructions.
    pub fn trace_entries(&mut self, which: Which, capacity: usize) -> bool {
        match self.board_mut(which) {
            Some(board) => {
                board.cpu.trace_to_buffer(capacity, false);
                true
            }
            None => false,
        }
    }

    /// Stops PC tracing and returns the recorded addresses.
    pub fn take_pc_trace(&mut self, which: Which) -> Vec<u32> {
        self.board_mut(which).map(|board| board.cpu.trace_take_pcs()).unwrap_or_default()
    }

    /// Stops entry tracing and returns the recorded entries.
    pub fn take_trace_entries(&mut self, which: Which) -> Vec<TraceEntry> {
        self.board_mut(which).map(|board| board.cpu.trace_take()).unwrap_or_default()
    }

    /// SHA-256 over the architectural and memory state of the system: registers, retire counts, both SRAMs of
    /// each board, the CAN trace and the UART tails. Used to prove that two runs (for example idle
    /// fast-forward on and off) are identical.
    pub fn fingerprint(&self) -> String {
        let mut hash = self.guest_hasher();
        // The output activity histories (they are derived from the guest state at the quantum boundaries, so two runs that
        // keep the guest identical must keep them identical too).
        if let Some(main) = self.main.as_ref() {
            if let Some(telemetry) = main.board.get::<Telemetry>(main.ids.telemetry) {
                hash.update(telemetry.history_digest_text().as_bytes());
            }
        }
        if let Some(telemetry) = self.handset.board.get::<Telemetry>(self.handset.ids.telemetry) {
            hash.update(telemetry.history_digest_text().as_bytes());
        }
        crate::sha256::to_hex(&hash.finalize())
    }

    /// [`fingerprint`](Self::fingerprint) of the guest state alone (registers, RAM, LCD, CAN trace, UART tails), without the
    /// output histories: the digest older engine builds produced, which a change to the observation code must not alter.
    pub fn guest_fingerprint(&self) -> String {
        crate::sha256::to_hex(&self.guest_hasher().finalize())
    }

    fn guest_hasher(&self) -> Sha256 {
        let mut hash = Sha256::new();
        hash.update(&self.time.to_le_bytes());
        for which in [Which::Main, Which::Handset] {
            let Some(board) = self.board(which) else { continue };
            hash.update(which.name().as_bytes());
            hash.update(&board.cpu.instructions().to_le_bytes());
            let snapshot = board.cpu.snapshot();
            for register in snapshot.r {
                hash.update(&register.to_le_bytes());
            }
            for word in [snapshot.xpsr, snapshot.msp, snapshot.psp, snapshot.control, snapshot.ipsr, u32::from(snapshot.itstate), u32::from(snapshot.basepri)] {
                hash.update(&word.to_le_bytes());
            }
            hash.update(&[u8::from(snapshot.primask), u8::from(snapshot.faultmask)]);
            for base in [crate::memory::SRAM1_BASE, crate::memory::SRAM2_BASE] {
                let size = if base == crate::memory::SRAM1_BASE { crate::memory::SRAM1_SIZE } else { crate::memory::SRAM2_SIZE };
                if let Some(bytes) = board.memory_slice(base, size as usize) {
                    hash.update(bytes);
                }
            }
        }
        if let Some(lcd) = self.handset.board.get::<NgcParallelLcd>(self.handset.ids.lcd) {
            // The GRAM is pure emulated state; the visible buffer depends on when the host last synced it.
            let pixels = lcd.gram_rgb565();
            hash.update(&[lcd.width() as u8, (lcd.width() >> 8) as u8, lcd.height() as u8, (lcd.height() >> 8) as u8]);
            let mut bytes = Vec::with_capacity(pixels.len() * 2);
            for pixel in pixels {
                bytes.extend_from_slice(&pixel.to_le_bytes());
            }
            hash.update(&bytes);
        }
        hash.update(self.link.trace_text().as_bytes());
        for stream in self.uart.snapshot() {
            hash.update(stream.name.as_bytes());
            hash.update(stream.hex.as_bytes());
        }
        hash
    }

    /// A short state summary (the runner's `snapshot()` fields that exist without the full state package).
    pub fn status_json(&self) -> Json {
        let mut json = Json::object()
            .with("engine", concat!("ngc-wasm/", env!("CARGO_PKG_VERSION")))
            .with("mode", if self.main.is_some() { "dual" } else { "handset" })
            .with("bootMode", self.config.boot_mode.name())
            .with("virtualNs", self.time)
            .with("virtualTime", to_secs_f64(self.time))
            .with("standby", self.is_standby())
            .with("standbyTime", self.standby_time.map(to_secs_f64))
            .with("error", self.error.as_deref())
            .with("handsetPowered", self.handset_released)
            .with("handsetReleaseTime", self.handset_release_time.map(to_secs_f64))
            .with("simultaneousStart", self.config.simultaneous_start)
            .with("idleFastForward", self.config.idle_fast_forward)
            .with("pc", u64::from(self.handset.pc()))
            .with("handsetInstructions", self.handset.instructions())
            .with("handsetLockup", self.handset.board.cpu.lockup_reason())
            .with("buttonSummary", self.button_summary())
            .with("hardwareOutputs", self.hardware_outputs());
        let lcd_summary = self.lcd_summary();
        if !lcd_summary.is_empty() {
            json.insert("lcdSummary", lcd_summary);
        }
        if let Some(main) = self.main.as_ref() {
            json.insert("mainPC", u64::from(main.pc()));
            json.insert("mainInstructions", main.instructions());
            json.insert("mainLockup", main.board.cpu.lockup_reason());
            json.insert("mainBatteryReady", self.main_battery_ready_flag());
            json.insert("canSummary", self.link.summary());
            json.insert("storageSummary", main.eeprom.summary_text());
            if let Some(adc) = main.board.get::<NgcMainAdc>(main.ids.adc) {
                json.insert("adcSummary", adc.describe());
            }
            if let Some(qspi) = main.board.get::<NgcQuadSpi>(main.ids.qspi) {
                json.insert("flashSummary", qspi.describe());
            }
            if let Some(app) = self.main_application() {
                json.insert("mainApplication", app.to_json());
            }
            json.insert("serialNumber", main.eeprom.get_double_word(0).map(u64::from).unwrap_or(0));
        }
        json
    }
}
