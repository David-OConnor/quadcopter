#![no_main]
#![no_std]
// Used on USB protocol. Allows adding to the const param buff size
// to make packet size.

// Potential markets:
// - Hobby / racing (duh)
// Tower inspections (Maybe market disruption by using fixed-wing + tpod?)
//
// https://www.youtube.com/watch?v=zOByx3Izf5U
// For state estimation
// https://www.youtube.com/watch?v=RZd6XDx5VXo (Series)
// https://www.youtube.com/watch?v=whSw42XddsU
// https://www.youtube.com/playlist?list=PLn8PRpmsu08ryYoBpEKzoMOveSTyS-h4a
// For quadrotor PID control
// https://www.youtube.com/playlist?list=PLn8PRpmsu08oOLBVYYIwwN_nvuyUqEjrj
// https://www.youtube.com/playlist?list=PLn8PRpmsu08pQBgjxYFXSsODEF3Jqmm-y
// https://www.youtube.com/playlist?list=PLn8PRpmsu08pFBqgd_6Bi7msgkWFKL33b
//
// todo: Movable camera that moves with head motion.
// - Ir cam to find or avoid people

use ahrs_fusion::Ahrs;
use cfg_if::cfg_if;
use control_interface::{ChannelData, LinkStats, PidTuneActuation};
use cortex_m::{self, asm, delay::Delay};
use defmt::println;
use defmt_rtt as _;
use drivers::{
    baro_dps310 as baro, gps_ublox as gps, imu_icm426xx as imu, mag_lis3mdl as mag,
    osd::{self, AutopilotData, OsdData},
    tof_vl53l1 as tof,
};
use filter_imu::ImuFilters;
use flight_ctrls::{
    autopilot::AutopilotStatus,
    common::{MotorRpm, MotorTimers, RatesCommanded},
    ctrl_logic::{self, PowerMaps},
    // pid::{
    //     self, CtrlCoeffGroup, PidDerivFilters, PidGroup, PID_CONTROL_ADJ_AMT,
    //     PID_CONTROL_ADJ_TIMEOUT,
    // },
    filters::FlightCtrlFilters,
    pid::{MotorCoeffs, MotorPidGroup},
    ControlMapping,
};
use lin_alg2::f32::Quaternion;
use panic_probe as _;
use ppks::{Location, LocationType};
use protocols::{crsf, dshot, usb_cfg};
use safety::ArmStatus;
use sensors_shared::{ExtSensor, V_A_ADC_READ_BUF};
use state::{OperationMode, SensorStatus, StateVolatile, SystemStatus, UserCfg};

use params::Params;
use stm32_hal2::{
    self,
    adc::{self, Adc, AdcConfig, AdcDevice},
    clocks::{self, Clocks, CrsSyncSrc, InputSrc, PllSrc},
    dma::{self, ChannelCfg, Dma, DmaInterrupt},
    flash::{Bank, Flash},
    gpio::{self, Edge, Pin, Port},
    i2c::I2c,
    pac::{self, DMA1, DMA2, I2C1, I2C2, SPI1, TIM1, TIM15, TIM16, TIM17, TIM5, USART2},
    spi::Spi,
    timer::{Timer, TimerConfig, TimerInterrupt},
    usart::{Usart, UsartInterrupt},
};
use usb_device::{bus::UsbBusAllocator, prelude::*};
use usbd_serial::{self, SerialPort};

mod ahrs_fusion;
mod atmos_model;
mod attitude_platform;
mod cfg_storage;
mod control_interface;
mod drivers;
mod filter_imu;
mod flight_ctrls;
mod imu_calibration;
mod imu_shared;
mod params;
mod ppks;
mod protocols;
mod safety;
mod sensors_shared;
mod setup;
mod state;
mod util;

cfg_if! {
    if #[cfg(feature = "h7")] {
        use stm32_hal2::{
            clocks::{PllCfg, VosRange},
            usb_otg::{Usb1, UsbBus, Usb1BusType as UsbBusType},
            pac::OCTOSPI1,
            qspi::{Qspi},
        };
        // This USART alias is made pub here, so we don't repeat this line in other modules.
        pub use stm32_hal2::pac::{UART7 as UART_ELRS, ADC1 as ADC};

        // type SpiFlash = Qspi<OCTOSPI>;
        type SpiFlash = Qspi;
    } else if #[cfg(feature = "g4")] {
        use stm32_hal2::{
            usb::{self, UsbBus, UsbBusType},
            pac::SPI3,
        };

        pub use stm32_hal2::pac::{USART3 as UART_ELRS, ADC2 as ADC};

        type SpiFlash = Spi<SPI3>;
    }
}

cfg_if! {
    if #[cfg(feature = "fixed-wing")] {
        use flight_ctrls::{autopilot::Orbit, ControlPositions};
    } else {
        use flight_ctrls::RotationDir;
    }
}

// Due to the way the USB serial lib is set up, the USB bus must have a static lifetime.
// In practice, we only mutate it at initialization.
static mut USB_BUS: Option<UsbBusAllocator<UsbBusType>> = None;

// todo: Can't get startup code working separately since Shared and Local must be private per an RTIC restriction.
// todo: See this GH issue: https://github.com/rtic-rs/cortex-m-rtic/issues/505
// mod startup;

// todo: Cycle flash pages for even wear. Can postpone this.

cfg_if! {
    if #[cfg(feature = "h7")] {
        // H723: 1Mb of flash, in one bank.
        // 8 sectors of 128kb each.
        // (H743 is similar, but may have 2 banks, each with those properties)
        const FLASH_CFG_SECTOR: usize = 6;
        const FLASH_WAYPOINT_SECTOR: usize = 7;

        // The rate our main program updates, in Hz.
        // todo note that we will have to scale up values slightly on teh H7 board with 32.768kHz oscillator:
        // ICM-42688 DS: The ODR values shown in the
        // datasheet are supported with external clock input frequency of 32kHz. For any other external
        // clock input frequency, these ODR values will scale by a factor of (External clock value in kHz / 32).
        // For example, if an external clock frequency of 32.768kHz is used,
        // instead of ODR value of 500Hz, it will be 500 * (32.768 / 32) = 512Hz.
        const IMU_UPDATE_RATE: f32 = 4_096.;  // todo: Experimenting
        // const IMU_UPDATE_RATE: f32 = 8_192.;
    } else {
        // G47x/G48x: 512k flash.
        // Assumes configured as a single bank: 128 pages of 4kb each.
        // (If using G4 dual bank mode: 128 pages of pages of 2kb each, per bank)
        const FLASH_CFG_PAGE: usize = 126;
        const FLASH_WAYPOINT_PAGE: usize = 127;

        // Todo: Measured: 8.042kHz (2022-10-26)
        const IMU_UPDATE_RATE: f32 = 4_000.; // todo experimenting.
        // const IMU_UPDATE_RATE: f32 = 8_000.;
    }
}

const UPDATE_RATE_MAIN_LOOP: f32 = 600.; // todo: Experiment with this.

// Every x main update loops, log parameters etc to flash.
const LOGGING_UPDATE_RATIO: usize = 100;

// Every x main update loops, print system status and sensor readings to console,
// if enabled with the `print-status` feature.
const PRINT_STATUS_RATIO: usize = 2_000;

// Every x main loops, log RPM (or servo posit) to angular accel (thrust) data.
const THRUST_LOG_RATIO: usize = 20;

const DT_IMU: f32 = 1. / IMU_UPDATE_RATE;
const DT_MAIN_LOOP: f32 = 1. / UPDATE_RATE_MAIN_LOOP;

#[cfg(feature = "h7")]
static mut USB_EP_MEMORY: [u32; 1024] = [0; 1024];

// We use `LOOP_I` to manage sequencing the velocity-update PID from within the attitude-update PID
// loop.
// const VELOCITY_ATTITUDE_UPDATE_RATIO: usize = 4;
// todo: Currently unused.
const FIXED_WING_RATE_UPDATE_RATIO: usize = 16; // 8k IMU update rate / 16 / 500Hz; apt for servos.

// todo: Temp as we switch from PID to other controls; we still will have
// todo params that can be adjusetd in flight.
const CTRL_COEFF_ADJ_TIMEOUT: f32 = 0.3; // seconds
const CTRL_COEFF_ADJ_AMT: f32 = 0.01; // seconds

// The time, in ms, to wait during initializing to allow the ESC and RX to power up and initialize.
const WARMUP_TIME: u32 = 2_000;

// todo: Bit flags that display as diff colored LEDs, and OSD items

// todo: dispatchers are temp
#[rtic::app(device = pac, peripherals = false, dispatchers= [EXTI9_5])]
mod app {
    use super::*;
    use core::sync::atomic::Ordering;

    use crate::flight_ctrls::common::Motor;
    use core::time::Duration; // todo temp
use stm32_hal2::instant::Instant;
    use stm32_hal2::timer::TimChannel; // todo temp

    #[monotonic(binds = TIM5, default = true)]
    // type MyMono = Timer<TIM5>; // todo temp
    #[shared]
    struct Shared {
        user_cfg: UserCfg,
        state_volatile: StateVolatile,
        system_status: SystemStatus,
        autopilot_status: AutopilotStatus,
        current_params: Params,
        // todo: `params_prev` is an experimental var used in our alternative/experimental
        // todo flight controls code as a derivative.
        params_prev: Params,
        control_channel_data: ChannelData,
        /// Link statistics, including Received Signal Strength Indicator (RSSI) from the controller's radio.
        link_stats: LinkStats,
        dma: Dma<DMA1>,
        dma2: Dma<DMA2>,
        spi1: Spi<SPI1>,
        cs_imu: Pin,
        i2c1: I2c<I2C1>,
        i2c2: I2c<I2C2>,
        altimeter: baro::Altimeter,
        flash_onboard: Flash,
        batt_curr_adc: Adc<ADC>,
        rf_limiter_timer: Timer<TIM16>,
        lost_link_timer: Timer<TIM17>,
        link_lost: bool, // todo: atomic bool? Separate froms StateVolatile due to how it's used.
        motor_timers: MotorTimers,
        usb_dev: UsbDevice<'static, UsbBusType>,
        usb_serial: SerialPort<'static, UsbBusType>,
        /// `power_used` is in rotor power (0. to 1. scale), summed for each rotor x milliseconds.
        power_used: f32,
        imu_filters: ImuFilters,
        flight_ctrl_filters: FlightCtrlFilters,
        // Note: We don't currently haveh PID filters, since we're not using a D term for the
        // RPM PID.
        ahrs: Ahrs,
        imu_calibration: imu_calibration::ImuCalibration,
        ext_sensor_active: ExtSensor,
        pwr_maps: PowerMaps,
        /// Store rotor RPM: (M1, M2, M3, M4). Quad only, but we can't feature gate
        /// shared fields.
        rotor_rpms: MotorRpm,
        motor_pid_state: MotorPidGroup,
        /// PID motor coefficients
        motor_pid_coeffs: MotorCoeffs,
    }

    #[local]
    struct Local {
        update_timer: Timer<TIM15>,
        uart_elrs: Usart<UART_ELRS>, // for ELRS over CRSF.
        // spi_flash: SpiFlash,  // todo: Fix flash in HAL, then do this.
        arm_signals_received: u8, // todo: Put sharedin state volatile.
        disarm_signals_received: u8,
        /// We use this counter to subdivide the main loop into longer intervals,
        /// for various tasks like logging, and outer loops.
        update_isr_loop_i: usize,
        imu_isr_loop_i: usize,
        aux_loop_i: usize, // todo temp
        ctrl_coeff_adj_timer: Timer<TIM1>,
        uart_osd: Usart<USART2>, // for our DJI OSD, via MSP protocol
        time_with_high_throttle: f32,
        measurement_timer: Timer<TIM5>,
    }

    #[init]
    fn init(cx: init::Context) -> (Shared, Local, init::Monotonics) {
        // See note above about an RTIC limit preventing us from initing this way.
        // startup::init(&cx.core)

        // Cortex-M peripherals
        let mut cp = cx.core;
        // Set up microcontroller peripherals
        let mut dp = pac::Peripherals::take().unwrap();

        // Improves performance, at a cost of slightly increased power use.
        // Note that these enable fns should automatically invalidate prior.
        cp.SCB.enable_icache();
        cp.SCB.enable_dcache(&mut cp.CPUID);

        cfg_if! {
            if #[cfg(feature = "h7")] {
                let clock_cfg = Clocks {
                    // Config for H723 at 520Mhz, or H743 at 480Mhz.
                    // Note: to get H723 working at 550Mhz. You'll need to increase divn to 275,
                    // and set CPU_FREQ_BOOST: "The CPU frequency boost can be enabled through the
                    // CPUFREQ_BOOST option byte in the FLASH_OPTSR2_PRG register."
                    pll_src: PllSrc::Hse(16_000_000),
                    pll1: PllCfg {
                        // todo: Do we want a 64Mhz HSE for H7?
                        divm: 8, // To compensate with 16Mhz HSE instead of 64Mhz HSI
                        // divn: 275, // For 550Mhz with freq boost enabled.
                        ..Default::default()
                    },
                    hsi48_on: true,
                    usb_src: clocks::UsbSrc::Hsi48,

                    ..Clocks::full_speed()
                };
            } else {
                let clock_cfg = Clocks {
                    input_src: InputSrc::Pll(PllSrc::Hse(16_000_000)),
                    hsi48_on: true,
                    clk48_src: clocks::Clk48Src::Hsi48,
                    ..Default::default()
                };
            }
        }

        clock_cfg.setup().unwrap();

        // Enable the Clock Recovery System, which improves HSI48 accuracy.
        #[cfg(feature = "h7")]
            clocks::enable_crs(CrsSyncSrc::OtgHs);
        #[cfg(feature = "g4")]
            clocks::enable_crs(CrsSyncSrc::Usb);

        let flash = unsafe {
            &(*pac::FLASH::ptr())
        };


        cp.SCB.enable_icache();
        println!("Icache: {:?}", cortex_m::peripheral::SCB::icache_enabled());
        println!("Icache: {:?}", cortex_m::peripheral::SCB::dcache_enabled());

        // Set up pins with appropriate modes.
        setup::setup_pins();

        let mut dma = Dma::new(dp.DMA1);
        let mut dma2 = Dma::new(dp.DMA2);
        #[cfg(feature = "g4")]
            dma::enable_mux1();

        setup::setup_dma(&mut dma, &mut dma2);

        #[cfg(feature = "h7")]
            let UART_ELRS = dp.UART7;
        #[cfg(feature = "g4")]
            let UART_ELRS = dp.USART3;

        let (mut spi1, mut cs_imu, mut cs_flash, mut i2c1, mut i2c2, uart_osd, mut uart_elrs) =
            setup::setup_busses(dp.SPI1, dp.I2C1, dp.I2C2, dp.USART2, UART_ELRS, &clock_cfg);

        let mut delay = Delay::new(cp.SYST, clock_cfg.systick());

        // We use the RTC to assist with power use measurement.
        // let rtc = Rtc::new(dp.RTC, Default::default());

        // We use the ADC to measure battery voltage and ESC current.
        let adc_cfg = AdcConfig {
            operation_mode: adc::OperationMode::Continuous,
            ..Default::default()
        };

        #[cfg(feature = "h7")]
            let mut batt_curr_adc = Adc::new_adc1(dp.ADC1, AdcDevice::One, adc_cfg, &clock_cfg);

        #[cfg(feature = "g4")]
            let mut batt_curr_adc = Adc::new_adc2(dp.ADC2, AdcDevice::Two, adc_cfg, &clock_cfg);

        // With non-timing-critical continuous reads, we can set a long sample time.
        batt_curr_adc.set_sample_time(setup::BATT_ADC_CH, adc::SampleTime::T601);

        // todo temp while we sort out HAL. We've fudged this to make the number come out correctly.
        batt_curr_adc.vdda_calibrated = 3.6;

        let mut update_timer = Timer::new_tim15(
            dp.TIM15,
            UPDATE_RATE_MAIN_LOOP,
            Default::default(),
            &clock_cfg,
        );
        update_timer.enable_interrupt(TimerInterrupt::Update);

        // This timer is used to prevent a stream of continuous RF control signals, should they arrive
        // at any reason, from crashing the FC. Ie, limit damage that can be done from external sources.
        let mut rf_limiter_timer = Timer::new_tim16(
            dp.TIM16,
            safety::MAX_RF_UPDATE_RATE,
            Default::default(),
            &clock_cfg,
        );

        rf_limiter_timer.enable_interrupt(TimerInterrupt::Update);

        // We use this measurement timer to count things, like time between IMU updates.
        // The effective period (1/freq) must be greater than the time we wish to measure.
        let mut measurement_timer =
            Timer::new_tim5(dp.TIM5, 2_000., Default::default(), &clock_cfg);

        // We use this PID adjustment timer to indicate the interval for updating PID from a controller
        // while the switch or button is held. (Equivalently, the min allowed time between actuations)

        let ctrl_coeff_adj_timer = Timer::new_tim1(
            dp.TIM1,
            1. / CTRL_COEFF_ADJ_TIMEOUT,
            TimerConfig {
                one_pulse_mode: true,
                ..Default::default()
            },
            &clock_cfg,
        );

        let rotor_timer_cfg = TimerConfig {
            // We use ARPE since we change duty with the timer running.
            auto_reload_preload: true,
            ..Default::default()
        };

        // We use multiple timers instead of a single one based on pin availability; a single
        // timer with 4 channels would be ideal.
        // Frequency here can be arbitrary; we set manually using PSC and ARR below.
        cfg_if! {
            if #[cfg(feature = "h7")] {
                let r1234 = Timer::new_tim3(dp.TIM3, 1., rotor_timer_cfg.clone(), &clock_cfg);

                // For fixed wing on H7; need a separate timer from the 4 used for DSHOT.
                let mut servos = Timer::new_tim8(dp.TIM8, 1., rotor_timer_cfg, &clock_cfg);

                let mut motor_timers = MotorTimers { rotors, servos };

            } else if #[cfg(feature = "g4")] {
                let mut r12 =
                    Timer::new_tim2(dp.TIM2, 1., rotor_timer_cfg.clone(), &clock_cfg);

                let mut r34_servos = Timer::new_tim3(dp.TIM3, 1., rotor_timer_cfg, &clock_cfg);

                #[cfg(feature = "quad")]
                let mut motor_timers = MotorTimers { r12, r34: r34_servos };
                #[cfg(feature = "fixed-wing")]
                let mut motor_timers = MotorTimers { r12, servos: r34_servos };
            }
        }

        let mut lost_link_timer = Timer::new_tim17(
            dp.TIM17,
            1. / safety::LOST_LINK_TIMEOUT,
            TimerConfig {
                one_pulse_mode: true,
                ..Default::default()
            },
            &clock_cfg,
        );
        lost_link_timer.enable_interrupt(TimerInterrupt::Update);

        let mut user_cfg = UserCfg::default();
        // todo temp
        user_cfg.waypoints[0] = Some(Location {
            type_: LocationType::LatLon,
            name: [0, 1, 2, 3, 4, 5, 6],
            lon: 1.,
            lat: 2.,
            alt_msl: 3.,
        });

        flight_ctrls::setup_timers(&mut motor_timers);

        // Note: With this circular DMA approach, we discard many readings,
        // but shouldn't have consequences other than higher power use, compared to commanding
        // conversions when needed.

        unsafe {
            batt_curr_adc.read_dma(
                &mut V_A_ADC_READ_BUF,
                &[setup::BATT_ADC_CH, setup::CURR_ADC_CH],
                setup::BATT_CURR_DMA_CH,
                ChannelCfg {
                    circular: dma::Circular::Enabled,
                    ..Default::default()
                },
                &mut dma,
            );
        }

        // todo: ID connected sensors etc by checking their device ID etc.
        let mut state_volatile = StateVolatile::default();

        cfg_if! {
            if #[cfg(feature = "h7")] {
                let usb = Usb1::new(
                    dp.OTG1_HS_GLOBAL,
                    dp.OTG1_HS_DEVICE,
                    dp.OTG1_HS_PWRCLK,
                    clock_cfg.hclk(),
                );

                unsafe { USB_BUS = Some(UsbBus::new(usb, unsafe { &mut USB_EP_MEMORY })) };
            } else {
                let usb = usb::Peripheral { regs: dp.USB };

                unsafe { USB_BUS = Some(UsbBus::new(usb)) };
            }
        }

        let usb_serial = SerialPort::new(unsafe { USB_BUS.as_ref().unwrap() });

        // todo: Note that you may need to either increment the flash page offset, or cycle flash pages, to
        // todo avoid wear on a given sector from erasing each time. Note that you can still probably get 10k
        // todo erase/write cycles per sector, so this is low priority.
        let mut flash_onboard = Flash::new(dp.FLASH);

        // todo: Testing flash
        let mut flash_buf = [0; 8];
        // let cfg_data =
        #[cfg(feature = "h7")]
            flash_onboard.read(Bank::B1, crate::FLASH_CFG_SECTOR, 0, &mut flash_buf);
        #[cfg(feature = "g4")]
            flash_onboard.read(Bank::B1, crate::FLASH_CFG_PAGE, 0, &mut flash_buf);

        // println!(
        //     "mem val: {}",
        //     flash_onboard.read(FLASH_CFG_PAGE, 0)
        // );

        println!("Flash Buf ( should be 1, 2, 3, 0, 0): {:?}", flash_buf);
        // flash_onboard.erase_write_page(Bank::B1, FLASH_CFG_PAGE, &[10, 11, 12, 13, 14, 15, 16, 17]).ok();
        println!("Flash write complete");

        let mut params = Default::default();

        // todo: For params, consider raw readings without DMA. Currently you're just passign in the
        // todo default; not going to cut it.?
        let (system_status, altimeter) = setup::init_sensors(
            &mut params,
            &mut state_volatile.base_point,
            &mut spi1,
            &mut i2c1,
            &mut i2c2,
            &mut cs_imu,
            &mut delay,
        );

        println!(
            "System status:\n IMU: {}, Baro: {}, Mag: {}, GPS: {}, TOF: {}",
            system_status.imu == SensorStatus::Pass,
            system_status.baro == SensorStatus::Pass,
            system_status.magnetometer == SensorStatus::Pass,
            system_status.gps == SensorStatus::Pass,
            system_status.tof == SensorStatus::Pass,
        );

        // loop {
        //     let pressure = altimeter.read_pressure(&mut i2c2);
        //     println!("Pressure: {}", pressure);
        //
        //     let temp = altimeter.read_temp(&mut i2c2);
        //     println!("Temp: {}", temp);
        //
        //     delay.delay_ms(500);
        // }

        // todo: Calibation proecedure, either in air or on ground.
        let ahrs_settings = ahrs_fusion::Settings::default();

        // Note: Calibration and offsets ares handled handled by their defaults currently.
        let imu_calibration = imu_calibration::ImuCalibration {
            // gyro_misalignment: Mat3 {
            //     data: [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0],
            // },
            // gyro_sensitivity: Vec3::new(1.0, 1.0, 1.0),
            // gyro_offset: Vec3::new(0.0, 0.0, 0.0),
            // accel_misalignment: Mat3 {
            //     data: [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0],
            // },
            // accel_sensitivity: Vec3::new(1.0, 1.0, 1.0),
            // accel_offset: Vec3::new(0.0, 0.0, 0.0),
            ..Default::default()
        }; // todo - load from flash

        let ahrs = Ahrs::new(&ahrs_settings, crate::IMU_UPDATE_RATE as u32);

        // Allow ESC to warm up and the radio to connect before starting the main loop.
        delay.delay_ms(WARMUP_TIME);

        // We must set up the USB device after the warmup delay, since its long blocking delay
        // leads teh host (PC) to terminate the connection. The (short, repeated) blocking delays
        // in the motor dir config appear to be acceptable.
        let usb_dev = UsbDeviceBuilder::new(
            unsafe { USB_BUS.as_ref().unwrap() },
            UsbVidPid(0x16c0, 0x27dd),
        )
            .manufacturer("Anyleaf")
            .product("Mercury")
            // We use `serial_number` to identify the device to the PC. If it's too long,
            // we get permissions errors on the PC.
            .serial_number("AN") // todo: Try 2 letter only if causing trouble?
            .device_class(usbd_serial::USB_CLASS_CDC)
            .build();

        // Set up the main loop, the IMU loop, the CRSF reception after the (ESC and radio-connection)
        // warmpup time.

        // Set up motor direction; do this once the warmup time has elapsed.
        let motors_reversed = (
            user_cfg.control_mapping.m1_reversed,
            user_cfg.control_mapping.m2_reversed,
            user_cfg.control_mapping.m3_reversed,
            user_cfg.control_mapping.m4_reversed,
        );

        // todo: temp removed to test bidir
        dshot::setup_motor_dir(motors_reversed, &mut motor_timers, &mut dma);

        crsf::setup(&mut uart_elrs);

        // Start our main loop
        update_timer.enable();

        // Start out IMU-driven loop
        // todo: This is an awk way; Already set up /configured like this in `setup`, albeit with
        // todo opendrain and pullup set, and without enabling interrupt.
        #[cfg(feature = "h7")]
            let mut imu_exti_pin = Pin::new(Port::B, 12, gpio::PinMode::Input);
        #[cfg(feature = "g4")]
            let mut imu_exti_pin = Pin::new(Port::C, 4, gpio::PinMode::Input);
        imu_exti_pin.enable_interrupt(Edge::Falling);

        println!("Init complete; starting main loops");

        (
            // todo: Make these local as able.
            Shared {
                user_cfg,
                state_volatile,
                system_status,
                autopilot_status: Default::default(),
                current_params: params.clone(),
                params_prev: params,
                control_channel_data: Default::default(),
                link_stats: Default::default(),
                dma,
                dma2,
                spi1,
                cs_imu,
                i2c1,
                i2c2,
                altimeter,
                batt_curr_adc,
                // rtc,
                rf_limiter_timer,
                lost_link_timer,
                link_lost: true, // Initialize to not being on the link
                motor_timers,
                usb_dev,
                usb_serial,
                flash_onboard,
                power_used: 0.,
                imu_filters: Default::default(),
                flight_ctrl_filters: Default::default(),
                ahrs,
                imu_calibration,
                ext_sensor_active: ExtSensor::Mag,
                pwr_maps: Default::default(),
                motor_pid_state: Default::default(),
                motor_pid_coeffs: Default::default(),
                rotor_rpms: Default::default(),
            },
            Local {
                update_timer,
                uart_elrs,
                // spi_flash, // todo: Fix flash in HAL, then do this.
                arm_signals_received: 0,
                disarm_signals_received: 0,
                update_isr_loop_i: 0,
                imu_isr_loop_i: 0,
                aux_loop_i: 0, // todo t
                ctrl_coeff_adj_timer,
                uart_osd,
                time_with_high_throttle: 0.,
                measurement_timer,
            },
            init::Monotonics(),
            // init::Monotonics(measurement_timer)
        )
    }

    #[idle(shared = [], local = [])]
    /// In this function, we perform setup code that must occur with interrupts enabled.
    fn idle(_cx: idle::Context) -> ! {
        loop {
            asm::nop();
        }
    }

    // // todo temp
    // #[task(priority = 10)]
    // fn foo(_: foo::Context) {
    //     println!("Mono!");
    //
    //     // Schedule `bar` to run 2 seconds in the future (1 second after foo runs)
    //     // bar::spawn_after(1.secs()).unwrap();
    // }

    // todo: Go through these tasks, and make sure they're not reserving unneded shared params.

    // binds = TIM15,
    // todo: Remove rotor timers, spi, and dma from this ISR; we only use it for tresting DSHOT
    #[task(
    binds = TIM1_BRK_TIM15,
    shared = [current_params,
    power_used, autopilot_status, user_cfg, flight_ctrl_filters,
    ahrs, control_channel_data, motor_timers, rotor_rpms,
    lost_link_timer, link_lost, altimeter, i2c1, i2c2, state_volatile, system_status, batt_curr_adc, dma, dma2,
    ],
    local = [arm_signals_received, disarm_signals_received, update_isr_loop_i, uart_osd,
    time_with_high_throttle],

    priority = 5
    )]
    /// This runs periodically, on a ~1kHz timer. It's used to trigger the attitude and velocity PID loops, ie for
    /// sending commands to the attitude and rate PID loop based on things like autopilot, command-mode etc.
    /// We give it a relatively high priority, to ensure it gets run despite faster processes ocurring.
    fn update_isr(mut cx: update_isr::Context) {
        unsafe { (*pac::TIM15::ptr()).sr.modify(|_, w| w.uif().clear_bit()) }
        *cx.local.update_isr_loop_i += 1;

        (
            cx.shared.current_params,
            cx.shared.ahrs,
            cx.shared.control_channel_data,
            cx.shared.power_used,
            cx.shared.autopilot_status,
            cx.shared.user_cfg,
            cx.shared.lost_link_timer,
            cx.shared.link_lost,
            cx.shared.altimeter,
            cx.shared.i2c1,
            cx.shared.i2c2,
            cx.shared.state_volatile,
            cx.shared.system_status,
            cx.shared.motor_timers,
            cx.shared.batt_curr_adc,
            cx.shared.dma,
            cx.shared.dma2,
            cx.shared.rotor_rpms,
            cx.shared.flight_ctrl_filters,
        )
            .lock(
                |params,
                 ahrs,
                 control_channel_data,
                 power_used,
                 autopilot_status,
                 cfg,
                 lost_link_timer,
                 link_lost,
                 altimeter,
                 i2c1,
                 i2c2,
                 state_volatile,
                 system_status,
                 motor_timers,
                 adc,
                 dma,
                 dma2,
                 rpms,
                 flight_ctrl_filters| {
                    #[cfg(feature = "print-status")]
                    if *cx.local.update_isr_loop_i % PRINT_STATUS_RATIO == 0 {
                        // todo: Flesh this out, and perhaps make it more like Preflight.

                        // println!("DSHOT: {:?}", unsafe { dshot::PAYLOAD_R1_2_REC });

                        println!(
                            "\n\nFaults. Rx: {}. RPM: {}", system_status.rf_control_fault, system_status.esc_rpm_fault,
                        );

                        println!(
                            "\nControl data:\nPitch: {} Roll: {}, Yaw: {}, Throttle: {}, Arm switch: {}",
                            control_channel_data.pitch, control_channel_data.roll,
                            control_channel_data.yaw, control_channel_data.throttle,
                            control_channel_data.arm_status == ArmStatus::Armed, // todo fixed-wing
                        );

                        #[cfg(feature = "quad")]
                        println!("Motors armed: {:?}", state_volatile.arm_status == ArmStatus::Armed);

                        #[cfg(feature = "fixed-wing")]
                        println!("Motors armed: {:?}", state_volatile.arm_status == ArmStatus::MotorsArmed ||
                            state_volatile.arm_status == ArmStatus::MotorsControlsArmed);

                        #[cfg(feature = "fixed-wing")]
                        println!("Controls armed: {:?}", state_volatile.arm_status == ArmStatus::ControlsArmed ||
                            state_volatile.arm_status == ArmStatus::MotorsControlsArmed);

                        #[cfg(feature = "quad")]
                        println!(
                            "Autopilot_status: Alt hold: {} Heading hold: {}, Yaw assist: {}, Direct to point: {}, \
                            sequence: {}, takeoff: {}, land: {}, recover: {}, loiter: {}",
                            autopilot_status.alt_hold.is_some(), autopilot_status.hdg_hold.is_some(),
                            autopilot_status.yaw_assist != flight_ctrls::autopilot::YawAssist::Disabled,
                            autopilot_status.direct_to_point.is_some(),
                            autopilot_status.sequence, autopilot_status.takeoff, autopilot_status.land.is_some(),
                            autopilot_status.recover.is_some(),
                            autopilot_status.loiter.is_some(),
                        );

                        #[cfg(feature = "fixed-wing")]
                        println!(
                            "Autopilot_status: Alt hold: {} Heading hold: {}, Direct to point: {}, \
                            sequence: {}, takeoff: {}, land: {}, recover: {}, loiter/orbit: {}",
                            autopilot_status.alt_hold.is_some(), autopilot_status.hdg_hold.is_some(),
                            autopilot_status.direct_to_point.is_some(),
                            autopilot_status.sequence, autopilot_status.takeoff, autopilot_status.land.is_some(),
                            autopilot_status.recover.is_some(),
                            autopilot_status.orbit.is_some(),
                        );

                        println!("Batt V: {} ESC current: {}", state_volatile.batt_v, state_volatile.esc_current);
                        //
                        // println!(
                        //     "Accel: Ax {}, Ay: {}, Az: {}",
                        //     params.a_x, params.a_y, params.a_z
                        // );
                        // //
                        // println!(
                        //     "Gyro: roll {}, pitch: {}, yaw: {}",
                        //     params.v_roll, params.v_pitch, params.v_yaw
                        // );
                        // // //
                        println!("Attitude quat: {} {} {} {}",
                                 ahrs.quaternion.w,
                                 ahrs.quaternion.x,
                                 ahrs.quaternion.y,
                                 ahrs.quaternion.z
                        );
                        println!(
                            "Attitude: pitch: {}, roll: {}, yaw: {}\n",
                            params.s_pitch, params.s_roll, params.s_yaw_heading
                        );

                        if let Some(q) = state_volatile.attitude_commanded.quat {
                            println!("Commanded attitude quat: {} {} {} {}",
                                     q.w,
                                     q.x,
                                     q.y,
                                     q.z
                            );

                            let (p, r, y) = q.to_euler();
                            println!("Commanded attitude: pitch: {}, roll: {}, yaw: {}\n", p, r, y);
                        }


                        println!(
                            "RPMs: FL {}, FR: {}, AL: {}, AR: {}\n",
                            rpms.front_left, rpms.front_right, rpms.aft_left, rpms.aft_right
                        );

                        // println!("In acro mode: {:?}", *input_mode == InputMode::Acro);
                        // println!(
                        //     "Input mode sw: {:?}",
                        //     control_channel_data.input_mode == InputModeSwitch::Acro
                        // );

                    }
                    // if *cx.local.update_isr_loop_i % LOGGING_UPDATE_RATIO == 0 {
                    // todo: Eg log params to flash etc.
                    // }

                    // Start DMA sequences for I2C sensors, ie baro, mag, GPS, TOF.
                    // DMA TC isrs are sequenced.
                    // todo: Put back; Troubleshooting control and motors issues.
                    // sensors_shared::start_transfers(i2c1, i2c2, dma2);

                    safety::handle_arm_status(
                        cx.local.arm_signals_received,
                        cx.local.disarm_signals_received,
                        control_channel_data.arm_status,
                        &mut state_volatile.arm_status,
                        control_channel_data.throttle,
                    );

                    if !state_volatile.has_taken_off {
                        safety::handle_takeoff_attitude_lock(
                            control_channel_data.throttle,
                            &mut cx.local.time_with_high_throttle,
                            &mut state_volatile.has_taken_off,
                            DT_MAIN_LOOP,
                        );
                    }

                    #[cfg(feature = "quad")]
                        flight_ctrls::set_input_mode(control_channel_data.input_mode, state_volatile, system_status);

                    // todo: Support UART telemetry from ESC.

                    // todo: Determine timing for OSD update, and if it should be in this loop,
                    // todo, or slower.

                    let batt_v =
                        adc.reading_to_voltage(unsafe { V_A_ADC_READ_BUF }[0]) * sensors_shared::ADC_BATT_V_DIV;
                    let curr_v =
                        adc.reading_to_voltage(unsafe { V_A_ADC_READ_BUF }[1]) * sensors_shared::ADC_CURR_DIV;

                    // todo: Find the current conversion factor. Probably slope + y int
                    let esc_current = curr_v;

                    state_volatile.batt_v = batt_v;
                    state_volatile.esc_current = esc_current;

                    // todo: Put back A/R
                    // This difference in approach between quad and fixed-wing for the
                    // control deltas is due to using an intermediate step between control settings
                    // and accel for quads (RPM), but doing it directly on fixed-wing,being unable
                    // to measure servo posit directly (which would be the equiv intermediate step)
                    // cfg_if! {
                    //     if #[cfg(feature = "quad")] {
                    //         let pitch_delta = rpms.pitch_delta();
                    //         let roll_delta = rpms.roll_delta();
                    //         let yaw_delta = rpms.yaw_delta(cfg.control_mapping.frontleft_aftright_dir);
                    //
                    //         let pitch_accel = state_volatile.accel_map.pitch_rpm_to_accel(pitch_delta);
                    //         let roll_accel = state_volatile.accel_map.roll_rpm_to_accel(roll_delta);
                    //         let yaw_accel = state_volatile.accel_map.yaw_rpm_to_accel(yaw_delta);
                    //     } else {
                    //         let pitch_delta = state_volatile.ctrl_positions.pitch_delta();
                    //         let roll_delta = state_volatile.ctrl_positions.roll_delta();
                    //         let yaw_delta = state_volatile.ctrl_positions.yaw_delta();
                    //
                    //         let pitch_accel = state_volatile.accel_map.pitch_cmd_to_accel(pitch_delta);
                    //         let roll_accel = state_volatile.accel_map.roll_cmd_to_accel(roll_delta);
                    //         let yaw_accel = state_volatile.accel_map.yaw_cmd_to_accel(yaw_delta);
                    //     }
                    // }
                    //
                    // // Estimate the angular drag coefficient for the current flight regime.
                    // let drag_coeff_pitch = flight_ctrls::ctrl_logic::calc_drag_coeff(
                    //     params.v_pitch,
                    //     params.a_pitch,
                    //     pitch_accel,
                    // );
                    //
                    // let drag_coeff_roll = flight_ctrls::ctrl_logic::calc_drag_coeff(
                    //     params.v_roll,
                    //     params.a_roll,
                    //     state_volatile.accel_map.roll_rpm_to_accel(roll_delta),
                    // );
                    //
                    // #[cfg(feature = "quad")]
                    //     let drag_coeff_yaw = flight_ctrls::ctrl_logic::calc_drag_coeff(
                    //     params.v_yaw,
                    //     params.a_yaw,
                    //     state_volatile.accel_map.yaw_rpm_to_accel(yaw_delta),
                    // );
                    //
                    // let (dcp, dcr, dcy) = flight_ctrl_filters.apply(drag_coeff_pitch, drag_coeff_roll, drag_coeff_yaw);
                    // state_volatile.drag_coeffs.pitch = dcp;
                    // state_volatile.drag_coeffs.roll = dcr;
                    // state_volatile.drag_coeffs.yaw = dcy;

                    // todo: Put back
                    // let osd_data = OsdData {
                    //     arm_status: state_volatile.arm_status,
                    //     battery_voltage: batt_v,
                    //     current_draw: esc_current,
                    //     alt_msl_baro: params.baro_alt_msl,
                    //     gps_fix: Location::default(),
                    //     pitch: params.s_pitch,
                    //     roll: params.s_roll,
                    //     yaw: params.s_yaw_heading,
                    //     // pid_p: coeffs.roll.k_p_rate,
                    //     // pid_i: coeffs.roll.k_i_rate,
                    //     // pid_d: coeffs.roll.k_d_rate,
                    //     autopilot: AutopilotData {
                    //         takeoff: autopilot_status.takeoff,
                    //         land: autopilot_status.land.is_some(),
                    //         direct_to_point: autopilot_status.direct_to_point.is_some(),
                    //         #[cfg(feature = "fixed-wing")]
                    //         orbit: autopilot_status.orbit.is_some(),
                    //         alt_hold: autopilot_status.alt_hold.is_some(),
                    //         #[cfg(feature = "quad")]
                    //         loiter: autopilot_status.loiter.is_some(),
                    //     },
                    //     base_dist_bearing: (
                    //         0., 0., // todo: Fill these out
                    //     ),
                    // };
                    //
                    // // todo: Put back
                    // osd::send_osd_data(cx.local.uart_osd, setup::OSD_CH, dma, &osd_data);

                    autopilot_status.set_modes_from_ctrls(control_channel_data, &params);

                    #[cfg(feature = "quad")]
                        let ap_cmds = autopilot_status.apply(
                        params,
                        // filters,
                        // coeffs,
                        system_status,
                    );

                    #[cfg(feature = "fixed-wing")]
                        let ap_cmds = autopilot_status.apply(
                        params,
                        // pid_attitude,
                        // filters,
                        // coeffs,
                        system_status,
                    );

                    // Don't apply autopilot modes if on the ground.
                    if !state_volatile.has_taken_off {
                        // The intermediate variable is due to a attribute binding
                        // issue with teh direct approach.
                        state_volatile.autopilot_commands = ap_cmds;
                    }

                    // if let OperationMode::Preflight = state_volatile.op_mode {
                    //     // exit this fn during preflight *after* measuring voltages using ADCs.
                    //     return;
                    // }

                    // todo: This should probably be delegatd to a fn; get it
                    // todo out here
                    if *cx.local.update_isr_loop_i % THRUST_LOG_RATIO == 0 {
                        cfg_if! {
                            if #[cfg(feature = "quad")] {
                                state_volatile.power_maps.rpm_to_accel_pitch.log_val(
                                // todo: Populate this, and consider if you want rpms to be by motor or rotor posit
                                //     pwr.front_left + pwr.front_right - pwr.aft_left - pwr.aft_right,
                                    // rpms.m1 + rpms.m2 + rpms.m3 + rpms.m4
                                    // todo: Motors. Map Motor num to rotor position here.
                                    // todo: Possibly with helper methods.
                                    0.,
                                    0.,
                                );

                                state_volatile.power_maps.rpm_to_accel_roll.log_val(
                                    0.,
                                    0.,
                                );

                                let mut yaw_pwr = 0.;
                                if cfg.control_mapping.frontleft_aftright_dir == RotationDir::Clockwise {
                                    yaw_pwr *= -1.;
                                }
                                state_volatile.power_maps.rpm_to_accel_yaw.log_val(
                                    yaw_pwr,
                                    0.,
                                );
                            }
                        }
                        // Note: We currently don't have a way to measure servo position,
                        // so we leave the default 1:1 mapping here.
                    }
                }
            )
    }

    /// Runs when new IMU data is ready. Trigger a DMA read.
    /// High priority since it's important, and quick-to-execute
    #[task(binds = EXTI4, shared = [cs_imu, dma, spi1], local = [], priority = 8)]
    fn imu_data_isr(cx: imu_data_isr::Context) {
        gpio::clear_exti_interrupt(4);

        // *cx.local.aux_loop_i += 1;
        // if *cx.local.aux_loop_i % 700 == 0 {
        // println!("I");
        // }

        (cx.shared.dma, cx.shared.cs_imu, cx.shared.spi1).lock(|dma, cs_imu, spi| {
            imu_shared::read_imu(imu::READINGS_START_ADDR, spi, cs_imu, dma);
        });
    }

    // binds = DMA1_STR2,
    #[task(binds = DMA1_CH2, shared = [dma, spi1, current_params, params_prev, control_channel_data,
    autopilot_status, imu_filters, flight_ctrl_filters, cs_imu, user_cfg, motor_pid_state, motor_pid_coeffs,
    motor_timers, ahrs, state_volatile], local = [imu_isr_loop_i], priority = 4)]
    /// This ISR Handles received data from the IMU, after DMA transfer is complete. This occurs whenever
    /// we receive IMU data; it nominally (and according to our measurements so far) runs at 8kHz.
    /// Note that on the H7 FC with the dedicated IMU LSE, it may run slightly faster.
    fn imu_tc_isr(mut cx: imu_tc_isr::Context) {
        // Clear DMA interrupt this way due to RTIC conflict.
        #[cfg(feature = "h7")]
            unsafe {
            (*DMA1::ptr()).lifcr.write(|w| w.ctcif2().set_bit())
        }
        #[cfg(feature = "g4")]
            unsafe {
            (*DMA1::ptr()).ifcr.write(|w| w.tcif2().set_bit())
        }

        cx.shared.cs_imu.lock(|cs| {
            cs.set_high();
        });

        // println!("IMU LOOP");

        *cx.local.imu_isr_loop_i += 1;

        (
            cx.shared.current_params,
            cx.shared.params_prev,
            cx.shared.ahrs,
            cx.shared.control_channel_data,
            cx.shared.autopilot_status,
            cx.shared.motor_timers,
            cx.shared.motor_pid_state,
            cx.shared.motor_pid_coeffs,
            cx.shared.dma,
            cx.shared.user_cfg,
            cx.shared.spi1,
            cx.shared.state_volatile,
            cx.shared.flight_ctrl_filters,
        )
            .lock(
                |params,
                 params_prev,
                 ahrs,
                 control_channel_data,
                 autopilot_status,
                 motor_timers,
                 pid_state,
                 pid_coeffs,
                 dma,
                 cfg,
                 spi1,
                 state_volatile,
                 flight_ctrl_filters| {
                    // Note that this step is mandatory, per STM32 RM.
                    spi1.stop_dma(setup::IMU_TX_CH, Some(setup::IMU_RX_CH), dma);

                    // todo: Temp testing setting the loop rate to 4k to TS motors/CRSF
                    // todo: If you use this long term, do something diff like have IMU update less
                    // todo frequently, or just apply filters each update.
                    if *cx.local.imu_isr_loop_i % 2 == 0 {
                        return
                    }

                    {
                        if *cx.local.imu_isr_loop_i % 700 == 0 {
                            // let period = cx.local.measurement_timer.time_elapsed().as_secs();
                            // println!("IMU time: {:?}. Freq: {:?}", period, 1. / period);

                            // println!("T");
                            // println!("IMU BUF: {:?}", unsafe { &imu_shared::IMU_READINGS });
                        }

                        // cx.local.measurement_timer.disable();
                        // cx.local.measurement_timer.reset_count();
                        // cx.local.measurement_timer.enable();
                    }

                    let mut imu_data =
                        imu_shared::ImuReadings::from_buffer(unsafe { &imu_shared::IMU_READINGS });

                    cx.shared.imu_filters.lock(|imu_filters| {
                        imu_filters.apply(&mut imu_data);
                    });

                    // Update `params_prev` with past-update data prior to updating params
                    *params_prev = params.clone();
                    params.update_from_imu_readings(imu_data);

                    // Note: Consider if you want to update the attitude using the primary update loop,
                    // vice each IMU update.
                    attitude_platform::update_attitude(ahrs, params);

                    // todo: Temp debug code.
                    let mut p = control_channel_data.throttle;
                    if p < 0.025 {
                        p = 0.025;
                    };


                    if state_volatile.arm_status == ArmStatus::Armed {
                        // dshot::set_power(p, p, p, p, motor_timers, dma);
                        dshot::set_power(p, p, p, p, motor_timers, dma);
                    } else {

                        dshot::stop_all(motor_timers, dma);
                        // dshot::set_power(0.025, 0., 0., 0., motor_timers, dma);
                    }

                     return; // todo temp!

                    // todo: Impl once you've sorted out your control logic.
                    // todo: Delegate this to another module, eg `attitude_ctrls`.
                    // Update the target attitude based on control inputs
                    // todo: Deconflict this with autopilot; probably by checking commanded
                    // todo pitch, roll, yaw etc!

                    state_volatile.rates_commanded = RatesCommanded {
                        pitch: Some(cfg.input_map.calc_pitch_rate(control_channel_data.pitch)),
                        roll: Some(cfg.input_map.calc_roll_rate(control_channel_data.roll)),
                        yaw: Some(cfg.input_map.calc_yaw_rate(control_channel_data.yaw)),
                    };

                    // If we haven't taken off, apply the attitude lock.
                    if state_volatile.has_taken_off {
                        state_volatile.attitude_commanded.quat = Some(ctrl_logic::modify_att_target(
                            state_volatile.attitude_commanded.quat.unwrap_or(Quaternion::new_identity()),
                            &state_volatile.rates_commanded,
                            DT_IMU,
                        ));
                    } else {
                        state_volatile.attitude_commanded.quat = Some(cfg.takeoff_attitude);
                    }

                    let throttle = match state_volatile.autopilot_commands.throttle {
                        Some(t) => t,
                        None => control_channel_data.throttle,
                    };

                    if let OperationMode::Preflight = state_volatile.op_mode {
                        return;
                    }

                    // todo: Temp testing motors and bidir dshot


                    // let p = control_channel_data.throttle;
                    let p = 0.03;
                    // if *cx.local.imu_isr_loop_i > 30_000 {
                    //     // state_volatile.arm_status = ArmStatus::Armed; // todo temp!
                    // }

                    // if state_volatile.arm_status == ArmStatus::Armed {
                    //     dshot::set_power(p, p, p, p, motor_timers, dma);
                    // } else {
                    //     dshot::stop_all(motor_timers, dma);
                    // }

                    return; // todo TS: Odd anomalies

                    cfg_if! {
                        if #[cfg(feature = "quad")] {
                            let (ctrl_mix, rpms) = ctrl_logic::rotor_rpms_from_att(
                                state_volatile.attitude_commanded.quat.unwrap(),
                                params.attitude_quat,
                                throttle,
                                cfg.control_mapping.frontleft_aftright_dir,
                                params,
                                params_prev,
                                // &state_volatile.ctrl_mix,
                                // &state_volatile.ctrl_mix_prev,
                                &cfg.ctrl_coeffs,
                                &state_volatile.drag_coeffs,
                                &state_volatile.accel_map,
                                flight_ctrl_filters,
                                DT_IMU,
                            );

                            rpms.send_to_motors(
                                pid_coeffs,
                                pid_state,
                                &rpms,
                                &cfg.control_mapping,
                                motor_timers,
                                state_volatile.arm_status,
                                dma
                            );

                            state_volatile.ctrl_mix = ctrl_mix;
                            // todo: Dynamics of `current_pwr` for quads, and `ctrl_posits`
                            // todo for fixed-wing. You're saving it, but when is it used?
                            // state_volatile.current_pwr = motor_power;
                        } else {
                            let (ctrl_mix, control_posits) = ctrl_logic::control_posits_from_att(
                                state_volatile.attitude_commanded.quat.unwrap(),
                                params.attitude_quat,
                                throttle,
                                params,
                                params_prev,
                                // &state_volatile.ctrl_mix,
                                &cfg.ctrl_coeffs,
                                &state_volatile.drag_coeffs,
                                &state_volatile.accel_map,
                                flight_ctrl_filters,
                                DT_IMU,
                            );

                            control_posits.set(&cfg.control_mapping, motor_timers, state_volatile.arm_status,  dma);

                            state_volatile.ctrl_mix = ctrl_mix;
                            state_volatile.ctrl_positions = control_posits;
                        }
                    }
                },
            );
    }

    // binds = OTG_HS
    // todo H735 issue on GH: https://github.com/stm32-rs/stm32-rs/issues/743 (works on H743)
    // todo: NVIC interrupts missing here for H723 etc!
    #[task(binds = USB_LP, shared = [usb_dev, usb_serial, current_params, control_channel_data,
    link_stats, user_cfg, state_volatile, system_status, motor_timers, batt_curr_adc, dma], local = [], priority = 4)]
    /// This ISR handles interaction over the USB serial port, eg for configuring using a desktop
    /// application.
    fn usb_isr(mut cx: usb_isr::Context) {
        // todo: Do we want to use an approach where we push stats, or this approach where
        // todo respond only?
        (
            cx.shared.usb_dev,
            cx.shared.usb_serial,
            cx.shared.current_params,
            cx.shared.control_channel_data,
            cx.shared.link_stats,
            cx.shared.user_cfg,
            cx.shared.state_volatile,
            cx.shared.system_status,
            cx.shared.motor_timers,
            cx.shared.batt_curr_adc,
            cx.shared.dma,
        )
            .lock(
                |usb_dev,
                 usb_serial,
                 params,
                 ch_data,
                 link_stats,
                 user_cfg,
                 state_volatile,
                 system_status,
                 motor_timers,
                 adc,
                 dma| {
                    if !usb_dev.poll(&mut [usb_serial]) {
                        return;
                    }

                    let mut buf = [0u8; 8];
                    match usb_serial.read(&mut buf) {
                        Ok(_count) => {
                            usb_cfg::handle_rx(
                                usb_serial,
                                &buf,
                                params.attitude_quat,
                                &state_volatile.attitude_commanded,
                                params.baro_alt_msl,
                                params.tof_alt,
                                state_volatile.batt_v,
                                state_volatile.esc_current,
                                ch_data,
                                &link_stats,
                                &user_cfg.waypoints,
                                system_status,
                                &mut state_volatile.arm_status,
                                &mut user_cfg.control_mapping,
                                &mut state_volatile.op_mode,
                                motor_timers,
                                adc,
                                dma,
                            );
                        }
                        Err(_) => {
                            // println!("Error reading USB signal from PC");
                        }
                    }
                },
            )
    }

    // Note: We don't use `dshot_isr_r12` on H7; this is associated with timer 2. DSHOT
    // uses a single timer on H7: `dshot_isr_r34`
    // These should be high priority, so they can shut off before the next 600kHz etc tick.
    #[cfg(feature = "g4")]
    #[task(binds = DMA1_CH3, shared = [motor_timers, dma], local = [], priority = 6)]
    /// We use this ISR to enable input capture if in bidirectional mode. Assocaited with Tim2,
    /// and is only used on G4.
    fn dshot_isr_r12(mut cx: dshot_isr_r12::Context) {
        // println!("12");
        // todo: Why is this gate required when we have feature-gated the fn?
        // todo: Maybe RTIC is messing up the fn-level feature gate?
        #[cfg(feature = "g4")]
            unsafe {
            (*DMA1::ptr()).ifcr.write(|w| w.tcif3().set_bit());
        }
        unsafe {
            (*pac::TIM2::ptr()).cr1.modify(|_, w| w.cen().clear_bit());
        }

        if dshot::BIDIR_EN {
            if dshot::CH_A_REC_MODE.load(Ordering::Relaxed) {
                (cx.shared.motor_timers, cx.shared.dma).lock(|motor_timers, dma| {
                    dshot::set_to_output(motor_timers, Motor::M1, Motor::M2, false);
                });
                dshot::CH_A_REC_MODE.store(false, Ordering::Relaxed);
            } else {
                (cx.shared.motor_timers, cx.shared.dma).lock(|motor_timers, dma| {
                    dshot::set_to_input(motor_timers, Motor::M1, Motor::M2, false);
                    dshot::receive_payload_a(motor_timers, dma);
                });
                dshot::CH_A_REC_MODE.store(true, Ordering::Relaxed);
            }
        }
    }

    // #[task(binds = DMA1_STR4, shared = [motor_timers], priority = 6)]
    #[task(binds = DMA1_CH4, shared = [motor_timers, dma], priority = 6)]
    /// We use this ISR to enable input capture if in bidirectional mode. For G4, it's only for motors
    /// 3 and 4. For h7, it's for all 4. Associated with Tim3.
    fn dshot_isr_r34(mut cx: dshot_isr_r34::Context) {
        // todo: Remove everythign in this ISR except for the interrupt-clear flag
        // todo if `dshot::set_to_input` handles all motors. Or, split
        // todo that fn based on M12 or M34. This interrupt still needs to fire apparently,
        // todo per the required timer DMA interrupt.

        // println!("34");
        unsafe {
                #[cfg(feature = "h7")]
            (*DMA1::ptr()).hifcr.write(|w| w.ctcif4().set_bit());
                #[cfg(feature = "g4")]
            (*DMA1::ptr()).ifcr.write(|w| w.tcif4().set_bit());
        }
        unsafe {
            (*pac::TIM3::ptr()).cr1.modify(|_, w| w.cen().clear_bit());
        }

        // cx.shared.motor_timers.lock(|timers| {
        //     #[cfg(feature = "h7")]
        //         timers.r1234.disable();
        //     #[cfg(all(feature = "g4", feature = "quad"))]
        //         timers.r34.disable();
        //     #[cfg(all(feature = "g4", feature = "fixed-wing"))]
        //         timers.servos.disable();
        // });
        // todo: Temp. Put motor timers etc back A/R. If you still have trouble, may need to
        // todo split by 1/2, 3/4 instead of sharing motor timers struct.

        if dshot::BIDIR_EN {
            if dshot::CH_B_REC_MODE.load(Ordering::Relaxed) {
                (cx.shared.motor_timers, cx.shared.dma).lock(|motor_timers, dma| {
                    #[cfg(feature = "h7")]
                        dshot::set_to_output(motor_timers, Motor::M1, Motor::M2, true);
                    dshot::set_to_output(motor_timers, Motor::M3, Motor::M4, true);
                });
                dshot::CH_B_REC_MODE.store(false, Ordering::Relaxed);
            } else {
                (cx.shared.motor_timers, cx.shared.dma).lock(|motor_timers, dma| {
                    #[cfg(feature = "h7")]
                        dshot::set_to_input(motor_timers, Motor::M1, Motor::M2, true);
                    dshot::set_to_input(motor_timers, Motor::M3, Motor::M4, true);
                    #[cfg(feature = "h7")]
                        dshot::receive_payload_a(motor_timers, dma);
                    dshot::receive_payload_b(motor_timers, dma);
                });
                dshot::CH_B_REC_MODE.store(true, Ordering::Relaxed);
            }
        }
    }

    // // #[task(binds = TIM8, // H7
    // // shared = [motor_timers], priority = 6)]
    // #[task(binds = TIM3,
    // shared = [motor_timers], priority = 6)] // G4
    // /// We use this for fixed wing, to disable the timer after each pulse. We don't enable this interrupt
    // /// on quadcopters.
    // fn servo_isr(mut cx: servo_isr::Context) {
    //     // todo: What is this for? Do you need it? Once the test platform is in working state,
    //     // todo: Remove this, and see if ther'es an ill effect. Where is this event enabled?
    //     // todo: Commented out for now. Put back if you have trouble.
    //     cx.shared.motor_timers.lock(|timers| {
    //         #[cfg(feature = "h7")]
    //         let timer = &mut timers.servos;
    //         #[cfg(feature = "g4")]
    //         let timer = &mut timers.r34_servos;
    //
    //         timer.clear_interrupt(TimerInterrupt::Update);
    //         timer.disable();
    //     });
    // }

    // todo: Evaluate priority.
    // #[task(binds = USART7,
    #[task(binds = USART3,
    shared = [dma, control_channel_data, link_stats, rf_limiter_timer, link_lost,
    lost_link_timer, system_status], local = [uart_elrs], priority = 5)]
    /// This ISR handles CRSF reception. It handles, in an alternating fashion, message starts,
    /// and message ends. For message starts, it begins a DMA transfer. For message ends, it
    /// processes the radio data, passing it into shared resources for control channel data,
    /// and link stats.
    fn crsf_isr(mut cx: crsf_isr::Context) {
        let mut recieved_ch_data = false; // Lets us split up the lock a bit more.
        let mut rx_fault = false;

        let uart = &mut cx.local.uart_elrs; // Code shortener

        uart.clear_interrupt(UsartInterrupt::CharDetect(0));
        uart.clear_interrupt(UsartInterrupt::Idle);

        (
            cx.shared.dma,
            cx.shared.control_channel_data,
            cx.shared.link_stats,
            cx.shared.rf_limiter_timer,
        )
            .lock(|dma, ch_data, link_stats, limiter_timer| {
                // todo: Attempting a software flag vice using interrupt flags, to TS CRSF
                // todo anomolies.
                if !crsf::TRANSFER_IN_PROG.load(Ordering::Relaxed) {
                    crsf::TRANSFER_IN_PROG.store(true, Ordering::Relaxed);
                    // if unsafe { !(*pac::USART3::ptr()).isr.read().idle().bit_is_set() } {
                    //     uart.clear_interrupt(UsartInterrupt::CharDetect(0));
                    // todo: Why/when/how to handle?
                    uart.clear_interrupt(UsartInterrupt::Overrun);
                    // Don't allow the starting char, as used in the middle of a message,
                    // to trigger an interrupt.
                    uart.disable_interrupt(UsartInterrupt::CharDetect(0));

                    // todo: Deal with this later.
                    // if limiter_timer.is_enabled() {
                    //     // todo: This is triggering off link stats. Find a way to accept that, but still
                    //     // todo cancel immediately. (?)
                    //     // println!("Time since last req: {}", limiter_timer.time_elapsed().as_secs());
                    //     println!("RF limiter triggered.");
                    //     // return; // todo
                    // } else {
                    //     limiter_timer.disable();
                    //     limiter_timer.reset_count();
                    //     limiter_timer.enable();
                    // }

                    dma.stop(setup::CRSF_RX_CH);

                    unsafe {
                        uart.read_dma(
                            &mut crsf::RX_BUFFER,
                            setup::CRSF_RX_CH,
                            ChannelCfg {
                                // Take precedence over the ADC, but not motors.
                                priority: dma::Priority::Medium,
                                ..Default::default()
                            },
                            dma,
                        );
                    }
                    // println!("S");
                    // println!(
                    //     "O S: {}",
                    //     uart.regs.isr.read().ore().bit_is_set()
                    // );
                } else {
                    crsf::TRANSFER_IN_PROG.store(false, Ordering::Relaxed);
                    // println!("I");
                    // Line is idle.
                    // uart.clear_interrupt(UsartInterrupt::Idle);
                    // println!("O I: {}", uart_elrs.regs.isr.read().ore().bit_is_set());

                    // uart.clear_interrupt(UsartInterrupt::Overrun); // todo?

                    // Stop the DMA read, since it will likely not have filled the buffer, due
                    // to the variable message sizes.
                    dma.stop(setup::CRSF_RX_CH);

                    // Re-enable
                    // Don't use the HAL method to re-enable the char-match interrupt, since it also
                    // sets the address field.
                    uart.regs.cr1.modify(|_, w| w.cmie().set_bit());

                    if let Some(crsf_data) =
                    crsf::handle_packet(uart, setup::CRSF_RX_CH, &mut rx_fault)
                    {
                        match crsf_data {
                            crsf::PacketData::ChannelData(data) => {
                                *ch_data = data;
                                recieved_ch_data = true;

                                // We have this PID adjustment here, since they're one-off actuations.
                                // We handle other things like autopilot mode entry in the update fn.
                                // if cx.local.ctrl_coeff_adj_timer.is_enabled() {
                                //     println!("PID timer is still running.");
                                // } else {
                                //     let pid_adjustment = match ch_data.pid_tune_actuation {
                                //         PidTuneActuation::Increase => CTRL_COEFF_ADJ_AMT,
                                //         PidTuneActuation::Decrease => -CTRL_COEFF_ADJ_AMT,
                                //         PidTuneActuation::Neutral => 0.,
                                //     };
                                //
                                //     match ch_data.pid_tune_actuation {
                                //         PidTuneActuation::Neutral => (),
                                //         _ => {
                                //             println!("Adjusting PID");
                                //             // match ch_data.pid_tune_mode {
                                //             //     PidTuneMode::Disabled => (),
                                //             //     PidTuneMode::P => {
                                //             //         // todo: for now or forever, adjust pitch, roll, yaw
                                //             //         // todo at once to keep UI simple
                                //             //         ctrl_coeffs.pitch.k_p_rate += pid_adjustment;
                                //             //         ctrl_coeffs.roll.k_p_rate += pid_adjustment;
                                //             //         // todo: Maybe skip yaw here?
                                //             //         ctrl_coeffs.yaw.k_p_rate += pid_adjustment;
                                //             //     }
                                //             //     PidTuneMode::I => {
                                //             //         ctrl_coeffs.pitch.k_i_rate += pid_adjustment;
                                //             //         ctrl_coeffs.roll.k_i_rate += pid_adjustment;
                                //             //         ctrl_coeffs.yaw.k_i_rate += pid_adjustment;
                                //             //     }
                                //             //     PidTuneMode::D => {
                                //             //         ctrl_coeffs.pitch.k_d_rate += pid_adjustment;
                                //             //         ctrl_coeffs.roll.k_d_rate += pid_adjustment;
                                //             //         ctrl_coeffs.yaw.k_d_rate += pid_adjustment;
                                //             //     }
                                //             // }
                                //         }
                                //     }
                                //     cx.local.ctrl_coeff_adj_timer.reset_count();
                                //     cx.local.ctrl_coeff_adj_timer.enable();
                                // }
                            }
                            crsf::PacketData::LinkStats(stats) => {
                                *link_stats = stats;
                            }
                        }
                    }
                }
            });

        (
            cx.shared.link_lost,
            cx.shared.lost_link_timer,
            cx.shared.system_status,
        )
            .lock(|link_lost, lost_link_timer, system_status| {
                if recieved_ch_data {
                    // We've received a packet successfully - reset the lost-link timer.
                    lost_link_timer.disable();
                    lost_link_timer.reset_count();
                    lost_link_timer.enable();

                    if *link_lost {
                        println!("Link re-aquired");
                        *link_lost = false;
                        // todo: Execute re-acq procedure
                    }
                    system_status.rf_control_link = SensorStatus::Pass;
                }

                if rx_fault {
                    system_status.rf_control_fault = true;
                }
            });
    }

    /// If this triggers, it means we've received no radio control signals for a significant
    ///period of time; we treat this as a lost-link situation.
    /// (Note that this is for TIM17 on both variants)
    // #[task(binds = TIM17,
    #[task(binds = TIM1_TRG_COM,
    shared = [lost_link_timer, link_lost, state_volatile, user_cfg, autopilot_status,
    current_params, system_status], priority = 1)]
    fn lost_link_isr(cx: lost_link_isr::Context) {
        println!("Lost the link!");

        (
            cx.shared.lost_link_timer,
            cx.shared.link_lost,
            cx.shared.state_volatile,
            cx.shared.user_cfg,
            cx.shared.autopilot_status,
            cx.shared.current_params,
            cx.shared.system_status,
        )
            .lock(
                |timer,
                 link_lost,
                 state_volatile,
                 user_cfg,
                 autopilot_status,
                 params,
                 system_status| {
                    timer.clear_interrupt(TimerInterrupt::Update);
                    timer.reset_count();
                    timer.disable(); // todo: Probably not required in one-pulse mode.

                    *link_lost = true;

                    // We run this during the main loop, but here the `entering` flag is set to true,
                    // to initialize setup steps.
                    safety::link_lost(
                        system_status,
                        autopilot_status,
                        params,
                        &state_volatile.base_point,
                    );
                },
            );
    }

    #[task(binds = TIM1_UP_TIM16, shared = [rf_limiter_timer], priority = 1)]
    fn rf_limiter_isr(mut cx: rf_limiter_isr::Context) {
        // println!("RF limiter ISR");
        cx.shared.rf_limiter_timer.lock(|timer| {
            timer.clear_interrupt(TimerInterrupt::Update);
            timer.disable();
            timer.reset_count();
        });
    }

    // binds = DMA2_STR1,
    #[task(binds = DMA2_CH1,
    shared = [dma2, i2c2], priority = 1)]
    /// Baro write complete; start baro read.
    fn baro_write_tc_isr(cx: baro_write_tc_isr::Context) {
        println!("Ext sensors D");
        (cx.shared.dma2, cx.shared.i2c2).lock(|dma2, i2c2| {
            dma2.clear_interrupt(setup::BARO_TX_CH, DmaInterrupt::TransferComplete);

            unsafe {
                i2c2.read_dma(
                    baro::ADDR,
                    &mut sensors_shared::BARO_READINGS,
                    setup::BARO_RX_CH,
                    Default::default(),
                    dma2,
                );
            }
        });
    }

    // todo: For now, we start new transfers in the main loop.

    // binds = DMA2_STR2,
    #[task(binds = DMA2_CH2,
    shared = [dma2, altimeter, current_params], priority = 1)]
    /// Baro read complete; handle data, and start next write.
    fn baro_read_tc_isr(cx: baro_read_tc_isr::Context) {
        println!("Ext sensors C");
        (
            cx.shared.dma2,
            cx.shared.altimeter,
            cx.shared.current_params,
        )
            .lock(|dma2, altimeter, params| {
                dma2.clear_interrupt(setup::BARO_RX_CH, DmaInterrupt::TransferComplete);

                // code shortener.
                let buf = unsafe { &sensors_shared::BARO_READINGS };
                // todo: Process your baro reading here.
                let pressure = altimeter
                    .pressure_from_readings(buf[0], buf[1], buf[2], buf[3], buf[4], buf[5]);

                // todo: Altitude from pressure! Maybe in a diff module? (which?)
                params.baro_alt_msl = pressure;
            });
    }

    // binds = DMA2_STR3,
    #[task(binds = DMA2_CH3,
    shared = [dma2, i2c1, ext_sensor_active], priority = 1)]
    /// Baro write complete; start baro read.
    fn ext_sensors_write_tc_isr(cx: ext_sensors_write_tc_isr::Context) {
        println!("Ext sensors B");
        (cx.shared.dma2, cx.shared.i2c1, cx.shared.ext_sensor_active).lock(
            |dma2, i2c1, ext_sensor_active| {
                dma2.clear_interrupt(setup::EXT_SENSORS_TX_CH, DmaInterrupt::TransferComplete);

                // todo: Skip sensors if marked as not connected?

                unsafe {
                    match ext_sensor_active {
                        ExtSensor::Mag => {
                            i2c1.read_dma(
                                mag::ADDR,
                                &mut sensors_shared::MAG_READINGS,
                                setup::EXT_SENSORS_RX_CH,
                                Default::default(),
                                dma2,
                            );
                        }
                        ExtSensor::Gps => {
                            i2c1.read_dma(
                                gps::ADDR,
                                &mut sensors_shared::GPS_READINGS,
                                setup::EXT_SENSORS_RX_CH,
                                Default::default(),
                                dma2,
                            );
                        }
                        ExtSensor::Tof => {
                            i2c1.read_dma(
                                tof::ADDR,
                                &mut sensors_shared::TOF_READINGS,
                                setup::EXT_SENSORS_RX_CH,
                                Default::default(),
                                dma2,
                            );
                        }
                    }
                }
            },
        );
    }

    // binds = DMA2_STR4,
    #[task(binds = DMA2_CH4,
    shared = [dma2, i2c1, ext_sensor_active], priority = 1)]
    /// Baro write complete; start baro read.
    fn ext_sensors_read_tc_isr(cx: ext_sensors_read_tc_isr::Context) {
        println!("Ext sensors A");
        (cx.shared.dma2, cx.shared.i2c1, cx.shared.ext_sensor_active).lock(
            |dma2, i2c1, ext_sensor_active| {
                dma2.clear_interrupt(setup::EXT_SENSORS_RX_CH, DmaInterrupt::TransferComplete);

                // todo: Skip sensors if marked as not connected?

                // todo: Interp data, and place data into its apt struct here.

                unsafe {
                    match ext_sensor_active {
                        ExtSensor::Mag => {
                            i2c1.write_dma(
                                gps::ADDR,
                                &mut sensors_shared::WRITE_BUF_GPS,
                                false,
                                setup::EXT_SENSORS_RX_CH,
                                Default::default(),
                                dma2,
                            );
                            *ext_sensor_active = ExtSensor::Gps;
                        }
                        ExtSensor::Gps => {
                            i2c1.write_dma(
                                tof::ADDR,
                                &mut sensors_shared::WRITE_BUF_TOF,
                                false,
                                setup::EXT_SENSORS_RX_CH,
                                Default::default(),
                                dma2,
                            );
                            *ext_sensor_active = ExtSensor::Tof;
                        }
                        ExtSensor::Tof => {
                            *ext_sensor_active = ExtSensor::Mag;
                            // End of sequence; don't start a new transfer.
                        }
                    }
                }
            },
        );
    }
}

// same panicking *behavior* as `panic-probe` but doesn't print a panic message
// this prevents the panic message being printed *twice* when `defmt::panic` is invoked
#[defmt::panic_handler]
fn panic() -> ! {
    cortex_m::asm::udf()
}
