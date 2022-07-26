//! This module contains code related to the flight control PID loop. It can be thought of
//! as a sub-module for `flight_ctrls`.
//!
//! See the OneNote document for notes on how we handle the more complicated / cascaded control modes.
//!
//! [Some info on the PID terms, focused on BF](https://gist.github.com/exocode/90339d7f946ad5f83dd1cf29bf5df0dc)
//! https://oscarliang.com/quadcopter-pid-explained-tuning/

use core::f32::consts::TAU;

use stm32_hal2::{dma::Dma, pac::DMA1};

use cmsis_dsp_api as dsp_api;
use cmsis_dsp_sys as dsp_sys;

use crate::{
    autopilot::AutopilotStatus,
    control_interface::ChannelData,
    flight_ctrls::{
        self,
        common::{AltType, CommandState, CtrlInputs, InputMap, MotorTimers, Params},
        quad::{InputMode, POWER_LUT, YAW_ASSIST_COEFF, YAW_ASSIST_MIN_SPEED},
    },
    util::IirInstWrapper,
    ArmStatus, ControlPositions, RotorMapping, ServoWingMapping, UserCfg, DT_ATTITUDE,
};

use crate::flight_ctrls::quad::MAX_ROTOR_POWER;
use defmt::println;

// todo: You need to take derivative of control inputs into account. For example, when sticks are moved,
// todo: reduce I term, and/or add a "FF"-scaled term direction to PID output in dir of
// todo the change etc

// todo: In rate/acro mode, instead of zeroing unused axes, have them store a value that they return to?'

// Amount each airborne, (from controller) PID adjustment modifies a given PID term.
pub const PID_CONTROL_ADJ_AMT: f32 = 0.001; // in whatever units are PID values are
pub const PID_CONTROL_ADJ_TIMEOUT: f32 = 0.3; // seconds

const INTEGRATOR_CLAMP_MAX_QUAD: f32 = 0.4;
const INTEGRATOR_CLAMP_MIN_QUAD: f32 = -INTEGRATOR_CLAMP_MAX_QUAD;
const INTEGRATOR_CLAMP_MAX_FIXED_WING: f32 = 0.4;
const INTEGRATOR_CLAMP_MIN_FIXED_WING: f32 = -INTEGRATOR_CLAMP_MAX_FIXED_WING;

// "TPA" stands for Throttle PID attenuation - reduction in D term (or more) past a certain
// throttle setting, linearly. This only applies to the rate loop.
// https://github-wiki-see.page/m/betaflight/betaflight/wiki/PID-Tuning-Guide
pub const TPA_MIN_ATTEN: f32 = 0.5; // At full throttle, D term is attenuated to this value.
pub const TPA_BREAKPOINT: f32 = 0.3; // Start engaging TPA at this value.
                                     // `TPA_SLOPE` and `TPA_Y_INT` are cached calculations.
const TPA_SLOPE: f32 = (TPA_MIN_ATTEN - 1.) / (MAX_ROTOR_POWER - TPA_BREAKPOINT);
const TPA_Y_INT: f32 = -(TPA_SLOPE * TPA_BREAKPOINT - 1.);

/// Update the D term with throttle PID attenuation; linear falloff of the D term at a cutoff throttle
/// setting. Multiple the D term by this function's output.
fn tpa_adjustment(throttle: f32) -> f32 {
    TPA_SLOPE * throttle + TPA_Y_INT
}

// These filter states are for the PID D term.
static mut FILTER_STATE_ROLL_ATTITUDE: [f32; 4] = [0.; 4];
static mut FILTER_STATE_PITCH_ATTITUDE: [f32; 4] = [0.; 4];
static mut FILTER_STATE_YAW_ATTITUDE: [f32; 4] = [0.; 4];

static mut FILTER_STATE_ROLL_RATE: [f32; 4] = [0.; 4];
static mut FILTER_STATE_PITCH_RATE: [f32; 4] = [0.; 4];
static mut FILTER_STATE_YAW_RATE: [f32; 4] = [0.; 4];

static mut FILTER_STATE_THRUST: [f32; 4] = [0.; 4];

// todo temp: BF constants, for use with BF code translation while figuring this uot
const BF_P: f32 = 10.;
const BF_I: f32 = 10.;
const BF_D: f32 = 10.;

// filter_ = signal.iirfilter(1, 100, btype="lowpass", ftype="bessel", output="sos", fs=8_000)
// coeffs = []
// for row in filter_:
//     coeffs.extend([row[0] / row[3], row[1] / row[3], row[2] / row[3], -row[4] / row[3], -row[5] / row[3]])

// todo: Diff coeffs for diff diff parts, as required.
#[allow(clippy::excessive_precision)]
static COEFFS_D: [f32; 5] = [
    0.037804754170896473,
    0.037804754170896473,
    0.0,
    0.9243904916582071,
    -0.0,
];

/// Cutoff frequency for our PID lowpass frequency, in Hz
#[derive(Clone, Copy)]
pub enum LowpassCutoff {
    // todo: What values should these be?
    H500,
    H1k,
    H10k,
    H20k,
}

/// Coefficients and other configurable parameters for controls, for pich and roll.
/// Has several variants, due to coupling with horizontal (X and Y) movement.
pub struct CtrlCoeffsPR {
    // These coefficients map desired change in flight parameters to rotor power change.
    // pitch, roll, and yaw s are in power / radians
    pub k_p_rate: f32,
    pub k_i_rate: f32,
    pub k_d_rate: f32,

    pub k_p_attitude: f32,
    pub k_i_attitude: f32,
    pub k_d_attitude: f32,

    pub k_p_velocity: f32,
    pub k_i_velocity: f32,
    // k_d_velocity: f32,
    // Note that we don't use the D component for our velocity PID.
    pub pid_deriv_lowpass_cutoff_rate: LowpassCutoff,
    pub pid_deriv_lowpass_cutoff_attitude: LowpassCutoff,
}

impl Default for CtrlCoeffsPR {
    fn default() -> Self {
        Self {
            k_p_rate: 0.10,
            k_i_rate: 0.50,
            k_d_rate: 0.0030,

            // pid for controlling pitch and roll from commanded horizontal velocity
            k_p_attitude: 47.,
            k_i_attitude: 84.,
            k_d_attitude: 34.,

            // PID for controlling pitch and roll rate directly (eg Acro)
            k_p_velocity: 0.1,
            k_i_velocity: 0.,
            // k_d_velocity: 0.,
            pid_deriv_lowpass_cutoff_rate: LowpassCutoff::H1k,
            pid_deriv_lowpass_cutoff_attitude: LowpassCutoff::H1k,
        }
    }
}

impl CtrlCoeffsPR {
    pub fn default_flying_wing() -> Self {
        Self {
            k_p_rate: 0.06,
            // k_i_rate: 0.60,
            k_i_rate: 0.0,
            // k_d_rate: 0.02,
            k_d_rate: 0.00,

            // Attitude not used.

            // pid for controlling pitch and roll from commanded horizontal velocity
            k_p_attitude: 0.,
            k_i_attitude: 0.,
            k_d_attitude: 0.,

            // PID for controlling pitch and roll rate directly (eg Acro)
            k_p_velocity: 0.1,
            k_i_velocity: 0.,
            // k_d_velocity: 0.,
            pid_deriv_lowpass_cutoff_rate: LowpassCutoff::H1k,
            pid_deriv_lowpass_cutoff_attitude: LowpassCutoff::H1k,
        }
    }
}

/// Coefficients and other configurable parameters for yaw and thrust. This is separate from, and
/// simpler than the variant for pitch and roll, since it's not coupled to X and Y motion.
pub struct CtrlCoeffsYT {
    // PID for controlling yaw or thrust from a velocity directly applied to them. (Eg Acro and attitude)
    pub k_p_rate: f32,
    pub k_i_rate: f32,
    pub k_d_rate: f32,

    // PID for controlling yaw or thrust from an explicitly-commanded heading or altitude.
    pub k_p_attitude: f32,
    pub k_i_attitude: f32,
    pub k_s_attitude: f32,

    pub pid_deriv_lowpass_cutoff: LowpassCutoff,
}

impl Default for CtrlCoeffsYT {
    fn default() -> Self {
        Self {
            // k_p_rate: 0.6 * K_U_YAW,
            // k_i_rate: 1.2 * K_U_YAW / T_U_YAW,
            // k_d_rate: 3. * K_U_YAW * T_U_YAW / 40.,
            k_p_rate: 0.30,
            k_i_rate: 0.01 * 0.,
            k_d_rate: 0.,

            k_p_attitude: 0.1,
            k_i_attitude: 0.0,
            k_s_attitude: 0.0,

            pid_deriv_lowpass_cutoff: LowpassCutoff::H1k,
        }
    }
}

pub struct CtrlCoeffGroup {
    pub pitch: CtrlCoeffsPR,
    pub roll: CtrlCoeffsPR,
    pub yaw: CtrlCoeffsYT,
    pub thrust: CtrlCoeffsYT,
}

impl Default for CtrlCoeffGroup {
    /// These starting values are Betaflight defaults.
    fn default() -> Self {
        Self {
            pitch: Default::default(),
            roll: Default::default(),
            yaw: Default::default(),
            thrust: Default::default(),
        }
    }
}

impl CtrlCoeffGroup {
    pub fn default_flying_wing() -> Self {
        Self {
            pitch: CtrlCoeffsPR::default_flying_wing(),
            roll: CtrlCoeffsPR::default_flying_wing(),
            yaw: Default::default(),
            thrust: Default::default(),
        }
    }
}

#[derive(Default)]
pub struct PidGroup {
    pub pitch: PidState,
    pub roll: PidState,
    pub yaw: PidState,
    pub thrust: PidState,
}

impl PidGroup {
    /// Reset the interator term on all components.
    pub fn reset_integrator(&mut self) {
        self.pitch.i = 0.;
        self.roll.i = 0.;
        self.yaw.i = 0.;
        self.thrust.i = 0.;
    }
}

/// Proportional, Integral, Derivative error, for flight parameter control updates.
/// For only a single set (s, v, a). Note that e is the error betweeen commanded
/// and measured, while the other terms include the PID coefficients (K_P) etc.
/// So, `p` is always `e` x `K_P`.
/// todo: Consider using Params, eg this is the error for a whole set of params.
#[derive(Default)]
pub struct PidState {
    /// Measurement: Used for the derivative.
    pub measurement: f32,
    /// Error term. (No coeff multiplication). Used for the integrator
    pub e: f32,
    /// Proportional term
    pub p: f32,
    /// Integral term
    pub i: f32,
    /// Derivative term
    pub d: f32,
}

impl PidState {
    /// Anti-windup integrator clamp
    pub fn anti_windup_clamp(&mut self, error_p: f32) {
        //  Dynamic integrator clamping, from https://www.youtube.com/watch?v=zOByx3Izf5U

        let lim_max_int = if INTEGRATOR_CLAMP_MAX_QUAD > error_p {
            INTEGRATOR_CLAMP_MAX_QUAD - error_p
        } else {
            0.
        };

        let lim_min_int = if INTEGRATOR_CLAMP_MIN_QUAD < error_p {
            INTEGRATOR_CLAMP_MIN_QUAD - error_p
        } else {
            0.
        };

        if self.i > lim_max_int {
            self.i = lim_max_int;
        } else if self.i < lim_min_int {
            self.i = lim_min_int;
        }
    }

    pub fn out(&self) -> f32 {
        self.p + self.i + self.d
    }
}

/// Store lowpass IIR filter instances, for use with the deriv terms of our PID loop. Note that we don't
/// need this for our horizontal velocity PIDs.
pub struct PidDerivFilters {
    pub roll_attitude: IirInstWrapper,
    pub pitch_attitude: IirInstWrapper,
    pub yaw_attitude: IirInstWrapper,

    pub roll_rate: IirInstWrapper,
    pub pitch_rate: IirInstWrapper,
    pub yaw_rate: IirInstWrapper,

    pub thrust: IirInstWrapper, // todo - do we need this?
}

impl Default for PidDerivFilters {
    fn default() -> Self {
        let mut result = Self {
            roll_attitude: IirInstWrapper {
                inner: dsp_api::biquad_cascade_df1_init_empty_f32(),
            },
            pitch_attitude: IirInstWrapper {
                inner: dsp_api::biquad_cascade_df1_init_empty_f32(),
            },
            yaw_attitude: IirInstWrapper {
                inner: dsp_api::biquad_cascade_df1_init_empty_f32(),
            },

            roll_rate: IirInstWrapper {
                inner: dsp_api::biquad_cascade_df1_init_empty_f32(),
            },
            pitch_rate: IirInstWrapper {
                inner: dsp_api::biquad_cascade_df1_init_empty_f32(),
            },
            yaw_rate: IirInstWrapper {
                inner: dsp_api::biquad_cascade_df1_init_empty_f32(),
            },

            thrust: IirInstWrapper {
                inner: dsp_api::biquad_cascade_df1_init_empty_f32(),
            },
        };

        unsafe {
            // todo: Re-initialize fn?
            dsp_api::biquad_cascade_df1_init_f32(
                &mut result.roll_attitude.inner,
                &COEFFS_D,
                &mut FILTER_STATE_ROLL_ATTITUDE,
            );
            dsp_api::biquad_cascade_df1_init_f32(
                &mut result.pitch_attitude.inner,
                &COEFFS_D,
                &mut FILTER_STATE_PITCH_ATTITUDE,
            );
            dsp_api::biquad_cascade_df1_init_f32(
                &mut result.yaw_attitude.inner,
                &COEFFS_D,
                &mut FILTER_STATE_YAW_ATTITUDE,
            );

            dsp_api::biquad_cascade_df1_init_f32(
                &mut result.roll_rate.inner,
                &COEFFS_D,
                &mut FILTER_STATE_ROLL_RATE,
            );
            dsp_api::biquad_cascade_df1_init_f32(
                &mut result.pitch_rate.inner,
                &COEFFS_D,
                &mut FILTER_STATE_PITCH_RATE,
            );
            dsp_api::biquad_cascade_df1_init_f32(
                &mut result.yaw_rate.inner,
                &COEFFS_D,
                &mut FILTER_STATE_YAW_RATE,
            );

            dsp_api::biquad_cascade_df1_init_f32(
                &mut result.thrust.inner,
                &COEFFS_D,
                &mut FILTER_STATE_THRUST,
            );
        }

        result
    }
}

/// Calculate the PID error given flight parameters, and a flight
/// command.
/// Example: https://github.com/pms67/PID/blob/master/PID.c
/// Example 2: https://github.com/chris1seto/OzarkRiver/blob/4channel/FlightComputerFirmware/Src/Pid.c
pub fn calc_pid_error(
    set_pt: f32,
    measurement: f32,
    prev_pid: &PidState,
    k_p: f32,
    k_i: f32,
    k_d: f32,
    filter: &mut IirInstWrapper,
    // This `dt` is dynamic, since we don't necessarily run this function at a fixed interval.
    dt: f32,
) -> PidState {
    // Find appropriate control inputs using PID control.

    let error = set_pt - measurement;

    // https://www.youtube.com/watch?v=zOByx3Izf5U
    let error_p = k_p * error;

    // For inegral term, use a midpoint formula, and use error, vice measurement.
    let error_i = k_i * (error + prev_pid.e) / 2. * dt + prev_pid.i;

    // Derivative on measurement vice error, to avoid derivative kick. Note that deriv-on-measurment
    // can be considered smoother, while deriv-on-error can be considered more responsive.
    let error_d_prefilt = k_d * (measurement - prev_pid.measurement) / dt;

    let mut error_d = [0.];
    dsp_api::biquad_cascade_df1_f32(&mut filter.inner, &[error_d_prefilt], &mut error_d, 1);

    // println!(
    //     "SP {} M{} e {} p {} i {} d {}",
    //     set_pt, measurement, error, error_p, error_i, error_d[0]
    // );

    let mut result = PidState {
        measurement,
        e: error,
        p: error_p,
        i: error_i,
        d: error_d[0],
    };

    result.anti_windup_clamp(error_p);

    // todo: Clamp output?

    result
}

/// Run the velocity (outer) PID Loop: This is used to determine attitude, eg based on commanded velocity
/// or position.
pub fn run_velocity(
    params: &Params,
    // inputs: &CtrlInputs,
    ch_data: &ChannelData,
    input_map: &InputMap,
    velocities_commanded: &mut CtrlInputs,
    attitude_commanded: &mut CtrlInputs,
    pid: &mut PidGroup,
    filters: &mut PidDerivFilters,
    input_mode: &InputMode,
    autopilot_status: &AutopilotStatus,
    cfg: &UserCfg,
    commands: &mut CommandState,
    coeffs: &CtrlCoeffGroup,
) {
    // todo: GO over this whole function; it's not ready! And the autopilot modes for all 3 PID fns.
    if let Some(alt_msl_commanded) = autopilot_status.recover {
        let dist_v = alt_msl_commanded - params.baro_alt_msl;

        // `enroute_speed_ver` returns a velocity of the appropriate sine for above vs below.
        let thrust =
            flight_ctrls::quad::enroute_speed_ver(dist_v, cfg.max_speed_ver, params.tof_alt);

        // todo: DRY from alt_hold autopilot code.

        // todo: Figure out exactly what you need to pass for the autopilot modes to inner_flt_cmd
        // todo while in acro mode.
        *velocities_commanded = CtrlInputs {
            pitch: input_map.calc_pitch_angle(0.),
            roll: input_map.calc_roll_angle(0.),
            yaw: input_map.calc_yaw_rate(0.),
            thrust,
        };
    }

    // If in acro or attitude mode, we can adjust the throttle setting to maintain a fixed altitude,
    // either MSL or AGL.
    if let Some((alt_type, alt_commanded)) = autopilot_status.alt_hold {
        // Set a vertical velocity for the inner loop to maintain, based on distance
        let dist = match alt_type {
            AltType::Msl => alt_commanded - params.baro_alt_msl,
            AltType::Agl => alt_commanded - params.tof_alt,
        };
        // `enroute_speed_ver` returns a velocity of the appropriate sine for above vs below.
        velocities_commanded.thrust =
            flight_ctrls::quad::enroute_speed_ver(dist, cfg.max_speed_ver, params.tof_alt);
    }

    match input_mode {
        InputMode::Acro => (),
        InputMode::Attitude => (),
        InputMode::Command => {
            // todo: Impl
            // if autopilot_status.takeoff {
            //     // AutopilotMode::Takeoff => {
            //     *velocities_commanded = CtrlInputs {
            //         pitch: 0.,
            //         roll: 0.,
            //         yaw: 0.,
            //         thrust: flight_ctrls::quad::takeoff_speed(params.tof_alt, cfg.max_speed_ver),
            //     };
            // }
            // else if autopilot_status.land {
            //     *velocities_commanded = CtrlInputs {
            //         pitch: 0.,
            //         roll: 0.,
            //         yaw: 0.,
            //         thrust: flight_ctrls::quad::landing_speed(params.tof_alt, cfg.max_speed_ver),
            //     };
            // }
        }
    }

    let mut param_x = params.v_x;
    let mut param_y = params.v_y;

    let mut k_p_pitch = coeffs.pitch.k_p_attitude;
    let mut k_i_pitch = coeffs.pitch.k_i_attitude;
    let mut k_d_pitch = coeffs.pitch.k_d_attitude;

    let mut k_p_roll = coeffs.roll.k_p_attitude;
    let mut k_i_roll = coeffs.roll.k_i_attitude;
    let mut k_d_roll = coeffs.roll.k_d_attitude;

    let eps1 = 0.01;
    if ch_data.pitch > eps1 || ch_data.roll > eps1 {
        commands.loiter_set = false;
    }

    let eps2 = 0.01;
    // todo: Commanded velocity 0 to trigger loiter logic, or actual velocity?
    // if mid_flight_cmd.y_pitch.unwrap().2 < eps && mid_flight_cmd.x_roll.unwrap().2 < eps {
    if params.s_x < eps2 && params.s_y < eps2 {
        if !commands.loiter_set {
            commands.x = params.s_x;
            commands.y = params.s_y;
            commands.loiter_set = true;
        }

        param_x = commands.x;
        param_y = commands.y;

        k_p_pitch = coeffs.pitch.k_p_rate;
        k_i_pitch = coeffs.pitch.k_i_rate;
        k_d_pitch = coeffs.pitch.k_d_rate;

        k_p_roll = coeffs.roll.k_p_rate;
        k_i_roll = coeffs.roll.k_i_rate;
        k_d_roll = coeffs.roll.k_d_rate;
    }

    pid.pitch = calc_pid_error(
        velocities_commanded.pitch,
        param_y,
        &pid.pitch,
        coeffs.pitch.k_p_velocity,
        coeffs.pitch.k_p_velocity,
        0., // first-order + delay system
        &mut filters.pitch_attitude,
        DT_ATTITUDE,
    );

    pid.roll = calc_pid_error(
        velocities_commanded.roll,
        param_x,
        &pid.roll,
        // coeffs,
        coeffs.roll.k_p_velocity,
        coeffs.roll.k_p_velocity,
        0.,
        &mut filters.roll_attitude,
        DT_ATTITUDE,
    );

    // todo: What should this be ??
    pid.yaw = calc_pid_error(
        velocities_commanded.yaw,
        params.s_yaw,
        &pid.yaw,
        0., // todo
        0., // todo
        0.,
        &mut filters.yaw_attitude,
        DT_ATTITUDE,
    );

    // todo: What should this be ??
    pid.thrust = calc_pid_error(
        velocities_commanded.thrust,
        params.baro_alt_msl,
        &pid.thrust,
        0., // todo
        0., // todo
        0.,
        &mut filters.thrust,
        DT_ATTITUDE,
    );

    // Determine commanded pitch and roll positions, and z velocity,
    // based on our middle-layer PID.

    // todo: the actual modification ofn attitude is commented out for now (july 27 2022)
    // todo until we get attitude, rate, and various autopilot modes sorted out.
    // *attitude_commanded = CtrlInputs {
    //     pitch: pid.pitch.out(),
    //     roll: pid.roll.out(),
    //     yaw: pid.yaw.out(),
    //     thrust: pid.thrust.out(),
    // };
}

/// Run the attitude (mid) PID loop: This is used to determine angular velocities, based on commanded
/// attitude.
pub fn run_attitude_quad(
    params: &Params,
    ch_data: &ChannelData,
    input_map: &InputMap,
    attitudes_commanded: &mut CtrlInputs,
    rates_commanded: &mut CtrlInputs,
    pid: &mut PidGroup,
    filters: &mut PidDerivFilters,
    input_mode: &InputMode,
    autopilot_status: &AutopilotStatus,
    cfg: &UserCfg,
    coeffs: &CtrlCoeffGroup,
) {
    autopilot_status.apply_attitude_quad(params, attitudes_commanded, input_map, cfg.max_speed_ver);

    match input_mode {
        InputMode::Acro => {
            // (Acro mode has handled by the rates loop)

            // todo: If a rate axis is centered. command an attitude where that
            // todo position was left off. (quaternion)
        }

        InputMode::Attitude => {
            *attitudes_commanded = CtrlInputs {
                pitch: input_map.calc_pitch_angle(ch_data.pitch),
                roll: input_map.calc_roll_angle(ch_data.roll),
                yaw: input_map.calc_yaw_rate(ch_data.yaw),
                thrust: ch_data.throttle,
            };
        }
        InputMode::Command => {
            // todo: Impl
        }
    }

    pid.pitch = calc_pid_error(
        attitudes_commanded.pitch,
        params.s_pitch,
        &pid.pitch,
        coeffs.pitch.k_p_attitude,
        coeffs.pitch.k_i_attitude,
        coeffs.pitch.k_d_attitude,
        &mut filters.pitch_attitude,
        DT_ATTITUDE,
    );

    pid.roll = calc_pid_error(
        attitudes_commanded.roll,
        params.s_roll,
        &pid.roll,
        // coeffs,
        coeffs.roll.k_p_attitude,
        coeffs.roll.k_i_attitude,
        coeffs.roll.k_d_attitude,
        &mut filters.roll_attitude,
        DT_ATTITUDE,
    );

    // Note that for attitude mode, we ignore commanded yaw attitude, and treat it
    // as a rate.
    pid.yaw = calc_pid_error(
        attitudes_commanded.yaw,
        params.s_yaw,
        &pid.yaw,
        coeffs.yaw.k_p_attitude,
        coeffs.yaw.k_i_attitude,
        coeffs.yaw.k_s_attitude,
        &mut filters.yaw_attitude,
        DT_ATTITUDE,
    );

    // todo: Consider how you want to handle thrust/altitude.
    pid.thrust = calc_pid_error(
        attitudes_commanded.thrust,
        params.baro_alt_msl,
        &pid.thrust,
        coeffs.thrust.k_p_attitude,
        coeffs.thrust.k_i_attitude,
        coeffs.thrust.k_s_attitude,
        &mut filters.thrust,
        DT_ATTITUDE,
    );

    *rates_commanded = CtrlInputs {
        pitch: pid.pitch.out(),
        roll: pid.roll.out(),
        yaw: pid.yaw.out(),
        thrust: pid.thrust.out(),
    };
}

/// Run the rate (inner) PID loop: This is what directly affects motor output by commanding pitch, roll, and
/// yaw rates. Also affects thrust. These rates are determined either directly by acro inputs, or by the
/// attitude PID loop.
///
/// If acro, we get our inputs each IMU update; ie the inner loop. In other modes,
/// (or with certain autopilot flags enabled?) the inner loop is commanded by the mid loop
/// once per update cycle, eg to set commanded angular rates.
pub fn run_rate_quad(
    params: &Params,
    input_mode: InputMode,
    autopilot_status: &AutopilotStatus,
    ch_data: &ChannelData,
    rates_commanded: &mut CtrlInputs,
    pid: &mut PidGroup,
    filters: &mut PidDerivFilters,
    current_pwr: &mut crate::MotorPower,
    rotor_mapping: &RotorMapping,
    motor_timers: &mut MotorTimers,
    dma: &mut Dma<DMA1>,
    coeffs: &CtrlCoeffGroup,
    max_speed_ver: f32,
    input_map: &InputMap,
    arm_status: ArmStatus,
    dt: f32,
) {
    // If in Acro mode, use control data to determine rates commanded. Otherwise, use the
    // `rates_commanded` data passed in as an argument.
    match input_mode {
        InputMode::Acro => {
            // todo: Power interp not yet implemented.
            // let power_interp_inst = dsp_sys::arm_linear_interp_instance_f32 {
            //     nValues: 11,
            //     x1: 0.,
            //     xSpacing: 0.1,
            //     pYData: [
            //         // Idle power.
            //         0.02, // Make sure this matches the above.
            //         POWER_LUT[0],
            //         POWER_LUT[1],
            //         POWER_LUT[2],
            //         POWER_LUT[3],
            //         POWER_LUT[4],
            //         POWER_LUT[5],
            //         POWER_LUT[6],
            //         POWER_LUT[7],
            //         POWER_LUT[8],
            //         POWER_LUT[9],
            //     ]
            //     .as_mut_ptr(),
            // };

            // todo: If pitch or roll stick is neutral, hold that attitude (quaternion)

            // Note: We may not need to modify the `rates_commanded` resource in place here; we don't
            // use it upstream.
            // Map the manual input rates (eg -1. to +1. etc) to real units, eg randians/s.
            *rates_commanded = CtrlInputs {
                pitch: input_map.calc_pitch_rate(ch_data.pitch),
                roll: input_map.calc_roll_rate(ch_data.roll),
                yaw: input_map.calc_yaw_rate(ch_data.yaw),
                // todo: If you do a non-linear throttle-to-thrust map, put something like this back.
                // thrust: flight_ctrls::power_from_throttle(ch_data.throttle, &power_interp_inst),
                thrust: input_map.calc_manual_throttle(ch_data.throttle),
            };
        }
        _ => (),
    }

    let throttle = rates_commanded.thrust;

    let tpa_scaler = if throttle > TPA_BREAKPOINT {
        tpa_adjustment(throttle)
    } else {
        1.
    };

    pid.pitch = calc_pid_error(
        rates_commanded.pitch,
        params.v_pitch,
        &pid.pitch,
        coeffs.pitch.k_p_rate,
        coeffs.pitch.k_i_rate,
        coeffs.pitch.k_d_rate * tpa_scaler,
        &mut filters.pitch_rate,
        dt,
    );

    pid.roll = calc_pid_error(
        rates_commanded.roll,
        params.v_roll,
        &pid.roll,
        coeffs.roll.k_p_rate,
        coeffs.roll.k_i_rate,
        coeffs.roll.k_d_rate * tpa_scaler,
        &mut filters.roll_rate,
        dt,
    );

    pid.yaw = calc_pid_error(
        rates_commanded.yaw,
        params.v_yaw,
        &pid.yaw,
        coeffs.yaw.k_p_rate,
        coeffs.yaw.k_i_rate,
        coeffs.yaw.k_d_rate * tpa_scaler,
        &mut filters.yaw_rate,
        dt,
    );

    let pitch = pid.pitch.out();
    let roll = pid.roll.out();
    let yaw = pid.yaw.out();

    autopilot_status.apply_rate_quad(params, rates_commanded, max_speed_ver, pid, filters, coeffs, dt);

    flight_ctrls::quad::apply_controls(
        pitch,
        roll,
        yaw,
        throttle,
        current_pwr,
        rotor_mapping,
        motor_timers,
        arm_status,
        dma,
    );
}

pub fn run_rate_fixed_wing(
    params: &Params,
    input_mode: InputMode,
    autopilot_status: &AutopilotStatus,
    ch_data: &ChannelData,
    rates_commanded: &mut CtrlInputs,
    pid: &mut PidGroup,
    filters: &mut PidDerivFilters,
    control_posits: &mut ControlPositions,
    mapping: &ServoWingMapping,
    motor_timers: &mut MotorTimers,
    dma: &mut Dma<DMA1>,
    coeffs: &CtrlCoeffGroup,
    input_map: &InputMap,
    arm_status: ArmStatus,
    dt: f32,
) {
    match input_mode {
        InputMode::Acro => {
            // todo: Power interp not yet implemented.
            // let power_interp_inst = dsp_sys::arm_linear_interp_instance_f32 {
            //     nValues: 11,
            //     x1: 0.,
            //     xSpacing: 0.1,
            //     pYData: [
            //         // Idle power.
            //         0.02, // Make sure this matches the above.
            //         POWER_LUT[0],
            //         POWER_LUT[1],
            //         POWER_LUT[2],
            //         POWER_LUT[3],
            //         POWER_LUT[4],
            //         POWER_LUT[5],
            //         POWER_LUT[6],
            //         POWER_LUT[7],
            //         POWER_LUT[8],
            //         POWER_LUT[9],
            //     ]
            //     .as_mut_ptr(),
            // };

            // todo: It pitch or roll stick is neutral, hold that attitude (quaternion)

            // Note: We may not need to modify the `rates_commanded` resource in place here; we don't
            // use it upstream.
            // Map the manual input rates (eg -1. to +1. etc) to real units, eg radians/s.
            *rates_commanded = CtrlInputs {
                pitch: input_map.calc_pitch_rate(ch_data.pitch),
                roll: input_map.calc_roll_rate(ch_data.roll),
                yaw: input_map.calc_yaw_rate(ch_data.yaw),
                // todo: If you do a non-linear throttle-to-thrust map, put something like this back.
                // thrust: flight_ctrls::power_from_throttle(ch_data.throttle, &power_interp_inst),
                thrust: input_map.calc_manual_throttle(ch_data.throttle),
            };
        }
        _ => (),
    }

    pid.pitch = calc_pid_error(
        rates_commanded.pitch,
        params.v_pitch,
        &pid.pitch,
        coeffs.pitch.k_p_rate,
        coeffs.pitch.k_i_rate,
        coeffs.pitch.k_d_rate,
        &mut filters.pitch_rate,
        dt,
    );

    pid.roll = calc_pid_error(
        rates_commanded.roll,
        params.v_roll,
        &pid.roll,
        coeffs.roll.k_p_rate,
        coeffs.roll.k_i_rate,
        coeffs.roll.k_d_rate,
        &mut filters.roll_rate,
        dt,
    );

    pid.yaw = calc_pid_error(
        rates_commanded.yaw,
        params.v_yaw,
        &pid.yaw,
        coeffs.yaw.k_p_rate,
        coeffs.yaw.k_i_rate,
        coeffs.yaw.k_d_rate,
        &mut filters.yaw_rate,
        dt,
    );

    let pitch = pid.pitch.out();
    let roll = pid.roll.out();
    let yaw = pid.yaw.out();
    let throttle = rates_commanded.thrust;

    autopilot_status.apply_rate_fixed_wing(params, rates_commanded);

    flight_ctrls::flying_wing::apply_controls(
        pitch,
        roll,
        throttle,
        control_posits,
        mapping,
        motor_timers,
        arm_status,
        dma,
    );
}