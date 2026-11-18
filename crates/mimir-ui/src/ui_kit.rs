use eframe::egui::{
    self, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, Margin, Pos2, Response,
    Sense, Stroke, TextStyle, Vec2,
};

pub mod palette {
    use eframe::egui::Color32;

    pub const VOID_BG: Color32 = Color32::from_rgb(7, 10, 16); // #070A10
    pub const RAIL_BG: Color32 = Color32::from_rgb(10, 14, 23); // #0A0E17
    pub const CARD_BG: Color32 = Color32::from_rgb(13, 19, 31); // #0D131F
    pub const ELEVATED_BG: Color32 = Color32::from_rgb(18, 26, 43); // #121A2B
    pub const ACTIVE_FILL: Color32 = Color32::from_rgb(14, 31, 54); // #0E1F36
    pub const HAIRLINE: Color32 = Color32::from_rgb(26, 38, 57); // #1A2639
    pub const HAIRLINE_SOFT: Color32 = Color32::from_rgb(36, 51, 74); // #24334A
    pub const ICE: Color32 = Color32::from_rgb(125, 211, 252); // #7DD3FC
    pub const CYAN: Color32 = Color32::from_rgb(56, 189, 248); // #38BDF8
    pub const AZURE: Color32 = Color32::from_rgb(14, 165, 233); // #0EA5E9
    pub const COBALT: Color32 = Color32::from_rgb(37, 99, 235); // #2563EB
    pub const TEXT_PRIMARY: Color32 = Color32::from_rgb(241, 245, 249); // #F1F5F9
    pub const TEXT_SECONDARY: Color32 = Color32::from_rgb(148, 163, 184); // #94A3B8
    pub const TEXT_MUTED: Color32 = Color32::from_rgb(100, 116, 139); // #64748B
    pub const DANGER: Color32 = Color32::from_rgb(248, 113, 113); // #F87171
    pub const DANGER_BG: Color32 = Color32::from_rgb(45, 18, 24); // #2D1218
    pub const SUCCESS: Color32 = Color32::from_rgb(52, 211, 153); // #34D399
    pub const SUCCESS_BG: Color32 = Color32::from_rgb(11, 38, 31); // #0B261F
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ButtonVariant {
    Primary,
    Secondary,
    Ghost,
    Danger,
}

pub fn configure_fonts(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();

    let sans_candidates = [
        "/System/Library/Fonts/SFNS.ttf",
        "/System/Library/Fonts/Helvetica.ttc",
        "/usr/share/fonts/truetype/inter/Inter-Regular.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        "C:\\Windows\\Fonts\\segoeui.ttf",
    ];
    for path in sans_candidates {
        if let Ok(bytes) = std::fs::read(path) {
            fonts
                .font_data
                .insert("ui_sans".into(), FontData::from_owned(bytes).into());
            if let Some(fam) = fonts.families.get_mut(&FontFamily::Proportional) {
                fam.insert(0, "ui_sans".into());
            }
            break;
        }
    }

    let mono_candidates = [
        "/System/Library/Fonts/SFNSMono.ttf",
        "/System/Library/Fonts/Menlo.ttc",
        "/usr/share/fonts/truetype/jetbrains-mono/JetBrainsMono-Regular.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
        "C:\\Windows\\Fonts\\consola.ttf",
    ];
    for path in mono_candidates {
        if let Ok(bytes) = std::fs::read(path) {
            fonts
                .font_data
                .insert("ui_mono".into(), FontData::from_owned(bytes).into());
            if let Some(fam) = fonts.families.get_mut(&FontFamily::Monospace) {
                fam.insert(0, "ui_mono".into());
            }
            break;
        }
    }

    ctx.set_fonts(fonts);
}

pub fn apply_obsidian_prism_theme(ctx: &egui::Context) {
    configure_fonts(ctx);

    let mut style = (*ctx.style()).clone();
    style.spacing.item_spacing = Vec2::new(8.0, 8.0);
    style.spacing.button_padding = Vec2::new(12.0, 6.0);
    style.spacing.window_margin = Margin::same(16);
    style.spacing.indent = 16.0;
    style.spacing.scroll = egui::style::ScrollStyle {
        bar_width: 6.0,
        handle_min_length: 24.0,
        bar_inner_margin: 2.0,
        bar_outer_margin: 2.0,
        ..Default::default()
    };

    style.text_styles.insert(
        TextStyle::Heading,
        egui::FontId::new(19.0, FontFamily::Proportional),
    );
    style.text_styles.insert(
        TextStyle::Body,
        egui::FontId::new(13.5, FontFamily::Proportional),
    );
    style.text_styles.insert(
        TextStyle::Button,
        egui::FontId::new(12.5, FontFamily::Proportional),
    );
    style.text_styles.insert(
        TextStyle::Small,
        egui::FontId::new(11.5, FontFamily::Proportional),
    );
    style.text_styles.insert(
        TextStyle::Monospace,
        egui::FontId::new(11.5, FontFamily::Monospace),
    );

    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = palette::VOID_BG;
    visuals.window_fill = palette::CARD_BG;
    visuals.extreme_bg_color = palette::VOID_BG;
    visuals.faint_bg_color = palette::RAIL_BG;
    visuals.code_bg_color = palette::VOID_BG;

    visuals.selection.bg_fill = palette::ACTIVE_FILL;
    visuals.selection.stroke = Stroke::new(1.0_f32, palette::CYAN);

    let r7 = CornerRadius::same(7);
    let r8 = CornerRadius::same(8);

    visuals.widgets.noninteractive.bg_fill = palette::CARD_BG;
    visuals.widgets.noninteractive.weak_bg_fill = palette::CARD_BG;
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, palette::HAIRLINE);
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, palette::TEXT_SECONDARY);
    visuals.widgets.noninteractive.corner_radius = r8;

    visuals.widgets.inactive.bg_fill = palette::ELEVATED_BG;
    visuals.widgets.inactive.weak_bg_fill = palette::ELEVATED_BG;
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, palette::HAIRLINE);
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0_f32, palette::TEXT_PRIMARY);
    visuals.widgets.inactive.corner_radius = r7;

    visuals.widgets.hovered.bg_fill = palette::ACTIVE_FILL;
    visuals.widgets.hovered.weak_bg_fill = palette::ACTIVE_FILL;
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, palette::CYAN);
    visuals.widgets.hovered.fg_stroke = Stroke::new(1.0_f32, palette::ICE);
    visuals.widgets.hovered.corner_radius = r7;

    visuals.widgets.active.bg_fill = palette::COBALT;
    visuals.widgets.active.weak_bg_fill = palette::COBALT;
    visuals.widgets.active.bg_stroke = Stroke::new(1.0_f32, palette::ICE);
    visuals.widgets.active.fg_stroke = Stroke::new(1.0_f32, Color32::WHITE);
    visuals.widgets.active.corner_radius = r7;

    visuals.window_corner_radius = CornerRadius::same(10);
    visuals.window_stroke = Stroke::new(1.0_f32, palette::HAIRLINE);

    style.visuals = visuals;
    ctx.set_style(style);
}

pub fn section_card(
    ui: &mut egui::Ui,
    title: &str,
    right_badge: Option<&str>,
    add_contents: impl FnOnce(&mut egui::Ui),
) {
    egui::Frame::new()
        .fill(palette::CARD_BG)
        .stroke(Stroke::new(1.0_f32, palette::HAIRLINE))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::same(18))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            if !title.is_empty() || right_badge.is_some() {
                ui.horizontal(|ui| {
                    if !title.is_empty() {
                        ui.label(
                            egui::RichText::new(title)
                                .monospace()
                                .size(10.5)
                                .strong()
                                .color(palette::TEXT_MUTED),
                        );
                    }
                    if let Some(badge) = right_badge {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            badge_pill(ui, badge, palette::ICE, palette::ACTIVE_FILL);
                        });
                    }
                });
                ui.add_space(12.0);
            }
            add_contents(ui);
        });
}

pub fn badge_pill(ui: &mut egui::Ui, text: &str, fg: Color32, bg: Color32) {
    egui::Frame::new()
        .fill(bg)
        .stroke(Stroke::new(1.0_f32, fg.linear_multiply(0.35)))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::symmetric(8, 3))
        .show(ui, |ui| {
            ui.label(
                egui::RichText::new(text)
                    .monospace()
                    .size(10.5)
                    .strong()
                    .color(fg),
            );
        });
}

pub fn action_button(ui: &mut egui::Ui, text: &str, variant: ButtonVariant) -> Response {
    let (fill, stroke, text_col) = match variant {
        ButtonVariant::Primary => (
            palette::ACTIVE_FILL,
            Stroke::new(1.0_f32, palette::CYAN),
            palette::ICE,
        ),
        ButtonVariant::Secondary => (
            palette::ELEVATED_BG,
            Stroke::new(1.0_f32, palette::HAIRLINE_SOFT),
            palette::TEXT_PRIMARY,
        ),
        ButtonVariant::Ghost => (
            Color32::TRANSPARENT,
            Stroke::new(1.0_f32, palette::HAIRLINE),
            palette::TEXT_SECONDARY,
        ),
        ButtonVariant::Danger => (
            palette::DANGER_BG,
            Stroke::new(1.0_f32, palette::DANGER.linear_multiply(0.55)),
            palette::DANGER,
        ),
    };
    let btn = egui::Button::new(egui::RichText::new(text).size(12.5).color(text_col))
        .fill(fill)
        .stroke(stroke)
        .corner_radius(CornerRadius::same(7));
    ui.add(btn)
}

pub fn pill_tab(ui: &mut egui::Ui, text: &str, active: bool) -> Response {
    let (fill, stroke, text_col) = if active {
        (
            palette::ACTIVE_FILL,
            Stroke::new(1.0_f32, palette::CYAN),
            palette::ICE,
        )
    } else {
        (
            Color32::TRANSPARENT,
            Stroke::new(1.0_f32, Color32::TRANSPARENT),
            palette::TEXT_SECONDARY,
        )
    };
    let btn = egui::Button::new(egui::RichText::new(text).size(12.0).strong().color(text_col))
        .fill(fill)
        .stroke(stroke)
        .corner_radius(CornerRadius::same(6));
    ui.add(btn)
}

pub fn styled_text_input(
    ui: &mut egui::Ui,
    text: &mut String,
    hint: &str,
    width: Option<f32>,
) -> Response {
    styled_input_internal(ui, text, hint, width, false)
}

pub fn styled_password_input(
    ui: &mut egui::Ui,
    text: &mut String,
    hint: &str,
    width: Option<f32>,
) -> Response {
    styled_input_internal(ui, text, hint, width, true)
}

fn styled_input_internal(
    ui: &mut egui::Ui,
    text: &mut String,
    hint: &str,
    width: Option<f32>,
    password: bool,
) -> Response {
    let mut out_resp: Option<Response> = None;
    egui::Frame::new()
        .fill(palette::VOID_BG)
        .stroke(Stroke::new(1.0_f32, palette::HAIRLINE_SOFT))
        .corner_radius(CornerRadius::same(7))
        .inner_margin(Margin::symmetric(10, 6))
        .show(ui, |ui| {
            let mut edit = egui::TextEdit::singleline(text)
                .frame(false)
                .password(password)
                .hint_text(egui::RichText::new(hint).color(palette::TEXT_MUTED))
                .text_color(palette::TEXT_PRIMARY);
            if let Some(w) = width {
                edit = edit.desired_width((w - 20.0).max(40.0));
            } else {
                edit = edit.desired_width(ui.available_width());
            }
            out_resp = Some(ui.add(edit));
        });
    out_resp.expect("frame always runs")
}

pub fn toggle_switch(
    ui: &mut egui::Ui,
    on: &mut bool,
    label: &str,
    subtitle: Option<&str>,
    enabled: bool,
) -> Response {
    let mut toggled = false;
    let resp = ui.add_enabled_ui(enabled, |ui| {
        ui.horizontal(|ui| {
            let desired_size = Vec2::new(38.0, 20.0);
            let (rect, mut sw_resp) = ui.allocate_exact_size(desired_size, Sense::click());
            if sw_resp.clicked() {
                *on = !*on;
                sw_resp.mark_changed();
                toggled = true;
            }
            if ui.is_rect_visible(rect) {
                let how_on = ui.ctx().animate_bool_responsive(sw_resp.id, *on);
                let track_fill = if *on {
                    palette::ACTIVE_FILL
                } else {
                    palette::VOID_BG
                };
                let track_stroke = if *on {
                    Stroke::new(1.0_f32, palette::CYAN)
                } else {
                    Stroke::new(1.0_f32, palette::HAIRLINE_SOFT)
                };
                ui.painter().rect(
                    rect,
                    CornerRadius::same(10),
                    track_fill,
                    track_stroke,
                    egui::StrokeKind::Inside,
                );
                let radius = 7.0;
                let circle_x = egui::lerp((rect.left() + 10.0)..=(rect.right() - 10.0), how_on);
                let center = Pos2::new(circle_x, rect.center().y);
                let thumb_col = if *on {
                    palette::ICE
                } else {
                    palette::TEXT_MUTED
                };
                ui.painter().circle_filled(center, radius, thumb_col);
            }
            ui.add_space(6.0);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                let lbl_resp = ui.add(
                    egui::Label::new(
                        egui::RichText::new(label)
                            .size(13.5)
                            .strong()
                            .color(palette::TEXT_PRIMARY),
                    )
                    .sense(Sense::click()),
                );
                if lbl_resp.clicked() {
                    *on = !*on;
                    toggled = true;
                }
                if let Some(sub) = subtitle {
                    ui.label(
                        egui::RichText::new(sub)
                            .size(11.5)
                            .color(palette::TEXT_MUTED),
                    );
                }
            });
            sw_resp
        })
        .inner
    });
    let mut inner = resp.inner;
    if toggled {
        inner.mark_changed();
    }
    inner
}

pub fn session_card_row(
    ui: &mut egui::Ui,
    id: impl std::hash::Hash,
    title: &str,
    meta: &str,
    has_summary: bool,
    is_archived: bool,
    selected: bool,
) -> Response {
    let fill = if selected {
        palette::ACTIVE_FILL
    } else {
        Color32::TRANSPARENT
    };
    let stroke = if selected {
        Stroke::new(1.0_f32, palette::CYAN.linear_multiply(0.65))
    } else {
        Stroke::new(1.0_f32, Color32::TRANSPARENT)
    };
    let resp = egui::Frame::new()
        .fill(fill)
        .stroke(stroke)
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::symmetric(10, 8))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 3.0;
                ui.horizontal(|ui| {
                    let title_col = if selected {
                        palette::ICE
                    } else if is_archived {
                        palette::TEXT_MUTED
                    } else {
                        palette::TEXT_PRIMARY
                    };
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(title)
                                .size(13.0)
                                .strong()
                                .color(title_col),
                        )
                        .truncate(),
                    );
                });
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(meta)
                            .monospace()
                            .size(10.5)
                            .color(palette::TEXT_MUTED),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if has_summary {
                            let (dot, _) =
                                ui.allocate_exact_size(Vec2::splat(6.0), Sense::hover());
                            ui.painter()
                                .circle_filled(dot.center(), 3.0, palette::CYAN);
                        }
                        if is_archived {
                            ui.label(
                                egui::RichText::new("ARCHIVED")
                                    .monospace()
                                    .size(9.0)
                                    .color(palette::TEXT_MUTED),
                            );
                        }
                    });
                });
            });
        });

    let interact = ui.interact(
        resp.response.rect,
        ui.make_persistent_id(("session_card", id)),
        Sense::click(),
    );
    if interact.hovered() && !selected {
        ui.painter().rect_stroke(
            resp.response.rect,
            CornerRadius::same(8),
            Stroke::new(1.0_f32, palette::HAIRLINE_SOFT),
            egui::StrokeKind::Inside,
        );
    }
    interact
}

pub fn empty_state(ui: &mut egui::Ui, heading: &str, body: &str) {
    ui.vertical_centered(|ui| {
        ui.add_space(48.0);
        ui.label(
            egui::RichText::new(heading)
                .size(15.5)
                .strong()
                .color(palette::TEXT_PRIMARY),
        );
        ui.add_space(6.0);
        ui.label(
            egui::RichText::new(body)
                .size(13.0)
                .color(palette::TEXT_MUTED),
        );
        ui.add_space(48.0);
    });
}
