//! The now-playing bar along the bottom of the window.

use egui::{Align, Frame, Layout, Margin, Rect, Sense, UiBuilder, Vec2, pos2, vec2};

use crate::app::{App, NowPlaying};
use crate::model::{Action, DragTrack, Page};
use crate::player::RepeatMode;
use crate::theme::{self, Icon};
use crate::util;

use super::widgets::{SliderEvent, thin_slider};

pub fn show(app: &mut App, ui: &mut egui::Ui) {
    let palette = app.palette;
    let tint = app.now_playing_tint();
    let fill = match tint {
        Some(tint) => super::blend(palette.panel, tint, 0.12),
        None => palette.panel,
    };
    egui::Panel::bottom("player-bar")
        .exact_size(theme::PLAYER_BAR_HEIGHT)
        .resizable(false)
        .show_separator_line(false)
        .frame(
            Frame::new()
                .fill(fill)
                .inner_margin(Margin::symmetric(16, 0)),
        )
        .show(ui, |ui| {
            let rect = ui.max_rect();
            ui.painter().hline(
                rect.x_range(),
                rect.top() + 0.5,
                egui::Stroke::new(1.0, palette.outline),
            );
            let now = app.now_playing();
            let width = rect.width();
            let side = (width * 0.3).clamp(200.0, 420.0);
            let cy = rect.center().y;
            let left = Rect::from_min_max(rect.min, pos2(rect.left() + side, rect.bottom()));
            let center = Rect::from_min_max(
                pos2(rect.left() + side, rect.top()),
                pos2(rect.right() - side, rect.bottom()),
            );

            // egui's cross-axis centring is unreliable across nested layouts of
            // mixed heights, so each region is placed in an explicit band that
            // is sized to its content and centred on the bar's midline.
            now_playing_block(app, ui, left, now.as_ref());

            transport(app, ui, now.as_ref(), center);

            let right_band =
                Rect::from_min_size(pos2(rect.right() - side, cy - 15.0), vec2(side, 30.0));
            let mut right_ui = ui.new_child(
                UiBuilder::new()
                    .max_rect(right_band)
                    .layout(Layout::right_to_left(Align::Center)),
            );
            extras(app, &mut right_ui, now.as_ref());
        });
}

fn now_playing_block(app: &mut App, ui: &mut egui::Ui, region: Rect, now: Option<&NowPlaying>) {
    let palette = app.palette;
    let cy = region.center().y;
    let cover_rect = Rect::from_min_size(pos2(region.left() + 4.0, cy - 28.0), Vec2::splat(56.0));

    let Some(now) = now else {
        super::widgets::paint_cover(ui, &palette, None, cover_rect, 6.0, Icon::Music, None);
        let text_left = cover_rect.right() + 12.0;
        let text_rect = Rect::from_min_size(
            pos2(text_left, cy - 17.0),
            vec2((region.right() - text_left - 8.0).max(40.0), 34.0),
        );
        let mut text_ui = ui.new_child(
            UiBuilder::new()
                .max_rect(text_rect)
                .layout(Layout::top_down(Align::Min)),
        );
        text_ui.spacing_mut().item_spacing.y = 2.0;
        theme::text(
            &mut text_ui,
            "Nothing playing",
            theme::medium(14.0),
            palette.secondary,
        );
        theme::text(
            &mut text_ui,
            "Pick a song, album, or playlist",
            theme::regular(12.0),
            palette.dim,
        );
        return;
    };

    super::widgets::paint_cover(
        ui,
        &palette,
        now.art_small.as_deref().or(now.art_url.as_deref()),
        cover_rect,
        6.0,
        Icon::Music,
        Some(app.backend.art()),
    );
    let song = app.now_playing_item();
    let drag_sense = if song.is_some() {
        Sense::click_and_drag()
    } else {
        Sense::click()
    };
    let cover_response = ui
        .interact(cover_rect, egui::Id::new("now-playing-cover"), drag_sense)
        .on_hover_cursor(if song.is_some() {
            egui::CursorIcon::Grab
        } else {
            egui::CursorIcon::PointingHand
        });
    // Hovering the cover offers to dock the art large at the sidebar's
    // bottom, the way Spotify expands it. (#92)
    let art_available = now.art_url.is_some() || now.art_small.is_some();
    let expand_rect = Rect::from_center_size(
        pos2(cover_rect.right() - 10.0, cover_rect.top() + 10.0),
        Vec2::splat(18.0),
    );
    let offer_expand = art_available && !app.settings.art_expanded && app.settings.sidebar_visible;
    let over_expand = offer_expand && ui.rect_contains_pointer(expand_rect);
    if cover_response.clicked() && !over_expand {
        if let Some(id) = &now.album_id {
            app.actions.push(Action::Open(Page::Album(id.clone())));
        } else if let Some(id) = &now.show_id {
            app.actions.push(Action::Open(Page::Show(id.clone())));
        }
    }
    if offer_expand && (cover_response.hovered() || over_expand) {
        let expand = ui
            .interact(
                expand_rect,
                egui::Id::new("now-playing-art-expand"),
                Sense::click(),
            )
            .on_hover_cursor(egui::CursorIcon::PointingHand);
        ui.painter()
            .circle_filled(expand_rect.center(), 9.0, palette.panel.gamma_multiply(0.9));
        Icon::ChevronUp.image(palette.text, 12.0).paint_at(
            ui,
            Rect::from_center_size(expand_rect.center(), Vec2::splat(12.0)),
        );
        if expand.clicked() {
            app.settings.art_expanded = true;
            app.actions.push(Action::SettingsChanged);
        }
    }
    let heart_width = if now.is_episode { 0.0 } else { 42.0 };
    let text_left = cover_rect.right() + 12.0;
    let text_width = (region.right() - text_left - heart_width).max(40.0);
    let text_rect = Rect::from_min_size(pos2(text_left, cy - 18.0), vec2(text_width, 36.0));
    let info_response = ui.interact(text_rect, egui::Id::new("now-playing-info"), drag_sense);
    let mut text_ui = ui.new_child(
        UiBuilder::new()
            .max_rect(text_rect)
            .layout(Layout::top_down(Align::Min)),
    );
    text_ui.set_clip_rect(text_rect.intersect(ui.clip_rect()));
    text_ui.spacing_mut().item_spacing.y = 2.0;
    let title_response = theme::link(&mut text_ui, &now.title, theme::medium(14.0), palette.text);
    if title_response.clicked() {
        if let Some(id) = &now.album_id {
            app.actions.push(Action::Open(Page::Album(id.clone())));
        } else if let Some(id) = &now.show_id {
            app.actions.push(Action::Open(Page::Show(id.clone())));
        }
    }
    text_ui.horizontal_top(|ui| {
        if now.artists.is_empty() {
            if theme::link(ui, &now.subtitle, theme::regular(12.0), palette.secondary).clicked()
                && let Some(id) = &now.show_id
            {
                app.actions.push(Action::Open(Page::Show(id.clone())));
            }
        } else {
            super::widgets::artist_links(
                ui,
                app,
                &now.artists,
                theme::regular(12.0),
                palette.secondary,
            );
        }
    });
    if (cover_response.drag_started_by(egui::PointerButton::Primary)
        || info_response.drag_started_by(egui::PointerButton::Primary))
        && let Some(item) = &song
    {
        egui::DragAndDrop::set_payload(
            ui.ctx(),
            DragTrack {
                uri: item.uri().to_string(),
                title: item.name().to_string(),
                image: item.image(64).map(str::to_string),
                item: item.clone(),
                from: None,
            },
        );
    }

    // The playing thing answers the same right-click menu as a table row,
    // from the cover, the empty space around the words, or the words.
    if let Some(item) = song {
        for response in [&cover_response, &info_response, &title_response] {
            egui::Popup::context_menu(response)
                .frame(super::widgets::menu_frame(&palette))
                .show(|ui| super::widgets::item_menu(ui, app, &item, None, None));
        }
    }

    if !now.is_episode {
        let saved = app.is_saved(&now.uri).unwrap_or(false);
        let (icon, color, tooltip) = if saved {
            (Icon::HeartFilled, palette.accent, "Remove from Liked Songs")
        } else {
            (Icon::Heart, palette.secondary, "Save to Liked Songs")
        };
        // Sit the heart just past the actual text, not at the region's far
        // edge, so it stays visually attached to the title.
        let natural = {
            let title =
                ui.painter()
                    .layout_no_wrap(now.title.clone(), theme::medium(14.0), palette.text);
            let subtitle = ui.painter().layout_no_wrap(
                now.subtitle.clone(),
                theme::regular(12.0),
                palette.secondary,
            );
            title.size().x.max(subtitle.size().x).min(text_width)
        };
        let heart_x = (text_left + natural + 21.0).min(region.right() - 21.0);
        let heart_rect = Rect::from_center_size(pos2(heart_x, cy), Vec2::splat(30.0));
        let mut heart_ui = ui.new_child(
            UiBuilder::new()
                .max_rect(heart_rect)
                .layout(Layout::centered_and_justified(egui::Direction::LeftToRight)),
        );
        if theme::icon_button(&mut heart_ui, icon, 17.0, color, palette.text, tooltip).clicked() {
            app.actions.push(Action::ToggleSaved(now.uri.clone()));
        }
    }
}

fn transport(app: &mut App, ui: &mut egui::Ui, now: Option<&NowPlaying>, region: Rect) {
    let palette = app.palette;
    // Everything here is placed with explicit rects: egui's implicit rows
    // centre each widget in the row height known when it is added, which
    // left earlier icons riding high next to the play disc.
    //
    // The buttons row (36) and the progress row (~15, after a 6px gap) form
    // one cluster, centred as a group in the 88px bar: the buttons sit 8px
    // above the bar's midline and the progress row 23px below it. Measured
    // on screen this puts equal breathing room above and beneath the
    // cluster.
    let cy = region.center().y - 8.0;
    let enabled = now.is_some_and(|now| now.can_control) || app.is_connected();
    let playing = now.is_some_and(|now| now.playing);
    let loading = now.is_some_and(|now| now.loading);
    let shuffle = now.is_some_and(|now| now.shuffle);
    let repeat = now.map(|now| now.repeat).unwrap_or_default();
    let dim = if enabled {
        palette.secondary
    } else {
        palette.dim
    };

    // Button widths: icon buttons occupy icon size + 12; the disc is 36.
    let widths = [29.0, 30.0, 36.0, 30.0, 29.0];
    let gap = 10.0;
    let total: f32 = widths.iter().sum::<f32>() + gap * 4.0;
    let mut x = region.center().x - total / 2.0;
    let mut slot = |width: f32| {
        let rect = Rect::from_center_size(pos2(x + width / 2.0, cy), vec2(width, 36.0));
        x += width + gap;
        rect
    };
    let centered = |ui: &mut egui::Ui, rect: Rect| {
        ui.new_child(
            UiBuilder::new()
                .max_rect(rect)
                .layout(Layout::centered_and_justified(egui::Direction::LeftToRight)),
        )
    };

    let shuffle_color = if shuffle { palette.accent } else { dim };
    let mut cell = centered(ui, slot(widths[0]));
    let shuffle_button = theme::icon_button(
        &mut cell,
        Icon::Shuffle,
        17.0,
        shuffle_color,
        if shuffle {
            palette.accent_hover
        } else {
            palette.text
        },
        "Shuffle",
    );
    shuffle_button.widget_info(|| {
        egui::WidgetInfo::selected(
            egui::WidgetType::Checkbox,
            cell.is_enabled(),
            shuffle,
            "Shuffle",
        )
    });
    if shuffle_button.clicked() {
        app.actions.push(Action::ToggleShuffle);
    }

    let mut cell = centered(ui, slot(widths[1]));
    if theme::icon_button(
        &mut cell,
        Icon::SkipBackFilled,
        18.0,
        dim,
        palette.text,
        "Previous",
    )
    .clicked()
    {
        app.actions.push(Action::Previous);
    }

    let disc = slot(widths[2]);
    if loading || app.any_play_pending() {
        ui.painter()
            .circle_filled(disc.center(), 18.0, palette.text);
        let mut cell = centered(ui, disc);
        theme::spinner(&mut cell, 22.0, palette.window);
    } else {
        let icon = if playing {
            Icon::PauseFilled
        } else {
            Icon::PlayFilled
        };
        let hover = if palette.dark {
            egui::Color32::WHITE
        } else {
            palette.text
        };
        let mut cell = centered(ui, disc);
        if theme::circle_button(
            &mut cell,
            icon,
            36.0,
            palette.text,
            hover,
            palette.window,
            if playing { "Pause" } else { "Play" },
        )
        .clicked()
        {
            app.actions.push(Action::TogglePlay);
        }
    }

    let mut cell = centered(ui, slot(widths[3]));
    if theme::icon_button(
        &mut cell,
        Icon::SkipForwardFilled,
        18.0,
        dim,
        palette.text,
        "Next",
    )
    .clicked()
    {
        app.actions.push(Action::Next);
    }

    let (repeat_icon, repeat_color, tooltip) = match repeat {
        RepeatMode::Off => (Icon::Repeat, dim, "Repeat"),
        RepeatMode::Context => (Icon::Repeat, palette.accent, "Repeat one"),
        RepeatMode::Track => (Icon::Repeat1, palette.accent, "Repeat off"),
    };
    let mut cell = centered(ui, slot(widths[4]));
    if theme::icon_button(
        &mut cell,
        repeat_icon,
        17.0,
        repeat_color,
        if repeat == RepeatMode::Off {
            palette.text
        } else {
            palette.accent_hover
        },
        tooltip,
    )
    .clicked()
    {
        app.actions.push(Action::CycleRepeat);
    }

    // Progress row, just below the buttons (disc bottom + 6px gap + half of
    // the time text's line height).
    let row_cy = cy + 31.0;
    let slider_width = (region.width() - 120.0).clamp(120.0, 620.0);
    let (position, duration) = now
        .map(|now| (now.position_ms, now.duration_ms))
        .unwrap_or((0, 0));
    let shown_position = match app.seek_preview {
        Some(fraction) => (fraction * duration as f32) as u32,
        None => position,
    };
    let time_color = if now.is_some() {
        palette.secondary
    } else {
        palette.dim
    };
    let slider_left = region.center().x - slider_width / 2.0;
    ui.painter().text(
        pos2(slider_left - 8.0, row_cy),
        egui::Align2::RIGHT_CENTER,
        util::format_duration_ms(shown_position),
        theme::regular(11.5),
        time_color,
    );
    let slider_rect =
        Rect::from_center_size(pos2(region.center().x, row_cy), vec2(slider_width, 16.0));
    // The transition, drawn on the bar the listener drags. It is the one
    // place in the interface where a moment in the track has a position, so a
    // plan can be read against the track rather than guessed at from a log.
    // The shared view is read once, for both the drawing and the log below.
    if now.is_some() {
        let view = match app.automix_view.lock() {
            Ok(view) => *view,
            Err(poison) => *poison.into_inner(),
        };
        transition_marks(ui, &view, slider_rect, &palette, duration);
        // And recorded, so the numbers behind that drawing can be checked
        // without having to describe pixels back. Written when the plan
        // changes and not on every frame, which is what keeps it to one line
        // per boundary.
        let signature = (
            view.fade_out_at.map(f64::to_bits),
            view.fade_in_at.map(f64::to_bits),
            view.playing_cuepoints.is_some(),
            view.incoming_cuepoints.is_some(),
        );
        if app.automix_logged != Some(signature) {
            app.automix_logged = Some(signature);
            // A line per boundary, on whichever side of it first exists: the
            // cue can land long before a plan is built from it, and that is
            // exactly the window in which the arrival has a value and nothing
            // has acted on it yet.
            let worth_writing = view.fade_out_at.is_some()
                || view.fade_in_at.is_some()
                || view.playing_cuepoints.is_some()
                || view.incoming_cuepoints.is_some();
            if worth_writing {
                let track = now.map(|now| now.uri.as_str()).unwrap_or("?");
                log_transition(track, &view, position);
            }
        }
    }
    let mut slider_ui = ui.new_child(
        UiBuilder::new()
            .max_rect(slider_rect)
            .layout(Layout::left_to_right(Align::Center)),
    );
    let fraction = if duration > 0 {
        position as f32 / duration as f32
    } else {
        0.0
    };
    match thin_slider(
        &mut slider_ui,
        &palette,
        egui::Id::new("seek-slider"),
        "Playback position (%)",
        fraction,
        slider_width,
        None,
    ) {
        SliderEvent::Dragging(value) => app.seek_preview = Some(value),
        SliderEvent::Committed(value) => {
            app.seek_preview = None;
            if duration > 0 {
                app.actions
                    .push(Action::Seek((value * duration as f32) as u32));
            }
        }
        SliderEvent::None => {}
    }
    ui.painter().text(
        pos2(slider_left + slider_width + 8.0, row_cy),
        egui::Align2::LEFT_CENTER,
        util::format_duration_ms(duration),
        theme::regular(11.5),
        time_color,
    );
}

/// The ruler the incoming side of a transition is drawn on, below the bar.
///
/// Both edges of a transition are drawn, but only one of them is a moment in
/// the track being played. `fade_in_at` is a position in the *next* track —
/// where that track's own fade starts — so it has no place on the seek bar:
/// pointing at a pixel there would name a moment of one track with a number
/// belonging to another. It is drawn on this lane instead, under the bar and
/// on the same seconds scale, so its position reads back as its value.
///
/// The room this needs is reserved by the bar's height. The lane is the last
/// thing in the panel, so its space is whatever remains below the progress
/// row — and that was one pixel, which is a lane that disappears the moment
/// anything above it moves. A mark clipped away is invisible rather than
/// wrong, so the arrival would stop being drawn with nothing to say so; the
/// geometry test at the foot of this file pins the lane inside the panel, and
/// `PLAYER_BAR_HEIGHT` in `theme` is what has to stay big enough for it.
const INCOMING_LANE_HEIGHT: f32 = 12.0;

/// Draws where the coming transition runs, and the values it runs at.
///
/// Automix places two moments around a boundary: where the outgoing deck
/// starts its fade, and where the incoming deck's own fade starts. The first
/// opens the overlap, so it is drawn on the seek bar as a band reaching to
/// the end of the fade; the second is drawn on the lane below. Both are
/// labelled with their own seconds, because one pixel is several seconds of
/// track and a number says what a mark's position cannot.
///
/// Every mark is drawn at its own value and nothing else: the band and the
/// exit line come from the plan's `fade_out_at`, the arrival line from its
/// `fade_in_at`, so the bar shows what the engine decided rather than a
/// re-derivation that could disagree with it. A plan that fell back to the
/// local analysis is drawn dimmer, since in the numbers it is identical to
/// one the service placed. The service's own cues are drawn in the secondary
/// colour, and only where the plan has not put a mark at that value already —
/// two marks at one position are one mark.
fn transition_marks(
    ui: &egui::Ui,
    view: &crate::automix_driver::AutomixView,
    bar: Rect,
    palette: &crate::theme::Palette,
    duration_ms: u32,
) {
    if duration_ms == 0 {
        return;
    }
    let duration = f64::from(duration_ms) / 1000.0;
    let at = |seconds: f64| bar.left() + bar.width() * (seconds / duration).clamp(0.0, 1.0) as f32;
    let plan = if view.from_cuepoints {
        palette.accent
    } else {
        palette.dim
    };
    // Marks within a pixel of each other are the same mark, and the plan's is
    // the one worth drawing: it is the value the engine will act on.
    let apart = |a: f32, b: f32| (a - b).abs() >= 1.0;
    let exit = view.fade_out_at.map(&at);
    let arrival = view.fade_in_at.map(&at);

    // The seek bar carries the outgoing side: the overlap as a band from the
    // exit to the end of the fade, and the exit itself as the edge the two
    // tracks meet at.
    if let (Some(fade_out_at), Some(overlap)) = (view.fade_out_at, view.overlap) {
        let start = at(fade_out_at);
        let end = at(fade_out_at + overlap);
        ui.painter().rect_filled(
            Rect::from_min_max(pos2(start, bar.top() + 2.0), pos2(end, bar.bottom() - 2.0)),
            2.0,
            plan.gamma_multiply(0.35),
        );
    }
    if let Some(exit) = exit {
        ui.painter()
            .vline(exit, bar.y_range(), egui::Stroke::new(1.5, plan));
    }
    // The service's own exit cue: the same moment as the plan's whenever the
    // plan is the cue, and a second answer worth seeing when it is not.
    if let Some(cuepoints) = view.playing_cuepoints {
        let x = at(cuepoints.fade_out_at);
        if exit.is_none_or(|exit| apart(exit, x)) {
            ui.painter()
                .vline(x, bar.y_range(), egui::Stroke::new(1.0, palette.secondary));
        }
    }

    // The lane below carries the incoming side. It is drawn even when no plan
    // is armed yet, because the cue is what a plan will be built from and a
    // boundary that never gets one is exactly what there is to see.
    let cued_arrival = view.incoming_cuepoints.map(|cue| at(cue.fade_in_at));
    if exit.is_none() && arrival.is_none() && cued_arrival.is_none() {
        return;
    }
    // Directly under the bar, on the same seconds scale. The room this has is
    // `INCOMING_LANE_RESERVE` below the progress row, and the test at the
    // bottom of this file pins it inside the panel: the lane is the last thing
    // in the bar, so anything that makes the bar shorter or its rows taller
    // would otherwise clip it away in silence.
    let lane = Rect::from_min_size(
        pos2(bar.left(), bar.bottom()),
        vec2(bar.width(), INCOMING_LANE_HEIGHT),
    );
    // The exit again, thinner: the two values of a plan are read together,
    // and the one that means something on this lane is the arrival.
    if let Some(exit) = exit {
        ui.painter()
            .vline(exit, lane.y_range(), egui::Stroke::new(1.0, plan));
    }
    if let Some(arrival) = arrival {
        ui.painter()
            .vline(arrival, lane.y_range(), egui::Stroke::new(1.5, plan));
    }
    if let Some(x) = cued_arrival
        && arrival.is_none_or(|arrival| apart(arrival, x))
    {
        ui.painter()
            .vline(x, lane.y_range(), egui::Stroke::new(1.0, palette.secondary));
    }

    // The values, beside their own marks, in the words the transition log
    // uses so the two can be read against each other. A label that would run
    // off the lane goes on the other side of its mark, and one that would
    // land on a label already placed is dropped rather than overprinted.
    let font = theme::regular(9.5);
    let mut placed: Vec<Rect> = Vec::new();
    let mut place = |anchor: f32, text: String, color: egui::Color32| {
        let galley = ui.painter().layout_no_wrap(text, font.clone(), color);
        let size = galley.size();
        let left = if anchor + 3.0 + size.x > lane.right() {
            (anchor - 3.0 - size.x).max(lane.left())
        } else {
            anchor + 3.0
        };
        let rect = Rect::from_min_size(pos2(left, lane.center().y - size.y / 2.0), size);
        if placed.iter().any(|placed| placed.intersects(rect)) {
            return;
        }
        ui.painter().galley(rect.min, galley, color);
        placed.push(rect);
    };
    if let (Some(exit), Some(seconds)) = (exit, view.fade_out_at) {
        place(exit, format!("exit {seconds:.1}s"), plan);
    }
    if let (Some(arrival), Some(seconds)) = (arrival, view.fade_in_at) {
        place(arrival, format!("arrival {seconds:.1}s"), plan);
    }
    if let (Some(x), Some(cuepoints)) = (cued_arrival, view.incoming_cuepoints) {
        place(
            x,
            format!("cue {:.1}s", cuepoints.fade_in_at),
            palette.secondary,
        );
    }
}

/// Logs what a transition is doing, once per track, as it changes.
///
/// The bar shows a plan, but reading it back means describing pixels, and
/// that is a poor way to find out whether the numbers are right. This prints
/// them instead: both cues as the service gave them, what the plan made of
/// them, and where the play head was when it did. A track's line is written
/// once, when the plan first exists, so a log of a listening session is one
/// line per track and read in one pass.
pub fn log_transition(track: &str, view: &crate::automix_driver::AutomixView, position_ms: u32) {
    let show = |seconds: Option<f64>| match seconds {
        Some(value) => format!("{value:7.2}s"),
        None => "   --   ".into(),
    };
    let cue = |cuepoints: Option<crate::automix_cuepoints::Cuepoints>| match cuepoints {
        Some(cue) => format!(
            "in {:7.2}s out {:7.2}s {:7.2} BPM",
            cue.fade_in_at, cue.fade_out_at, cue.bpm
        ),
        None => "none".into(),
    };
    log::info!(
        "automix transition track={track} at {:6.2}s | cuepoints: playing [{}] incoming [{}] \
         | plan: exit {} arrival {} overlap {} ratio {:.4} | source {}",
        f64::from(position_ms) / 1000.0,
        cue(view.playing_cuepoints),
        cue(view.incoming_cuepoints),
        show(view.fade_out_at),
        show(view.fade_in_at),
        show(view.overlap),
        view.tempo_ratio.unwrap_or(1.0),
        if view.from_cuepoints {
            "server"
        } else if view.fade_out_at.is_some() {
            "local"
        } else {
            "none"
        },
    );
}

fn extras(app: &mut App, ui: &mut egui::Ui, now: Option<&NowPlaying>) {
    let palette = app.palette;
    ui.spacing_mut().item_spacing.x = 6.0;
    let volume = now
        .map(|now| now.volume_percent)
        .unwrap_or_else(|| crate::app::volume_to_percent(app.local.volume));
    let shown = match app.volume_preview {
        Some(fraction) => (fraction * 100.0).round() as u8,
        None => volume,
    };
    match thin_slider(
        ui,
        &palette,
        egui::Id::new("volume-slider"),
        "Volume (%)",
        shown as f32 / 100.0,
        92.0,
        Some(0.05),
    ) {
        SliderEvent::Dragging(value) => {
            app.volume_preview = Some(value);
            // Local volume is cheap to apply continuously; remote goes on release.
            if now.is_none_or(|now| now.local) {
                app.actions
                    .push(Action::PreviewVolume((value * 100.0).round() as u8));
            }
        }
        SliderEvent::Committed(value) => {
            app.volume_preview = None;
            app.actions
                .push(Action::SetVolume((value * 100.0).round() as u8));
        }
        SliderEvent::None => {}
    }
    let volume_icon = match shown {
        0 => Icon::VolumeX,
        1..=33 => Icon::Volume,
        34..=66 => Icon::Volume1,
        _ => Icon::Volume2,
    };
    if theme::icon_button(
        ui,
        volume_icon,
        18.0,
        palette.secondary,
        palette.text,
        if shown == 0 { "Unmute" } else { "Mute" },
    )
    .clicked()
    {
        app.actions.push(Action::ToggleMute);
    }
    ui.add_space(4.0);
    let remote = now.is_some_and(|now| !now.local);
    let devices = theme::icon_button(
        ui,
        Icon::Speaker,
        18.0,
        if remote {
            palette.accent
        } else {
            palette.secondary
        },
        palette.text,
        "Connect to a device",
    );
    ui.ctx().data_mut(|data| {
        data.insert_temp(egui::Id::new(super::devices::BUTTON_RECT_ID), devices.rect)
    });
    if devices.clicked() {
        app.actions.push(Action::ToggleDevicesPopup);
    }
    let queue_open = app.show_queue_panel || matches!(app.page(), Page::Queue);
    if theme::icon_button(
        ui,
        Icon::ListVideo,
        18.0,
        if queue_open {
            palette.accent
        } else {
            palette.secondary
        },
        palette.text,
        "Queue",
    )
    .clicked()
    {
        app.actions.push(Action::ToggleQueuePanel);
    }
    if theme::icon_button(
        ui,
        Icon::Mic,
        18.0,
        if app.show_lyrics_panel {
            palette.accent
        } else {
            palette.secondary
        },
        palette.text,
        "Lyrics",
    )
    .clicked()
    {
        app.actions.push(Action::ToggleLyricsPanel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::automix_cuepoints::Cuepoints;
    use crate::automix_driver::AutomixView;
    use crate::theme::Palette;

    /// One frame of the marks: the vertical lines with their spans and
    /// colours, and the laid-out labels with the rect each was placed in.
    struct Drawn {
        lines: Vec<(f32, f32, f32, egui::Color32)>,
        labels: Vec<(String, Rect)>,
    }

    /// Runs the marks over a 600px bar against a `duration_ms` track. The bar
    /// starts at x=160 and the track runs 200s over 600px by default, so a
    /// value in seconds lands at `160 + 3 * seconds`.
    fn marks(view: &AutomixView, duration_ms: u32) -> Drawn {
        let ctx = egui::Context::default();
        theme::install(&ctx);
        let mut drawn = Drawn {
            lines: Vec::new(),
            labels: Vec::new(),
        };
        let mut output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(
                    egui::Pos2::ZERO,
                    vec2(1000.0, theme::PLAYER_BAR_HEIGHT),
                )),
                ..Default::default()
            },
            |ui| {
                let bar = Rect::from_min_size(pos2(160.0, 40.0), vec2(600.0, 16.0));
                transition_marks(ui, view, bar, &Palette::dark(), duration_ms);
            },
        );
        output.textures_delta.clear();
        fn walk(shape: &egui::epaint::Shape, drawn: &mut Drawn) {
            match shape {
                egui::epaint::Shape::LineSegment { points, stroke } => {
                    if points[0].x == points[1].x {
                        let (top, bottom) = if points[0].y < points[1].y {
                            (points[0].y, points[1].y)
                        } else {
                            (points[1].y, points[0].y)
                        };
                        drawn.lines.push((points[0].x, top, bottom, stroke.color));
                    }
                }
                egui::epaint::Shape::Text(text) => drawn.labels.push((
                    text.galley.job.text.clone(),
                    text.galley.rect.translate(text.pos.to_vec2()),
                )),
                egui::epaint::Shape::Vec(shapes) => {
                    shapes.iter().for_each(|shape| walk(shape, drawn));
                }
                _ => {}
            }
        }
        for shape in &output.shapes {
            walk(&shape.shape, &mut drawn);
        }
        drawn
    }

    /// The x a value in seconds is drawn at, on either ruler.
    fn axis(seconds: f64) -> f32 {
        160.0 + 3.0 * seconds as f32
    }

    /// The arrival and the exit are drawn at their own seconds, and both are
    /// printed. A regression here is the silent one this drawing exists to
    /// make visible: the arrival is a moment in the *next* track, so drawing
    /// it wherever the overlap happened to end put a number at a position
    /// belonging to another track, and an arrival that never moved then looked
    /// identical to one that did.
    #[test]
    fn both_edges_are_drawn_and_printed_at_their_own_seconds() {
        let view = AutomixView {
            fade_out_at: Some(60.0),
            fade_in_at: Some(20.0),
            overlap: Some(4.0),
            tempo_ratio: Some(1.0),
            from_cuepoints: true,
            playing_cuepoints: None,
            incoming_cuepoints: Some(Cuepoints {
                fade_in_at: 20.0,
                fade_out_at: 100.0,
                bpm: 128.0,
            }),
        };
        let drawn = marks(&view, 200_000);

        for (seconds, what) in [(60.0, "exit"), (20.0, "arrival")] {
            let x = axis(seconds);
            assert!(
                drawn.lines.iter().any(|(at, ..)| (at - x).abs() < 0.5),
                "{what} at {seconds}s should be drawn at {x}: {:?}",
                drawn.lines
            );
        }
        assert!(
            !drawn
                .lines
                .iter()
                .any(|(at, ..)| (at - axis(64.0)).abs() < 0.5),
            "the arrival must not be drawn at the overlap's far end: {:?}",
            drawn.lines
        );
        let printed: Vec<&str> = drawn.labels.iter().map(|(text, _)| text.as_str()).collect();
        assert!(printed.contains(&"exit 60.0s"), "printed {printed:?}");
        assert!(printed.contains(&"arrival 20.0s"), "printed {printed:?}");
        // Each label sits beside the mark it names, not somewhere else on the
        // lane, which is what makes the pair readable as two values.
        for (text, x) in [("exit 60.0s", axis(60.0)), ("arrival 20.0s", axis(20.0))] {
            let (_, rect) = drawn
                .labels
                .iter()
                .find(|(label, _)| label == text)
                .unwrap_or_else(|| panic!("{text} was never placed"));
            assert!(
                rect.left() >= x && rect.left() - x <= 6.0,
                "{text} should sit just past its mark at {x}: {rect:?}"
            );
        }
        // The plan is the cue here, so the cue is not drawn a second time.
        assert!(
            !printed.iter().any(|text| text.starts_with("cue ")),
            "the cue agrees with the plan and must not double-print: {printed:?}"
        );
    }

    /// The bug this covers: the arrival stopped being drawn. `fade_in_at` has
    /// no place on the seek bar — it is a moment in the *next* track — so it
    /// is drawn on a lane under the bar, and that lane is the last thing in
    /// the panel. Sizing it from the space left over gave it one pixel to
    /// spare, and a mark that is clipped away is invisible rather than wrong:
    /// the arrival would simply stop appearing, with nothing about the bar to
    /// say which of "no arrival yet" and "arrival not drawn" it was.
    #[test]
    fn the_transition_lane_fits_inside_the_bar_it_is_drawn_in() {
        // The bar's own geometry, from `transport`: the progress row sits 23px
        // below the cluster's centre, the slider is 16px tall, and the lane
        // starts at the slider's bottom edge.
        const PROGRESS_ROW_BELOW_CENTRE: f32 = 23.0;
        const SLIDER_HEIGHT: f32 = 16.0;
        let bar_height = crate::theme::PLAYER_BAR_HEIGHT;
        let half = bar_height / 2.0;
        let lane_top = PROGRESS_ROW_BELOW_CENTRE + SLIDER_HEIGHT / 2.0;
        let lane_bottom = lane_top + INCOMING_LANE_HEIGHT;
        assert!(
            lane_bottom <= half,
            "the lane runs to {lane_bottom}px below the bar's centre but the bar \
             ends at {half}px, so the arrival is clipped away by {:.1}px",
            lane_bottom - half
        );
        // One pixel of slack is what this was before, and that is the state
        // worth failing on: it means the lane is not actually reserved for.
        assert!(
            half - lane_bottom >= 2.0,
            "the lane has only {:.1}px of room left below it ({lane_bottom} of \
             {half}); anything above it moving makes the arrival invisible",
            half - lane_bottom
        );
    }

    /// A bar with nothing to say draws nothing: no plan, no cues, no marks.
    #[test]
    fn an_empty_view_leaves_the_bar_alone() {
        let drawn = marks(&AutomixView::default(), 200_000);
        assert!(
            drawn.lines.is_empty() && drawn.labels.is_empty(),
            "nothing to mark, but drew {:?}",
            drawn.lines
        );
    }

    /// The cue is drawn even before a plan is armed for it, and it is drawn
    /// on the lane rather than on the outgoing bar: the two are moments in
    /// different tracks, and a lane at the cue's own seconds is what says
    /// which track the number belongs to.
    #[test]
    fn a_cue_without_a_plan_is_still_drawn_on_the_lane() {
        let cue = Cuepoints {
            fade_in_at: 20.0,
            fade_out_at: 100.0,
            bpm: 128.0,
        };
        let view = AutomixView {
            incoming_cuepoints: Some(cue),
            playing_cuepoints: Some(cue),
            ..AutomixView::default()
        };
        let drawn = marks(&view, 200_000);
        let arrival = drawn
            .lines
            .iter()
            .find(|(at, ..)| (at - axis(20.0)).abs() < 0.5)
            .unwrap_or_else(|| panic!("the cue's arrival was not drawn: {:?}", drawn.lines));
        let exit = drawn
            .lines
            .iter()
            .find(|(at, ..)| (at - axis(100.0)).abs() < 0.5)
            .unwrap_or_else(|| panic!("the cue's exit was not drawn: {:?}", drawn.lines));
        assert!(
            arrival.1 >= 55.0,
            "the arrival belongs on the lane below the bar, not on it: {arrival:?}"
        );
        assert!(
            exit.2 <= 57.0,
            "the exit belongs on the bar itself: {exit:?}"
        );
        let printed: Vec<&str> = drawn.labels.iter().map(|(text, _)| text.as_str()).collect();
        assert!(printed.contains(&"cue 20.0s"), "printed {printed:?}");
        assert!(
            !drawn
                .lines
                .iter()
                .any(|(at, ..)| (at - axis(120.0)).abs() < 0.5),
            "no view here has an overlap, so no band edge is drawn: {:?}",
            drawn.lines
        );
    }

    /// The local fallback is drawn in the palette's dim colour so it is not
    /// mistaken for a plan the service placed: the two carry the same numbers
    /// and are otherwise indistinguishable.
    #[test]
    fn a_local_plan_is_drawn_dimmer_than_a_cued_one() {
        let palette = Palette::dark();
        let plan = |from_cuepoints| AutomixView {
            fade_out_at: Some(60.0),
            fade_in_at: Some(20.0),
            overlap: Some(4.0),
            tempo_ratio: Some(1.0),
            from_cuepoints,
            ..AutomixView::default()
        };
        let color_of = |view: &AutomixView, seconds: f64| {
            marks(view, 200_000)
                .lines
                .into_iter()
                .find(|(at, ..)| (at - axis(seconds)).abs() < 0.5)
                .unwrap_or_else(|| panic!("{seconds}s was not drawn"))
                .3
        };
        assert_eq!(color_of(&plan(true), 60.0), palette.accent);
        assert_eq!(color_of(&plan(false), 60.0), palette.dim);
        assert_eq!(color_of(&plan(true), 20.0), palette.accent);
        assert_eq!(color_of(&plan(false), 20.0), palette.dim);
    }
}
