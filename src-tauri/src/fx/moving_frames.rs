//! Explicit native-stored frame inputs for the moving-emitter preview tests.
//!
//! Dense frames or sparse source keys feed the same owned one-millisecond clock.
//! Sparse keys use the original native linear quaternion/position interpolation,
//! not spherical interpolation. This is not a variable engine-update scheduler.

use crate::fx::cloud_motion::Frame;
use serde_json::Value;

pub struct Timeline {
    frames: Vec<Frame>,
    quaternions: Vec<[f32; 4]>,
}

fn vector(value: &Value) -> Result<[f32; 3], String> {
    let values = value.as_array().ok_or("Frame vector needs three numbers")?;
    if values.len() != 3 {
        return Err("Frame vector needs three numbers".into());
    }
    let mut result = [0.0; 3];
    for (index, number) in values.iter().enumerate() {
        result[index] = number.as_f64().ok_or("Invalid frame coordinate")? as f32;
        if !result[index].is_finite() {
            return Err("Frame coordinates must be finite".into());
        }
    }
    Ok(result)
}

fn quaternion(value: &Value) -> Result<[f32; 4], String> {
    let raw = value.as_array().ok_or("Missing native-stored quaternion")?;
    if raw.len() != 4 {
        return Err("Moving frame quaternion needs four components".into());
    }
    let mut result = [0.0; 4];
    for (index, number) in raw.iter().enumerate() {
        result[index] = number.as_f64().ok_or("Invalid frame quaternion")? as f32;
        if !result[index].is_finite() {
            return Err("Frame quaternion must be finite".into());
        }
    }
    let norm: f64 = result
        .iter()
        .map(|value| *value as f64 * *value as f64)
        .sum();
    if (norm - 1.0).abs() > 0.001 {
        return Err("Moving frame requires a native unit quaternion".into());
    }
    Ok(result)
}

impl Timeline {
    pub fn from_effect(effect: &Value, horizon: u32) -> Result<Option<Self>, String> {
        if let Some(keys) = effect.get("previewEmitterKeyframes") {
            if effect.get("previewEmitterFrame").is_some()
                || effect.get("previewEmitterFrames").is_some()
            {
                return Err("Supply one static, dense or sparse emitter frame input".into());
            }
            return Self::from_keys(keys, horizon).map(Some);
        }
        let Some(raw) = effect.get("previewEmitterFrames") else {
            return Ok(None);
        };
        if effect.get("previewEmitterFrame").is_some() {
            return Err("Supply a static frame or a moving frame track, not both".into());
        }
        let entries = raw
            .as_array()
            .ok_or("Moving frame track must be an array")?;
        if entries.len() != horizon as usize + 1 {
            return Err(
                "Preview moving frame track needs every millisecond through the horizon".into(),
            );
        }
        let mut frames = Vec::with_capacity(entries.len());
        let mut quaternions = Vec::with_capacity(entries.len());
        for (time, entry) in entries.iter().enumerate() {
            if entry["time"].as_u64() != Some(time as u64) {
                return Err("Moving frame times must be contiguous, starting at zero".into());
            }
            let origin = vector(&entry["origin"])?;
            let raw_quat = entry["storedQuat"]
                .as_array()
                .ok_or("Missing native-stored quaternion")?;
            if raw_quat.len() != 4 {
                return Err("Moving frame quaternion needs four components".into());
            }
            let mut quaternion = [0.0; 4];
            for (index, number) in raw_quat.iter().enumerate() {
                quaternion[index] = number.as_f64().ok_or("Invalid frame quaternion")? as f32;
                if !quaternion[index].is_finite() {
                    return Err("Frame quaternion must be finite".into());
                }
            }
            let norm: f64 = quaternion
                .iter()
                .map(|value| *value as f64 * *value as f64)
                .sum();
            if (norm - 1.0).abs() > 0.001 {
                return Err("Moving frame requires a native unit quaternion".into());
            }
            let raw_axis = entry["storedAxis"]
                .as_array()
                .ok_or("Missing native-stored axes")?;
            if raw_axis.len() != 3 {
                return Err("Moving frame needs three basis vectors".into());
            }
            let axis = [
                vector(&raw_axis[0])?,
                vector(&raw_axis[1])?,
                vector(&raw_axis[2])?,
            ];
            for first in 0..3 {
                for second in 0..3 {
                    let dot: f64 = (0..3)
                        .map(|component| {
                            axis[first][component] as f64 * axis[second][component] as f64
                        })
                        .sum();
                    let expected = if first == second { 1.0 } else { 0.0 };
                    if (dot - expected).abs() > 0.001 {
                        return Err("Moving frame stored axes must be orthonormal".into());
                    }
                }
            }
            frames.push(Frame { origin, axis });
            quaternions.push(quaternion);
        }
        Ok(Some(Self {
            frames,
            quaternions,
        }))
    }

    fn from_keys(raw: &Value, horizon: u32) -> Result<Self, String> {
        let entries = raw
            .as_array()
            .ok_or("Sparse emitter keys must be an array")?;
        if entries.is_empty() || entries.len() > 60001 {
            return Err("Sparse emitter track needs a bounded, nonempty key list".into());
        }
        let mut keys = Vec::with_capacity(entries.len());
        for entry in entries {
            let time = entry["time"]
                .as_u64()
                .filter(|time| *time <= 60000)
                .ok_or("Sparse key time must be an integer in 0..60000")?
                as u32;
            if keys
                .last()
                .is_some_and(|key: &(u32, [f32; 3], [f32; 4])| key.0 >= time)
            {
                return Err("Sparse key times must be strictly increasing".into());
            }
            keys.push((
                time,
                vector(&entry["origin"])?,
                quaternion(&entry["storedQuat"])?,
            ));
        }
        if keys[0].0 != 0 || keys.last().unwrap().0 < horizon {
            return Err("Sparse keys must start at zero and cover the playback horizon".into());
        }
        let mut frames = Vec::with_capacity(horizon as usize + 1);
        let mut quaternions = Vec::with_capacity(horizon as usize + 1);
        let mut end = 1;
        for time in 0..=horizon {
            let (quat, origin) = if time == 0 {
                // Startup receives the supplied source frame. Subsequent
                // current frames pass through the independently tested lerp.
                (keys[0].2, keys[0].1)
            } else {
                while keys[end].0 < time {
                    end += 1;
                }
                let begin = keys[end - 1];
                let finish = keys[end];
                let fraction = (time - begin.0) as f32 / (finish.0 - begin.0) as f32;
                crate::fx::sparse_frame_math::lerp_frame(
                    begin.2, begin.1, finish.2, finish.1, fraction,
                )
                .map_err(str::to_owned)?
            };
            frames.push(Frame {
                origin,
                axis: crate::fx::sparse_frame_math::unit_quat_axis(quat),
            });
            quaternions.push(quat);
        }
        Ok(Self {
            frames,
            quaternions,
        })
    }

    pub fn at(&self, time: u32) -> Frame {
        self.frames[time as usize]
    }

    /// Future loop allocations pass through native FX_FrameLerp. Even at the
    /// current millisecond endpoint it normalizes the supplied quaternion;
    /// using the untouched current axis would lose those native float stores.
    pub fn when_played(&self, time: u32) -> Frame {
        let current = self.at(time);
        let previous = self.at(time.saturating_sub(1));
        Frame {
            origin: crate::fx::moving_frame_math::played_origin(previous.origin, current.origin),
            axis: crate::fx::moving_frame_math::played_axis(self.quaternions[time as usize]),
        }
    }
}
