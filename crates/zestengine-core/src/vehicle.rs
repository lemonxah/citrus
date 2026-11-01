//! Wheeled-vehicle physics: wheel suspension and a simple longitudinal drive
//! model.

use serde::{Deserialize, Serialize};

/// One wheel of a wheeled vehicle.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Wheel {
    /// Suspension rest length (metres).
    pub rest_length: f32,
    /// Spring stiffness (N/m).
    pub spring: f32,
    /// Damping (N·s/m).
    pub damper: f32,
    /// Wheel radius (metres).
    pub radius: f32,
    /// This wheel steers.
    pub steer: bool,
    /// This wheel is driven by the engine and brakes.
    pub drive: bool,
}

impl Default for Wheel {
    fn default() -> Self {
        Self { rest_length: 0.4, spring: 40_000.0, damper: 2_500.0, radius: 0.35, steer: false, drive: true }
    }
}

/// Suspension force (N, up) for a wheel from its compression and compression
/// velocity: the spring pushes back against compression, and the damper resists
/// motion (adds force while compressing).
pub fn suspension_force(wheel: &Wheel, compression: f32, compression_velocity: f32) -> f32 {
    compression.max(0.0) * wheel.spring + compression_velocity * wheel.damper
}

/// Net longitudinal drive force (N). Throttle (0..1) accelerates; brake (0..1)
/// decelerates against the current travel direction, so the vehicle can also
/// reverse under brake.
pub fn drive_force(engine: f32, throttle: f32, brake: f32, forward_speed: f32) -> f32 {
    let accel = throttle.clamp(0.0, 1.0) * engine;
    let brake_dir = if forward_speed >= 0.0 { -1.0 } else { 1.0 };
    accel + brake.clamp(0.0, 1.0) * engine * brake_dir
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suspension_is_zero_at_rest_length() {
        let w = Wheel::default();
        assert_eq!(suspension_force(&w, 0.0, 0.0), 0.0);
        assert_eq!(suspension_force(&w, -0.1, 0.0), 0.0, "negative compression clamps");
    }

    #[test]
    fn suspension_spring_and_damper() {
        let w = Wheel::default();
        let f = suspension_force(&w, 0.1, 0.5);
        assert!((f - (0.1 * w.spring + 0.5 * w.damper)).abs() < 1e-3);
    }

    #[test]
    fn drive_throttles_and_brakes() {
        assert!(drive_force(1000.0, 1.0, 0.0, 0.0) > 0.0);
        assert!(drive_force(1000.0, 0.0, 1.0, 5.0) < 0.0, "brake opposes forward speed");
        assert!(drive_force(1000.0, 0.0, 1.0, -5.0) > 0.0, "brake opposes reverse speed");
        assert_eq!(drive_force(1000.0, 0.0, 0.0, 3.0), 0.0);
    }
}
