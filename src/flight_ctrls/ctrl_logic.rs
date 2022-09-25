//! This module contains code for attitude-based controls. This includes sticks mapping
//! to attitude, and an internal attitude model with rate-like controls, where attitude is the target.

use crate::control_interface::ChannelData;

use super::{
    common::{CtrlMix, Params, RatesCommanded},
    filters::FlightCtrlFilters,
};

use lin_alg2::f32::{Quaternion, Vec3};

use num_traits::float::Float; // For sqrt.

use cfg_if::cfg_if;

// todo: YOu probably need filters.

cfg_if! {
    if #[cfg(feature = "quad")] {
        use super::{MotorPower, RotationDir};
    } else {
        use super::ControlPositions;
    }
}

const RIGHT: Vec3 = Vec3 {
    x: 1.,
    y: 0.,
    z: 0.,
};
const UP: Vec3 = Vec3 {
    x: 0.,
    y: 1.,
    z: 0.,
};
const FWD: Vec3 = Vec3 {
    x: 0.,
    y: 0.,
    z: 1.,
};

/// Control coefficients that affect the toleranaces and restrictions of the flight controls.
pub struct CtrlCoeffs {
    /// This field is used for a simple target angular velocity for a given angular distance
    /// from the target angle (eg pitch, roll, yaw). Units are (rad/s) / rad = 1/s
    /// Eg with value = 1, at tau/2 rad (max angle), we get target a rate of tau/2 rad/s. At 0 angle,
    /// we command 0 rad/s regardless of thi value. Higher values for this constant
    /// raise the rate for a given angular distance. This value is the time, at this
    /// rate, it takes to travel a whole revolution.
    /// todo: More complicated, non-linear model? Eg specified by a LUT.
    /// todo: Another possible approach is with a clipped value past a certain angle.
    /// todo: Consider different values for pitch, roll, and yaw.
    p_ω: f32,
    /// Time to correction is a coefficient that determines how quickly the angular
    /// velocity will be corrected to the target.
    /// Lower values mean more aggressive corrections.
    time_to_correction_p_ω: f32,
    time_to_correction_p_θ: f32,
    /// In rad/s^2. A higher value will allow for more aggressive corrections.
    max_ω_dot: f32,
}

// todo: Maybe a sep `CtrlCoeffs` struct for each axis.

impl Default for CtrlCoeffs {
    #[cfg(feature = "quad")]
    fn default() -> Self {
        Self {
            p_ω: 10.,
            time_to_correction_p_ω: 0.1, // todo?
            time_to_correction_p_θ: 0.5, // todo?
            max_ω_dot: 10.,         // todo: What should this be?
        }
    }

    #[cfg(feature = "fixed-wing")]
    fn default() -> Self {
        Self {
            p_ω: 10.,
            time_to_correction_p_ω: 0.1,
            time_to_correction_p_θ: 0.5,
            max_ω_dot: 10.,
        }
    }
}

// todo: COde shortner 3x.

// #[cfg(feature = "quad")]
/// Find the desired control setting on a single axis; loosely corresponds to a
/// commanded angular acceleration.
fn find_ctrl_setting(
    dθ: f32,
    ω: f32,
    ω_dot: f32,
    ctrl_cmd_prev: f32,
    // dt: f32,
    coeffs: &CtrlCoeffs,
    filters: &mut FlightCtrlFilters,
) -> f32 {
    // todo: Take RPM and/or time-to-spin up/down into account.
    // todo: It's likely the best plan is to set up RPM measurement, and create a model
    // todo based on that that relates speed change time to various regimes.

    // Compare the current (measured) angular velocities to what we need to apply this rotation.
    let ω_target = dθ * coeffs.p_ω;

    let dω = ω_target - ω;

    // todo: Evaluate how this will work. Should possibly take dθ into account.
    let time_to_correction = coeffs.time_to_correction_p_ω * dω.abs() + coeffs.time_to_correction_p_θ * dθ.abs();

    let mut ω_dot_target = dω / time_to_correction;
    if ω_dot_target > coeffs.max_ω_dot {
        ω_dot_target = coeffs.max_ω_dot;
    }

    // Calculate how, most recently, the control command is affecting angular accel.
    // A higher constant means a given command has a higher affect on angular accel.
    // todo: Track and/or lowpass effectiveness over recent history, at diff params.
    // todo: Once you have bidir dshot, use RPM instead of power.


    let ctrl_effectiveness = ω_dot / ctrl_cmd_prev;

    // Apply our lowpass.
    let ctrl_effectiveness = filters.apply(ctrl_effectiveness);



    // const EPS_1: f32 = 0.0001;

    // This distills to: (dω / time_to_correction) / (ω_dot / ctrl_cmd_prev) =
    // (dω / time_to_correction) x (ctrl_cmd_prev / ω_dot) =
    // (dω x ctrl_cmd_prev) / (time_to_correction x ω_dot)
    // Units: rad x cmd / (s * rad/s) = rad x cmd / rad = cmd
    // `cmd` is the unit we use for ctrl inputs. Not sure what (if any?) units it has.
    ω_dot_target / ctrl_effectiveness

    // Estimate the time it will take to arrive at our target attitude,
    // given the current angular velocity, and angular acceleration.
    // We numerically integrate, then solve for time.

    // todo: Take into account reduction in accel as velocity increases due to drag?

    // Integrate using angular rate, and angular accel:
    // θ(t) = θ_0 + ω_0 * t + ω_dot * t^2

    // wolfram alpha: `theta = h + v * t + a * Power[t,2] solve for t`
    // 1/(2*ω_dot) * (sqrt(-4 * 0. + 4ω_dot θ(t) + ω_0^2) +/- ω_0)

    // todo: Should you apply filtering to any of these terms?

    // todo: From tests, the second variant (b2) appears to work for test examples.
    // todo: Figure out when to use each
    // todo: Figure out what to do when it will never convege; ie it it's accelerating in the wrong
    // todo direction.
    // let time_to_tgt_att_pitch = if ang_accel_pitch.abs() < EPS_1 {
    //     Some(rot_pitch / params.v_pitch)
    // } else {
    //     let a = 2. * ang_accel_pitch;
    //
    //     let inner = 4. * ang_accel_pitch * rot_pitch + params.v_pitch.powi(2);
    //     if inner < 0. {
    //         // Will never reach target (without wrapping around).
    //         // todo: Use modulus TAU and allow going the wrong way?
    //         None
    //     } else {
    //         let b1 = inner.sqrt() + params.v_pitch;
    //         let b2 = inner.sqrt() - params.v_pitch;
    //
    //         let v1 = b2 / a;
    //         // todo: Is this the approach you want? Pick the positive one?
    //         // todo: Maybe choose the one with smaller abs? I think that indicates going
    //         // todo back in time, so probably not.
    //         if v1 > 0. {
    //             Some(v1)
    //         } else {
    //             Some(-(b1 / a))
    //         }
    //     }
    // };
    //
    // let target_time_to_tgt_att_pitch = rot_pitch * asdf;
    //
    // // For each channel, compare the previous control positions to the [rate? change in rate?)
    // // Then adjust the motor powers A/R.
    //
    // // todo OK, say it is like a pid...
    // let d_term_pitch = d_pitch_param * p_pitch;
    // let p_term_pitch = tgt_rate_chg_pitch * p_pitch;
    //
    // // Calculate how long it will take to reach the target pitch rate at the current rate
    // // of change.
    // let time_to_tgt_rate_pitch = tgt_rate_chg_pitch / d_term_pitch;
    //
    // // eg 5 m/s delta
    // //
    //
    // // if tgt_rate_chg_pitch <= rate_thresh {
    // //     // power_chg_pitch = 0.
    // // }
    //
    // let power_chg_pitch = if tgt_rate_chg_pitch > rate_thresh {
    //     // todo: YOu need to take rate changes into effect
    //     tgt_rate_chg_pitch * p_pitch
    // } else {
    //     0.
    // };
    //
    // let pitch_cmd = prev_power.pitch + power_chg_pitch;
    // let roll_cmd = prev_power.roll + power_chg_roll;
    // let yaw_cmd = prev_power.yaw + power_chg_yaw;
    //
    // let pitch_cmd = tgt_rate_chg_pitch * p_pitch;
    // let roll_cmd = tgt_rate_chg_roll * p_roll;
    // let yaw_cmd = tgt_rate_chg_yaw * p_yaw;
}

#[cfg(feature = "quad")]
pub fn motor_power_from_atts(
    target_attitude: Quaternion,
    current_attitude: Quaternion,
    throttle: f32,
    front_left_dir: RotationDir,
    // todo: Params is just for current angular rates. Maybe just pass those?
    params: &Params,
    params_prev: &Params,
    mix_prev: &CtrlMix,
    coeffs: &CtrlCoeffs,
    filters: &mut FlightCtrlFilters,
    dt: f32, // seconds
) -> (CtrlMix, MotorPower) {
    // todo: This fn and approach is a WIP!!

    // This is the rotation we need to cause to arrive at the target attitude.
    let rotation_cmd = target_attitude * current_attitude.inverse();
    // Split the rotation into 3 euler angles. We do this due to our controls acting primarily
    // along individual axes.
    let (rot_pitch, rot_roll, rot_yaw) = rotation_cmd.to_euler();

    let ang_accel_pitch = (params.v_pitch - params_prev.v_pitch) * dt;
    let ang_accel_roll = (params.v_roll - params_prev.v_roll) * dt;
    let ang_accel_yaw = (params.v_yaw - params_prev.v_yaw) * dt;

    let pitch = find_ctrl_setting(
        rot_pitch,
        params.v_pitch,
        ang_accel_pitch,
        mix_prev.pitch,
        // dt,
        coeffs,
        filters,
    );
    let roll = find_ctrl_setting(
        rot_roll,
        params.v_roll,
        ang_accel_roll,
        mix_prev.roll,
        // dt,
        coeffs,
        filters,
    );
    let yaw = find_ctrl_setting(
        rot_yaw,
        params.v_yaw,
        ang_accel_yaw,
        mix_prev.yaw,
        // dt,
        coeffs,
        filters,
    );

    let mix_new = CtrlMix {
        pitch,
        roll,
        yaw,
        throttle,
    };

    let power = MotorPower::from_cmds(&mix_new, front_left_dir);

    // Examine if our current control settings are appropriately effecting the change we want.
    (mix_new, power)
}

#[cfg(feature = "fixed-wing")]
/// Similar to the above fn on quads. Note that we do not handle yaw command using this. Yaw
/// is treated as coupled to pitch and roll, with yaw controls used to counter adverse-yaw.
pub fn control_posits_from_atts(
    target_attitude: Quaternion,
    current_attitude: Quaternion,
    throttle: f32,
    // todo: Params is just for current angular rates. Maybe just pass those?
    params: &Params,
    params_prev: &Params,
    mix_prev: &CtrlMix,
    coeffs: &CtrlCoeffs,
    filters: &mut FlightCtrlFilters,
    dt: f32, // seconds
) -> (CtrlMix, ControlPositions) {
    // todo: Modulate based on airspeed.

    let rotation_cmd = target_attitude * current_attitude.inverse();
    let (rot_pitch, rot_roll, _rot_yaw) = rotation_cmd.to_euler();

    let ang_accel_pitch = (params.v_pitch - params_prev.v_pitch) * dt;
    let ang_accel_roll = (params.v_roll - params_prev.v_roll) * dt;

    let pitch = find_ctrl_setting(
        rot_pitch,
        params.v_pitch,
        ang_accel_pitch,
        mix_prev.pitch,
        // dt,
        coeffs,
        filters,
    );
    let roll = find_ctrl_setting(
        rot_roll,
        params.v_roll,
        ang_accel_roll,
        mix_prev.roll,
        // dt,
        coeffs,
        filters,
    );

    let yaw = 0.; // todo?

    let mix_new = CtrlMix {
        pitch,
        roll,
        yaw,
        throttle,
    };

    let posits = ControlPositions::from_cmds(&mix_new);

    (mix_new, posits)
}

/// Modify our attitude commanded from rate-based user inputs. `ctrl_crates` are in radians/s, and `dt` is in s.
pub fn modify_att_target(orientation: Quaternion, rates: &RatesCommanded, dt: f32) -> Quaternion {
    // todo: Error handling on this?

    // Rotate our basis vecs using the orientation, such that control inputs are relative to the
    // current attitude.
    let right = orientation.rotate_vec(RIGHT);
    let fwd = orientation.rotate_vec(FWD);
    let up = orientation.rotate_vec(UP);

    let rotation_pitch = Quaternion::from_axis_angle(right, rates.pitch.unwrap() * dt);
    let rotation_roll = Quaternion::from_axis_angle(fwd, rates.roll.unwrap() * dt);
    let rotation_yaw = Quaternion::from_axis_angle(up, rates.yaw.unwrap() * dt);

    // todo: Order?
    rotation_yaw * rotation_roll * rotation_pitch * orientation
}

/// Calculate an attitude based on control input, in `attitude mode`.
pub fn from_controls(ch_data: &ChannelData) -> Quaternion {
    // todo: How do you deal with heading? That's a potential disadvantage of using a quaternion:
    // todo we can calculate pitch and roll, but not yaw.
    let rotation_pitch = Quaternion::from_axis_angle(RIGHT, ch_data.pitch);
    let rotation_roll = Quaternion::from_axis_angle(FWD, ch_data.roll);

    rotation_roll * rotation_pitch
}
