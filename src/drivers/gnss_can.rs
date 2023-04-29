//! C+P from gps_mag_can, `protocol.rs`

//! Contains code for transmitting fixes and related info
//! using the DroneCan or Cyphal protocols.
//!
//! [DroneCAN standard types ref](https://dronecan.github.io/Specification/7._List_of_standard_data_types/)

use core::sync::atomic::AtomicUsize;

use packed_struct::{prelude::*, PackedStruct};

use dronecan::{
    gnss::{EcefPositionVelocity, FixDronecan, FixStatus, GnssMode, GnssSubMode, GnssTimeStandard},
    CanError, ConfigCommon, HardwareVersion, MsgPriority, SoftwareVersion, CONFIG_COMMON_SIZE,
};

use half::f16;

use crate::gps::{Fix, FixType};

pub const GNSS_PAYLOAD_SIZE: usize = 38;

pub const GNSS_FIX_ID: u16 = 1_063;

pub const CONFIG_SIZE: usize = 4;

pub static TRANSFER_ID_FIX: AtomicUsize = AtomicUsize::new(0);

// todo: Node status.

pub struct Config {
    pub common: ConfigCommon,
    /// Hz. Maximum of 18Hz with a single constellation. Lower rates with fused data. For example,
    /// GPS + GAL is 10Hz max.
    pub broadcast_rate_gnss: u8,
    /// Hz. Broadcasting the fused solution can occur at a much higher rate.
    pub broadcast_rate_fused: u16,
    /// Compatibility workaround; we normally send fused data in a condensed format,
    /// with GNSS metadata removed. This sends it using the same packet. Helps compatibility
    /// with FCs that don't support our format, but sends more data.
    pub broadcast_rate_baro: u16,
    pub broadcast_rate_mag: u16,
    pub broadcast_fused_as_fix: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            common: Default::default(),
            broadcast_rate_gnss: 5,
            broadcast_rate_fused: 100,
            broadcast_rate_baro: 100,
            broadcast_rate_mag: 100,
            broadcast_fused_as_fix: true,
        }
    }
}

impl Config {
    pub fn from_bytes(buf: &[u8]) -> Self {
        const CCS: usize = CONFIG_COMMON_SIZE;
        Self {
            common: ConfigCommon::from_bytes(&buf[0..CCS]),
            broadcast_rate_gnss: buf[CCS],
            broadcast_rate_fused: u16::from_le_bytes(buf[CCS + 1..CCS + 3].try_into().unwrap()),
            broadcast_rate_baro: u16::from_le_bytes(buf[CCS + 3..CCS + 5].try_into().unwrap()),
            broadcast_rate_mag: u16::from_le_bytes(buf[CCS + 5..CCS + 7].try_into().unwrap()),
            broadcast_fused_as_fix: buf[CCS + 7] != 0,
        }
    }

    pub fn to_bytes(&self) -> [u8; CONFIG_SIZE] {
        const CCS: usize = CONFIG_COMMON_SIZE;
        let mut result = [0; CONFIG_SIZE];

        result[0..CCS].clone_from_slice(&self.common.to_bytes());
        result[CCS] = self.broadcast_rate_gnss;
        result[CCS + 1..CCS + 3].copy_from_slice(&self.broadcast_rate_fused.to_le_bytes());
        result[CCS + 3..CCS + 5].copy_from_slice(&self.broadcast_rate_baro.to_le_bytes());
        result[CCS + 5..CCS + 7].copy_from_slice(&self.broadcast_rate_mag.to_le_bytes());
        result[CCS + 7] = self.broadcast_fused_as_fix as u8;

        result
    }
}

/// Create a Dronecan Fix2 from our Fix format, based on Ublox's.
pub fn from_fix(fix: &Fix, timestamp: f32) -> FixDronecan {
    let fix_status = match fix.type_ {
        FixType::NoFix => FixStatus::NoFix,
        FixType::DeadReckoning => FixStatus::Fix2d, // todo?
        FixType::Fix2d => FixStatus::Fix2d,
        FixType::Fix3d => FixStatus::Fix3d,
        FixType::Combined => FixStatus::Fix3d,
        FixType::TimeOnly => FixStatus::TimeOnly,
    };

    // todo
    let ecef_position_velocity = EcefPositionVelocity {
        velocity_xyz: [0.; 3],
        position_xyz_mm: [0; 3], // [i36; 3]
        // todo: Tail optimization (no len field) since this is the last field?
        covariance: [None; 36], // todo: [f16; <=36?]
    };

    // `packed_struct` doesn't support floats; convert to integers.
    let ned0_bytes = fix.ned_velocity[0].to_le_bytes();
    let ned1_bytes = fix.ned_velocity[1].to_le_bytes();
    let ned2_bytes = fix.ned_velocity[2].to_le_bytes();

    let ned_velocity = [
        u32::from_le_bytes(ned0_bytes),
        u32::from_le_bytes(ned1_bytes),
        u32::from_le_bytes(ned2_bytes),
    ];

    // packed-struct workaround for not having floats.
    // And handling 100x factor between Ublox and DroneCan.
    let pdop = f16::from_f32((fix.pdop as f32) * 100.);
    let pdop_bytes = pdop.to_le_bytes();
    // todo: order?
    let pdop = u16::from_le_bytes([pdop_bytes[0], pdop_bytes[1]]);

    FixDronecan {
        timestamp: (timestamp * 1_000_000.) as u64, // us.
        gnss_timestamp: fix.datetime.timestamp_micros() as u64,
        gnss_time_standard: GnssTimeStandard::Utc, // todo
        // 13-bit pad
        num_leap_seconds: 0, // todo
        // We must multiply by 10 due to the higher precion format used
        // in DroneCan.
        longitude_deg_1e8: (fix.lon * 10) as i64,
        latitude_deg_1e8: (fix.lat * 10) as i64,
        height_ellipsoid_mm: fix.elevation_hae,
        height_msl_mm: fix.elevation_msl,
        // todo: Groundspeed? Or is that only from NED vel?
        // todo: NED vel DroneCAN is in m/s, like our format, right?
        ned_velocity,
        sats_used: fix.sats_used,
        fix_status,
        mode: GnssMode::Dgps,                     // todo
        sub_mode: GnssSubMode::DgpsOtherRtkFloat, // todo?
        // covariance: [None; 36],                   // todo?
        pdop,
        // ecef_position_velocity,
    }
}