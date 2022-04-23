//! Present-position keeping system. Fuses GPS, and dead-reckoning.

use stm32_hal2::{i2c::I2c, pac::I2C1};

pub enum LocationType {
    /// Lattitude and longitude. Available after a GPS fix
    LatLon,
    /// Start at 0, and count in meters away from it.
    Rel0,
}

impl Default for LocationType {
    fn default() -> Self {
        Self::Rel0
    }
}

/// If type is LatLon, `x` and `y` are in degrees. If Rel0, in meters. `z` is in m MSL.
#[derive(Default)]
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