//! Which model the app runs, and how it is loaded.
//!
//! Mirrors the desktop's preset table. The catalogue itself is
//! [`stemd_models`], so the artefact names and digests cannot drift between the
//! two binaries; what is duplicated here is the four-line decision of which
//! architecture each preset runs, because the desktop's version lives inside
//! its startup path and carries concerns (CUDA precision selection, model
//! switching) an app with no GPU selection does not have.
//!
//! Precision is `F32` for the first version. `F16` is worth about 1.3x on the
//! desktop and costs -54 dB against the model's own residual, but it is also
//! the first thing that turns into NaNs on a smaller GPU; correctness first,
//! then measure on an A18.

use std::path::Path;

use anyhow::Result;
use stemd_core::{HybridConfig, HybridSeparator, MlxConfig, MlxSeparator, Precision, Separate};
use stemd_models::Preset;

pub fn load(dir: &Path, preset: Preset) -> Result<Box<dyn Separate>> {
    let precision = Precision::F32;
    let defaults = MlxConfig::default();
    let hybrid = HybridConfig {
        overlap: defaults.overlap,
        vocals_precision: precision,
        drums_precision: precision,
        cascade: false,
    };
    match preset {
        Preset::Fast => Ok(Box::new(MlxSeparator::load(
            dir,
            preset.artefact(),
            MlxConfig {
                overlap: defaults.overlap,
                precision,
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
