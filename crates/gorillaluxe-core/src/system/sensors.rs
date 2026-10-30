//! Virtual sensor HAL simulator (spec §3.3.2).
//!
//! Produces continuous 1D Perlin-style micro-noise (±0.002 m/s² at
//! 100 Hz) layered over realistic static fields so sensor-polling apps
//! never see dead-zero output. Fully deterministic per seed — the same
//! generator runs host-side today and is source-identical to what
//! `sensors.gorillaluxe.so` embeds in the guest.

/// One floating-point sensor sample (axis field chosen by caller).
#[derive(Debug, Clone, Copy)]
pub struct Reading {
    /// 100 Hz tick index.
    pub tick: u64,
    /// Accelerometer (x, y, z) in m/s² (gravity on Z + noise).
    pub accel: [f32; 3],
    /// Gyroscope (x, y, z) in rad/s (drift + noise).
    pub gyro: [f32; 3],
    /// Ambient light in lux.
    pub lux: f32,
}

/// Deterministic lattice hash → [-1.0, 1.0].
fn lattice(i: i64, seed: u64) -> f32 {
    let mut h = (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ seed;
    h ^= h >> 33;
    h = h.wrapping_mul(0xFF51_AFD7_ED55_8CCD);
    h ^= h >> 33;
    // Top 24 bits → [0,1) → [-1,1).
    ((h >> 40) & 0xFFFFFF) as f32 / 8388607.5 - 1.0
}

fn smooth(t: f32) -> f32 {
    // Quintic smootherstep 6t⁵−15t⁴+10t³.
    t * t * t * (t * (t * 6.0 - 15.0) + 10.0)
}

/// Classic 1D Perlin value noise at time `t` (in lattice units).
fn perlin1(t: f32, seed: u64) -> f32 {
    let i = t.floor() as i64;
    let f = t - i as f32;
    let a = lattice(i, seed);
    let b = lattice(i + 1, seed);
    a + (b - a) * smooth(f)
}

/// Continuous micro-noise source: two Perlin octaves, scaled to
/// ±0.002 peak. Tick rate is 100 Hz ⇒ `t = tick * freq`.
pub struct NoiseGen {
    seed: u64,
    /// Lattice crossings per second (low freq = slow drift).
    freq: f32,
    /// Peak amplitude in output units.
    amp: f32,
}

impl NoiseGen {
    pub fn accel(seed: u64) -> Self {
        NoiseGen {
            seed,
            freq: 1.7,
            amp: 0.002,
        }
    }
    pub fn gyro(seed: u64) -> Self {
        NoiseGen {
            seed,
            freq: 0.9,
            amp: 0.0004,
        }
    }
    pub fn light(seed: u64) -> Self {
        NoiseGen {
            seed,
            freq: 0.05,
            amp: 4.0,
        }
    }
    /// Sample at 100 Hz tick `n`.
    pub fn at(&self, n: u64) -> f32 {
        let t = n as f32 * (self.freq / 100.0);
        perlin1(t, self.seed) * self.amp + perlin1(t * 2.7, self.seed ^ 0x51A7) * self.amp * 0.35
    }
}

/// Full virtual sensor feed: accel + gyro + light.
pub struct SensorFeed {
    ax: NoiseGen,
    ay: NoiseGen,
    az: NoiseGen,
    gx: NoiseGen,
    gy: NoiseGen,
    gz: NoiseGen,
    lx: NoiseGen,
    base_lux: f32,
}

impl SensorFeed {
    pub fn new(seed: u64) -> Self {
        SensorFeed {
            ax: NoiseGen::accel(seed),
            ay: NoiseGen::accel(seed ^ 1),
            az: NoiseGen::accel(seed ^ 2),
            gx: NoiseGen::gyro(seed ^ 3),
            gy: NoiseGen::gyro(seed ^ 4),
            gz: NoiseGen::gyro(seed ^ 5),
            lx: NoiseGen::light(seed ^ 6),
            base_lux: 120.0,
        }
    }

    /// Sample all sensors at 100 Hz tick `n`.
    pub fn sample(&self, n: u64) -> Reading {
        Reading {
            tick: n,
            accel: [self.ax.at(n), self.ay.at(n), 9.806_65 + self.az.at(n)],
            gyro: [self.gx.at(n), self.gy.at(n), self.gz.at(n)],
            lux: (self.base_lux + self.lx.at(n)).max(0.5),
        }
    }
}

/// Peak-to-peak amplitude of `v` — used by the Phase-4 self-test to
/// prove continuous non-static variance.
pub fn variance_pp(samples: &[f32]) -> f32 {
    let (mut lo, mut hi) = (f32::MAX, f32::MIN);
    for &s in samples {
        lo = lo.min(s);
        hi = hi.max(s);
    }
    hi - lo
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accel_micro_noise_in_spec_range() {
        let f = SensorFeed::new(42);
        let xs: Vec<f32> = (0..100).map(|t| f.sample(t).accel[0]).collect();
        let pp = variance_pp(&xs);
        assert!(pp > 1e-6, "static zero output");
        assert!(pp < 0.02, "noise too large: {pp}");
        // Determinism.
        let f2 = SensorFeed::new(42);
        assert_eq!(f.sample(50).accel[0], f2.sample(50).accel[0]);
        // Gravity baseline on Z.
        let g = f.sample(10).accel[2];
        assert!((g - 9.80665).abs() < 0.02);
    }
}
