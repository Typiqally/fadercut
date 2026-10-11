//! Mastering presets — single source of truth for loudness targets.
//!
//! A preset bundles a loudness target with a true-peak ceiling and the limiter
//! timing used when peaks have to be brought under that ceiling. A single constant
//! (linear) gain sets overall loudness; the limiter is only inserted when the gained
//! signal would otherwise exceed the ceiling.

use clap::ValueEnum;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Preset {
    pub name: &'static str,
    /// Integrated loudness (LUFS). `None` => keep the measured loudness.
    pub target_i: Option<f64>,
    /// True-peak ceiling (dBTP).
    pub target_tp: f64,
    /// Limiter look-ahead/attack (ms).
    pub lim_attack_ms: f64,
    /// Limiter release (ms).
    pub lim_release_ms: f64,
    pub description: &'static str,
}

impl Preset {
    /// Return a copy with any provided values replacing the preset defaults.
    pub fn with_overrides(self, target_i: Option<f64>, target_tp: Option<f64>) -> Self {
        Self {
            target_i: target_i.or(self.target_i),
            target_tp: target_tp.unwrap_or(self.target_tp),
            ..self
        }
    }
}

// Limiter timing, chosen by measurement on hard-groove material (see
// docs/2026-10-11-mastering-fix-plan.md): 100 ms release is the middle ground
// between bass distortion (shorter) and audible pumping plus lost loudness (longer).
// Attack between 5 and 10 ms made no measurable difference.
const ATTACK_MS: f64 = 5.0;
const RELEASE_MS: f64 = 100.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
pub enum PresetName {
    /// Loud master for platforms that do not normalise (-9 LUFS)
    #[default]
    Club,
    /// SoundCloud/Spotify/YouTube reference level (-14 LUFS)
    Streaming,
    /// Keep current loudness; only bring peaks under the ceiling
    EvenOnly,
}

impl PresetName {
    pub fn preset(self) -> Preset {
        match self {
            PresetName::Club => Preset {
                name: "club",
                target_i: Some(-9.0),
                target_tp: -1.0,
                lim_attack_ms: ATTACK_MS,
                lim_release_ms: RELEASE_MS,
                description: "Loud master for platforms that do not normalise.",
            },
            PresetName::Streaming => Preset {
                name: "streaming",
                target_i: Some(-14.0),
                target_tp: -1.0,
                lim_attack_ms: ATTACK_MS,
                lim_release_ms: RELEASE_MS,
                description: "SoundCloud/Spotify/YouTube reference level.",
            },
            PresetName::EvenOnly => Preset {
                name: "even-only",
                target_i: None,
                target_tp: -1.0,
                lim_attack_ms: ATTACK_MS,
                lim_release_ms: RELEASE_MS,
                description: "Keep current loudness; only bring peaks under the ceiling.",
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn club_targets_minus_nine() {
        assert_eq!(PresetName::Club.preset().target_i, Some(-9.0));
    }

    #[test]
    fn preset_names_match_cli_values() {
        for name in PresetName::value_variants() {
            let cli_value = name.to_possible_value().unwrap();
            assert_eq!(cli_value.get_name(), name.preset().name);
        }
    }

    #[test]
    fn with_overrides_changes_only_given_fields() {
        let base = PresetName::Club.preset();
        let out = base.with_overrides(Some(-11.0), None);
        assert_eq!(out.target_i, Some(-11.0));
        assert_eq!(out.target_tp, base.target_tp);
        assert_eq!(base.target_i, Some(-9.0));
    }

    #[test]
    fn with_overrides_none_returns_same_targets() {
        let out = PresetName::Streaming.preset().with_overrides(None, None);
        assert_eq!((out.target_i, out.target_tp), (Some(-14.0), -1.0));
    }

    #[test]
    fn even_only_has_no_fixed_target() {
        assert_eq!(PresetName::EvenOnly.preset().target_i, None);
    }
}
