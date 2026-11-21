//! The decks seen on Pro DJ Link, and what became of each track.
//!
//! Read-only: there is nothing to click. The row exists to answer "what is
//! loaded, and has stemd got it yet" while the separation happens in the
//! window's normal list below it.

use eframe::egui::{self, Color32, Stroke};

#[cfg(test)]
use crate::prolink::MetaView;
use crate::prolink::{FetchState, PlayState, Snapshot};

use super::theme::{CARD_RADIUS, Palette, Sheen};

const ROW_HEIGHT: f32 = 40.0;
const DOT_RADIUS: f32 = 3.5;
/// How many rows before the list scrolls, in rows. Half a row at the bottom is
/// the hint that there is more.
const VISIBLE_ROWS: f32 = 3.5;

pub fn list(
    ui: &mut egui::Ui,
    palette: &Palette,
    sheen: &mut Sheen,
    snapshot: Option<&Snapshot>,
    prolink: Option<&crate::prolink::ProLink>,
) {
    match snapshot {
        None => {
            ui.add_space(2.0);
            muted(
                ui,
                palette,
                "ProLink is off. Start stemd with --prolink to watch for decks.",
            );
        }
        Some(snapshot) if snapshot.decks.is_empty() => {
            ui.add_space(2.0);
            muted(
                ui,
                palette,
                &format!(
                    "Watching {}. No decks on the network yet.",
                    snapshot.interface
                ),
            );
        }
        Some(snapshot) => {
            egui::ScrollArea::vertical()
                .id_salt("prolink-decks")
                .max_height(visible_height(ui))
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    for deck in &snapshot.decks {
                        row(ui, palette, sheen, deck);
                    }
                });
        }
    }

    if let Some(prolink) = prolink {
        ui.add_space(6.0);
        if ui
            .small_button("Forget downloaded tracks")
            .on_hover_text(
                "Delete the copies stemd fetched from decks and the record that marks \
                 them as already downloaded. Loading one again fetches and separates it. \
                 Separated stems are separate; Empty cache removes those.",
            )
            .clicked()
        {
            prolink.forget_downloads();
        }
    }
}

fn visible_height(ui: &egui::Ui) -> f32 {
    ROW_HEIGHT * VISIBLE_ROWS + ui.spacing().item_spacing.y * (VISIBLE_ROWS - 1.0)
}

fn row(ui: &mut egui::Ui, palette: &Palette, sheen: &mut Sheen, deck: &crate::prolink::DeckView) {
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, ROW_HEIGHT), egui::Sense::hover());
    let inner = rect.shrink2(egui::vec2(12.0, 6.0));
    sheen.card(
        ui,
        rect,
        CARD_RADIUS,
        palette.card,
        Stroke::new(1.0, palette.outline),
    );

    let (title, detail, status, colour) = match &deck.loaded {
        Some(loaded) => {
            let title = loaded
                .meta
                .as_ref()
                .map(|meta| meta.title.clone())
                .filter(|title| !title.is_empty())
                .unwrap_or_else(|| format!("track {}", loaded.track_id));
            let mut parts = Vec::new();
            // Source and deck first: an artist name may be long, and the deck
            // is what tells someone where the track came from.
            parts.push(if loaded.source == deck.number {
                loaded.slot.name().to_owned()
            } else {
                format!("{} on deck {}", loaded.slot.name(), loaded.source)
            });
            parts.push(if deck.model.is_empty() {
                format!("deck {}", deck.number)
            } else {
                format!("{} deck {}", deck.model, deck.number)
            });
            // The address stays on the row even while a track is loaded: it is
            // what a mount or an NFS failure is looked up with.
            parts.push(deck.address.to_string());
            if let Some(meta) = &loaded.meta
                && !meta.artist.is_empty()
            {
                parts.push(meta.artist.clone());
            }
            if let FetchState::Failed(reason) = &loaded.fetch {
                parts.push(reason.clone());
            }
            let detail = parts.join(" · ");
            (
                title,
                detail,
                status_label(&loaded.fetch, loaded.play),
                colour_for(palette, &loaded.fetch),
            )
        }
        None => {
            let mut parts = Vec::new();
            if !deck.model.is_empty() {
                parts.push(deck.model.clone());
            }
            parts.push(format!("deck {}", deck.number));
            parts.push(deck.address.to_string());
            (
                "Nothing loaded".to_owned(),
                parts.join(" · "),
                String::new(),
                palette.muted,
            )
        }
    };

    let painter = ui.painter();
    painter.circle_filled(
        egui::pos2(inner.left() + DOT_RADIUS, inner.center().y),
        DOT_RADIUS,
        colour,
    );

    // Leave room for the status on the right; a long title is clipped instead of
    // painting over it.
    let status_width = 104.0;
    let clip = egui::Rect::from_min_max(
        egui::pos2(inner.left(), inner.top()),
        egui::pos2(inner.right() - status_width, inner.bottom()),
    );
    let text_left = inner.left() + DOT_RADIUS * 2.0 + 8.0;
    let text = painter.with_clip_rect(clip);
    text.text(
        egui::pos2(text_left, inner.center().y - 1.0),
        egui::Align2::LEFT_BOTTOM,
        title,
        egui::FontId::proportional(12.5),
        palette.text,
    );
    text.text(
        egui::pos2(text_left, inner.center().y + 2.0),
        egui::Align2::LEFT_TOP,
        detail,
        egui::FontId::proportional(10.5),
        palette.muted,
    );

    painter.text(
        egui::pos2(inner.right(), inner.center().y),
        egui::Align2::RIGHT_CENTER,
        status,
        egui::FontId::proportional(10.5),
        colour,
    );
}

fn muted(ui: &mut egui::Ui, palette: &Palette, text: &str) {
    ui.label(egui::RichText::new(text).size(11.5).color(palette.muted));
}

fn status_label(fetch: &FetchState, play: PlayState) -> String {
    match fetch {
        FetchState::Idle => play.name().to_owned(),
        FetchState::Resolving => "reading library".to_owned(),
        FetchState::Downloading { done, total } => {
            let percent = if *total == 0 {
                0
            } else {
                (done.saturating_mul(100) / total).min(100)
            };
            format!("downloading {percent}%")
        }
        FetchState::Downloaded => "downloaded".to_owned(),
        FetchState::Seen => "already downloaded".to_owned(),
        FetchState::Unavailable => "no metadata".to_owned(),
        FetchState::Failed(_) => "download failed".to_owned(),
    }
}

fn colour_for(palette: &Palette, fetch: &FetchState) -> Color32 {
    match fetch {
        FetchState::Resolving | FetchState::Downloading { .. } => palette.warn,
        FetchState::Downloaded | FetchState::Seen => palette.good,
        FetchState::Failed(_) => palette.bad,
        FetchState::Unavailable => palette.muted,
        FetchState::Idle => palette.accent,
    }
}

/// Kept for the window's tests to describe a metadata row without a real deck.
#[cfg(test)]
fn description(meta: &MetaView) -> String {
    if meta.artist.is_empty() {
        meta.title.clone()
    } else {
        format!("{} — {}", meta.title, meta.artist)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_download_reports_its_progress_and_never_past_100() {
        assert_eq!(
            status_label(
                &FetchState::Downloading {
                    done: 0,
                    total: 100
                },
                PlayState::Playing
            ),
            "downloading 0%"
        );
        assert_eq!(
            status_label(
                &FetchState::Downloading {
                    done: 200,
                    total: 100
                },
                PlayState::Playing
            ),
            "downloading 100%"
        );
        assert_eq!(
            status_label(&FetchState::Seen, PlayState::Playing),
            "already downloaded"
        );
    }

    #[test]
    fn an_idle_row_says_what_the_deck_is_doing() {
        assert_eq!(
            status_label(&FetchState::Idle, PlayState::Playing),
            "playing"
        );
        assert_eq!(status_label(&FetchState::Idle, PlayState::Paused), "paused");
    }

    #[test]
    fn metadata_falls_back_to_the_title_when_there_is_no_artist() {
        assert_eq!(
            description(&MetaView {
                title: "Title".into(),
                artist: String::new(),
            }),
            "Title"
        );
    }
}
