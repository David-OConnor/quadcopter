//! This module contains code related to the flight control PID loop. It can be thought of
//! as a sub-module for `flight_ctrls`.
//!
//! See the OneNote document for notes on how we handle the more complicated / cascaded control modes.

use core::f32::consts::TAU;

use stm32_hal2::{
    dma::Dma,
    pac::{DMA1, TIM2, TIM3},
    timer::Timer,
};

use cmsis_dsp_api as dsp_api;
use cmsis_dsp_sys as dsp_sys;

use crate::{
    flight_ctrls::{
        self, AltType, AutopilotStatus, CommandState, CtrlInputs, IirInstWrapper, InputMap,
        InputMode, Params, YAW_ASSIST_COEFF, YAW_ASSIST_MIN_SPEED, POWER_LUT,
    },
    UserCfg, DT_ATTITUDE,
};

// todo: What should these be?
const INTEGRATOR_CLAMP_MIN: f32 = -10.;
const INTEGRATOR_CLAMP_MAX: f32 = 10.;

static mut FILTER_STATE_MID_X: [f32; 4] = [0.; 4];
static mut FILTER_STATE_MID_Y: [f32; 4] = [0.; 4];
static mut FILTER_STATE_MID_YAW: [f32; 4] = [0.; 4];
static mut FILTER_STATE_MID_THRUST: [f32; 4] = [0.; 4];

static mut FILTER_STATE_INNER_X: [f32; 4] = [0.; 4];
static mut FILTER_STATE_INNER_Y: [f32; 4] = [0.; 4];
static mut FILTER_STATE_INNER_YAW: [f32; 4] = [0.; 4];
static mut FILTER_STATE_INNER_THRUST: [f32; 4] = [0.; 4];

/// Cutoff frequency for our PID lowpass frequency, in Hz
#[derive(Clone, Copy)]
enum LowpassCutoff {
    // todo: What values should these be?
    H500,
    H1k,
    H10k,
    H20k,
}

/// Coefficients and other configurable parameters for controls, for pich and roll.
/// Has several variants, due to coupling with horizontal (X and Y) movement.
pub struct CtrlCoeffsPR {
    // // These coefficients map desired change in flight parameters to rotor power change.
    // // pitch, roll, and yaw s are in power / radians
    k_p_rate: f32,
    k_i_rate: f32,
    k_d_rate: f32,

    k_p_attitude: f32,
    k_i_attitude: f32,
    k_d_attitude: f32,

    k_p_velocity: f32,
    k_i_velocity: f32,
    // k_d_velocity: f32,
    // Note that we don't use the D component for our velocity PID.

    pid_deriv_lowpass_cutoff_rate: LowpassCutoff,
    pid_deriv_lowpass_cutoff_attitude: LowpassCutoff,
}

impl Default for CtrlCoeffsPR {
    fn default() -> Self {
        Self {
            // pid for controlling pitch and roll from commanded horizontal position
            k_p_rate: 0.1,
            k_i_rate: 0.,
            k_d_rate: 0.,

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

/// Coefficients and other configurable parameters for yaw and thrust. This is separate from, and
/// simpler than the variant for pitch and roll, since it's not coupled to X and Y motion.
pub struct CtrlCoeffsYT {
    // PID for controlling yaw or thrust from an explicitly-commanded heading or altitude.
    k_p_s: f32,
    k_i_s: f32,
    k_d_s: f32,

    // PID for controlling yaw or thrust from a velocity directly applied to them. (Eg Acro and attitude)
    k_p_v: f32,
    k_i_v: f32,
    k_d_v: f32,

    pid_deriv_lowpass_cutoff: LowpassCutoff,
}

impl Default for CtrlCoeffsYT {
    fn default() -> Self {
        Self {
            k_p_s: 0.1,
            k_i_s: 0.0,
            k_d_s: 0.0,

            k_p_v: 45.,
            k_i_v: 80.0,
            k_d_v: 0.0,
            pid_deriv_lowpass_cutoff: LowpassCutoff::H1k,
        }
    }
}

pub struct CtrlCoeffGroup {
    pub pitch: CtrlCoeffsPR,
    pub roll: CtrlCoeffsPR,
    pub yaw: CtrlCoeffsYT,
    pub thrust: CtrlCoeffsYT,
    // These coefficients are our rotor gains.
    // todo: Think about how to handle these, and how to map them to the aircraft data struct,
    // todo, and the input range.
    // pub gain_pitch: f32,
    // pub gain_roll: f32,
    // pub gain_yaw: f32,
    // pub gain_thrust: f32,
}

impl Default for CtrlCoeffGroup {
    /// These starting values are Betaflight defaults.
    fn default() -> Self {
        Self {
            pitch: Default::default(),
            roll: CtrlCoeffsPR {
                k_p_attitude: 45.,
                k_i_attitude: 80.,
                k_d_attitude: 30.,
                ..Default::default()
            },
            yaw: Default::default(),
            thrust: CtrlCoeffsYT {
                k_p_s: 0.1,
                k_i_s: 0.0,
                k_d_s: 0.0,

                k_p_v: 45.,
                k_i_v: 80.0,
                k_d_v: 0.0,
                pid_deriv_lowpass_cutoff: LowpassCutoff::H1k,
            },
        }
    }
}

#[derive(Default)]
pub struct PidGroup {
    pitch: PidState,
    roll: PidState,
    yaw: PidState,
    thrust: PidState,
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
    pub fn anti_windup_clamp(&mut self) {
        if self.i > INTEGRATOR_CLAMP_MAX {
            self.i = INTEGRATOR_CLAMP_MAX;
        } else if self.i < INTEGRATOR_CLAMP_MIN {
            self.i = INTEGRATOR_CLAMP_MIN;
        }
    }

    pub fn out(&self) -> f32 {
        self.p + self.i + self.d
    }
}

/// Store lowpass IIR filter instances, for use with the deriv terms of our PID loop.
pub struct PidDerivFilters {
    pub mid_x: IirInstWrapper,
    pub mid_y: IirInstWrapper,
    pub mid_yaw: IirInstWrapper,
    pub mid_thrust: IirInstWrapper,

    pub inner_x: IirInstWrapper,
    pub inner_y: IirInstWrapper,
    pub inner_yaw: IirInstWrapper,
    pub inner_thrust: IirInstWrapper,
}

impl PidDerivFilters {
    pub fn new() -> Self {
        // todo: Instead of initing empty here, maybe we use the proper constructor?
        // todo: Maybe you can use this cleaner approach for Headphones too?

        // todo: Put useful params here.
        // filter_ = signal.iirfilter(1, 60, btype="lowpass", ftype="bessel", output="sos", fs=32_000)
        // coeffs = []
        // for row in filter_:
        //     coeffs.extend([row[0] / row[3], row[1] / row[3], row[2] / row[3], -row[4] / row[3], -row[5] / row[3]])

        let coeffs = [
            0.00585605892206321,
            0.00585605892206321,
            0.0,
            0.9882878821558736,
            -0.0,
        ];

        let mut result = Self {
            mid_x: IirInstWrapper {
                inner: dsp_api::biquad_cascade_df1_init_empty_f32(),
            },
            mid_y: IirInstWrapper {
                inner: dsp_api::biquad_cascade_df1_init_empty_f32(),
            },
            mid_yaw: IirInstWrapper {
                inner: dsp_api::biquad_cascade_df1_init_empty_f32(),
            },
            mid_thrust: IirInstWrapper {
                inner: dsp_api::biquad_cascade_df1_init_empty_f32(),
            },

            inner_x: IirInstWrapper {
                inner: dsp_api::biquad_cascade_df1_init_empty_f32(),
            },
            inner_y: IirInstWrapper {
                inner: dsp_api::biquad_cascade_df1_init_empty_f32(),
            },
            inner_yaw: IirInstWrapper {
                inner: dsp_api::biquad_cascade_df1_init_empty_f32(),
            },
            inner_thrust: IirInstWrapper {
                inner: dsp_api::biquad_cascade_df1_init_empty_f32(),
            },
        };

        unsafe {
            // todo: Re-initialize fn?
            dsp_api::biquad_cascade_df1_init_f32(
                &mut result.mid_x.inner,
                &coeffs,
                &mut FILTER_STATE_INNER_X,
            );
            dsp_api::biquad_cascade_df1_init_f32(
                &mut result.mid_y.inner,
                &coeffs,
                &mut FILTER_STATE_INNER_Y,
            );
            dsp_api::biquad_cascade_df1_init_f32(
                &mut result.mid_yaw.inner,
                &coeffs,
                &mut FILTER_STATE_INNER_YAW,
            );
            dsp_api::biquad_cascade_df1_init_f32(
                &mut result.mid_thrust.inner,
                &coeffs,
                &mut FILTER_STATE_INNER_THRUST,
            );

            dsp_api::biquad_cascade_df1_init_f32(
                &mut result.inner_x.inner,
                &coeffs,
                &mut FILTER_STATE_INNER_X,
            );
            dsp_api::biquad_cascade_df1_init_f32(
                &mut result.inner_y.inner,
                &coeffs,
                &mut FILTER_STATE_INNER_Y,
            );
            dsp_api::biquad_cascade_df1_init_f32(
                &mut result.inner_yaw.inner,
                &coeffs,
                &mut FILTER_STATE_INNER_YAW,
            );
            dsp_api::biquad_cascade_df1_init_f32(
                &mut result.inner_thrust.inner,
                &coeffs,
                &mut FILTER_STATE_INNER_THRUST,
            );
        }

        result
    }
}

/// Calculate the PID error given flight parameters, and a flight
/// command.
/// Example: https://github.com/pms67/PID/blob/master/PID.c
fn calc_pid_error(
    set_pt: f32,
    measurement: f32,
    prev_pid: &PidState,
    // coeffs: CtrlCoeffsGroup,
    k_p: f32,
    k_i: f32,
    k_d: f32,
    filter: &mut IirInstWrapper,
    // This `dt` is dynamic, since we don't necessarily run this function at a fixed interval.
    dt: f32,
) -> PidState {
    // Find appropriate control inputs using PID control.

    let error = set_pt - measurement;

    // todo: Determine if you want a deriv component at all; it's apparently not commonly used.

    // todo: Minor optomization: Store the constant terms once, and recall instead of calcing
    // todo them each time (eg the parts with DT, 2., tau.
    // https://www.youtube.com/watch?v=zOByx3Izf5U
    let error_p = k_p * error;
    let error_i = k_i * dt / 2. * (error + prev_pid.e) + prev_pid.i;
    // Derivative on measurement vice error, to avoid derivative kick.
    let error_d_prefilt = k_d * (measurement - prev_pid.measurement) / dt;

    // todo: Avoid this DRY with a method on `filter`
    // let mut error_d_v_pitch = [0.];
    let mut error_d = [0.];

    dsp_api::biquad_cascade_df1_f32(&mut filter.inner, &[error_d_prefilt], &mut error_d, 1);

    let mut error = PidState {
        measurement,
        e: error,
        p: error_p,
        i: error_i,
        d: error_d[0],
    };

    error.anti_windup_clamp();

    error
}

/// Run the velocity (outer) PID Loop: This is used to determine attitude, eg based on commanded velocity
/// or position.
pub fn run_velocity(
    params: &Params,
    inputs: &CtrlInputs,
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
        let dist_v = alt_msl_commanded - params.s_z_msl;

        // `enroute_speed_ver` returns a velocity of the appropriate sine for above vs below.
        let thrust = flight_ctrls::enroute_speed_ver(dist_v, cfg.max_speed_ver, params.s_z_agl);

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
            AltType::Msl => alt_commanded - params.s_z_msl,
            AltType::Agl => alt_commanded - params.s_z_agl,
        };
        // `enroute_speed_ver` returns a velocity of the appropriate sine for above vs below.
        velocities_commanded.thrust =
            flight_ctrls::enroute_speed_ver(dist, cfg.max_speed_ver, params.s_z_agl);
    }

    match input_mode {
        InputMode::Acro => (),
        InputMode::Attitude => (),
        InputMode::Command => {
            // todo: Impl
            // match autopilot_mode {
            if autopilot_status.takeoff {
                // AutopilotMode::Takeoff => {
                *velocities_commanded = CtrlInputs {
                    pitch: 0.,
                    roll: 0.,
                    yaw: 0.,
                    thrust: flight_ctrls::takeoff_speed(params.s_z_agl, cfg.max_speed_ver),
                };
            }
            // AutopilotMode::Land => {
            else if autopilot_status.land {
                *velocities_commanded = CtrlInputs {
                    pitch: 0.,
                    roll: 0.,
                    yaw: 0.,
                    thrust: flight_ctrls::landing_speed(params.s_z_agl, cfg.max_speed_ver),
                };
            }
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
    if inputs.pitch > eps1 || inputs.roll > eps1 {
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
        &mut filters.mid_y,
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
        &mut filters.mid_x,
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
        &mut filters.mid_thrust,
        DT_ATTITUDE,
    );

    // todo: What should this be ??
    pid.thrust = calc_pid_error(
        velocities_commanded.thrust,
        params.s_z_msl,
        &pid.thrust,
        0., // todo
        0., // todo
        0.,
        &mut filters.mid_thrust,
        DT_ATTITUDE,
    );

    // Determine commanded pitch and roll positions, and z velocity,
    // based on our middle-layer PID.

    *attitude_commanded = CtrlInputs {
        pitch: pid.pitch.out(),
        roll: pid.roll.out(),
        yaw: pid.yaw.out(),
        thrust: pid.thrust.out(),
    };
}

/// Run the attitude (mid) PID loop: This is used to determine angular velocities, based on commanded
/// attitude.
pub fn run_attitude(
    params: &Params,
    inputs: &CtrlInputs,
    input_map: &InputMap,
    attitudes_commanded: &mut CtrlInputs,
    rates_commanded: &mut CtrlInputs,
    pid: &mut PidGroup,
    filters: &mut PidDerivFilters,
    input_mode: &InputMode,
    autopilot_status: &AutopilotStatus,
    cfg: &UserCfg,
    commands: &mut CommandState,
    coeffs: &CtrlCoeffGroup,
) {

    // todo: Come back to these autopilot modes.
    // Initiate a recovery, regardless of control mode.
    // todo: Set commanded alt to current alt.
    if let Some(alt_msl_commanded) = autopilot_status.recover {
        let dist_v = alt_msl_commanded - params.s_z_msl;

        // `enroute_speed_ver` returns a velocity of the appropriate sine for above vs below.
        let thrust = flight_ctrls::enroute_speed_ver(dist_v, cfg.max_speed_ver, params.s_z_agl);

        // todo: DRY from alt_hold autopilot code.

        // todo: Figure out exactly what you need to pass for the autopilot modes to inner_flt_cmd
        // todo while in acro mode.
        *attitudes_commanded = CtrlInputs {
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
            AltType::Msl => alt_commanded - params.s_z_msl,
            AltType::Agl => alt_commanded - params.s_z_agl,
        };
        // `enroute_speed_ver` returns a velocity of the appropriate sine for above vs below.
        attitudes_commanded.thrust =
            flight_ctrls::enroute_speed_ver(dist, cfg.max_speed_ver, params.s_z_agl);
    }

    match input_mode {
        InputMode::Acro => {
            // (Acro mode has handled by the rates loop)
        }

        InputMode::Attitude => {
            *attitudes_commanded = CtrlInputs {
                pitch: input_map.calc_pitch_angle(inputs.pitch),
                roll: input_map.calc_roll_angle(inputs.roll),
                yaw: input_map.calc_yaw_rate(inputs.yaw),
                thrust: inputs.thrust,
            };
        }
        InputMode::Command => {
            // todo: Impl
            // match autopilot_mode {
            if autopilot_status.takeoff {
                // AutopilotMode::Takeoff => {
                *attitudes_commanded = CtrlInputs {
                    pitch: 0.,
                    roll: 0.,
                    yaw: 0.,
                    thrust: flight_ctrls::takeoff_speed(params.s_z_agl, cfg.max_speed_ver),
                };
            }
            // AutopilotMode::Land => {
            else if autopilot_status.land {
                *attitudes_commanded = CtrlInputs {
                    pitch: 0.,
                    roll: 0.,
                    yaw: 0.,
                    thrust: flight_ctrls::landing_speed(params.s_z_agl, cfg.max_speed_ver),
                };
            }
        }
    }

    pid.pitch = calc_pid_error(
        attitudes_commanded.pitch,
        params.s_pitch,
        &pid.pitch,
        coeffs.pitch.k_p_attitude,
        coeffs.pitch.k_i_attitude,
        coeffs.pitch.k_d_attitude,
        &mut filters.mid_y,
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
        &mut filters.mid_x,
        DT_ATTITUDE,
    );

    // Note that for attitude mode, we ignore commanded yaw attitude, and treat it
    // as a rate.
    pid.yaw = calc_pid_error(
        attitudes_commanded.yaw,
        params.s_yaw,
        &pid.yaw,
        coeffs.yaw.k_p_s,
        coeffs.yaw.k_i_s,
        coeffs.yaw.k_d_s,
        &mut filters.mid_yaw,
        DT_ATTITUDE,
    );

    // todo: Consider how you want to handle thrust/altitude.
    pid.thrust = calc_pid_error(
        attitudes_commanded.thrust,
        params.s_z_msl,
        &pid.thrust,
        coeffs.thrust.k_p_s,
        coeffs.thrust.k_i_s,
        coeffs.thrust.k_d_s,
        &mut filters.mid_thrust,
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
pub fn run_rate(
    params: &Params,
    input_mode: InputMode,
    autopilot_status: &AutopilotStatus,
    cfg: &UserCfg,
    manual_inputs: &mut CtrlInputs,
    rates_commanded: &mut CtrlInputs,
    pid: &mut PidGroup,
    filters: &mut PidDerivFilters,
    current_pwr: &mut crate::RotorPower,
    rotor_timer_a: &mut Timer<TIM2>,
    rotor_timer_b: &mut Timer<TIM3>,
    dma: &mut Dma<DMA1>,
    coeffs: &CtrlCoeffGroup,
    max_speed_ver: f32,
    input_map: &InputMap,
    dt: f32,
) {
    match input_mode {
        InputMode::Acro => {
             let power_interp_inst = dsp_sys::arm_linear_interp_instance_f32 {
                nValues: 11,
                x1: 0.,
                xSpacing: 0.1,
                pYData: [
                    // Idle power.
                    0.02, // Make sure this matches the above.
                    POWER_LUT[0],
                    POWER_LUT[1],
                    POWER_LUT[2],
                    POWER_LUT[3],
                    POWER_LUT[4],
                    POWER_LUT[5],
                    POWER_LUT[6],
                    POWER_LUT[7],
                    POWER_LUT[8],
                    POWER_LUT[9],
                ].as_mut_ptr()
            };


            *rates_commanded = CtrlInputs {
                pitch: input_map.calc_pitch_rate(manual_inputs.pitch),
                roll: input_map.calc_roll_rate(manual_inputs.roll),
                yaw: input_map.calc_yaw_rate(manual_inputs.yaw),
                thrust: flight_ctrls::power_from_throttle(manual_inputs.thrust, &power_interp_inst),
                // thrust: flight_ctrls::power_from_throttle(manual_inputs.thrust, &cfg.power_interp_inst),
            };

            // todo: Come back to these!
            if let Some((alt_type, alt_commanded)) = autopilot_status.alt_hold {
                // todo: In this mode, consider having the left stick being in the middle ~50% of the range mean
                // todo hold alt, and the upper and lower 25% meaning increase set point and decrease set
                // todo point respectively.
                // Set a vertical velocity for the inner loop to maintain, based on distance
                let dist = match alt_type {
                    AltType::Msl => alt_commanded - params.s_z_msl,
                    AltType::Agl => alt_commanded - params.s_z_agl,
                };
                // `enroute_speed_ver` returns a velocity of the appropriate sine for above vs below.
                rates_commanded.thrust =
                    flight_ctrls::enroute_speed_ver(dist, max_speed_ver, params.s_z_agl);
            }

            if autopilot_status.yaw_assist {
                // Blend manual inputs with the autocorrection factor. If there are no manual inputs,
                // this autopilot mode should neutralize all sideslip.
                let hor_dir = 0.; // radians
                let hor_speed = 0.; // m/s

                let yaw_correction_factor = ((hor_dir - params.s_yaw) / TAU) * YAW_ASSIST_COEFF;

                // todo: Impl for Attitude mode too? Or is it not appropriate there?
                if hor_speed > YAW_ASSIST_MIN_SPEED {
                    rates_commanded.yaw += yaw_correction_factor;
                }
            } else if autopilot_status.roll_assist {
                // todo!
                let hor_dir = 0.; // radians
                let hor_speed = 0.; // m/s

                let roll_correction_factor = (-(hor_dir - params.s_yaw) / TAU) * YAW_ASSIST_COEFF;

                // todo: Impl for Attitude mode too?
                if hor_speed > YAW_ASSIST_MIN_SPEED {
                    rates_commanded.yaw += roll_correction_factor;
                }
            }
        }
        _ => (),
    }

    // Map the manual input rates (eg -1. to +1. etc) to real units, eg randians/s.
    pid.pitch = calc_pid_error(
        input_map.calc_pitch_rate(rates_commanded.pitch),
        params.v_pitch,
        &pid.pitch,
        coeffs.pitch.k_p_rate,
        coeffs.pitch.k_i_rate,
        coeffs.pitch.k_d_rate,
        &mut filters.inner_y,
        dt,
    );

    pid.roll = calc_pid_error(
        input_map.calc_roll_rate(rates_commanded.roll),
        params.v_roll,
        &pid.roll,
        coeffs.roll.k_p_rate,
        coeffs.roll.k_i_rate,
        coeffs.roll.k_d_rate,
        &mut filters.inner_x,
        dt,
    );

    pid.yaw = calc_pid_error(
        input_map.calc_yaw_rate(rates_commanded.yaw),
        params.v_yaw,
        &pid.yaw,
        coeffs.yaw.k_p_v,
        coeffs.yaw.k_i_v,
        coeffs.yaw.k_d_v,
        &mut filters.inner_yaw,
        dt,
    );

    // Adjust gains to map control range and pid out in radians/s to the -1. to 1 rates used by the motor
    // control logic.
    let pitch = input_map.calc_pitch_rate_pwr(pid.pitch.out());
    let roll = input_map.calc_roll_rate_pwr(pid.roll.out());
    let yaw = input_map.calc_yaw_rate_pwr(pid.yaw.out());

    // todo temp, at least for Command mode.
    let throttle = match input_mode {
        InputMode::Acro => rates_commanded.thrust,
        InputMode::Attitude => manual_inputs.thrust,
        InputMode::Command => rates_commanded.thrust,
    };

    flight_ctrls::apply_controls(
        pitch,
        roll,
        yaw,
        throttle,
        current_pwr,
        rotor_timer_a,
        rotor_timer_b,
        dma,
    );
}
