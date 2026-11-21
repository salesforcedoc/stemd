//! demucs v4 on MLX, through [`stemd_mlx`].
//!
//! The model is built layer by layer from a safetensors artefact rather than
//! loaded as a traced graph, so this file is only the adapter between
//! [`Audio`](crate::pcm::Audio) and MLX arrays, plus the geometry checks and
//! progress reporting the server expects.
//!
//! Two things a caller has to know:
//!
//! * MLX belongs to one thread, and that thread is the one that built it, not
//!   merely the one using it. The CUDA backend keeps its stream registry in
//!   thread-local storage, so weights allocated on another thread fail their
//!   first `eval` with `There is no Stream(gpu, N) in current thread`. The
//!   server's queue worker constructs its own separator and never accepts one:
//!   see `stemd_server::queue::BuildSeparator`.
//! * The whole track is held as one array. A six-minute stereo track is about
//!   140 MB in and the four separated sources about 560 MB out, which is the
//!   price of doing the overlap-add on the GPU.

use std::path::Path;

use anyhow::{Context, Result, bail};
use mlx_rs::Array;
use stemd_mlx::Precision;
use stemd_mlx::apply::{DEFAULT_OVERLAP, over_track};
use stemd_mlx::htdemucs::{Config, HtDemucs};
use stemd_mlx::weights::Weights;

use crate::backend::{BackendInfo, Separate};
use crate::mixture::{Normalisation, fold_into_shipped};
use crate::pcm::Audio;
use crate::progress::{Cancelled, Progress, ProgressSink, Stage};
use crate::stems::{FOUR_STEM_SOURCES, Stems, unexplained_db};

pub struct MlxConfig {
    /// Fraction of a segment shared with its neighbour. demucs uses 0.25.
    pub overlap: f32,
    /// What the encoder, transformer and decoder run at.
    ///
    /// [`Precision::F16`] is worth about 1.3x and costs -54 dB against the same model
    /// at [`Precision::F32`], 20 dB below htdemucs's own residual, which does not move.
    ///
    /// The normalisations accumulate in full precision whatever this says. Without
    /// that, half precision here produces NaN rather than separating slightly worse.
    pub precision: Precision,
}

impl Default for MlxConfig {
    fn default() -> Self {
        Self {
            overlap: DEFAULT_OVERLAP,
            precision: Precision::F32,
        }
    }
}

pub struct MlxSeparator {
    demucs: HtDemucs,
    /// The artefact's stem, e.g. `htdemucs`, for [`BackendInfo`].
    model: String,
    sources: Vec<String>,
    sample_rate: u32,
    channels: usize,
    config: MlxConfig,
}

impl MlxSeparator {
    /// Load `<dir>/<name>.safetensors`, which must hold exactly one model.
    ///
    /// There is no manifest beside it. The architecture is compiled in as [`Config`],
    /// every layer checks the shape of each tensor it pulls, and a file that is not
    /// this architecture fails at load naming the tensor that disagreed.
    pub fn load(dir: &Path, name: &str, config: MlxConfig) -> Result<Self> {
        let path = dir.join(format!("{name}.safetensors"));
        let weights = Weights::load(&path)?;
        //  Cast only on the way down. Widening a stored f16 artefact would compute the
        //  same, since mlx promotes and f16 to f32 is exact, but would double the memory
        //  the weights occupy, and every demucs artefact this loads is f32 already.
        //  `hybrid::open` casts unconditionally because its RoFormer half is published at
        //  f16.
        let weights = if config.precision == Precision::F16 {
            weights.cast(config.precision)?
        } else {
            weights
        };
        let architecture = || Config {
            precision: config.precision,
            ..Config::default()
        };

        //  How many models the file holds is asked of the tensor names rather than guessed
        //  from the filename. More than one means a fine-tuned set like `htdemucs_ft`,
        //  whose four checkpoints each produce a usable version of exactly one source:
        //  running it here would take all four sources from whichever was stored first.
        //  See [`crate::hybrid::HybridSeparator::demucs_specialists`].
        let demucs = match weights.models() {
            0 => bail!(
                "{} holds no model_N tensors; it is not a converted demucs artefact",
                path.display()
            ),
            1 => HtDemucs::load(&weights, "model_0", architecture())?,
            n => bail!(
                "{} holds {n} fine-tuned models; loading it as a single model would \
                 take every source from one of them. It needs a preset that says \
                 which model each source comes from.",
                path.display()
            ),
        };

        let architecture = Config::default();
        Ok(Self {
            demucs,
            model: name.to_owned(),
            sources: FOUR_STEM_SOURCES.iter().map(|s| (*s).into()).collect(),
            sample_rate: u32::try_from(architecture.sample_rate)?,
            channels: usize::try_from(architecture.audio_channels)?,
            config,
        })
    }

    pub const fn model(&self) -> &String {
        &self.model
    }

    /// Reject a mix the model cannot accept, before any work is done.
    fn check_geometry(&self, mix: &Audio) -> Result<()> {
        if mix.channels() != self.channels {
            bail!(
                "model expects {} channels, got {}",
                self.channels,
                mix.channels()
            );
        }
        if mix.sample_rate != self.sample_rate {
            bail!(
                "model expects {} Hz, got {} Hz",
                self.sample_rate,
                mix.sample_rate
            );
        }
        Ok(())
    }

    /// The normalised mixture as a `[1, C, T]` array.
    fn to_array(&self, mix: &Audio, norm: &Normalisation) -> Result<Array> {
        let frames = mix.frames();
        stemd_mlx::trace::note("to_array: begin");
        let mut flat = Vec::with_capacity(self.channels * frames);
        for channel in &mix.data {
            flat.extend(channel.iter().map(|&v| norm.apply(v)));
        }
        let shape = [1, i32::try_from(self.channels)?, i32::try_from(frames)?];
        stemd_mlx::trace::note("to_array: flat built");
        let array = Array::from_slice(&flat, &shape);
        stemd_mlx::trace::note("to_array: array built");
        Ok(array)
    }

    /// One `[1, C, T]` result per model source back into [`Audio`], with the
    /// normalisation undone.
    fn to_sources(&self, out: &[Array], norm: &Normalisation, frames: usize) -> Result<Vec<Audio>> {
        let mut sources = Vec::with_capacity(out.len());
        for array in out {
            // Evaluated already by `over_track`, but reading the buffer of a
            // lazy array is undefined rather than merely slow, so this is not
            // optional.
            mlx_rs::transforms::eval([array])?;
            let flat = array
                .try_as_slice::<f32>()
                .map_err(|e| anyhow::anyhow!("the separated track is not contiguous f32: {e:?}"))?;
            let data = (0..self.channels)
                .map(|c| {
                    flat[c * frames..(c + 1) * frames]
                        .iter()
                        .map(|&v| norm.restore(v))
                        .collect()
                })
                .collect();
            sources.push(Audio::new(data, self.sample_rate));
        }
        Ok(sources)
    }
}

impl MlxSeparator {
    /// The whole track in one go.
    ///
    /// Fine where the GPU can hold arrays the length of the track. On iOS it
    /// cannot for anything deck-length: see [`Self::separate_windows`].
    fn separate_whole(&mut self, mix: &Audio, sink: &dyn ProgressSink) -> Result<Stems> {
        stemd_mlx::trace::note("sep: begin");
        self.check_geometry(mix)?;
        sink.update(Progress::new(Stage::Analysing));

        let frames = mix.frames();
        if frames == 0 {
            bail!("nothing to separate: the mix has no frames");
        }
        let norm = Normalisation::of(mix);
        let input = self.to_array(mix, &norm)?;
        stemd_mlx::trace::note("sep: input array");

        // Cancellation costs one segment rather than one track, because the
        // callback can refuse to continue and `over_track` stops where it is.
        // The partial overlap-add is worth nothing on its own and is dropped.
        let separated = over_track(
            &self.demucs,
            &input,
            self.config.overlap,
            &mut |done, total| {
                if sink.cancelled() {
                    return Err(Cancelled.into());
                }
                sink.update(Progress::counted(
                    Stage::Separating,
                    u32::try_from(done).unwrap_or(u32::MAX),
                    u32::try_from(total).unwrap_or(u32::MAX),
                ));
                Ok(())
            },
        )?;
        stemd_mlx::trace::note("sep: over_track done");

        sink.update(Progress::new(Stage::Reconstructing));
        let sources = self.to_sources(&separated, &norm, frames)?;
        stemd_mlx::trace::note("sep: sources");

        Ok(Stems {
            model_residual_db: unexplained_db(mix, &sources),
            shipped: fold_into_shipped(&sources, &self.sources)
                .context("folding the model's sources into the shipped stems")?,
        })
    }

    /// Separate a track in windows, crossfading the seams.
    ///
    /// iOS only. The whole-track path builds arrays as long as the track, and
    /// on an iPad an eight-minute track's array takes the process down at the
    /// first evaluation even with gigabytes free; two-minute tracks have been
    /// running all along. Window sizes that work are a smaller and safer change
    /// than teaching the backend to like the large ones.
    ///
    /// The shipped stems are plain vectors, so the crossfade and the
    /// accumulation are host work: no window ever becomes an array longer than
    /// [`chunk`].
    #[cfg(target_os = "ios")]
    fn separate_windows(
        &mut self,
        mix: &Audio,
        sink: &dyn ProgressSink,
        chunk: usize,
        overlap: usize,
    ) -> Result<Stems> {
        let frames = mix.frames();
        let channels = self.channels;
        let mut windows = Vec::new();
        let mut start = 0usize;
        while start < frames {
            let end = (start + chunk).min(frames);
            windows.push((start, end));
            if end == frames {
                break;
            }
            start += chunk - overlap;
        }
        let count = windows.len();
        stemd_mlx::trace::note(&format!("sep: {} windows of {chunk} frames", count));

        let mut merged: Vec<(&'static str, Vec<Vec<f32>>)> = Vec::new();
        let mut merged_len = 0usize;
        let mut residual = 0.0f64;
        for (index, &(from, to)) in windows.iter().enumerate() {
            let slice = Audio::new(
                (0..channels)
                    .map(|c| mix.data[c][from..to].to_vec())
                    .collect(),
                mix.sample_rate,
            );
            let window_sink = WindowSink {
                inner: sink,
                index,
                count,
            };
            let stems = self.separate_whole(&slice, &window_sink)?;
            residual += stems.model_residual_db;

            if index == 0 {
                merged = stems
                    .shipped
                    .into_iter()
                    .map(|(name, audio)| (name, audio.data))
                    .collect();
                merged_len = to - from;
            } else {
                let shared = merged_len.saturating_sub(from);
                for (name, audio) in stems.shipped {
                    if let Some((_, data)) = merged.iter_mut().find(|(known, _)| *known == name) {
                        crossfade_append(data, audio.data, shared);
                    }
                }
                merged_len += (to - from) - shared;
            }

            // MLX keeps every buffer a window's graph frees. One window's worth
            // (about 2 GB here) is survivable; two is not, and the app is
            // killed before the job-level release ever runs. Give it back after
            // each window: the model is live and stays, everything else is dead.
            let freed = stemd_mlx::memory::release();
            stemd_mlx::trace::note(&format!(
                "sep: window {index}/{count} done, released {freed}"
            ));
        }

        stemd_mlx::trace::note("sep: windows merged");
        Ok(Stems {
            model_residual_db: if count == 0 {
                0.0
            } else {
                residual / count as f64
            },
            shipped: merged
                .into_iter()
                .map(|(name, data)| (name, Audio::new(data, self.sample_rate)))
                .collect(),
        })
    }
}

impl Separate for MlxSeparator {
    fn separate(&mut self, mix: &Audio, sink: &dyn ProgressSink) -> Result<Stems> {
        #[cfg(target_os = "ios")]
        {
            const CHUNK_SECS: usize = 120;
            const OVERLAP_SECS: usize = 2;
            let chunk = CHUNK_SECS * self.sample_rate as usize;
            if mix.frames() > chunk {
                let overlap = OVERLAP_SECS * self.sample_rate as usize;
                return self.separate_windows(mix, sink, chunk, overlap);
            }
        }
        self.separate_whole(mix, sink)
    }

    fn info(&self) -> BackendInfo {
        BackendInfo {
            backend: "mlx".into(),
            model: self.model.clone(),
            sample_rate: self.sample_rate,
            channels: self.channels,
            stems: crate::stems::SHIPPED.iter().map(|s| (*s).into()).collect(),
            // Asked rather than asserted. This was the literal `"gpu"` for as
            // long as Metal was the only backend and the answer could not be
            // anything else; a CUDA build on a machine with no usable card
            // falls back to the CPU and used to say otherwise.
            device: stemd_mlx::Accelerator::detect().as_str().into(),
        }
    }
}

/// Maps one window's progress onto the whole track's.
#[cfg(target_os = "ios")]
struct WindowSink<'a> {
    inner: &'a dyn ProgressSink,
    index: usize,
    count: usize,
}

#[cfg(target_os = "ios")]
impl ProgressSink for WindowSink<'_> {
    fn update(&self, progress: Progress) {
        self.inner
            .update(window_progress(self.index, self.count, progress));
    }

    fn cancelled(&self) -> bool {
        self.inner.cancelled()
    }
}

/// Map one window's progress onto the whole track's.
///
/// Each window runs its own analysing/separating/reconstructing pass and
/// restarts its counters, so the inner stage and counts describe that window,
/// not the track. On the track they are all one separating pass: reporting
/// them verbatim made a client that draws a bar from `completed/total` run it
/// backwards every two minutes, and made the stage flip to reconstructing
/// between windows.
///
/// `fraction` was always monotonic across windows. The stage and counters now
/// are too: separating throughout, in thousandths of the whole track.
#[cfg(any(target_os = "ios", test))]
fn window_progress(index: usize, count: usize, progress: Progress) -> Progress {
    let count = count.max(1) as f32;
    let fraction = ((index as f32 + progress.fraction.clamp(0.0, 1.0)) / count).clamp(0.0, 1.0);
    Progress {
        stage: Stage::Separating,
        completed: (fraction * 1000.0).round() as u32,
        total: 1000,
        fraction,
        detail: progress.detail,
    }
}

/// Append `next` to `into`, crossfading the `shared` samples they overlap.
///
/// A linear ramp over the shared region. Both sides are the same audio through
/// the same model, so any smooth pair that sums to one will do; the ramp is
/// offset by half a sample so neither end gets zero weight.
#[cfg(target_os = "ios")]
fn crossfade_append(into: &mut [Vec<f32>], next: Vec<Vec<f32>>, shared: usize) {
    let available = into
        .first()
        .map_or(0, Vec::len)
        .min(next.first().map_or(0, Vec::len));
    let shared = shared.min(available);
    if shared > 0 {
        let len = into[0].len();
        for (c, channel) in into.iter_mut().enumerate() {
            for i in 0..shared {
                let t = (i as f32 + 0.5) / shared as f32;
                let tail = len - shared + i;
                channel[tail] = channel[tail] * (1.0 - t) + next[c][i] * t;
            }
        }
    }
    for (c, channel) in into.iter_mut().enumerate() {
        channel.extend_from_slice(&next[c][shared..]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windowed_progress_never_runs_backwards() {
        // The shape a deck saw on a three-window track: each window restarts
        // its own 21-chunk counter and dips into reconstructing at its end.
        let updates = [
            Progress::counted(Stage::Separating, 17, 21),
            Progress::new(Stage::Reconstructing),
            Progress::counted(Stage::Separating, 2, 21),
            Progress::new(Stage::Reconstructing),
            Progress::counted(Stage::Separating, 5, 7),
            Progress::new(Stage::Reconstructing),
        ];
        let seen: Vec<_> = updates
            .into_iter()
            .enumerate()
            .map(|(i, progress)| window_progress(i / 2, 3, progress))
            .collect();

        for pair in seen.windows(2) {
            assert!(
                pair[0].fraction <= pair[1].fraction,
                "fraction went backwards: {:?} -> {:?}",
                pair[0],
                pair[1]
            );
            assert!(
                pair[0].completed <= pair[1].completed,
                "completed went backwards: {} -> {}",
                pair[0].completed,
                pair[1].completed
            );
        }
        assert!(seen.iter().all(|p| p.stage == Stage::Separating));
        assert_eq!(seen[0].total, 1000);
    }
}
