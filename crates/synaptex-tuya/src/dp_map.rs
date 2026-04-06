/// Mapping between Tuya data points (DPs) and Synaptex capabilities.
///
/// DP numbers are u16 to cover extended schemas (e.g. IR transceiver DP 201).
use std::collections::HashMap;

use serde_json::{json, Value};
use synaptex_types::plugin::DeviceState;

// ─── Color format ─────────────────────────────────────────────────────────────

/// Encoding used by the device's color DP.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ColorFormat {
    /// 14-char hex: `rrggbb0hhhssvv`
    /// R/G/B in 2-char hex; "0" literal; H 0–360 in 3-char hex; S/V 0–255 in 2-char hex.
    Rgb8,
    /// 12-char hex: `hhhhssssvvvv`
    /// H 0–360 in 4-char hex; S/V 0–1000 in 4-char hex.
    Hsv16,
}

// ─── DpMap ────────────────────────────────────────────────────────────────────

/// Known DP → field mappings for common Tuya device types.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DpMap {
    /// DP number that controls power (bool).
    pub power_dp:         u16,
    /// DP number for brightness (uint, device-native range).
    pub brightness_dp:    Option<u16>,
    /// DP number for colour temperature (uint, device-native range).
    pub color_temp_dp:    Option<u16>,
    /// DP number for RGB/HSV colour (string, hex-encoded).
    pub color_dp:         Option<u16>,
    /// Mode DP (str: "white" | "colour").  When set, colour commands also
    /// set this DP to "colour", and white commands set it to "white".
    pub mode_dp:          Option<u16>,
    /// Color encoding format used by `color_dp`.
    pub color_format:     ColorFormat,
    /// Native brightness range (device minimum).
    pub brightness_min:   u16,
    /// Native brightness range (device maximum).
    pub brightness_max:   u16,
    /// Native colour-temp range (device minimum, cooler).
    pub color_temp_min:   u16,
    /// Native colour-temp range (device maximum, warmer).
    pub color_temp_max:   u16,
    /// Fan speed DP (string enum: "auto" | "low" | "middle" | "high").
    pub fan_speed_dp:     Option<u16>,
    /// Fan mode DP (string enum: "cold" | "hot" | "dehumidify").
    pub fan_mode_dp:      Option<u16>,
    /// IR transceiver send DP.
    pub ir_send_dp:       Option<u16>,
    /// IR control type (1 = type-1 JSON blob, 2 = type-2 multi-DP).
    pub ir_control_type:  Option<u8>,
}

// ─── Presets ──────────────────────────────────────────────────────────────────

impl DpMap {
    /// Tuya Type A bulbs (older firmware, DPs 1–9).
    /// power=1, mode=2, brightness=3 (25–255), coltemp=4 (0–255), color=5 rgb8
    pub fn tuya_bulb_a() -> Self {
        Self {
            power_dp:        1,
            brightness_dp:   Some(3),
            color_temp_dp:   Some(4),
            color_dp:        Some(5),
            mode_dp:         Some(2),
            color_format:    ColorFormat::Rgb8,
            brightness_min:  25,
            brightness_max:  255,
            color_temp_min:  0,
            color_temp_max:  255,
            fan_speed_dp:    None,
            fan_mode_dp:     None,
            ir_send_dp:      None,
            ir_control_type: None,
        }
    }

    /// Tuya Type B bulbs (newer firmware, DPs 20–28).
    /// power=20, mode=21, brightness=22 (10–1000), coltemp=23 (0–1000), color=24 hsv16
    pub fn tuya_bulb_b() -> Self {
        Self {
            power_dp:        20,
            brightness_dp:   Some(22),
            color_temp_dp:   Some(23),
            color_dp:        Some(24),
            mode_dp:         Some(21),
            color_format:    ColorFormat::Hsv16,
            brightness_min:  10,
            brightness_max:  1000,
            color_temp_min:  0,
            color_temp_max:  1000,
            fan_speed_dp:    None,
            fan_mode_dp:     None,
            ir_send_dp:      None,
            ir_control_type: None,
        }
    }

    /// Generic power-only switch (DP 1).
    pub fn switch() -> Self {
        Self {
            power_dp:        1,
            brightness_dp:   None,
            color_temp_dp:   None,
            color_dp:        None,
            mode_dp:         None,
            color_format:    ColorFormat::Hsv16,
            brightness_min:  0,
            brightness_max:  1000,
            color_temp_min:  0,
            color_temp_max:  1000,
            fan_speed_dp:    None,
            fan_mode_dp:     None,
            ir_send_dp:      None,
            ir_control_type: None,
        }
    }

    /// Fan device: power=1, speed=3 (str), mode=4 (str).
    pub fn fan() -> Self {
        Self {
            power_dp:        1,
            brightness_dp:   None,
            color_temp_dp:   None,
            color_dp:        None,
            mode_dp:         None,
            color_format:    ColorFormat::Hsv16,
            brightness_min:  0,
            brightness_max:  1000,
            color_temp_min:  0,
            color_temp_max:  1000,
            fan_speed_dp:    Some(3),
            fan_mode_dp:     Some(4),
            ir_send_dp:      None,
            ir_control_type: None,
        }
    }

    /// IR type-1 transceiver (single DP 201, JSON blob payload).
    pub fn ir_type1() -> Self {
        Self {
            power_dp:        1,
            brightness_dp:   None,
            color_temp_dp:   None,
            color_dp:        None,
            mode_dp:         None,
            color_format:    ColorFormat::Hsv16,
            brightness_min:  0,
            brightness_max:  1000,
            color_temp_min:  0,
            color_temp_max:  1000,
            fan_speed_dp:    None,
            fan_mode_dp:     None,
            ir_send_dp:      Some(201),
            ir_control_type: Some(1),
        }
    }

    /// IR type-2 transceiver (DPs 1–13 scheme).
    pub fn ir_type2() -> Self {
        Self {
            power_dp:        1,
            brightness_dp:   None,
            color_temp_dp:   None,
            color_dp:        None,
            mode_dp:         None,
            color_format:    ColorFormat::Hsv16,
            brightness_min:  0,
            brightness_max:  1000,
            color_temp_min:  0,
            color_temp_max:  1000,
            fan_speed_dp:    None,
            fan_mode_dp:     None,
            ir_send_dp:      Some(1),
            ir_control_type: Some(2),
        }
    }

    /// Resolve a string profile name to a preset (falls back to `tuya_bulb_b`).
    pub fn from_profile(profile: &str) -> Self {
        match profile {
            "bulb_a"  => Self::tuya_bulb_a(),
            "bulb_b"  => Self::tuya_bulb_b(),
            "switch"  => Self::switch(),
            "fan"     => Self::fan(),
            "ir1"     => Self::ir_type1(),
            "ir2"     => Self::ir_type2(),
            _         => Self::default(),
        }
    }
}

impl Default for DpMap {
    fn default() -> Self {
        Self::tuya_bulb_b()
    }
}

// ─── State application ────────────────────────────────────────────────────────

impl DpMap {
    /// Merge a JSON `dps` object into `DeviceState`.
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
                state.rgb = parse_color_hex(hex, self.color_format);
            }
        }
    }

    /// Build a `(dp, native_value)` pair for a brightness command (0–1000 input).
    pub fn brightness_dp_value(&self, brightness: u16) -> (u16, u64) {
        let native = self.denormalize_brightness(brightness);
        (self.brightness_dp.unwrap_or(22), native as u64)
    }

    /// Build a `(dp, native_value)` pair for a colour-temperature command (Kelvin input).
    pub fn color_temp_dp_value(&self, kelvin: u16) -> (u16, u64) {
        let native = self.kelvin_to_native(kelvin);
        (self.color_temp_dp.unwrap_or(23), native as u64)
    }

    /// Build a `dps` JSON value for `SetRgb`, honouring `color_format` and `mode_dp`.
    /// Returns `None` when no `color_dp` is configured.
    pub fn rgb_dps(&self, r: u8, g: u8, b: u8) -> Option<Value> {
        let color_dp = self.color_dp?;
        let hex = match self.color_format {
            ColorFormat::Rgb8  => rgb_to_hex_rgb8(r, g, b),
            ColorFormat::Hsv16 => rgb_to_hex_hsv16(r, g, b),
        };
        let mut map = serde_json::Map::new();
        map.insert(color_dp.to_string(), Value::String(hex));
        if let Some(mode) = self.mode_dp {
            map.insert(mode.to_string(), Value::String("colour".into()));
        }
        Some(Value::Object(map))
    }

    /// Build a `dps` JSON value for `SendIr`.
    /// Returns `None` when no `ir_send_dp` is configured.
    pub fn ir_dps(&self, head: Option<&str>, key: &str) -> Option<Value> {
        let send_dp = self.ir_send_dp?;
        match self.ir_control_type {
            Some(1) | None => {
                // Type 1: single DP with JSON blob
                let blob = json!({
                    "control": "send_ir",
                    "type": 0,
                    "head": head.unwrap_or(""),
                    "key1": format!("0{key}"),
                });
                Some(json!({ send_dp.to_string(): blob.to_string() }))
            }
            Some(2) => {
                // Type 2: multiple DPs (DP 2=mode, DP 7=head, DP 8=key_code)
                let mut map = serde_json::Map::new();
                map.insert("2".into(), Value::String("send_ir".into()));
                if let Some(h) = head {
                    map.insert("7".into(), Value::String(h.into()));
                }
                map.insert("8".into(), Value::String(key.into()));
                Some(Value::Object(map))
            }
            _ => None,
        }
    }

    // ── Capability derivation ─────────────────────────────────────────────────

    /// Derive the `Capability` set implied by this DP map.
    ///
    /// - `Power` is always included (every device has a power DP).
    /// - `Dimmer`, `ColorTemp`, `Rgb`, `Fan`, `Ir` are included only when
    ///   the corresponding DP is `Some`.
    pub fn capabilities(&self) -> Vec<synaptex_types::capability::Capability> {
        use synaptex_types::capability::Capability;
        let mut caps = vec![Capability::Power];
        if self.brightness_dp.is_some() {
            caps.push(Capability::Dimmer { min: 0, max: 1000 });
        }
        if self.color_temp_dp.is_some() {
            caps.push(Capability::ColorTemp { min_k: 2700, max_k: 6500 });
        }
        if self.color_dp.is_some() {
            caps.push(Capability::Rgb);
        }
        if self.fan_speed_dp.is_some() {
            caps.push(Capability::Fan);
        }
        if self.ir_send_dp.is_some() {
            caps.push(Capability::Ir);
        }
        caps
    }

    // ── Range normalisation ───────────────────────────────────────────────────

    fn normalize_brightness(&self, native: u16) -> u16 {
        let range_in = (self.brightness_max - self.brightness_min) as f32;
        if range_in == 0.0 { return 0; }
        let fraction = (native.saturating_sub(self.brightness_min)) as f32 / range_in;
        (fraction * 1000.0).round() as u16
    }

    fn denormalize_brightness(&self, synaptex: u16) -> u16 {
        let range_out = (self.brightness_max - self.brightness_min) as f32;
        let native    = (synaptex as f32 / 1000.0) * range_out + self.brightness_min as f32;
        native.round() as u16
    }

    /// Map native device value to Kelvin (linear interpolation).
    /// min native = 6500 K (cool), max native = 2700 K (warm).
    fn native_to_kelvin(&self, native: u16) -> u16 {
        let range = (self.color_temp_max - self.color_temp_min).max(1) as f32;
        let fraction = (native.saturating_sub(self.color_temp_min)) as f32 / range;
        (6500.0 - fraction * 3800.0).round() as u16
    }

    fn kelvin_to_native(&self, kelvin: u16) -> u16 {
        let fraction = (6500_f32 - kelvin as f32) / 3800.0;
        let native   = fraction * (self.color_temp_max - self.color_temp_min) as f32
            + self.color_temp_min as f32;
        native
            .clamp(self.color_temp_min as f32, self.color_temp_max as f32)
            .round() as u16
    }
}

// ─── Color parsing ────────────────────────────────────────────────────────────

/// Dispatch to the correct color-hex decoder based on format.
pub fn parse_color_hex(s: &str, fmt: ColorFormat) -> Option<(u8, u8, u8)> {
    match fmt {
        ColorFormat::Rgb8  => parse_color_hex_rgb8(s),
        ColorFormat::Hsv16 => parse_color_hex_hsv16(s),
    }
}

/// Parse a 14-char Type-A hex string `"rrggbb0hhhssvv"` → (r, g, b).
pub fn parse_color_hex_rgb8(s: &str) -> Option<(u8, u8, u8)> {
    if s.len() != 14 { return None; }
    let r = u8::from_str_radix(&s[0..2], 16).ok()?;
    let g = u8::from_str_radix(&s[2..4], 16).ok()?;
    let b = u8::from_str_radix(&s[4..6], 16).ok()?;
    Some((r, g, b))
}

/// Parse a 12-char Type-B hex string `"hhhhssssvvvv"` → approximate (r, g, b).
pub fn parse_color_hex_hsv16(s: &str) -> Option<(u8, u8, u8)> {
    if s.len() != 12 { return None; }
    let h_raw = u16::from_str_radix(&s[0..4], 16).ok()? as f32; // 0–360
    let s_raw = u16::from_str_radix(&s[4..8], 16).ok()? as f32; // 0–1000
    let v_raw = u16::from_str_radix(&s[8..12], 16).ok()? as f32; // 0–1000
    Some(hsv_to_rgb(h_raw / 360.0, s_raw / 1000.0, v_raw / 1000.0))
}

// ─── Color encoding ───────────────────────────────────────────────────────────

/// Encode (r, g, b) as a 14-char Type-A hex string `"rrggbb0hhhssvv"`.
pub fn rgb_to_hex_rgb8(r: u8, g: u8, b: u8) -> String {
    let (h, s, v) = rgb_to_hsv(r, g, b);
    // h: 0.0–360.0, s: 0.0–1.0, v: 0.0–1.0
    let h_raw = h.round() as u16;
    let s_raw = (s * 255.0).round() as u8;
    let v_raw = (v * 255.0).round() as u8;
    format!("{:02x}{:02x}{:02x}0{:03x}{:02x}{:02x}", r, g, b, h_raw, s_raw, v_raw)
}

/// Encode (r, g, b) as a 12-char Type-B hex string `"hhhhssssvvvv"`.
pub fn rgb_to_hex_hsv16(r: u8, g: u8, b: u8) -> String {
    let (h, s, v) = rgb_to_hsv(r, g, b);
    let h_raw = h.round() as u16;             // 0–360
    let s_raw = (s * 1000.0).round() as u16;  // 0–1000
    let v_raw = (v * 1000.0).round() as u16;  // 0–1000
    format!("{:04x}{:04x}{:04x}", h_raw, s_raw, v_raw)
}

// ─── HSV ↔ RGB conversion helpers ────────────────────────────────────────────

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

fn rgb_to_hsv(r: u8, g: u8, b: u8) -> (f32, f32, f32) {
    let r = r as f32 / 255.0;
    let g = g as f32 / 255.0;
    let b = b as f32 / 255.0;

    let max  = r.max(g).max(b);
    let min  = r.min(g).min(b);
    let diff = max - min;

    let v = max;
    let s = if max == 0.0 { 0.0 } else { diff / max };
    let h = if diff == 0.0 {
        0.0
    } else if max == r {
        60.0 * (((g - b) / diff) % 6.0)
    } else if max == g {
        60.0 * ((b - r) / diff + 2.0)
    } else {
        60.0 * ((r - g) / diff + 4.0)
    };
    let h = if h < 0.0 { h + 360.0 } else { h };
    (h, s, v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgb_hsv16_round_trip() {
        let (r, g, b) = (255, 128, 0);
        let hex       = rgb_to_hex_hsv16(r, g, b);
        assert_eq!(hex.len(), 12);
        let (r2, g2, b2) = parse_color_hex_hsv16(&hex).unwrap();
        // Allow ±5 rounding error
        assert!((r as i32 - r2 as i32).abs() <= 5);
        assert!((g as i32 - g2 as i32).abs() <= 5);
        assert!((b as i32 - b2 as i32).abs() <= 5);
    }

    #[test]
    fn rgb_rgb8_round_trip() {
        let hex = rgb_to_hex_rgb8(200, 100, 50);
        assert_eq!(hex.len(), 14);
        let (r, g, b) = parse_color_hex_rgb8(&hex).unwrap();
        assert_eq!((r, g, b), (200, 100, 50));
    }

    #[test]
    fn tuya_bulb_b_default() {
        let dm = DpMap::default();
        assert_eq!(dm.power_dp, 20);
        assert_eq!(dm.color_format, ColorFormat::Hsv16);
    }

    #[test]
    fn brightness_normalise() {
        let dm = DpMap::tuya_bulb_b();
        // Full brightness → 1000
        let native = dm.denormalize_brightness(1000);
        assert_eq!(native, 1000);
        let norm = dm.normalize_brightness(1000);
        assert_eq!(norm, 1000);
    }
}
