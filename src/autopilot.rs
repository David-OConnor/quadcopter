//! This module contains code related to various autopilot modes.

use core::f32::consts::TAU;

use num_traits::float::Float;

use crate::{
    flight_ctrls::{
        self,
        common::{AltType, CtrlInputs, InputMap, Params},
    },
    pid::{self, CtrlCoeffGroup, PidDerivFilters, PidGroup},
    ppks::Location,
    state::OptionalSensorStatus,
    DT_ATTITUDE,
};

use cfg_if::cfg_if;

cfg_if! {
    if #[cfg(feature = "fixed-wing")] {
    } else {
        use crate::flight_ctrls::{InputMode, POWER_LUT, YAW_ASSIST_COEFF, YAW_ASSIST_MIN_SPEED};
    }
}

// todo: FOr various autopilot modes, check if variou sensors are connected like GPS, TOF, and MAG!

use cmsis_dsp_sys::{arm_cos_f32, arm_sin_f32};

const R: f32 = 6_371_000.; // Earth's radius in meters. (ellipsoid?)

// Highest bank to use in all autopilot modes.
const MAX_BANK: f32 = TAU / 6.;

// Tolerances we use when setting up a glideslope for landing. Compaerd to the landing structs,
// these are independent of the specific landing spot and aircraft.

// todo: Evaluate if you want `START` in these names, and in how you use them.

// The aircraft's heading must be within this range of the landing heading to initiate the descent.
// (Also must have the minimum ground track, as set in the struct.)
const GLIDESLOPE_START_HEADING_TOLERANCE: f32 = 0.25; // ranians. (0.17 = ~10°)

// The angle distance from landing point to aircraft, and landing point to a point abeam
// the aircraft on glideslope
const GLIDESLOPE_START_LATERAL_TOLERANCE: f32 = 0.30; // radians.

// Orbit heading difference in radians from heading on nearest point on obrit track.
const ORBIT_START_HEADING_TOLERANCE: f32 = 0.40; // radians

// Orbit lateral tolerance is in meters. Aircraft dist to nearest point on orbit track
const ORBIT_START_LATERAL_TOLERANCE: f32 = 10.;

pub const ORBIT_DEFAULT_RADIUS: f32 = 20.; // meters.
pub const ORBIT_DEFAULT_GROUNDSPEED: f32 = 10.; // m/s

fn cos(v: f32) -> f32 {
    unsafe { arm_cos_f32(v) }
}

fn sin(v: f32) -> f32 {
    unsafe { arm_sin_f32(v) }
}

/// Calculate the great circle bearing to fly to arrive at a point, given the aircraft's current position.
/// Params and output are in radians. Does not take into account turn radius.
/// https://www.movable-type.co.uk/scripts/latlong.html
/// θ = atan2( sin Δλ ⋅ cos φ2 , cos φ1 ⋅ sin φ2 − sin φ1 ⋅ cos φ2 ⋅ cos Δλ )
fn find_bearing(target: (f32, f32), aircraft: (f32, f32)) -> f32 {
    let y = sin(target.1 - aircraft.1) * cos(target.0);
    let x = cos(aircraft.0) * sin(target.0)
        - sin(aircraft.0) * cos(target.0) * cos(target.1 - aircraft.1);
    y.atan2(x) % TAU
}

/// Calculate the distance between two points, in meters.
/// Params are in radians. Uses the 'haversine' formula
/// https://www.movable-type.co.uk/scripts/latlong.html
/// a = sin²(Δφ/2) + cos φ1 ⋅ cos φ2 ⋅ sin²(Δλ/2)
/// c = 2 ⋅ atan2( √a, √(1−a) )
/// d = R ⋅ c
#[allow(non_snake_case)]
fn find_distance(target: (f32, f32), aircraft: (f32, f32)) -> f32 {
    // todo: LatLon struct with named fields.

    let φ1 = aircraft.0; // φ, λ in radians
    let φ2 = target.0;
    let Δφ = target.0 - aircraft.0;
    let Δλ = target.1 - aircraft.1;

    let a = sin(Δφ / 2.) * sin(Δφ / 2.) + cos(φ1) * cos(φ2) * sin(Δλ / 2.) * sin(Δλ / 2.);

    let c = 2. * a.sqrt().atan2((1. - a).sqrt());

    R * c
}

#[cfg(feature = "fixed-wing")]
#[derive(Clone, Copy)]
pub enum OrbitShape {
    Circular,
    Racetrack,
}

#[cfg(feature = "fixed-wing")]
impl Default for OrbitShape {
    fn default() -> Self {
        OrbitShape::Circular
    }
}

#[cfg(feature = "fixed-wing")]
#[derive(Clone, Copy)]
/// Direction from a top-down perspective.
pub enum OrbitDirection {
    Clockwise,
    CounterClockwise,
}

#[cfg(feature = "fixed-wing")]
impl Default for OrbitDirection {
    fn default() -> Self {
        OrbitDirection::Clockwise
    }
}

#[cfg(feature = "fixed-wing")]
/// Represents an autopilot orbit, centered around a point. The point may remain stationary, or
/// move over time.
pub struct Orbit {
    pub shape: OrbitShape,
    pub center_lat: f32,   // radians
    pub center_lon: f32,   // radians
    pub radius: f32,       // m
    pub ground_speed: f32, // m/s
    pub direction: OrbitDirection,
}

#[cfg(feature = "quad")]
#[derive(Default)]
/// A vertical descent.
pub struct LandingCfg {
    // todo: Could also land at an angle.
    pub descent_starting_alt_msl: f32, // altitude to start the descent in QFE msl.
    pub descent_speed: f32,            // m/s
    pub touchdown_point: Location,
}

#[cfg(feature = "fixed-wing")]
#[derive(Default)]
pub struct LandingCfg {
    /// Radians magnetic.
    pub heading: f32,
    /// radians, down from level
    pub glideslope: f32,
    /// Touchdown location, ideally with GPS (requirement?)
    pub touchdown_point: Location,
    /// Groundspeed in m/s
    /// todo: Remove ground_speed in favor of AOA once you figure out how to measure AOA.
    pub ground_speed: f32,
    /// Angle of attack in radians
    /// todo: Include AOA later once you figure out how to measure it.
    pub angle_of_attack: f32,
    /// Altitude to start the flare in AGL. Requires TOF sensor or similar.
    pub flare_alt_agl: f32,
    /// Minimum ground track distance in meters the craft must fly while aligned on the heading
    pub min_ground_track: f32,
}

/// Categories of control mode, in regards to which parameters are held fixed.
/// Note that some settings are mutually exclusive.
#[derive(Default)]
pub struct AutopilotStatus {
    /// Altitude is fixed. (MSL or AGL)
    pub alt_hold: Option<(AltType, f32)>,
    /// Heading is fixed.
    pub hdg_hold: Option<f32>,
    // todo: Airspeed hold
    /// Automatically adjust raw to zero out slip. Quad only.
    pub yaw_assist: bool,
    /// Automatically adjust roll (rate? angle?) to zero out slip, ie based on rudder inputs.
    /// Don't enable both yaw assist and roll assist at the same time. Quad only.
    pub roll_assist: bool,
    /// Continuously fly towards a path. Note that `pitch` and `yaw` for the
    /// parameters here correspond to the flight path; not attitude.
    pub velocity_vector: Option<(f32, f32)>, // pitch, yaw
    /// Fly direct to a point
    pub direct_to_point: Option<Location>,
    /// The aircraft will fly a fixed profile between sequence points
    pub sequence: bool,
    /// Terrain following mode. Similar to TF radar in a jet. Require a forward-pointing sensor.
    /// todo: Add a forward (or angled) TOF sensor, identical to the downward-facing one?
    pub terrain_following: Option<f32>, // AGL to hold
    /// Take off automatically
    pub takeoff: bool, // todo: takeoff cfg struct[s].
    /// Land automatically
    pub land: Option<LandingCfg>,
    /// Recover to stable, altitude-holding flight. Generally initiated by a "panic button"-style
    /// switch activation
    pub recover: Option<f32>, // value is MSL alt to hold, eg our alt at time of command.
    #[cfg(feature = "quad")]
    /// Maintain a geographic position and altitude
    pub loiter: Option<Location>,
    #[cfg(feature = "fixed-wing")]
    /// Orbit over a point on the ground
    pub orbit: Option<Orbit>,
}

// todo: Here or PID: If you set something like throttle to some or none via an AP mode etc,
// todo make sure you set it back to none A/R.

impl AutopilotStatus {
    #[cfg(feature = "quad")]
    pub fn apply(
        &self,
        params: &Params,
        attitudes_commanded: &mut CtrlInputs,
        rates_commanded: &mut CtrlInputs,
        pid: &mut PidGroup,
        filters: &mut PidDerivFilters,
        coeffs: &CtrlCoeffGroup,
        input_map: &InputMap,
        max_speed_ver: f32,
        optional_sensors: &OptionalSensorStatus,
    ) {
        // We use if/else logic on these to indicate they're mutually-exlusive. Modes listed first
        // take precedent.

        // todo: sensors check for this fn, and for here and fixed.
        // todo sensor check for alt hold agl

        // todo: add hdg hold here and fixed

        // If in acro or attitude mode, we can adjust the throttle setting to maintain a fixed altitude,
        // either MSL or AGL.
        if self.takeoff {
            // *attitudes_commanded = CtrlInputs {
            //     pitch: Some(0.),
            //     roll: Some(0.),
            //     yaw: Some(0.),
            //     thrust: Some(flight_ctrls::quad::takeoff_speed(params.tof_alt, max_speed_ver)),
            // };
        } else if let Some(ldg_cfg) = &self.land {
            if optional_sensors.gps_connected {}
        } else if let Some(pt) = &self.direct_to_point {
            if optional_sensors.gps_connected {
                let target_heading = find_bearing((params.lat, params.lon), (pt.lat, pt.lon));

                attitudes_commanded.yaw = Some(target_heading);
            }
        } else if let Some(pt) = &self.loiter {
            if optional_sensors.gps_connected {
                // todo
            }
        }

        if self.alt_hold.is_none()
            && !self.takeoff
            && self.land.is_none()
            && self.direct_to_point.is_none()
        {
            attitudes_commanded.thrust = None;
        }

        if self.alt_hold.is_some() && !self.takeoff && self.land.is_none() {
            let (alt_type, alt_commanded) = self.alt_hold.unwrap();
            if !(alt_type == AltType::Agl && !optional_sensors.tof_connected) {
                // Set a vertical velocity for the inner loop to maintain, based on distance
                let dist = match alt_type {
                    AltType::Msl => alt_commanded - params.baro_alt_msl,
                    AltType::Agl => alt_commanded - params.tof_alt.unwrap_or(0.),
                };

                // todo: Instead of a PID, consider something simpler.
                pid.thrust = pid::calc_pid_error(
                    // If just entering this mode, default to 0. throttle as a starting point.
                    attitudes_commanded.thrust.unwrap_or(0.),
                    dist,
                    &pid.thrust,
                    coeffs.thrust.k_p_attitude,
                    coeffs.thrust.k_i_attitude,
                    coeffs.thrust.k_d_attitude,
                    &mut filters.thrust,
                    DT_ATTITUDE,
                );

                // Note that thrust is set using the rate loop.
                rates_commanded.thrust = Some(pid.thrust.out());
            }
        }
    }

    #[cfg(feature = "fixed-wing")]
    pub fn apply(
        &self,
        params: &Params,
        attitudes_commanded: &mut CtrlInputs,
        rates_commanded: &mut CtrlInputs,
        pid_attitude: &mut PidGroup,
        filters: &mut PidDerivFilters,
        coeffs: &CtrlCoeffGroup,
        optional_sensors: &OptionalSensorStatus,
        // input_map: &InputMap,
        // max_speed_ver: f32,
    ) {
        if self.takeoff {
            // *attitudes_commanded = CtrlInputs {
            //     pitch: Some(0.),
            //     roll: Some(0.),
            //     yaw: Some(0.),
            //     thrust: Some(flight_ctrls::quad::takeoff_speed(params.tof_alt, max_speed_ver)),
            // };
        } else if let Some(ldg_cfg) = &self.land {
            if optional_sensors.gps_connected {
                let dist_to_touchdown = find_distance(
                    (ldg_cfg.touchdown_point.lat, ldg_cfg.touchdown_point.lon),
                    (params.lat, params.lon),
                );

                let heading_diff = 0.; // todo

                let established = dist_to_touchdown < ORBIT_START_LATERAL_TOLERANCE
                    && heading_diff < ORBIT_START_HEADING_TOLERANCE;
            }
            // todo: DRY between quad and FC here, although the diff is power vs pitch.
        } else if let Some(orbit) = &self.orbit {
            if optional_sensors.gps_connected {
                // todo: You'll get a smoother entry if you initially calculate, and fly to a point on the radius
                // todo on a heading similar to your own angle to it. For now, fly directly to the point for
                // todo simpler logic and good-enough.

                let dist_to_center = find_distance(
                    (orbit.center_lat, orbit.center_lon),
                    (params.lat, params.lon),
                );

                let heading_diff = 0.; // todo

                let established = dist_to_center < ORBIT_START_LATERAL_TOLERANCE
                    && heading_diff < ORBIT_START_HEADING_TOLERANCE;

                if !established {
                    // If we're not established and outside the radius...
                    if dist_to_center > orbit.radius {

                        // If we're not established and inside the radius...
                    } else {
                    }
                }

                let target_heading = if established {
                    find_bearing(
                        (params.lat, params.lon),
                        (orbit.center_lat, orbit.center_lon),
                    )
                } else {
                    find_bearing(
                        (params.lat, params.lon),
                        (orbit.center_lat, orbit.center_lon),
                    )
                };
            }
        } else if let Some(pt) = &self.direct_to_point {
            if optional_sensors.gps_connected {
                let target_heading = find_bearing((params.lat, params.lon), (pt.lat, pt.lon));

                let target_pitch = ((pt.alt_msl - params.baro_alt_msl)
                    / find_distance((pt.lat, pt.lon), (params.lat, params.lon)))
                .atan();

                // todo: Crude algo here. Is this OK? Important distinction: Flight path does'nt mean
                // todo exactly pitch! Might be close enough for good enough.
                let roll_const = 2.; // radians bank / radians heading  todo: Const?
                attitudes_commanded.roll =
                    Some(((target_heading - params.s_yaw_heading) * roll_const).max(MAX_BANK));
                attitudes_commanded.pitch = Some(target_pitch);
            }
        }

        if self.alt_hold.is_some()
            && !self.takeoff
            && self.land.is_none()
            && self.direct_to_point.is_none()
        {
            let (alt_type, alt_commanded) = self.alt_hold.unwrap();

            if !(alt_type == AltType::Agl && !optional_sensors.tof_connected) {
                // Set a vertical velocity for the inner loop to maintain, based on distance
                let dist = match alt_type {
                    AltType::Msl => alt_commanded - params.baro_alt_msl,
                    AltType::Agl => alt_commanded - params.tof_alt.unwrap_or(0.),
                };

                pid_attitude.pitch = pid::calc_pid_error(
                    // If just entering this mode, default to 0. pitch as a starting point.
                    attitudes_commanded.pitch.unwrap_or(0.),
                    dist,
                    &pid_attitude.pitch,
                    coeffs.pitch.k_p_attitude,
                    coeffs.pitch.k_i_attitude,
                    coeffs.pitch.k_d_attitude,
                    &mut filters.pitch_attitude,
                    DT_ATTITUDE,
                );

                // todo: Set this at rate or attitude level?

                attitudes_commanded.pitch = Some(pid_attitude.pitch.out());

                // todo: Commented out code below is if we use the velocity loop.
                // // `enroute_speed_ver` returns a velocity of the appropriate sine for above vs below.
                // attitudes_commanded.pitch =
                //     Some(flight_ctrls::quad::enroute_speed_ver(dist, max_speed_ver, params.tof_alt));
            }
        }

        // If not in an autopilot mode, reset commands that may have been set by the autopilot, and
        // wouldn't have been reset by manual controls. For now, this only applie to throttle.
        if self.alt_hold.is_none()
            && !self.takeoff
            && self.land.is_none()
            && self.direct_to_point.is_none()
        {
            attitudes_commanded.pitch = None;
            attitudes_commanded.roll = None;
        }
    }
}
