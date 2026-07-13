//! Non-destructive develop adjustment settings (per photo).

/// White balance presets.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WhiteBalancePreset {
    AsShot,
    Auto,
    Daylight,
    Cloudy,
    Shade,
    Tungsten,
    Fluorescent,
    Flash,
    Custom,
}

impl WhiteBalancePreset {
    pub fn name(&self) -> &'static str {
        match self {
            Self::AsShot => "As Shot",
            Self::Auto => "Auto",
            Self::Daylight => "Daylight",
            Self::Cloudy => "Cloudy",
            Self::Shade => "Shade",
            Self::Tungsten => "Tungsten",
            Self::Fluorescent => "Fluorescent",
            Self::Flash => "Flash",
            Self::Custom => "Custom",
        }
    }

    /// All presets that can be applied (excluding Custom).
    pub const ALL: &'static [WhiteBalancePreset] = &[
        Self::AsShot,
        Self::Auto,
        Self::Daylight,
        Self::Cloudy,
        Self::Shade,
        Self::Tungsten,
        Self::Fluorescent,
        Self::Flash,
    ];

    /// Kelvin value this preset targets (ignored for AsShot and Auto).
    fn target_kelvin(&self) -> Option<f32> {
        match self {
            Self::Daylight => Some(5500.0),
            Self::Cloudy => Some(6500.0),
            Self::Shade => Some(7500.0),
            Self::Tungsten => Some(3200.0),
            Self::Fluorescent => Some(4000.0),
            Self::Flash => Some(5500.0),
            _ => None,
        }
    }

    fn target_tint(&self) -> Option<f32> {
        match self {
            Self::Shade => Some(5.0),
            Self::Fluorescent => Some(10.0),
            _ => None,
        }
    }
}

/// Neutral Kelvin used when mapping relative temp ↔ absolute Kelvin.
const TEMP_BASE_K: f32 = 5500.0;
/// Kelvin at the warm end of the slider (temp = +100).
const TEMP_WARM_K: f32 = 2000.0;
/// Kelvin at the cool end of the slider (temp = -100).
const TEMP_COOL_K: f32 = 25000.0;

/// Map absolute Kelvin to relative UI temp using an exponential mapping
/// that spans 2000K..25000K across -100..+100.
pub fn kelvin_to_temp(k: f32) -> f32 {
    let k = k.clamp(TEMP_WARM_K, TEMP_COOL_K);
    if k >= TEMP_BASE_K {
        -100.0 * (k / TEMP_BASE_K).ln() / (TEMP_COOL_K / TEMP_BASE_K).ln()
    } else {
        100.0 * (k / TEMP_BASE_K).ln() / (TEMP_WARM_K / TEMP_BASE_K).ln()
    }
}

/// Map relative UI temp to absolute Kelvin.
pub fn temp_to_kelvin(temp: f32) -> f32 {
    let temp = temp.clamp(-100.0, 100.0);
    if temp >= 0.0 {
        TEMP_BASE_K * (TEMP_WARM_K / TEMP_BASE_K).powf(temp / 100.0)
    } else {
        TEMP_BASE_K * (TEMP_COOL_K / TEMP_BASE_K).powf(-temp / 100.0)
    }
}

/// Develop adjustment settings (basic panel).
#[derive(Debug, Clone, PartialEq)]
pub struct DevelopSettings {
    // Light
    pub exposure: f32,
    pub contrast: f32,
    pub highlights: f32,
    pub shadows: f32,
    pub whites: f32,
    pub blacks: f32,
    // Presence
    pub clarity: f32,
    pub vibrance: f32,
    pub saturation: f32,
    // Color (relative; temp offset in UI units -100..100)
    pub temp: f32,
    pub tint: f32,
}

impl Default for DevelopSettings {
    fn default() -> Self {
        Self {
            exposure: 0.0,
            contrast: 0.0,
            highlights: 0.0,
            shadows: 0.0,
            whites: 0.0,
            blacks: 0.0,
            clarity: 0.0,
            vibrance: 0.0,
            saturation: 0.0,
            temp: 0.0,
            tint: 0.0,
        }
    }
}

impl DevelopSettings {
    /// True when every slider is at its neutral default.
    pub fn is_identity(&self) -> bool {
        *self == Self::default()
    }

    /// Light-panel params used by the pixel tone stage.
    pub fn tone(&self) -> ToneParams {
        ToneParams {
            exposure: self.exposure,
            contrast: self.contrast,
            highlights: self.highlights,
            shadows: self.shadows,
            whites: self.whites,
            blacks: self.blacks,
            temp: self.temp,
            tint: self.tint,
            saturation: self.saturation,
        }
    }

    /// Kelvin value corresponding to the current temp offset.
    pub fn kelvin(&self) -> f32 {
        temp_to_kelvin(self.temp)
    }

    /// Set temp offset from a target Kelvin value.
    pub fn set_kelvin(&mut self, k: f32) {
        self.temp = kelvin_to_temp(k);
    }

    /// Determine which preset (if any) matches the current temp/tint values.
    pub fn wb_preset(&self) -> WhiteBalancePreset {
        if self.temp == 0.0 && self.tint == 0.0 {
            return WhiteBalancePreset::AsShot;
        }
        for p in WhiteBalancePreset::ALL {
            if *p == WhiteBalancePreset::AsShot || *p == WhiteBalancePreset::Auto {
                continue;
            }
            let k_target = match p.target_kelvin() {
                Some(k) => k,
                None => continue,
            };
            let t_target = p.target_tint().unwrap_or(0.0);
            if (self.temp - kelvin_to_temp(k_target)).abs() < 1.0
                && (self.tint - t_target).abs() < 1.0
            {
                return *p;
            }
        }
        WhiteBalancePreset::Custom
    }

    /// Apply a white balance preset, setting temp and tint accordingly.
    /// For AsShot, resets to identity. For Auto, does nothing (caller computes).
    pub fn apply_wb_preset(&mut self, preset: WhiteBalancePreset) {
        match preset {
            WhiteBalancePreset::AsShot => {
                self.temp = 0.0;
                self.tint = 0.0;
            }
            WhiteBalancePreset::Auto => {
                // Computed by caller via pipeline.
            }
            WhiteBalancePreset::Custom => {}
            _ => {
                if let Some(k) = preset.target_kelvin() {
                    self.set_kelvin(k);
                }
                self.tint = preset.target_tint().unwrap_or(0.0);
            }
        }
    }
}

/// Light-panel tone parameters applied in the develop pixel pipeline.
///
/// Ranges: `exposure` in EV stops; `temp`, `tint`, and other sliders `-100..=100`
/// (0 = identity).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ToneParams {
    pub exposure: f32,
    pub contrast: f32,
    pub highlights: f32,
    pub shadows: f32,
    pub whites: f32,
    pub blacks: f32,
    pub temp: f32,
    pub tint: f32,
    pub saturation: f32,
}

impl Default for ToneParams {
    fn default() -> Self {
        Self {
            exposure: 0.0,
            contrast: 0.0,
            highlights: 0.0,
            shadows: 0.0,
            whites: 0.0,
            blacks: 0.0,
            temp: 0.0,
            tint: 0.0,
            saturation: 0.0,
        }
    }
}

impl ToneParams {
    /// Exposure only; all other sliders neutral.
    pub fn exposure_only(exposure: f32) -> Self {
        Self {
            exposure,
            ..Self::default()
        }
    }

    /// True when no tone op changes pixels (all neutral).
    pub fn is_identity(&self) -> bool {
        *self == Self::default()
    }

    /// Compare tone params for re-render dirty checks.
    pub fn approx_eq(&self, other: &Self) -> bool {
        (self.exposure - other.exposure).abs() < 1e-6
            && (self.contrast - other.contrast).abs() < 1e-6
            && (self.highlights - other.highlights).abs() < 1e-6
            && (self.shadows - other.shadows).abs() < 1e-6
            && (self.whites - other.whites).abs() < 1e-6
            && (self.blacks - other.blacks).abs() < 1e-6
            && (self.temp - other.temp).abs() < 1e-6
            && (self.tint - other.tint).abs() < 1e-6
            && (self.saturation - other.saturation).abs() < 1e-6
    }

    /// Kelvin value corresponding to the current temp offset.
    pub fn kelvin(&self) -> f32 {
        temp_to_kelvin(self.temp)
    }
}
