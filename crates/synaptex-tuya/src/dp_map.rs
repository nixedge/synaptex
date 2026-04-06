/// Mapping between Tuya data points (DPs) and Synaptex capabilities.
///
/// DP numbers are device-model dependent.  The defaults below cover the most
/// common Tuya bulb/switch firmware schema.  A `DpMap` can be overridden per
/// device via configuration.
use std::collections::HashMap;

use serde_json::Value;
use synaptex_types::plugin::DeviceState;

/// Known DP → field mappings for common Tuya device types.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DpMap {
    /// DP number that controls power (bool).
    pub power_dp:        u8,
    /// DP number for brightness (uint, device-native range).
    pub brightness_dp:   Option<u8>,
    /// DP number for colour temperature (uint, device-native range).
    pub color_temp_dp:   Option<u8>,
    /// DP number for RGB/HSV colour (string, hex-encoded).
    pub color_dp:        Option<u8>,
    /// Native brightness range (device minimum).
    pub brightness_min:  u16,
    /// Native brightness range (device maximum).
    pub brightness_max:  u16,
    /// Native colour-temp range (device minimum, cooler).
    pub color_temp_min:  u16,
    /// Native colour-temp range (device maximum, warmer).
    pub color_temp_max:  u16,
}

impl Default for DpMap {
    /// Common defaults for Tuya RGBW/CW bulbs (e.g. DP schema "dj" firmware).
    fn default() -> Self {
        Self {
            power_dp:       1,
            brightness_dp:  Some(3),
            color_temp_dp:  Some(4),
            color_dp:       Some(5),
            brightness_min: 25,
            brightness_max: 255,
            color_temp_min: 0,
            color_temp_max: 255,
        }
    }
}

impl DpMap {
    /// Merge a JSON `dps` object (from a Tuya status response) into
    /// `DeviceState`, normalising values to Synaptex ranges.
    pub fn apply_dps(&self, dps: &HashMap<String, Value>, state: &mut DeviceState) {
        if let Some(v) = dps.get(&self.power_dp.to_string()) {
            state.power = v.as_bool();
        }

        if let Some(dp) = self.brightness_dp {
            if let Some(v) = dps.get(&dp.to_string()).and_then(Value::as_u64) {
                state.brightness = Some(self.normalize_brightness(v as u16));
            }
        }

        if let Some(dp) = self.color_temp_dp {
            if let Some(v) = dps.get(&dp.to_string()).and_then(Value::as_u64) {
                state.color_temp_k = Some(self.native_to_kelvin(v as u16));
            }
        }

        if let Some(dp) = self.color_dp {
            if let Some(hex) = dps.get(&dp.to_string()).and_then(Value::as_str) {
                state.rgb = parse_hsv_hex(hex);
            }
        }
    }

    /// Build a `dps` payload from a brightness value (0–1000 Synaptex range).
    pub fn brightness_dp_value(&self, brightness: u16) -> (u8, u64) {
        let native = self.denormalize_brightness(brightness);
        (self.brightness_dp.unwrap_or(3), native as u64)
    }

    /// Build a `dps` payload for colour temperature (Kelvin → device native).
    pub fn color_temp_dp_value(&self, kelvin: u16) -> (u8, u64) {
        let native = self.kelvin_to_native(kelvin);
        (self.color_temp_dp.unwrap_or(4), native as u64)
    }

    // ── Range normalisation ───────────────────────────────────────────────────

    fn normalize_brightness(&self, native: u16) -> u16 {
        let range_in  = (self.brightness_max - self.brightness_min) as f32;
        let fraction  = (native.saturating_sub(self.brightness_min)) as f32 / range_in;
        (fraction * 1000.0).round() as u16
    }

    fn denormalize_brightness(&self, synaptex: u16) -> u16 {
        let range_out = (self.brightness_max - self.brightness_min) as f32;
        let native    = (synaptex as f32 / 1000.0) * range_out + self.brightness_min as f32;
        native.round() as u16
    }

    /// Map native device value to Kelvin (linear interpolation).
    /// Assumes min native = 6500 K (cool), max native = 2700 K (warm).
    fn native_to_kelvin(&self, native: u16) -> u16 {
        let fraction = (native.saturating_sub(self.color_temp_min)) as f32
            / (self.color_temp_max - self.color_temp_min).max(1) as f32;
        // Interpolate: 0 → 6500 K (cool), 1 → 2700 K (warm)
        (6500.0 - fraction * 3800.0).round() as u16
    }

    fn kelvin_to_native(&self, kelvin: u16) -> u16 {
        let fraction = (6500_f32 - kelvin as f32) / 3800.0;
        let native   = fraction * (self.color_temp_max - self.color_temp_min) as f32
            + self.color_temp_min as f32;
        native.clamp(self.color_temp_min as f32, self.color_temp_max as f32).round() as u16
    }
}

/// Parse a 12-char hex HSV string `"HHHHSSSSVVVV"` (each 4 hex digits) into
/// an approximate sRGB triplet.
fn parse_hsv_hex(s: &str) -> Option<(u8, u8, u8)> {
    if s.len() != 12 {
        return None;
    }
    let h_raw = u16::from_str_radix(&s[0..4], 16).ok()? as f32; // 0–360
    let s_raw = u16::from_str_radix(&s[4..8], 16).ok()? as f32; // 0–1000
    let v_raw = u16::from_str_radix(&s[8..12], 16).ok()? as f32; // 0–1000

    let h = h_raw / 360.0;
    let s = s_raw / 1000.0;
    let v = v_raw / 1000.0;

    Some(hsv_to_rgb(h, s, v))
}

fn hsv_to_rgb(h: f32, s: f32, v: f32) -> (u8, u8, u8) {
    let i = (h * 6.0).floor() as u32;
    let f = h * 6.0 - i as f32;
    let p = v * (1.0 - s);
    let q = v * (1.0 - f * s);
    let t = v * (1.0 - (1.0 - f) * s);
    let (r, g, b) = match i % 6 {
        0 => (v, t, p),
        1 => (q, v, p),
        2 => (p, v, t),
        3 => (p, q, v),
        4 => (t, p, v),
        _ => (v, p, q),
    };
    ((r * 255.0) as u8, (g * 255.0) as u8, (b * 255.0) as u8)
}
