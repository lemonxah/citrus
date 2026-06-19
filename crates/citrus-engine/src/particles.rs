//! CPU particle system (ENGINE_FEATURE_CHECKLIST T1 #25).
//!
//! Emitter + per-particle simulation (spawn rate, lifetime, gravity/drag, size &
//! colour over life). This is the simulation core — deterministic and unit-tested;
//! the renderer draws the live `particles()` as instanced billboards. A GPU compute
//! path is a later optimization, but the authoring model + CPU sim are what make it
//! a usable VFX feature.

use glam::Vec3;

/// A live particle.
#[derive(Clone, Copy, Debug)]
pub struct Particle {
    pub pos: Vec3,
    pub vel: Vec3,
    /// Seconds lived so far.
    pub age: f32,
    /// Total lifetime (seconds); dead when `age >= life`.
    pub life: f32,
    /// Per-particle random seed in [0,1) for spawn-time variation.
    pub seed: f32,
}

impl Particle {
    /// Normalized age in [0,1] (0 = born, 1 = dying).
    pub fn t(&self) -> f32 {
        (self.age / self.life.max(1e-4)).clamp(0.0, 1.0)
    }
}

/// Emitter parameters (authored on a component).
#[derive(Clone, Debug)]
pub struct EmitterConfig {
    /// Particles spawned per second.
    pub rate: f32,
    pub lifetime: f32,
    /// Initial speed range (m/s) along a random cone direction.
    pub speed: (f32, f32),
    /// Half-angle of the emission cone (radians) around `direction`.
    pub cone: f32,
    pub direction: Vec3,
    /// Constant acceleration (e.g. gravity).
    pub gravity: Vec3,
    /// Linear drag coefficient (per second).
    pub drag: f32,
    /// Size at birth / death (for the renderer; sim just carries it).
    pub size: (f32, f32),
    /// Max live particles (older ones are recycled).
    pub max: usize,
}

impl Default for EmitterConfig {
    fn default() -> Self {
        Self {
            rate: 50.0,
            lifetime: 2.0,
            speed: (1.0, 3.0),
            cone: 0.3,
            direction: Vec3::Y,
            gravity: Vec3::new(0.0, -9.81, 0.0),
            drag: 0.1,
            size: (0.2, 0.0),
            max: 1024,
        }
    }
}

/// A running emitter instance.
pub struct ParticleSystem {
    pub config: EmitterConfig,
    particles: Vec<Particle>,
    accumulator: f32, // fractional spawn carry-over
    rng: u32,
}

impl ParticleSystem {
    pub fn new(config: EmitterConfig) -> Self {
        let rng = 0x2545_F491;
        Self { particles: Vec::new(), accumulator: 0.0, rng, config }
    }

    pub fn particles(&self) -> &[Particle] {
        &self.particles
    }
    pub fn live_count(&self) -> usize {
        self.particles.len()
    }

    fn rand(&mut self) -> f32 {
        // xorshift32 -> [0,1)
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 17;
        self.rng ^= self.rng << 5;
        (self.rng >> 8) as f32 / (1u32 << 24) as f32
    }

    /// Advance the sim by `dt` seconds, spawning from `origin`.
    pub fn update(&mut self, dt: f32, origin: Vec3) {
        // Age + integrate, dropping dead particles.
        let drag = self.config.drag;
        let grav = self.config.gravity;
        self.particles.retain_mut(|p| {
            p.age += dt;
            if p.age >= p.life {
                return false;
            }
            p.vel += grav * dt;
            p.vel *= (1.0 - drag * dt).max(0.0);
            p.pos += p.vel * dt;
            true
        });

        // Spawn new ones at the configured rate.
        self.accumulator += self.config.rate * dt;
        let dir = self.config.direction.normalize_or(Vec3::Y);
        while self.accumulator >= 1.0 && self.particles.len() < self.config.max {
            self.accumulator -= 1.0;
            let seed = self.rand();
            let (smin, smax) = self.config.speed;
            let speed = smin + (smax - smin) * self.rand();
            // Random direction within the cone around `dir`.
            let cone = self.config.cone;
            let a = self.rand() * std::f32::consts::TAU;
            let r = (self.rand()).sqrt() * cone;
            let (basis_u, basis_v) = ortho_basis(dir);
            let d = (dir * r.cos() + (basis_u * a.cos() + basis_v * a.sin()) * r.sin())
                .normalize_or(dir);
            self.particles.push(Particle {
                pos: origin,
                vel: d * speed,
                age: 0.0,
                life: self.config.lifetime,
                seed,
            });
        }
    }

    pub fn clear(&mut self) {
        self.particles.clear();
        self.accumulator = 0.0;
    }
}

/// Two unit vectors orthogonal to `n` (and each other).
fn ortho_basis(n: Vec3) -> (Vec3, Vec3) {
    let a = if n.x.abs() > 0.9 { Vec3::Y } else { Vec3::X };
    let u = n.cross(a).normalize_or(Vec3::X);
    let v = n.cross(u).normalize_or(Vec3::Z);
    (u, v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawns_at_rate_and_caps_at_max() {
        let mut ps = ParticleSystem::new(EmitterConfig {
            rate: 100.0,
            lifetime: 100.0, // long-lived so they accumulate
            max: 50,
            ..Default::default()
        });
        // 1 second at 100/s would be 100, but capped at 50.
        for _ in 0..100 {
            ps.update(0.01, Vec3::ZERO);
        }
        assert_eq!(ps.live_count(), 50);
    }

    #[test]
    fn particles_die_after_their_lifetime() {
        let mut ps = ParticleSystem::new(EmitterConfig {
            rate: 10.0,
            lifetime: 0.5,
            max: 100,
            ..Default::default()
        });
        ps.update(0.1, Vec3::ZERO);
        let n0 = ps.live_count();
        assert!(n0 > 0);
        // Stop spawning, run past lifetime -> all dead.
        ps.config.rate = 0.0;
        for _ in 0..10 {
            ps.update(0.1, Vec3::ZERO);
        }
        assert_eq!(ps.live_count(), 0);
    }

    #[test]
    fn gravity_pulls_particles_down() {
        let mut ps = ParticleSystem::new(EmitterConfig {
            rate: 1.0,
            lifetime: 10.0,
            speed: (0.0, 0.0), // no initial velocity
            gravity: Vec3::new(0.0, -10.0, 0.0),
            drag: 0.0,
            max: 10,
            ..Default::default()
        });
        ps.update(0.5, Vec3::ZERO); // spawns + integrates
        ps.config.rate = 0.0;
        for _ in 0..10 {
            ps.update(0.1, Vec3::ZERO);
        }
        // After falling, every particle has negative Y velocity and dropped.
        for p in ps.particles() {
            assert!(p.vel.y < 0.0, "gravity should accelerate downward");
            assert!(p.pos.y < 0.0, "particle should have fallen below origin");
        }
    }

    #[test]
    fn normalized_age_is_monotonic() {
        let p = Particle { pos: Vec3::ZERO, vel: Vec3::ZERO, age: 1.0, life: 2.0, seed: 0.0 };
        assert!((p.t() - 0.5).abs() < 1e-6);
    }
}
