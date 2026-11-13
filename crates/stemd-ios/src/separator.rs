//! Which model the app runs, and how it is loaded.
//!
//! Mirrors the desktop's preset table. The catalogue itself is
//! [`stemd_models`], so the artefact names and digests cannot drift between the
//! two binaries; what is duplicated here is the four-line decision of which
//! architecture each preset runs, because the desktop's version lives inside
//! its startup path and carries concerns (CUDA precision selection, model
//! switching) an app with no GPU selection does not have.
//!
//! Precision follows the desktop's rule: half precision on Metal, full on the
//! CPU. `F16` is worth about 1.3x on htdemucs and costs -54 dB against the
//! model's own residual; the simulator is CPU-only, where MLX's `layer_norm`
//! loses enough accuracy that full precision is the right answer regardless of
//! speed. The chosen precision is part of the model identity the cache is keyed
//! on, so stems separated before a precision change cannot be mistaken for the
//! new ones.

use std::path::Path;

use anyhow::Result;
use stemd_core::{
    Accelerator, Family, HybridConfig, HybridSeparator, MlxConfig, MlxSeparator, Precision,
    Separate,
};
use stemd_models::Preset;

/// What each model in a preset runs at on this device.
///
/// The first value is the vocals half (the RoFormer in `Quality`), the second
/// the drums half (always demucs). The two agree on every backend this app can
/// run on, but they are kept separate because that is the shape the hybrid
/// configuration and the identity need.
fn precisions(preset: Preset) -> (Precision, Precision) {
    let on = Accelerator::detect();
    let demucs = Precision::preferred(Family::HtDemucs, on);
    match preset {
        Preset::Fast | Preset::Balanced => (demucs, demucs),
        Preset::Quality => (Precision::preferred(Family::Roformer, on), demucs),
    }
}

/// The precision part of the model identity a cache is keyed on.
///
/// Same spelling as the desktop's (`f16`, `f32`, or `f16+f32` when a preset's
/// models disagree), so a deck that stores the identity sees the same shape from
/// either server.
pub fn precision_key(preset: Preset) -> String {
    let (vocals, drums) = precisions(preset);
    if vocals == drums {
        vocals.to_string()
    } else {
        format!("{vocals}+{drums}")
    }
}

pub fn load(dir: &Path, preset: Preset) -> Result<Box<dyn Separate>> {
    let (vocals_precision, drums_precision) = precisions(preset);
    let defaults = MlxConfig::default();
    let hybrid = HybridConfig {
        overlap: defaults.overlap,
        vocals_precision,
        drums_precision,
        cascade: false,
    };
    match preset {
        Preset::Fast => Ok(Box::new(MlxSeparator::load(
            dir,
            preset.artefact(),
            MlxConfig {
                overlap: defaults.overlap,
                precision: drums_precision,
            },
        )?)),
        // Two of the artefact's four fine-tuned models: the vocals specialist
        // and the drums specialist, with harmonics as the remainder. See
        // `docs/evaluation.md`.
        Preset::Balanced => Ok(Box::new(HybridSeparator::demucs_specialists(
            dir,
            preset.artefact(),
            hybrid,
        )?)),
        // Two artefacts: the vocals model first, then demucs on what is left.
        Preset::Quality => {
            let names = preset.artefacts();
            Ok(Box::new(HybridSeparator::roformer_and_demucs(
                dir,
                names[0],
                names[1],
                HybridConfig {
                    cascade: true,
                    ..hybrid
                },
            )?))
        }
    }
}
