#![no_main]
#![no_std]
// #![allow(non_ascii_idents)] // todo: May no longer be required
#![allow(mixed_script_confusables)] // eg variable names that include greek letters.

use core::{
    f32::consts::TAU,
    sync::atomic::{AtomicBool, AtomicU32, Ordering},
};


use cfg_if::cfg_if;

use cortex_m::{self, asm};

use stm32_hal2::{
    self,
    adc::{Adc, AdcDevice},
    clocks::{self, Clk48Src, Clocks, CrsSyncSrc, InputSrc, PllCfg, PllSrc},
    debug_workaround,
    dma::{self, Dma, DmaChannel, DmaInterrupt},
    flash::Flash,
    gpio::{Edge, OutputSpeed, OutputType, Pin, PinMode, Port, Pull},
    i2c::{I2c, I2cConfig, I2cSpeed},
    pac::{self, DMA1, I2C1, I2C2, SPI1, SPI2, SPI3, TIM15},
    rtc::Rtc,
    spi::{BaudRate, Spi},
    timer::{OutputCompare, TimChannel, Timer, TimerConfig, TimerInterrupt},
    usart::Usart,

};

use defmt::println;

cfg_if! {
    if #[cfg(feature = "anyleaf-mercury-g4")] {
        use stm32_hal2::usb::{Peripheral, UsbBus, UsbBusType};

        use usbd_serial::{SerialPort, USB_CLASS_CDC};
        use usb_device::bus::UsbBusAllocator;
        use usb_device::prelude::*;
        use usbd_serial::{SerialPort, USB_CLASS_CDC};
    }
}

use usb_device::bus::UsbBusAllocator;
use usb_device::prelude::*;
use usbd_serial::{SerialPort, USB_CLASS_CDC};

#[cfg(feature = "matek-h743slim")]
use crate::{
    clocks::VosRange,
    pac::SPI4,
    power::{SupplyConfig, VoltageLevel},
};


use defmt_rtt as _; // global logger
use panic_probe as _;
use stm32_hal2::dma::DmaInput;

mod drivers;
mod flight_ctrls;
// mod osd;
mod pid;
mod pid_tuning;
mod protocols;
mod sensor_fusion;

// cfg_if! {
// if #[cfg(feature = "matek-h743slim")] {
use drivers::baro_dps310 as baro;
use drivers::gps_x as gps;
// use drivers::imu_icm42605 as imu;
use drivers::imu_ism330dhcx as imu;
// use drivers::osd_max7456 as osd;
use drivers::tof_vl53l1 as tof;

use protocols::{dshot, elrs};
// }
// }

use flight_ctrls::{
    ArmStatus, AutopilotStatus, CommandState, CtrlInputs, InputMap, InputMode, Params, RotorPower,
};

use pid::{CtrlCoeffGroup, PidDerivFilters, PidGroup};

// Due to the way USB serial is set up, the USB bus must have a static lifetime.
// In practice, we only mutate it at initialization.
static mut USB_BUS: Option<UsbBusAllocator<UsbBusType>> = None;

// Our DT timer speed, in Hz.
const DT_TIM_FREQ: u32 = 200_000_000;

// The frequency our motor-driving PWM operates at, in Hz.
// todo: Make this higher (eg 96kHz) after making sure the ESC
// const PWM_FREQ: f32 = 12_000.;

// Timer prescaler for rotor PWM. We leave this, and ARR constant, and explicitly defined,
// so we can set duty cycle appropriately for DSHOT.
// These are set for a 200MHz timer frequency.
// (PSC+1)*(ARR+1) = TIMclk/Updatefrequency = TIMclk * period.

// todo: On H7, will we get more precision with 400mhz tim clock, vice 200? Is that possible?

// Set up for DSHOT-600. (600k bits/second) So, timer frequency = 600kHz.
// todo: (PSC = 0, AAR = 332 results in a 600.6kHz update freq; not 600kHz exactly. Is that ok?)
// todo: Is this even what we want?

cfg_if! {
    if #[cfg(feature = "matek-h743slim")] {
        const DSHOT_PSC: u32 = 0;
        const DSHOT_ARR: u32 = 332;
    } else if #[cfg(feature = "anyleaf-mercury-g4")] {
        // 170Mhz tim clock. Results in 600.707kHz.
        const DSHOT_PSC: u32 = 0;
        const DSHOT_ARR: u32 = 282;
    }
}

// For 200Mhz, 32-bit timer:
// For CNT = us:
// PSC = (200Mhz / 1Mhz) - 1 = 199. ARR = (1<<32) - 1
// const DT_PSC: u32 = 199;
// const DT_ARR: u32 = u32::MAX - 1;

// The rate our main program updates, in Hz.
// Currently set to
const IMU_UPDATE_RATE: f32 = 8_000.;
const UPDATE_RATE: f32 = 1_600.; // IMU rate / 5.

// How many inner loop ticks occur between mid and outer loop.
// const OUTER_LOOP_RATIO: usize = 10;

const DT_IMU: f32 = 1. / IMU_UPDATE_RATE;
const DT: f32 = 1. / UPDATE_RATE;

// Speed in meters per second commanded by full power.
// todo: This may not be what you want; could be unachievable, or maybe you really want
// full speed.
// const V_FULL_DEFLECTION: f32 = 20.;

// Max distance from curent location, to point, then base a
// direct-to point can be, in meters. A sanity check
// todo: Take into account flight time left.
const DIRECT_AUTOPILOT_MAX_RNG: f32 = 500.;

// We use `LOOP_I` to manage inner vs outer loops.
static LOOP_I: AtomicU32 = AtomicU32::new(0);

// Enable this to print parameters (eg location, altitude, attitude, angular rates etc) to the console.
const DEBUG_PARAMS: bool = true;

// todo: Course set mode. Eg, point at thing using controls, activate a button,
// todo then the thing flies directly at the target.

// With this in mind: Store params in a global array. Maybe [ParamsInst; N], or maybe a numerical array for each param.

// todo: Consider a nested loop, where inner manages fine-tuning of angle, and outer
// manages directions etc. (?) look up prior art re quads and inner/outer loops.

// todo: Panic button that recovers the aircraft if you get spacial D etc.

/// Data dump from Hypershield on Matrix:
/// You typically don't change the PID gains during flight. They are often tuned experimentally by
///  first tuning the attitude gains, then altitude pid and then the horizontal pids. If you want your
///  pids to cover a large flight envelope (agile flight, hover) then you can use different flight modes
///  that switch between the different gains or use gain scheduling. I don't see a reason to use pitot
/// tubes for a quadrotor. Pitot tubes are used to get the airspeed for fixed-wing cause it has a large
/// influence on the aerodynamics. For a quadrotor it's less important and if you want your velocity
/// it's more common to use a down ward facing camera that uses optical flow. If you want LIDAR
/// (velodyne puck for instance) then we are talking about a very large quadrotor, similar to
///
/// the ones used for the darpa subterranean challenge. Focus rather on something smaller first.
///  The STM32F4 series is common for small quadrotors but if you want to do any sort of SLAM or
/// VIO processing you'll need a companion computer since that type of processing is very demanding.
///  You don't need a state estimator if you are manually flying it and the stick is provided desired
/// angular velocities (similar to what emuflight does). For autonomous flight you need a state estimator
///  where the Extended Kalman Filter is the most commonly used one. A state estimator does not
/// estimate flight parameters, but it estimates the state of the quadrotor (typically position,
/// velocity, orientation). Flight parameters would need to be obtained experimentally for
/// instance through system identification methods (an EKF can actually be used for this purpose
/// by pretending the unknown parameters are states). When it comes to the I term for a PID you
/// would typically create a PID struct or class where the I term is a member, then whenever
///  you compute the output of the PID you also update this variable. See here for instance:
// https://www.youtube.com/watch?v=zOByx3Izf5U
// For state estimation
// https://www.youtube.com/watch?v=RZd6XDx5VXo (Series)
// https://www.youtube.com/watch?v=whSw42XddsU
// https://www.youtube.com/playlist?list=PLn8PRpmsu08ryYoBpEKzoMOveSTyS-h4a
// For quadrotor PID control
// https://www.youtube.com/playlist?list=PLn8PRpmsu08oOLBVYYIwwN_nvuyUqEjrj
// https://www.youtube.com/playlist?list=PLn8PRpmsu08pQBgjxYFXSsODEF3Jqmm-y
// https://www.youtube.com/playlist?list=PLn8PRpmsu08pFBqgd_6Bi7msgkWFKL33b
///
///
///
/// todo: Movable camera that moves with head motion.
/// - Ir cam to find or avoid people
/// ///
/// 3 level loop? S, v, angle?? Or just 2? (position cmds in outer loop)

/// Utility fn to make up for `core::cmp::max` requiring f32 to impl `Ord`, which it doesn't.
/// todo: Move elsewhere?
fn max(a: f32, b: f32) -> f32 {
    if a > b {
        a
    } else {
        b
    }
}

fn abs(x: f32) -> f32 {
    f32::from_bits(x.to_bits() & 0x7FFF_FFFF)
}

pub enum LocationType {
    /// Lattitude and longitude. Available after a GPS fix
    LatLon,
    /// Start at 0, and count in meters away from it.
    Rel0,
}

/// If type is LatLon, `x` and `y` are in degrees. If Rel0, in meters. `z` is in m MSL.
pub struct Location {
    pub type_: LocationType,
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

impl Location {
    pub fn new(type_: LocationType, x: f32, y: f32, z: f32) -> Self {
        Self { type_, x, y, z }
    }
}

/// User-configurable settings
pub struct UserCfg {
    /// Set a ceiling the aircraft won't exceed. Defaults to 400' (Legal limit in US for drones).
    /// In meters.
    ceiling: f32,
    /// In Attitude and related control modes, max pitch angle (from straight up), ie
    /// full speed, without going horizontal or further.
    max_angle: f32, // radians
    max_velocity: f32, // m/s
    idle_pwr: f32,
    /// These input ranges map raw output from a manual controller to full scale range of our control scheme.
    /// (min, max). Set using an initial calibration / setup procedure.
    pitch_input_range: (f32, f32),
    roll_input_range: (f32, f32),
    yaw_input_range: (f32, f32),
    throttle_input_range: (f32, f32),
    /// Is the aircraft continuously collecting data on obstacles, and storing it to external flash?
    mapping_obstacles: bool,
    max_speed_hor: f32,
    max_speed_ver: f32,
    /// The GPS module is connected
    gps_attached: bool,
    /// The time-of-flight sensor module is connected
    tof_attached: bool,
    /// It's common to arbitrarily wire motors to the ESC. Reverse each from its
    /// default direction, as required.
    motors_reversed: (bool, bool, bool, bool)
}

impl Default for UserCfg {
    fn default() -> Self {
        Self {
            ceiling: 122.,
            // todo: Do we want max angle and vel here? Do we use them, vice settings in InpuMap?
            max_angle: TAU * 0.22,
            max_velocity: 30., // todo: raise?
            idle_pwr: 0.,      // scale of 0 to 1.
            // todo: Find apt value for these
            pitch_input_range: (0., 1.),
            roll_input_range: (0., 1.),
            yaw_input_range: (0., 1.),
            throttle_input_range: (0., 1.),
            mapping_obstacles: false,
            max_speed_hor: 20.,
            max_speed_ver: 20.,
        }
    }
}

/// A quaternion. Used for attitude state
struct Quaternion {
    i: f32,
    j: f32,
    k: f32,
    l: f32,
}

// impl Sub for Quaternion {
//     type Output = Self;
//
//     fn sub(self, other: Self) -> Self::Output {
//         Self {
//             x: self.x - other.x,
//             y: self.y - other.y,
//             z: self.z - other.z,
//         }
//     }
// }

impl Quaternion {
    pub fn new(i: f32, j: f32, k: f32, l: f32) -> Self {
        Self { i, j, k, l }
    }
}

/// A generalized quaternion
struct RotorMath {}

/// Represents a complete quadcopter. Used for setting control parameters.
struct AircraftProperties {
    mass: f32,               // grams
    arm_len: f32,            // meters
    drag_coeff: f32,         // unitless
    thrust_coeff: f32,       // N/m^2
    moment_of_intertia: f32, // kg x m^2
    rotor_inertia: f32,      // kg x m^2
}

impl AircraftProperties {
    /// Calculate the power level required, applied to each rotor, to maintain level flight
    /// at a given MSL altitude. (Alt is in meters)
    pub fn level_pwr(&self, alt: f32) -> f32 {
        return 0.1; // todo
    }
}

/// Specify the rotor. Includdes methods that get information regarding timer and DMA, per
/// specific board setups.
#[derive(Clone, Copy)]
pub enum Rotor {
    R1,
    R2,
    R3,
    R4,
}

impl Rotor {
    // todo: Feature gate these methods based on board, as required.
    pub fn tim_channel(&self) -> TimChannel {
        match self {
            Self::R1 => TimChannel::C1,
            Self::R2 => TimChannel::C4,
            Self::R3 => TimChannel::C3,
            Self::R4 => TimChannel::C4,
        }
    }

    /// Dma input channel. This should be in line with `tim_channel`.
    pub fn dma_input(&self) -> DmaInput {
        match self {
            Self::R1 => DmaInput::Tim2Ch1,
            Self::R2 => DmaInput::Tim2Ch4,
            Self::R3 => DmaInput::Tim3Ch3,
            Self::R4 => DmaInput::Tim3Ch4,
        }
    }

    /// Used for commanding timer DMA, for DSHOT protocol. Maps to CCR1, 2, 3, or 4.
    pub fn dma_channel(&self) -> DmaChannel {
        // Offset 16 for DMA base register of CCR1?
        // 17 for CCR2, 18 CCR3, and 19 CCR4?
        match self {
            Self::R1 => DmaChannel::C2,
            Self::R2 => DmaChannel::C3,
            Self::R3 => DmaChannel::C4,
            Self::R4 => DmaChannel::C5,
        }
    }

    /// Used for commanding timer DMA, for DSHOT protocol. Maps to CCR1, 2, 3, or 4.
    pub fn base_addr_offset(&self) -> u8 {
        // Offset 16 for DMA base register of CCR1?
        // 17 for CCR2, 18 CCR3, and 19 CCR4?
        match self.tim_channel() {
            TimChannel::C1 => 16,
            TimChannel::C2 => 17,
            TimChannel::C3 => 18,
            TimChannel::C4 => 19,
        }
    }
}

#[derive(Clone, Copy)]
/// Role in a swarm of drones
pub enum SwarmRole {
    Queen,
    Worker(u16),    // id
    PersonFollower, // When your queen is human.
}

// pub enum FlightProfile {
//     /// On ground, with rotors at 0. to 1., with 0. being off, and 1. being 1:1 lift-to-weight
//     OnGround,
//     /// Maintain a hover
//     Hover,
//     /// In transit to a given location, in Location and speed
//     Transit(Location, f32),
//     /// Landing
//     Landing,
// }

/// Set up the pins that have structs that don't need to be accessed after.
pub fn setup_pins() {
    // SAI pins to accept input from the 4 PDM ICs, using SAI1, and 4, both blocks.
    // We use the same SCK and FS clocks for all 4 ICs.

    cfg_if! {
        if #[cfg(feature = "matek-h743slim")] {
            // For use with Matek H7 board:
            // http://www.mateksys.com/?portfolio=h743-slim#tab-id-7

            // todo: Determine what output speeds to use.

            // Rotors connected to TIM3 CH3, 4; TIM5 CH1, 2
            let mut rotor1 = Pin::new(Port::B, 0, PinMode::Alt(2));
            let mut rotor2 = Pin::new(Port::B, 1, PinMode::Alt(2));
            let mut rotor3 = Pin::new(Port::A, 0, PinMode::Alt(2));
            let mut rotor4 = Pin::new(Port::A, 1, PinMode::Alt(2));

            rotor1.output_speed(OutputSpeed::High);
            rotor2.output_speed(OutputSpeed::High);
            rotor3.output_speed(OutputSpeed::High);
            rotor4.output_speed(OutputSpeed::High);

            let current_sense_adc_ = Pin::new(Port::C, 0, PinMode::Analog);

            // SPI4 for IMU.
            let mosi4_ = Pin::new(Port::E, 14, PinMode::Alt(5));
            let miso4_ = Pin::new(Port::E, 13, PinMode::Alt(5));
            let sck4_ = Pin::new(Port::E, 12, PinMode::Alt(5));

            // SPI2 for Matek OSD (MAX7456?)
            let mosi4_ = Pin::new(Port::B, 15, PinMode::Alt(5));
            let miso4_ = Pin::new(Port::B, 14, PinMode::Alt(5));
            let sck4_ = Pin::new(Port::B, 13, PinMode::Alt(5));

            // We use  UARTs for ESC telemetry, "Smart Audio" (for video) and...
            // todo: set these up
            let uart1_tx = Pin::new(Port::D, 0, PinMode::Alt(0));
            let uart1_rx = Pin::new(Port::D, 1, PinMode::Alt(0));
            let uart2_tx = Pin::new(Port::D, 2, PinMode::Alt(0));
            let uart2_rx = Pin::new(Port::D, 3, PinMode::Alt(0));

            // Used to trigger a PID update based on new IMU data.
            let mut imu_interrupt = Pin::new(Port::C, 15, PinMode::Input);
            imu_interrupt.enable_interrupt(Edge::Falling); // todo: Rising or falling? Configurable on IMU I think.

            // I2C1 for Matek digital airspeed and compass
            let mut scl1 = Pin::new(Port::B, 6, PinMode::Alt(4));
            scl1.output_type(OutputType::OpenDrain);
            scl1.pull(Pull::Up);

            let mut sda1 = Pin::new(Port::B, 7, PinMode::Alt(4));
            sda1.output_type(OutputType::OpenDrain);
            sda1.pull(Pull::Up);

            // I2C2 for Matek's DPS310 barometer
            let mut scl2 = Pin::new(Port::B, 10, PinMode::Alt(4));
            scl2.output_type(OutputType::OpenDrain);
            scl2.pull(Pull::Up);

            let mut sda2 = Pin::new(Port::B, 11, PinMode::Alt(4));
            sda2.output_type(OutputType::OpenDrain);
            sda2.pull(Pull::Up);

            // todo: Use one of these buses for TOF sensor, or its own?

            let bat_adc_ = Pin::new(Port::C, 0, PinMode::Analog);
        } else if #[cfg(feature = "anyleaf-mercury-g4")] {
            // Rotors connected to Tim2 CH3, 4; Tim3 ch3, 4
            let mut rotor1 = Pin::new(Port::A, 0, PinMode::Alt(1)); // Tim2 ch1
            let mut rotor2 = Pin::new(Port::A, 10, PinMode::Alt(10)); // Tim2 ch4
            let mut rotor3 = Pin::new(Port::B, 0, PinMode::Alt(2)); // Tim3 ch3
            let mut rotor4 = Pin::new(Port::B, 1, PinMode::Alt(2)); // Tim3 ch4

            rotor1.output_speed(OutputSpeed::High);
            rotor2.output_speed(OutputSpeed::High);
            rotor3.output_speed(OutputSpeed::High);
            rotor4.output_speed(OutputSpeed::High);

            // todo: USB? How do we set them up (no alt fn) PA11(DN) and PA12 (DP).
            let _usb_dm = gpioa.new_pin(11, PinMode::Output);
            let _usb_dp = gpioa.new_pin(12, PinMode::Output);

            let batt_v_adc_ = Pin::new(Port::A, 4, PinMode::Analog);  // ADC2, channel 17
            let current_sense_adc_ = Pin::new(Port::B, 2, PinMode::Analog);  // ADC2, channel 12

            // SPI1 for the IMU. Nothing else on the bus, since we use it with DMA
            let sck1_ = Pin::new(Port::A, 5, PinMode::Alt(5));
            let miso1_ = Pin::new(Port::A, 6, PinMode::Alt(5));
            let mosi1_ = Pin::new(Port::A, 7, PinMode::Alt(5));

            // SPI2 for the LoRa chip
            let sck2_ = Pin::new(Port::B, 13, PinMode::Alt(5));
            let miso2_ = Pin::new(Port::B, 14, PinMode::Alt(5));
            let mosi2_ = Pin::new(Port::B, 15, PinMode::Alt(5));

            // SPI3 for flash
            let sck3_ = Pin::new(Port::B, 3, PinMode::Alt(6));
            let miso3_ = Pin::new(Port::B, 4, PinMode::Alt(6));
            let mosi3_ = Pin::new(Port::B, 5, PinMode::Alt(6));


            // We use UARTs for misc external devices, including ESC telemetry,
            // and "Smart Audio" (for video)

            let uart1_tx = Pin::new(Port::B, 6, PinMode::Alt(7));
            let uart1_rx = Pin::new(Port::B, 7, PinMode::Alt(7));
            let uart2_tx = Pin::new(Port::A, 2, PinMode::Alt(7));
            let uart2_rx = Pin::new(Port::A, 3, PinMode::Alt(7));
            let uart3_tx = Pin::new(Port::B, 10, PinMode::Alt(7));
            let uart3_rx = Pin::new(Port::B, 11, PinMode::Alt(7));
            let uart4_tx = Pin::new(Port::C, 10, PinMode::Alt(7));
            let uart4_rx = Pin::new(Port::C, 11, PinMode::Alt(7));

            // Used to trigger a PID update based on new IMU data.
            let mut imu_interrupt = Pin::new(Port::C, 4, PinMode::Input); // PA4 for IMU interrupt.
            imu_interrupt.enable_interrupt(Edge::Falling); // todo: Rising or falling? Configurable on IMU I think.

            // I2C1 for external sensors, via pads
            let mut scl1 = Pin::new(Port::A, 15, PinMode::Alt(4));
            scl1.output_type(OutputType::OpenDrain);
            scl1.pull(Pull::Up);

            let mut sda1 = Pin::new(Port::B, 7, PinMode::Alt(4));
            sda1.output_type(OutputType::OpenDrain);
            sda1.pull(Pull::Up);

            // I2C2 for the DPS310 barometer, and pads.
            let mut scl2 = Pin::new(Port::A, 9, PinMode::Alt(4));
            scl2.output_type(OutputType::OpenDrain);
            scl2.pull(Pull::Up);

            let mut sda2 = Pin::new(Port::A, 8, PinMode::Alt(4));
            sda2.output_type(OutputType::OpenDrain);
            sda2.pull(Pull::Up);

        }
    }
}

/// Run on startup, or when desired. Run on the ground. Gets an initial GPS fix,
/// and other initialization functions.
fn init(params: &mut Params, baro: baro::Barometer, base_pt: &mut Location, i2c: &mut I2c<I2C1>) {
    let eps = 0.001;

    // Don't init if in motion.
    if params.v_x > eps
        || params.v_y > eps
        || params.v_z > eps
        || params.v_pitch > eps
        || params.v_roll > eps
    {
        return;
    }

    if let Some(agl) = tof::read(params.s_pitch, params.s_roll, i2c) {
        if agl > 0.01 {
            return;
        }
    }

    let fix = gps::get_fix(i2c);

    match fix {
        Ok(f) => {
            params.s_x = f.x;
            params.s_y = f.y;
            params.s_z_msl = f.z;

            *base_pt = Location::new(LocationType::LatLon, f.y, f.x, f.z);
        }
        Err(e) => (), // todo
    }

    // todo: Use Rel0 location type if unable to get fix.

    let temp = 0.; // todo: Which sensor reads temp? The IMU?

    // todo: Put back etc
    // let barometer: baro::Barometer::new(&mut i2c);
    // barometer.calibrate(fix.alt, temp);
}

#[rtic::app(device = pac, peripherals = false)]
mod app {
    use super::*;

    // todo: Move vars from here to `local` as required.
    #[shared]
    struct Shared {
        // profile: FlightProfile,
        user_cfg: UserCfg,
        input_map: InputMap,
        input_mode: InputMode,
        autopilot_status: AutopilotStatus,
        ctrl_coeffs: CtrlCoeffGroup,
        current_params: Params,
        inner_flt_cmd: CtrlInputs,
        /// Proportional, Integral, Differential error
        pid_mid: PidGroup,
        pid_inner: PidGroup,
        manual_inputs: CtrlInputs,
        current_pwr: RotorPower,
        dma: Dma<DMA1>,
        spi1: Spi<SPI1>,
        spi2: Spi<SPI2>,
        spi3: Spi<SPI3>,
        cs_imu: Pin,
        i2c1: I2c<I2C1>,
        i2c2: I2c<I2C2>,
        // rtc: Rtc,
        update_timer: Timer<TIM15>,
        rotor_timer_a: Timer<TIM2>,
        rotor_timer_b: Timer<TIM3>,
        usb_dev: UsbDevice<UsbBusType>,
        usb_serial: SerialPort<UsbBusType>,
        // `power_used` is in rotor power (0. to 1. scale), summed for each rotor x milliseconds.
        power_used: f32,
        // Store filter instances for the PID loop derivatives. One for each param used.
        pid_deriv_filters: PidDerivFilters,
        base_point: Location,
        command_state: CommandState,
    }

    #[local]
    struct Local {}

    #[init]
    fn init(cx: init::Context) -> (Shared, Local, init::Monotonics) {
        // Cortex-M peripherals
        let mut cp = cx.core;
        // Set up microcontroller peripherals
        let mut dp = pac::Peripherals::take().unwrap();

        #[cfg(feature = "matek-h743slim")]
            SupplyConfig::DirectSmps.setup(&mut dp.PWR, VoltageLevel::V2_5);

        // Set up clocks
        let clock_cfg = Clocks {
            // Config for 480Mhz full speed:
            #[cfg(feature = "matek-h743slim")]
            pll_src: PllSrc::Hse(8_000_000),
            #[cfg(feature = "anyleaf-mercury-g4")]
            input_src: InputSrc::Pll(PllSrc::Hse(16_000_000)),
            // vos_range: VosRange::VOS0, // Note: This may use extra power. todo: Put back!
            #[cfg(feature = "matek-h743slim")]
            pll1: PllCfg {
                divm: 4, // To compensate with 8Mhz HSE instead of 64Mhz HSI
                // divn: 480,// todo: Put back! No longer working??
                ..Default::default()
            },
            hsi48_on: true,
            clk48_src: Clk48Src::Hsi48,
            ..Default::default()
        };

        clock_cfg.setup().unwrap();

        // Enable the Clock Recovery System, which improves HSI48 accuracy.
        clocks::enable_crs(CrsSyncSrc::Usb);

        defmt::println!("Clocks setup successfully");
        debug_workaround();

        // Improves performance, at a cost of slightly increased power use.
        // May be required to prevent sound problems.
        cp.SCB.invalidate_icache();
        cp.SCB.enable_icache();

        // Set up pins with appropriate modes.
        setup_pins();

        // We use SPI1 for the IMU
        // SPI input clock is 400MHz. 400MHz / 32 = 12.5 MHz. The limit is the max SPI speed
        // of the ICM-42605 IMU of 24 MHz. This IMU can use any SPI mode, with the correct config on it.
        let mut spi1 = Spi::new(dp.SPI1, Default::default(), BaudRate::Div32);

        // We use SPI2 for the LoRa ELRS chip.  // todo: Find max speed and supported modes.
        let spi2 = Spi::new(dp.SPI2, Default::default(), BaudRate::Div32);

        let elrs_dio = Pin::new(Port::C, 13, PinMode::Output); // todo: input or output?

        // We use SPI3 for flash. // todo: Find max speed and supported modes.
        let spi3 = Spi::new(dp.SPI3, Default::default(), BaudRate::Div32);

        let cs_flash = Pin::new(Port::C, 6, PinMode::Output);

        // We use I2C for the TOF sensor.(?) and for Matek digital airspeed and compass
        let i2c_cfg = I2cConfig {
            speed: I2cSpeed::FastPlus1M,
            // speed: I2cSpeed::Fast400k,
            ..Default::default()
        };
        let i2c1 = I2c::new(dp.I2C1, i2c_cfg.clone(), &clock_cfg);

        // We use (Matek) I2C2 for the barometer.
        let i2c2 = I2c::new(dp.I2C2, i2c_cfg, &clock_cfg);

        // We use `uart1` for the radio controller receiver, via CRSF protocol.
        // CRSF protocol uses a single wire half duplex uart connection.
        //  * The master sends one frame every 4ms and the slave replies between two frames from the master.
        //  *
        //  * 420000 baud
        //  * not inverted
        //  * 8 Bit
        //  * 1 Stop bit
        //  * Big endian
        //  * 420000 bit/s = 46667 byte/s (including stop bit) = 21.43us per byte
        //  * Max frame size is 64 bytes
        //  * A 64 byte frame plus 1 sync byte can be transmitted in 1393 microseconds.

        // The STM32-HAL default UART config includes stop bits = 1, parity disabled, and 8-bit words,
        // which is what we want.
        let uart1 = Usart::new(dp.USART1, 420_000, Default::default(), &clock_cfg);
        // let uart2 = Usart::new(dp.USART1, 9_600., Default::default(), &clock_cfg);

        // We use the RTC to assist with power use measurement.
        let rtc = Rtc::new(dp.RTC, Default::default());

        // We use the ADC to measure battery voltage.
        let batt_adc = Adc::new_adc1(dp.ADC1, AdcDevice::One, Default::default(), &clock_cfg);

        let mut update_timer =
            Timer::new_tim15(dp.TIM15, UPDATE_RATE, Default::default(), &clock_cfg);
        update_timer.enable_interrupt(TimerInterrupt::Update);

        let rotor_timer_cfg = TimerConfig {
            // We use ARPE since we change duty with the timer running.
            auto_reload_preload: true,
            ..Default::default()
        };

        // We use multiple timers instead of a single one based on pin availability; a single
        // timer with 4 channels would be fine.

        cfg_if! {
            if #[cfg(feature = "matek-h743slim")] {
                let mut rotor_timer_a =
                    Timer::new_tim3(dp.TIM3, 1., rotor_timer_cfg.clone(), &clock_cfg);

                // todo: Double check these - may not be correct.
                rotor_timer_a.enable_pwm_output(TimChannel::C3, OutputCompare::Pwm1, 0.);
                rotor_timer_a.enable_pwm_output(TimChannel::C4, OutputCompare::Pwm1, 0.);

                let mut rotor_timer_b = Timer::new_tim5(dp.TIM5, 1., rotor_timer_cfg, &clock_cfg);

                rotor_timer_b.enable_pwm_output(TimChannel::C3, OutputCompare::Pwm1, 0.);
                rotor_timer_b.enable_pwm_output(TimChannel::C4, OutputCompare::Pwm1, 0.);
            } else if #[cfg(feature = "anyleaf-mercury-g4")] {
                let mut rotor_timer_a =
               Timer::new_tim2(dp.TIM2, dshot::TIM_FREQ, rotor_timer_cfg.clone(), &clock_cfg);

                rotor_timer_a.enable_pwm_output(TimChannel::C3, OutputCompare::Pwm1, 0.);
                rotor_timer_a.enable_pwm_output(TimChannel::C4, OutputCompare::Pwm1, 0.);

                let mut rotor_timer_b = Timer::new_tim3(dp.TIM3, dshot::TIM_FREQ, rotor_timer_cfg, &clock_cfg);

                rotor_timer_b.enable_pwm_output(TimChannel::C3, OutputCompare::Pwm1, 0.);
                rotor_timer_b.enable_pwm_output(TimChannel::C4, OutputCompare::Pwm1, 0.);
            }
        }

        rotor_timer_a.set_prescaler(DSHOT_PSC);
        rotor_timer_a.set_auto_reload(DSHOT_ARR);
        rotor_timer_b.set_prescaler(DSHOT_PSC);
        rotor_timer_b.set_auto_reload(DSHOT_ARR);

        let mut user_cfg = UserCfg::default();
        dshot::setup_motor_dir(user_cfg.motors_reversed, &mut rotor_timer_a, &mut rotor_timer_b, &mut dma);

        // We use `dt_timer` to count the time between IMU updates, for use in the PID loop
        // integral, derivative, and filters. If set to 1Mhz, the CNT value is the number of
        // µs elapsed.

        // let mut dt_timer = Timer::new_tim15(dp.TIM3, 1_., Default::default(), &clock_cfg);
        // We expect the IMU to update every 8kHz. Set 1kHz as the freq here, which is low,
        // by a good margin. Not too low, as to keep resolution high.
        // We use manual PSC and ARR, as to maximize resolution through a high ARR.
        // dt_timer.set_prescaler(DT_PSC);
        // dt_timer.set_auto_reload(DT_ARR);

        // From Matek
        #[cfg(feature = "matek-h743slim")]
            let mut cs_imu = Pin::new(Port::E, 11, PinMode::Output);
        #[cfg(feature = "anyleaf-mercury-g4")]
            let mut cs_imu = Pin::new(Port::B, 12, PinMode::Output);

        imu::setup(&mut spi4, &mut cs_imu);

        // let mut cs_osd = Pin::new(Port::B, 12, PinMode::Output);

        // In Betaflight, DMA is required for the ADC (current/voltage sensor),
        // motor outputs running bidirectional DShot, and gyro SPI bus.

        // todo: Consider how you use DMA, and bus splitting.
        // todo: Feature-gate these based on board, as required.


        cfg_if! {
            if #[cfg(feature = "anyleaf-mercury-g4")] {
                let usb = Peripheral { usb: dp.USB };
                let usb_bus = UsbBus::new(usb);
                let usb_serial = SerialPort::new(usb_bus);

                let usb_dev = UsbDeviceBuilder::new(USB_BUS.as_ref().unwrap(), UsbVidPid(0x16c0, 0x27dd))
                    .manufacturer("Anyleaf")
                    .product("Serial port")
                    // We use `serial_number` to identify the device to the PC. If it's too long,
                    // we get permissions errors on the PC.
                    .serial_number("Mercury-G4"), // todo: Try 2 letter only if causing trouble?
                    .device_class(USB_CLASS_CDC)
                    .build();
            }
        }


        // Used to update the input data from the ELRS radio
        let mut elrs_cs = Pin::new(Port::C, 15, PinMode::Output);
        let mut elrs_busy = Pin::new(Port::C, 14, PinMode::Input);
        let mut elrs_reset = Pin::new(Port::A, 1, PinMode::Output);
        elrs_busy.enable_interrupt(Edge::Falling); // todo: Rising or falling?

        // IMU
        dma::mux(DmaChannel::C0, dma::DmaInput::Spi1Tx, &mut dp.DMAMUX1);
        dma::mux(DmaChannel::C1, dma::DmaInput::Spi1Rx, &mut dp.DMAMUX1);

        // todo: TIMUP??
        // DSHOT, motor 1
        dma::mux(
            Rotor::R1.dma_channel(),
            Rotor::R1.dma_input(),
            &mut dp.DMAMUX1,
        );
        // DSHOT, motor 2
        dma::mux(
            Rotor::R2.dma_channel(),
            Rotor::R2.dma_input(),
            &mut dp.DMAMUX1,
        );
        // DSHOT, motor 3
        dma::mux(
            Rotor::R3.dma_channel(),
            Rotor::R3.dma_input(),
            &mut dp.DMAMUX1,
        );
        // DSHOT, motor 4
        dma::mux(
            Rotor::R4.dma_channel(),
            Rotor::R4.dma_input(),
            &mut dp.DMAMUX1,
        );

        // todo: DMA for voltage ADC (?)

        // TOF sensor
        // dma::mux(DmaChannel::C2, dma::DmaInput::I2c1Tx, &mut dp.DMAMUX1);
        // dma::mux(DmaChannel::C3, dma::DmaInput::I2c1Rx, &mut dp.DMAMUX1);
        // Baro
        // dma::mux(DmaChannel::C4, dma::DmaInput::I2c2Tx, &mut dp.DMAMUX1);
        // dma::mux(DmaChannel::C5, dma::DmaInput::I2c2Rx, &mut dp.DMAMUX1);

        let mut flash = Flash::new(dp.FLASH); // todo temp mut to test

        // rotor_timer_a.enable();
        // rotor_timer_b.enable();

        update_timer.enable();

        (
            // todo: Make these local as able.
            Shared {
                user_cfg,
                input_map: Default::default(),
                input_mode: InputMode::Attitude,
                autopilot_status: Default::default(),
                ctrl_coeffs: Default::default(),
                current_params: Default::default(),
                inner_flt_cmd: Default::default(),
                pid_mid: Default::default(),
                pid_inner: Default::default(),
                manual_inputs: Default::default(),
                current_pwr: Default::default(),
                dma,
                spi1,
                spi2,
                spi3,
                cs_imu,
                i2c1,
                i2c2,
                // rtc,
                update_timer,
                rotor_timer_a,
                rotor_timer_b,
                usb_dev,
                usb_serial,
                power_used: 0.,
                pid_deriv_filters: PidDerivFilters::new(),
                base_point: Location::new(LocationType::Rel0, 0., 0., 0.),
                command_state: Default::default(),
            },
            Local {},
            init::Monotonics(),
        )
    }

    #[idle(shared = [], local = [])]
    fn idle(cx: idle::Context) -> ! {
        loop {
            asm::nop();
        }
    }

    #[task(
    binds = TIM15,
    shared = [current_params, manual_inputs, input_map, current_pwr,
    inner_flt_cmd, pid_mid, pid_deriv_filters,
    power_used, input_mode, autopilot_status, user_cfg, command_state, ctrl_coeffs,
    ],
    local = [],
    priority = 2
    )]
    fn update_isr(cx: update_isr::Context) {
        // todo: Loop index is to dtermine if we need to run the outer (position-tier)
        // todo loop.
        let loop_i = LOOP_I.fetch_add(1, Ordering::Relaxed);

        (
            cx.shared.current_params,
            cx.shared.manual_inputs,
            cs.shared.input_map,
            cx.shared.current_pwr,
            cx.shared.inner_flt_cmd,
            cx.shared.pid_mid,
            cx.shared.pid_deriv_filters,
            cx.shared.power_used,
            cx.shared.input_mode,
            cx.shared.autopilot_status,
            cx.shared.user_cfg,
            cx.shared.command_state,
            cx.shared.ctrl_coeffs,
        )
            .lock(
                |params,
                 manual_inputs,
                 input_map,
                 current_pwr,
                 inner_flt_cmd,
                 pid_mid,
                 filters,
                 spi,
                 power_used,
                 input_mode,
                 autopilot_status,
                 cfg,
                 command_state,
                 coeffs| {
                    // todo: Do we want to update manual/radio inputs here, or in the faster IMU update
                    // ISR?


                    // todo: Support both UART telemetry from ESC, and analog current sense pin.
                    // todo: Read from an ADC or something, from teh ESC.
                    // let current_current = None;
                    // if let Some(current) = current_current {
                    // *power_used += current * DT;

                    // }
                    // else {
                    // *power_used += current_pwr.total() * DT;
                    // }

                    // todo: Placeholder for sensor inputs/fusion.

                    // if loop_i % OUTER_LOOP_RATIO == 0 {
                    //     match input_mode {
                    //         InputMode::Command => {
                    //             // let flt_cmd = FlightCmd::from_inputs(inputs, input_mode);
                    //
                    //             // todo: impl ?

                    //         _ => (),
                    //     }
                    // }

                    // todo: Don't run this debug every loop maybe in a timer
                    if DEBUG_PARAMS {
                        defmt::println!(
                            "Pitch rate: {}", params.v_pitch,
                            "Roll rate: {}", params.v_roll,
                            "Yaw rate: {}", params.v_yaw,
                            // todo etc
                        );
                    }


                    pid::run_pid_mid(
                        params,
                        manual_inputs,
                        input_map,
                        inner_flt_cmd,
                        pid_mid,
                        filters,
                        input_mode,
                        autopilot_status,
                        cfg,
                        command_state,
                        coeffs,
                    );
                },
            )
    }

    /// Runs when new IMU data is recieved. This functions as our PID inner loop, and updates
    /// pitch and roll. We use this ISR with an interrupt from the IMU, since we wish to
    /// update rotor power settings as soon as data is available.
    #[task(binds = EXTI15_10, shared = [
    spi1, cs_imu, dma, rotor_timer_a, rotor_timer_b,
    ], local = [], priority = 2)]
    fn imu_data_isr(mut cx: imu_data_isr::Context) {
        unsafe {
            // Clear the interrupt flag.
            (*pac::EXTI::ptr()).c1pr1.modify(|_, w| w.pr15().set_bit());
        }

        (cx.shared.command_state, cx.shared.rotor_timer_a, cx.shared.rotor_timer_b).lock(|state, rotor_timer_a, rotor_timer_b| {
            // todo: Put this armed check in the update isr? Somewhere else?
            if state.armed != ArmStatus::Armed || state.pre_armed != ArmStatus::Armed {
                dshot::stop_all(rotor_timer_a, rotor_timer_b);
            }
        });


        // let timer_val = cx.local.dt_timer.read_count();
        // cx.local.dt_timer.disable();
        // cx.local.dt_timer.reset_countdown();
        // cx.local.tim_timer.enable();

        // the DT timer is set to tick at 1Mhz. This means each tick is a microsecond. We want
        // seconds.
        // todo: Can we get away with a fixed rate, like 8kHz?
        // todo: If you use this

        // todo: Don't just use IMU; use sensor fusion. Starting with this by default

        // todo: Consider making this spi bus dedicated to the IMU, and making it local.
        (cx
             .shared
             .spi1, cx.shared.cs_imu, cx.shared.dma)
            .lock(|spi, cs, dma| {
                imu::read_all_dma(spi, cs_imu, dma);

            });
    }


    #[task(binds = DMA1_STR0, shared = [dma, current_params, input_mode, autopilot_status,
    inner_flt_cmd, pid_inner, pid_deriv_filters, current_pwr, ctrl_coeffs, command_state, cs_imu], local = [], priority = 1)]
    /// This ISR Handles received data from the IMU, after DMA transfer is complete.
    fn imu_tc_isr(mut cx: usb_isr::Context) {
        (cx.shared.dma, cx.shared.imu).lock(|dma, cs| {
            dma.clear_interrupt(DmaChannel::C0, DmaInterrupt::TransferComplete);
            cs.set_high();
        });

        let mut sensor_data_fused = sensor_fusion::estimate_attitude(&imu_data);

        // todo: Temp replacing back in imu data while we sort out the fusion.
        sensor_data_fused.v_pitch = imu_data.v_pitch;
        sensor_data_fused.v_roll = imu_data.v_roll;
        sensor_data_fused.v_yaw = imu_data.v_yaw;

        sensor_data_fused.a_x = imu_data.a_x;
        sensor_data_fused.a_y = imu_data.a_y;
        sensor_data_fused.a_z = imu_data.a_z;

        // todo: Reflow this section once you have a better grasp of what you get from teh sensor
        // fusion data. Most of those lines below will likely go away, and be replaced with
        // an ekf already in `sensor_data_fused`.

        // todo: Move these into a `Params` method?
        // We have acceleration data for x, y, z: Integrate to get velocity and position.
        // let v_x = sensor_data_fused.v_x + sensor_data_fused.a_x * DT;
        // let v_y = sensor_data_fused.v_y + sensor_data_fused.a_y * DT;
        // let v_z = sensor_data_fused.v_z + sensor_data_fused.a_z * DT;
        //
        // // Estimate position by integrating velocity.
        // let s_x = sensor_data_fused.s_x + params.v_x * DT;
        // let s_y = sensor_data_fused.s_y + params.v_y * DT;
        // let s_z_msl = sensor_data_fused.s_z_msl + params.v_z * DT;
        // let s_z_agl = sensor_data_fused.s_z_agl + params.v_z * DT;
        //
        // // We have position data for pitch, roll, yaw: Take derivative to get velocity and acceleration
        // let s_pitch = sensor_data_fused.s_pitch + sensor_data_fused.v_pitch * DT;
        // let s_roll = sensor_data_fused.s_roll + sensor_data_fused.v_roll * DT;
        // let s_yaw = sensor_data_fused.s_yaw + sensor_data_fused.v_yaw * DT;
        //
        // // Estimate attitude acceleration by taking a derivative of its position.
        // // todo: Handle thsi with EKF etc.
        // let a_pitch = (sensor_data_fused.v_pitch - params.v_pitch) / DT;
        // let a_roll = (sensor_data_fused.v_roll - params.v_roll) * DT;
        // let a_yaw = (sensor_data_fused.v_yaw - params.v_yaw) / DT;

        cx.shared.current_params.lock(|params| {
            *params = sensor_data_fused;



        (
            cx.shared.current_params,
            cx.shared.manual_inputs,
            cx.shared.input_mode,
            cx.shared.autopilot_status,
            cx.shared.manual_inputs,
            cx.shared.inner_flt_cmd,
            cx.shared.pid_inner,
            cx.shared.pid_deriv_filters,
            cx.shared.current_pwr,
            // cx.shared.rotor_timer_a,
            // cx.shared.rotor_timer_b,
            cx.shared.ctrl_coeffs,
        )
            .lock(
                |params,
                 input_mode,
                 autopilot_status,
                 manual_inputs,
                 inner_flt_cmd,
                 pid_inner,
                 filters,
                 current_pwr,
                 // rotor_timer_a,
                 // rotor_timer_b,
                 coeffs| {
                    pid::run_pid_inner(
                        params,
                        *input_mode,
                        autopilot_status,
                        manual_inputs,
                        inner_flt_cmd,
                        pid_inner,
                        filters,
                        current_pwr,
                        rotor_timer_a,
                        rotor_timer_b,
                        coeffs,
                        DT_IMU,
                    );
                },
            );
        })
    }


    #[task(binds = EXTI3_10, shared = [manual_inputs, spi3], local = [], priority = 3)]
    /// We use this ISR when receiving data from the radio, via ELRS
    fn radio_data_isr(mut cx: imu_data_isr::Context) {
        (cx.shared.manual_inputs, cx.shared.spi3).lock(|manual_inputs, spi| {
            *manual_inputs = elrs::get_inputs(spi);
            *manual_inputs = CtrlInputs::get_manual_inputs(cfg); ; // todo: this?
        })
    }

    #[task(binds = USB, shared = [usb_dev, usb_serial, params], local = [], priority = 3)]
    /// This ISR handles interaction over the USB serial port, eg for configuring using a desktop
    /// application.
    fn usb_isr(mut cx: usb_isr::Context) {
        (cx.shared.usb_dev, cx.shared.usb_serial, cx.shared.params).lock(|usb_dev, usb_serial, params| {


            if !usb_dev.poll(&mut [usb_serial]) {
                continue;
            }

            let mut buf = [0u8; 8];
            match usb_serial.read(&mut buf) {
                // todo: match all start bits and end bits. Running into an error using the naive approach.
                Ok(count) => {
                    serial.write(&[1, 2, 3]).ok();
                }
                Err(_) => {
                    //...
                }
            }

        })
    }


}

    // same panicking *behavior* as `panic-probe` but doesn't print a panic message
// this prevents the panic message being printed *twice* when `defmt::panic` is invoked
    #[defmt::panic_handler]
    fn panic() -> ! {
        cortex_m::asm::udf()
    }

    /// Terminates the application and makes `probe-run` exit with exit-code = 0
    pub fn exit() -> ! {
        loop {
            cortex_m::asm::bkpt();
        }
    }
