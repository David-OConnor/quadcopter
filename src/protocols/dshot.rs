//! This module contains code for the DSHOT digital protocol, using to control motor speed.
//!
//! [Some information on the protocol](https://brushlesswhoop.com/dshot-and-bidirectional-dshot/):
//! Every digital protocol has a structure, also called a frame. It defines which information is at
//! which position in the data stream. And the frame structure of DSHOT is pretty straight forward:
//!
//! 11 bit throttle: 2048 possible values. 0 is reserved for disarmed. 1-47 are reserved for special commands.
//! Leaving 48 to 2047 (2000 steps) for the actual throttle value
//! 1 bit telemetry request - if this is set, telemetry data is sent back via a separate channel
//! 4 bit CRC: (Cyclic Redundancy) Check to validate data (throttle and telemetry request bit)
//! 1 and 0 in the DSHOT frame are distinguished by their high time. This means that every bit has a certain (constant) length,
//! and the length of the high part of the bit dictates if a 1 or 0 is being received.
//!
//! The DSHOT protocol (DSHOT-300, DSHOT-600 etc) is determined by the `DSHOT_ARR_600` and
//! `DSHOT_PSC_600` settings; ie set a 600kHz countdown for DSHOT-600.

use core::sync::atomic::{AtomicBool, AtomicUsize};

use cortex_m::delay::Delay;

use stm32_hal2::{
    dma::{self, ChannelCfg, Priority},
    gpio,
    pac::{self, TIM3},
    timer::{CaptureCompare, CountDir, OutputCompare, Polarity},
};

use crate::{
    flight_ctrls::{
        common::{Motor, MotorRpm},
        ControlMapping,
    },
    setup::MotorTimer,
};

use defmt::println;

// todo: Bidirectional: Set timers to active low, set GPIO idle to high, and perhaps set down counting
// todo if required. Then figure out input capture, and fix in HAL.

// todo (Probalby in another module) - RPM filtering, once you have bidirectional DSHOT working.
// Article: https://brushlesswhoop.com/betaflight-rpm-filter/
// todo: Basically, you set up a notch filter at rotor RPM. (I think; QC this)

use crate::setup;
use cfg_if::cfg_if;
use usb_device::device::UsbDeviceState::Default;

// Enable bidirectional DSHOT, which returns RPM data
pub const BIDIR_EN: bool = true;

// Timer prescaler for rotor PWM. We leave this, and ARR constant, and explicitly defined,
// so we can set duty cycle appropriately for DSHOT.
// (PSC+1)*(ARR+1) = TIMclk/Updatefrequency = TIMclk * period.
// ARR = (TIMclk/Updatefrequency) / (PSC + 1) - 1

pub const DSHOT_PSC_600: u16 = 0;

// ESC telemetry is false except when setting motor direction.
static mut ESC_TELEM: bool = false;

// We use these flags to determine how to handle the TC ISRs, ie when
// a send command is received, set the mode to input and vice versa.

// The number of motors here affects our payload interleave logic, and DMA burst length written.
const NUM_MOTORS: usize = 4;

// Update frequency: 600kHz
// 170Mhz tim clock on G4.
// 240Mhz tim clock on H743
// 260Mhz tim clock on H723 @ 520Mhz. 275Mhz @ 550Mhz
cfg_if! {
    if #[cfg(feature = "h7")] {
        // pub const DSHOT_ARR_600: u32 = 399;  // 240Mhz tim clock
        pub const DSHOT_ARR_600: u32 = 432;  // 260Mhz tim clock
        // pub const DSHOT_ARR_600: u32 = 457; // 275Mhz tim clock
    } else if #[cfg(feature = "g4")] {
        // pub const DSHOT_ARR_600: u32 = 282; // 170Mhz tim clock
        pub const DSHOT_ARR_600: u32 = 567; // 170Mhz tim clock // todo: This is for DSHOT 300.
        pub const DSHOT_ARR_300: u32 = 567; // 170Mhz tim clock // todo: This is for DSHOT 300.

        // This runs immediately after completion of transmission, prior to the
        // start of reception
        pub const READ_TIMER_ARR_INIT: u32 = 4_200; // A 24.7us delay. Note that in practice we measure 35; 25 is conservative.
        pub const READ_TIMER_ARR_READING: u32 = 452; // This results in a frequency of 375kHz; for DSHOT 300.
    }
}

// Duty cycle values (to be written to CCMRx), based on our ARR value. 0. = 0%. ARR = 100%.
const DUTY_HIGH: u32 = DSHOT_ARR_600 * 3 / 4;
const DUTY_LOW: u32 = DSHOT_ARR_600 * 3 / 8;

// We use this during config that requires multiple signals sent, eg setting. motor direction.

// Use this pause duration, in ms, when setting up motor dir.
pub const PAUSE_BETWEEN_COMMANDS: u32 = 1;
pub const PAUSE_AFTER_SAVE: u32 = 40; // Must be at least 35ms.
                                      // BLHeli_32 requires you repeat certain commands, like motor direction, 6 times.
pub const REPEAT_COMMAND_COUNT: u32 = 10; // todo: Set back to 6 once sorted out.

// DMA buffers for each rotor. 16-bit data.
// Last 2 entries will be 0 per channel. Required to prevent extra pulses. (Not sure why)
static mut PAYLOAD: [u16; 18 * NUM_MOTORS] = [0; 18 * NUM_MOTORS];
// todo: The receive payload may be shorter due to how it's encoded; come back to this.

// The position we're reading when updating the DSHOT read.
pub static READ_I: AtomicUsize = AtomicUsize::new(0);
// pub static READ_MSG_STARTED: AtomicBool = AtomicBool::new(false);
// todo: TS
// pub static mut PAYLOAD_REC: [u16; 18 * NUM_MOTORS] = [0; 18 * NUM_MOTORS];

// There are 21 bits in each DSHOT RPM reception message. Value is true for line low (bit = 1), and false
// for line high (bit = 0); idle high.
pub const REC_BUF_LEN: usize = 20;
// todo: Maybe don't start rec process until first down edge.
pub static mut PAYLOAD_REC_BB_1: [bool; REC_BUF_LEN] = [false; REC_BUF_LEN];
pub static mut PAYLOAD_REC_BB_2: [bool; REC_BUF_LEN] = [false; REC_BUF_LEN];
pub static mut PAYLOAD_REC_BB_3: [bool; REC_BUF_LEN] = [false; REC_BUF_LEN];
pub static mut PAYLOAD_REC_BB_4: [bool; REC_BUF_LEN] = [false; REC_BUF_LEN];

/// Possible DSHOT commands (ie, DSHOT values 0 - 47). Does not include power settings.
/// [Special commands section](https://brushlesswhoop.com/dshot-and-bidirectional-dshot/)
/// [BlHeli command code](https://github.com/bitdump/BLHeli/blob/master/BLHeli_32%20ARM/BLHeli_32%20Firmware%20specs/Digital_Cmd_Spec.txt)
///
/// Commands are only executed when motors are stopped
/// Note that throttle has to be zero, and the telemetry bit must be set in the command frames.
/// Also note that a significant delay (several hundred ms) may be needed between commands.
#[derive(Copy, Clone)]
#[repr(u16)]
pub enum Command {
    /// Note: Motor Stop is perhaps not yet implemented.
    _MotorStop = 0,
    _Beacon1 = 1,
    _Beacon2 = 2,
    _Beacon3 = 3,
    _Beacon4 = 4,
    _Beacon5 = 5,
    _EscInfo = 6,
    /// SpinDir1 and 2 are forced normal and reversed. If you have the ESC set to reversed in the config,
    /// these will not reverse the motor direction, since it is already operating in reverse.
    SpinDir1 = 7, // 6x
    SpinDir2 = 8,   // 6x
    _3dModeOff = 9, // 6x
    _3dModeOn = 10, // 6x
    _SettingsRequest = 11,
    SaveSettings = 12, // 6x, wait at least 35ms before next command.
    /// Normal and reversed with respect to configuration.
    _SpinDirNormal = 20, // 6x
    _SpinDirReversed = 21, // 6x
    _Led0On = 22,      // BLHeli32 only
    _Led1On = 23,      // BLHeli32 only
    _Led2On = 24,      // BLHeli32 only
    _Led3On = 25,      // BLHeli32 only
    _Led0Off = 26,     // BLHeli32 only
    _Led1Off = 27,     // BLHeli32 only
    _Led2Off = 28,     // BLHeli32 only
    _Led3Off = 29,     // BLHeli32 only
    _AudioStreamModeOnOff = 30, // KISS audio Stream mode on/Off
    _SilendModeOnOff = 31, // KISS silent Mode on/Off
    /// Disables commands 42 to 47
    _TelemetryEnable = 32, // 6x
    /// Enables commands 42 to 47
    _TelemetryDisable = 33, // 6x
    /// Need 6x. Enables commands 42 to 47 and sends erpm if normal Dshot frame
    _ContinuousErpmTelemetry = 34, // 6x
    /// Enables commands 42 to 47 and sends erpm period if normal Dshot frame
    _ContinuousErpmPeriodTelemetry = 35, // 6x
    /// 1°C per LSB
    _TemperatureTelemetry = 42,
    /// 10mV per LSB, 40.95V max
    _VoltageTelemetry = 43,
    /// 100mA per LSB, 409.5A max
    _CurrentTelemetry = 44,
    /// 10mAh per LSB, 40.95Ah max
    _ConsumptionTelemetry = 45,
    /// 100erpm per LSB, 409500erpm max
    _ErpmTelemetry = 46,
    /// 16us per LSB, 65520us max TBD
    _ErpmPeriodTelemetry = 47,
    // Max = 47, // todo: From Betaflight, but not consistent with the Brushlesswhoop article
}

pub enum CmdType {
    Command(Command),
    Power(f32),
}

/// Stop all motors, by setting their power to 0. Note that the Motor Stop command may not
/// be implemented, and this approach gets the job done. Run this at program init, so the ESC
/// get its required zero-throttle setting, generally required by ESC firmware to complete
/// initialization.
pub fn stop_all(timer: &mut MotorTimer) {
    // Note that the stop command (Command 0) is currently not implemented, so set throttles to 0.
    set_power(0., 0., 0., 0., timer);
}

/// Set up the direction for each motor, in accordance with user config. Note: This blocks!
/// (at least for now). The intended use case is to run this only at init, and during Preflight,
/// if adjusting motor mapping.
pub fn setup_motor_dir(motors_reversed: (bool, bool, bool, bool), timer: &mut MotorTimer) {
    // A blocking delay.
    let cp = unsafe { cortex_m::Peripherals::steal() };
    let mut delay = Delay::new(cp.SYST, 170_000_000);

    // Throttle must have been commanded to 0 a certain number of timers,
    // and the telemetry bit must be bit set to use commands.
    // Setting the throttle twice (with 1ms delay) doesn't work; 10x works. The required value is evidently between
    // these 2 bounds.
    for i in 0..30 {
        stop_all(timer);
        delay.delay_ms(PAUSE_BETWEEN_COMMANDS);
    }
    // I've confirmed that setting direction without the telemetry bit set will fail.
    unsafe { ESC_TELEM = true };

    delay.delay_ms(PAUSE_BETWEEN_COMMANDS);

    println!("Setting up motor direction");

    // Spin dir commands need to be sent 6 times. (or 10?) We're using the "forced" spin dir commands,
    // ie not with respect to ESC configuration; although that would be acceptable as well.
    for _ in 0..REPEAT_COMMAND_COUNT {
        let cmd_1 = if motors_reversed.0 {
            Command::SpinDir2
        } else {
            Command::SpinDir1
        };
        let cmd_2 = if motors_reversed.1 {
            Command::SpinDir2
        } else {
            Command::SpinDir1
        };
        let cmd_3 = if motors_reversed.2 {
            Command::SpinDir2
        } else {
            Command::SpinDir1
        };
        let cmd_4 = if motors_reversed.3 {
            Command::SpinDir2
        } else {
            Command::SpinDir1
        };

        setup_payload(Motor::M1, CmdType::Command(cmd_1));
        setup_payload(Motor::M2, CmdType::Command(cmd_2));
        setup_payload(Motor::M3, CmdType::Command(cmd_3));
        setup_payload(Motor::M4, CmdType::Command(cmd_4));

        send_payload(timer);

        delay.delay_ms(PAUSE_BETWEEN_COMMANDS);
    }

    for _ in 0..REPEAT_COMMAND_COUNT {
        setup_payload(Motor::M1, CmdType::Command(Command::SaveSettings));
        setup_payload(Motor::M2, CmdType::Command(Command::SaveSettings));
        setup_payload(Motor::M3, CmdType::Command(Command::SaveSettings));
        setup_payload(Motor::M4, CmdType::Command(Command::SaveSettings));

        send_payload(timer);

        delay.delay_ms(PAUSE_BETWEEN_COMMANDS);
    }
    delay.delay_ms(PAUSE_AFTER_SAVE);

    unsafe { ESC_TELEM = false };
}

/// Calculate CRC. Used for both sending and receiving.
fn calc_crc(packet: u16) -> u16 {
    if BIDIR_EN {
        (!(packet ^ (packet >> 4) ^ (packet >> 8))) & 0x0F
    } else {
        (packet ^ (packet >> 4) ^ (packet >> 8)) & 0x0F
    }
}

/// Update our DSHOT payload for a given rotor, with a given power level. This created a payload
/// of tick values to send to the CCMR register; the output pin is set high or low for each
/// tick duration in succession.
pub fn setup_payload(rotor: Motor, cmd: CmdType) {
    // First 11 (0:10) bits are the throttle settings. 0 means disarmed. 1-47 are reserved
    // for special commands. 48 - 2_047 are throttle value (2_000 possible values)

    // Bit 11 is 1 to request telemetry; 0 otherwise.
    // Bits 12:15 are CRC, to validate data.

    let data_word = match cmd {
        CmdType::Command(c) => c as u16,
        CmdType::Power(pwr) => (pwr * 1_999.) as u16 + 48,
    };

    let packet = (data_word << 1) | (unsafe { ESC_TELEM } as u16);

    // Compute the checksum
    let packet = (packet << 4) | calc_crc(packet);

    let offset = match rotor {
        Motor::M1 => 0,
        Motor::M2 => 1,
        Motor::M3 => 2,
        Motor::M4 => 3,
    };

    // Create a DMA payload of 16 timer CCR (duty) settings, each for one bit of our data word.
    for i in 0..16 {
        let bit = (packet >> i) & 1;
        let val = if bit == 1 { DUTY_HIGH } else { DUTY_LOW };
        // DSHOT uses MSB first alignment.
        // Values alternate in the buffer between the 4 registers we're editing, so
        // we interleave values here. (Each timer and DMA stream is associated with 2 channels).
        unsafe { PAYLOAD[(15 - i) * NUM_MOTORS + offset] = val as u16 };
    }

    // Note that the end stays 0-padded, since we init with 0s, and never change those values.
}

/// Set a rotor pair's power, using a 16-bit DHOT word, transmitted over DMA via timer CCR (duty)
/// settings. `power` ranges from 0. to 1.
pub fn set_power(power1: f32, power2: f32, power3: f32, power4: f32, timer: &mut MotorTimer) {
    setup_payload(Motor::M1, CmdType::Power(power1));
    setup_payload(Motor::M2, CmdType::Power(power2));
    setup_payload(Motor::M3, CmdType::Power(power3));
    setup_payload(Motor::M4, CmdType::Power(power4));

    send_payload(timer);
}

/// Set a single rotor's power. Used by preflight; not normal operations.
pub fn set_power_single(rotor: Motor, power: f32, timer: &mut MotorTimer) {
    setup_payload(rotor, CmdType::Power(power));
    send_payload(timer)
}

use core::sync::atomic::Ordering;
use stm32_hal2::gpio::PinMode; // todo move up if you keep this.

/// Send the stored payload.
fn send_payload(timer: &mut MotorTimer) {
    // Stop any transations in progress.
    // dma::stop(setup::MOTORS_DMA_PERIPH, setup::MOTOR_CH);

    // Note that timer enabling is handled by `write_dma_burst`.

    // todo: Reset ARR here and pin in ISR, or in dshot::send_payload? Currently here
    let alt_mode = 0b10;
    // todo: H7 pins too.
    unsafe {
        cfg_if! {
            if #[cfg(feature = "h7")] {
                (*pac::GPIOC::ptr())
                    .moder
                    .modify(|_, w| {
                        w.moder6().bits(alt_mode);
                        w.moder7().bits(alt_mode);
                        w.moder8().bits(alt_mode);
                        w.moder9().bits(alt_mode)
                    });

            } else {
                // todo: Put these in once you have the new board
                // (*pac::GPIOC::ptr())
                //     .moder
                //     .modify(|_, w| w.moder6().bits(alt_mode));
                // (*pac::GPIOA::ptr())
                //     .moder
                //     .modify(|_, w| w.moder4().bits(alt_mode));
                (*pac::GPIOB::ptr())
                    .moder
                    .modify(|_, w| {
                    w.moder0().bits(alt_mode);
                    w.moder1().bits(alt_mode)
                });
            }
        }
    }

    unsafe {
        timer.write_dma_burst(
            &PAYLOAD,
            setup::DSHOT_BASE_DIR_OFFSET,
            NUM_MOTORS as u8, // Update a channel per number of motors, up to 4.
            setup::MOTOR_CH,
            ChannelCfg {
                // Take precedence over CRSF and ADCs.
                priority: Priority::High,
                ..ChannelCfg::default()
            },
            true,
            setup::MOTORS_DMA_PERIPH,
        );
    }
}

/// Receive an RPM payload for all channels in bidirectional mode.
/// Note that we configure what won't affect the FC-ESC transmission in the reception timer's
/// ISR on payload-reception-complete. Here, we configure things that would affect transmission.
// pub fn receive_payload(timer: &mut MotorTimer) {
pub fn receive_payload() {
    // Note: Should already be stopped, as we call this from a transfer-complete interrupt.
    // dma::stop(setup::MOTORS_DMA_PERIPH, setup::MOTOR_CH);

    // set_to_input(timer);
    // DSHOT_REC_MODE.store(true, Ordering::Relaxed);
    //
    // #[cfg(feature = "h7")]
    // let high_prec_timer = true;
    // #[cfg(feature = "g4")]
    // let high_prec_timer = false;
    //
    // unsafe {
    //     timer.read_dma_burst(
    //         &PAYLOAD_REC,
    //         setup::DSHOT_BASE_DIR_OFFSET,
    //         NUM_MOTORS as u8,
    //         setup::MOTOR_CH,
    //         ChannelCfg {
    //             // Take precedence over CRSF and ADCs.
    //             priority: Priority::High,
    //             ..ChannelCfg::default()
    //         },
    //         high_prec_timer,
    //         setup::MOTORS_DMA_PERIPH,
    //     );
    // }

    // todo: Trying a different approach
    let input_mode = 0b00;
    unsafe {
        cfg_if! {
            if #[cfg(feature = "h7")] {
                (*pac::GPIOC::ptr())
                    .moder
                    .modify(|_, w| {
                        w.moder6().bits(input_mode);
                        w.moder7().bits(input_mode);
                        w.moder8().bits(input_mode);
                        w.moder9().bits(input_mode)
                    });

            } else {
                // todo: Put these in once you have the new board
                // (*pac::GPIOC::ptr())
                //     .moder
                //     .modify(|_, w| w.moder6().bits(input_mode));
                // (*pac::GPIOA::ptr())
                //     .moder
                //     .modify(|_, w| w.moder4().bits(input_mode));
                (*pac::GPIOB::ptr())
                    .moder
                    .modify(|_, w| {
                    w.moder0().bits(input_mode);
                    w.moder1().bits(input_mode)
                });
            }
        }

        // gpio::read_dma(
        //     &PAYLOAD_REC,
        //     setup::MOTOR_CH,
        //     ChannelCfg {
        //         // Take precedence over CRSF and ADCs.
        //         priority: Priority::High,
        //         ..ChannelCfg::default()
        //     },
        //     setup::MOTORS_DMA_PERIPH,
        // );
    }

    // todo: Shared resource for pins?
    let exti = unsafe { &(*pac::EXTI::ptr()) };

    // todo: Diff syntax on H7.
    // exti.rtsr1.modify(|_, w| w.rt6().clear_bit());
    // exti.ftsr1.modify(|_, w| w.ft6().set_bit());
    // syscfg.exticr2.modify(|_, w| unsafe { w.exti6().bits(2) }); // Points to port C.

    // exti.rtsr1.modify(|_, w| w.rt1().clear_bit());
    exti.ftsr1.modify(|_, w| w.ft1().set_bit());

    // todo: Why is the below working but not above???
    let mut a = gpio::Pin::new(gpio::Port::B, 1, gpio::PinMode::Input);
    a.enable_interrupt(gpio::Edge::Falling);

    // unsafe {
    //     (*pac::TIM2::ptr()).arr.write(|w| w.bits(READ_TIMER_ARR_INIT));
    // }
}

/// Change timer polarity and count direction, to enable or disable bidirectional DSHOT.
/// This results in the signal being active low for enabled, and active high for disabled.
/// Timer settings default (in HAL and hardware) to disabled.
pub fn set_bidirectional(enabled: bool, timer: &mut MotorTimer) {
    // todo: We need a way to configure channel 2 as a servo, eg for fixed-wing
    // todo with a rudder.

    let mut polarity = Polarity::ActiveHigh;
    let mut count_dir = CountDir::Up;

    if enabled {
        polarity = Polarity::ActiveLow;
        count_dir = CountDir::Down;
    }

    // Set up channels 1 and 2: This is for both quadcopters and fixed-wing.
    timer.set_polarity(Motor::M1.tim_channel(), polarity);
    timer.set_polarity(Motor::M2.tim_channel(), polarity);

    #[cfg(feature = "quad")]
    timer.set_polarity(Motor::M3.tim_channel(), polarity);
    #[cfg(feature = "quad")]
    timer.set_polarity(Motor::M4.tim_channel(), polarity);

    timer.cfg.direction = count_dir;
    timer.set_dir();
}

/// Set the timer(s) to output mode. Do this in init, and after a receive
/// phase of bidirectional DSHOT.
pub fn set_to_output(timer: &mut MotorTimer) {
    // todo: The code below may be removed if using bitbang receive

    // todo: Awk place for this.
    // Point the EXTI0 interrupt to the C port for RPM reception.
    let syscfg = unsafe { &(*pac::SYSCFG::ptr()) };
    syscfg.exticr1.modify(|_, w| unsafe { w.exti1().bits(1) }); // Points to port B.

    let oc = OutputCompare::Pwm1;

    timer.set_auto_reload(DSHOT_ARR_600 as u32);

    // todo: Here and elsewhere in this module, if you allocate timers/motors differently than 2/2
    // todo for fixed-wing, you'll need to change this logic.

    timer.enable_pwm_output(Motor::M1.tim_channel(), oc, 0.);
    timer.enable_pwm_output(Motor::M2.tim_channel(), oc, 0.);

    #[cfg(feature = "quad")]
    timer.enable_pwm_output(Motor::M3.tim_channel(), oc, 0.);
    #[cfg(feature = "quad")]
    timer.enable_pwm_output(Motor::M4.tim_channel(), oc, 0.);
}

/// Set the timer(s) to input mode. Used to receive PWM data in bidirectional mode.
/// Assumes the timer is stopped prior to calling.
pub fn _set_to_input(timer: &mut MotorTimer) {
    let cc = CaptureCompare::InputTi1;
    let pol_p = Polarity::ActiveLow;
    let pol_n = Polarity::ActiveHigh;

    // todo: Don't use `set_period here; set ARR and PSC
    // 100us is longer that we expect the longest received pulse to be. Reduce A/R
    // timer.set_freq(300_000.);
    timer.set_auto_reload(14_000); // todo: Use const. And will change H7 vs G4

    timer.set_input_capture(Motor::M1.tim_channel(), cc, pol_p, pol_n);
    timer.set_input_capture(Motor::M2.tim_channel(), cc, pol_p, pol_n);
    #[cfg(feature = "quad")]
    timer.set_input_capture(Motor::M3.tim_channel(), cc, pol_p, pol_n);
    #[cfg(feature = "quad")]
    timer.set_input_capture(Motor::M4.tim_channel(), cc, pol_p, pol_n);

    // todo: Experimenting how to capture both edges.
    let cc2 = CaptureCompare::InputTi2; // For example, this maps CC3 to ch4 and CC4 to TIM3 etc, I believe.

    // timer.set_input_capture(Motor::M3.tim_channel(), cc, pol_p, pol_n);
    // timer.set_input_capture(Motor::M3.tim_channel(), cc2, pol_n, pol_p);
    // #[cfg(feature = "quad")]
    // timer.set_input_capture(Motor::M4.tim_channel(), cc, pol_p, pol_n);
    // #[cfg(feature = "quad")]
    // timer.set_input_capture(Motor::M4.tim_channel(), cc2, pol_n, pol_p);
}

#[derive(Clone, Copy)]
enum EscData {
    Rpm(f32),
    Telem(EscTelemType, u8),
}

#[derive(Clone, Copy)]
// todo: These could hold a value
enum EscTelemType {
    Temp,
    Voltage,
    Current,
    Debug1,
    Debug2,
    Debug3,
    State,
}

/// Return RPM in radians-per-second
/// See https://brushlesswhoop.com/dshot-and-bidirectional-dshot/, "eRPM Telemetry Frame (from ESC)".
fn rpm_from_data(packet: u16) -> Result<EscData, RpmError> {
    let crc = packet & 0b1111;

    if crc != calc_crc(packet) {
        return Err(RpmError::Crc);
    }

    // Parse extended telemetry if avail. (This may be required to avoid misreading the data?)
    if ((packet >> 8) & 1) == 0 {
        // Telemetry is passed
        let telem_type_val = packet >> 12;
        let val = (packet >> 4) & 0xff; // 8 bits vice 9 for rpm data

        let telem_type = match telem_type_val {
            0x02 => EscTelemType::Temp,
            0x04 => EscTelemType::Voltage,
            0x06 => EscTelemType::Current,
            0x08 => EscTelemType::Debug1,
            0x0A => EscTelemType::Debug2,
            0x0C => EscTelemType::Debug3,
            0x0E => EscTelemType::State,
            _ => return Err(RpmError::TelemType),
        };

        Ok(EscData::Telem(telem_type, val as u8))
    } else {
        // RPM data
        let shift = packet >> 13;
        let base = (packet >> 4) & 0b1_1111_1111;
        let period_us = base << shift;

        // Period is in us. Convert to Radians-per-second using motor pole count.
        // todo: Pole count in user cfg.

        let num_poles = 14.; // todo placeholder

        Ok(EscData::Rpm(1_000_000. / (period_us as f32 * num_poles)))
    }
}

/// u32 since it's 20 bits.
fn bool_array_to_u32(arr: &[bool]) -> u32 {
    let mut result = 0;

    for (i, v) in arr.iter().enumerate() {
        result |= (*v as u32) << (20 - i)
    }

    result
}

enum RpmError {
    Gcr,
    Crc,
    TelemType,
}

/// Make it stop
fn reduce_bit_count_helper(val: u16) -> Result<u16, RpmError> {
    let result = match val {
        0x19 => 0,
        0x1b => 1,
        0x12 => 2,
        0x13 => 3,
        0x1d => 4,
        0x15 => 5,
        0x16 => 6,
        0x17 => 7,
        0x1a => 8,
        0x09 => 9,
        0x0a => 0xa,
        0x0b => 0xb,
        0x1e => 0xc,
        0x0d => 0xd,
        0x0e => 0xe,
        0x0f => 0xf,
        _ => return Err(RpmError::Gcr),
    };

    Ok(result)
}

/// https://brushlesswhoop.com/dshot-and-bidirectional-dshot/: `eRPM Transmission`
/// Reduce from
/// This is really obnoxious.
fn reduce_gcr_bit_count(val: u32) -> Result<u16, RpmError> {
    let nibble_0 = (val & 0b1_1111) as u16;
    let nibble_1 = ((val >> 5) & 0b1_1111) as u16;
    let nibble_2 = ((val >> 10) & 0b1_1111) as u16;
    let nibble_3 = ((val >> 15) & 0b1_1111) as u16;

    Ok(reduce_bit_count_helper(nibble_0)?
        | (reduce_bit_count_helper(nibble_1)? << 4)
        | (reduce_bit_count_helper(nibble_2)? << 8)
        | (reduce_bit_count_helper(nibble_3)? << 12))
}

/// https://brushlesswhoop.com/dshot-and-bidirectional-dshot/: `Decoding eRPM frame`
fn gcr_step_1(val: u32) -> u32 {
    val ^ (val >> 1)
}

/// Helper fn
fn update_rpm_from_packet(
    rpm: &mut f32,
    packet: Result<u16, RpmError>,
    fault: &mut bool,
) -> Result<(), RpmError> {
    match packet {
        Ok(packet) => {
            match rpm_from_data(packet) {
                Ok(r) => {
                    match r {
                        EscData::Rpm(rpm_) => {
                            *rpm = rpm_;
                        }
                        EscData::Telem(_, _) => {
                            // todo
                        }
                    }
                }
                Err(e) => return Err(e),
            }
        }
        Err(e) => return Err(e),
    }

    Ok(())
}

/// Update the motor RPM struct with our buffer data.
pub fn update_rpms(rpms: &mut MotorRpm, fault: &mut bool) {
    // pub fn update_rpms(rpms: &mut MotorRpm, mapping: &ControlMapping) {
    // Convert our boolean array to a 20-bit integer.
    let gcr1 = bool_array_to_u32(unsafe { &PAYLOAD_REC_BB_1 });
    let gcr2 = bool_array_to_u32(unsafe { &PAYLOAD_REC_BB_2 });
    let gcr3 = bool_array_to_u32(unsafe { &PAYLOAD_REC_BB_3 });
    let gcr4 = bool_array_to_u32(unsafe { &PAYLOAD_REC_BB_3 });

    // Perform some initial de-obfuscation.
    let gcr1 = gcr_step_1(gcr1);
    let gcr2 = gcr_step_1(gcr2);
    let gcr3 = gcr_step_1(gcr3);
    let gcr4 = gcr_step_1(gcr4);

    // println!("1: {}", gcr3);

    // Convert our 20-bit raw GCR data to the 16-bit data packet.
    let packet1 = reduce_gcr_bit_count(gcr1);
    let packet2 = reduce_gcr_bit_count(gcr2);
    let packet3 = reduce_gcr_bit_count(gcr3);
    let packet4 = reduce_gcr_bit_count(gcr4);

    // todo: Don't hard code teh mapping!!

    if update_rpm_from_packet(&mut rpms.aft_right, packet1, fault).is_err() {
        *fault = true;
    };
    if update_rpm_from_packet(&mut rpms.front_right, packet2, fault).is_err() {
        *fault = true;
    };
    if update_rpm_from_packet(&mut rpms.aft_left, packet3, fault).is_err() {
        *fault = true;
    };
    if update_rpm_from_packet(&mut rpms.front_left, packet4, fault).is_err() {
        *fault = true;
    };

    // println!("RPM 3: {}", rpms.aft_left)
    // todo: Mapping! You may need to pass in the mapping struct.
}
