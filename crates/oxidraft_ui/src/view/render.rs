//! Canvas frame rendering: draws entities, selection/hover highlights, grid,
//! HUD prompts, and per-entity styling (color/linetype/lineweight) onto the
//! egui painter each frame.

use super::tessellate::{
    draw_curve, draw_curve_patterned, draw_patterned_polyline, flatten_curve_world, is_closed_curve,
};
use crate::state::AppState;
use crate::tools::{DimSubject, DimTarget, Tool};
use egui::{Color32, Stroke, pos2, vec2};
use oxidraft_document::{Color, EntityId, EntityKind, LineTypeRef};
use oxidraft_geometry::{Curve, CurveSegment, Point2d};

pub(super) const HAIRLINE_PX: f32 = 1.5;

pub(super) const HATCH_SELECT: Color32 = Color32::from_rgb(64, 120, 255);

pub(super) fn tool_prompt(tool: &Tool) -> String {
    match tool {
        Tool::Line { first } => match first {
            None => "Specify start point, or pick a circle/arc to start tangent to".into(),
            Some(_) => "Specify next point or length, or pick a circle/arc for a tangent".into(),
        },
        Tool::Circle { parts, .. } => {
            let left = crate::tools::CIRCLE_DOF.saturating_sub(crate::tools::used_dof(parts));
            match (parts.is_empty(), left) {
                (true, _) => "Specify center, or pick a point / edge".into(),
                (_, 1) => "Specify radius, or pick a point / edge".into(),
                _ => "Pick another point or edge".into(),
            }
        }
        Tool::Rectangle { first } => {
            if first.is_none() {
                "Specify first corner".into()
            } else {
                "Specify opposite corner".into()
            }
        }
        Tool::PlotWindow { first } => {
            if first.is_none() {
                "Specify first corner of plot window".into()
            } else {
                "Specify opposite corner of plot window".into()
            }
        }
        Tool::Arc { parts, .. } => match parts.len() {
            0 => "Specify start point, or pick a circle/arc's center".into(),
            1 => "Specify next point, or pick a center".into(),
            _ => "Specify end point".into(),
        },
        Tool::CircleTwoPoint { first } => {
            if first.is_none() {
                "Specify first end of diameter".into()
            } else {
                "Specify second end of diameter".into()
            }
        }
        Tool::CircleThreePoint { pts } => match pts.len() {
            0 => "Specify first point on circle".into(),
            1 => "Specify second point on circle".into(),
            _ => "Specify third point on circle".into(),
        },
        Tool::CircleTtr { first, .. } => {
            if first.is_none() {
                "Pick the first object to touch (type a radius first)".into()
            } else {
                "Pick the second object to touch".into()
            }
        }
        Tool::CircleTtt { picks } => match picks.len() {
            0 => "Pick the first object to touch".into(),
            1 => "Pick the second object to touch".into(),
            _ => "Pick the third object to touch".into(),
        },
        Tool::Dimension { subject } => match subject {
            None => "Pick a line (length), a circle/arc (radius), a first line for an \
                     angle, or a point for a free two-point dimension"
                .into(),
            Some(DimSubject::Point(_)) => "Specify second dimension point".into(),
            Some(DimSubject::Points(_, _)) => {
                "Place the dimension line — aside for vertical, above/below for horizontal".into()
            }
            Some(DimSubject::Line(..)) => {
                "Pick a second line for an angle, or click to place this line's length".into()
            }
            Some(DimSubject::LinePair(..)) => "Click to place the dimension arc".into(),
            Some(DimSubject::Radial { diameter, .. }) => {
                let what = if *diameter { "diameter" } else { "radius" };
                format!("Click to place the {what} leader — Tab switches radius/diameter")
            }
        },
        Tool::DimConstraint { first, pending, .. } => match (first, pending) {
            (_, Some(_)) => "Click to place the dimension".into(),
            (Some(DimTarget::Anchor(..)), None) => {
                "Pick a second point, or a line to measure to — Tab re-reads the pick".into()
            }
            (Some(DimTarget::Entity(_)), None) => {
                "Pick a second line for an angle or width, or click to place this length".into()
            }
            (None, None) => "Pick a line, a circle/arc, or a point to dimension from".into(),
        },
        Tool::Weld { first } => {
            if first.is_none() {
                "Pick the first point to weld (endpoint, midpoint, center, or point)".into()
            } else {
                "Pick the point to weld it to".into()
            }
        }
        Tool::ConPick { kind, picks } => {
            let plan = crate::tools::con_pick_plan(*kind);
            match plan.get(picks.len()) {
                Some(crate::tools::ConPickStep::Point) => {
                    "Pick a point (endpoint, midpoint, center, or point)".into()
                }
                Some(crate::tools::ConPickStep::Line) => "Pick a line".into(),
                Some(crate::tools::ConPickStep::Arc) => "Pick a circle or arc".into(),
                Some(crate::tools::ConPickStep::Anything) => {
                    "Pick a line (its middle for a midpoint) or a circle/arc".into()
                }
                None => "Applying…".into(),
            }
        }
        Tool::Ellipse { center, axis_end } => match (center, axis_end) {
            (None, _) => "Specify center of ellipse".into(),
            (Some(_), None) => "Specify end of first axis".into(),
            (Some(_), Some(_)) => "Specify distance to other axis".into(),
        },
        Tool::Move { base, .. } => {
            if base.is_none() {
                "Specify base point".into()
            } else {
                "Specify destination".into()
            }
        }
        Tool::Copy { base, .. } => {
            if base.is_none() {
                "Specify base point".into()
            } else {
                "Specify destination".into()
            }
        }
        Tool::Rotate { base, .. } => {
            if base.is_none() {
                "Specify base point".into()
            } else {
                "Specify rotation angle".into()
            }
        }
        Tool::Scale { base, .. } => {
            if base.is_none() {
                "Specify base point".into()
            } else {
                "Specify scale factor".into()
            }
        }
        Tool::Mirror { first, .. } => {
            if first.is_none() {
                "Specify first point of mirror axis".into()
            } else {
                "Specify second point of mirror axis".into()
            }
        }
        Tool::Polygon {
            center,
            radius_point,
            sides,
        } => {
            if center.is_none() {
                "Click the center point".into()
            } else if radius_point.is_none() {
                "Click to set the radius".into()
            } else {
                let n = sides.unwrap_or(6);
                format!("Sides: {n} — pick a new count, then Apply or Enter to confirm")
            }
        }
        Tool::Spline { pts } => {
            if pts.is_empty() {
                "Specify first control point".into()
            } else {
                format!(
                    "Specify next control vertex ({} placed) — click the start to close, Enter finishes",
                    pts.len()
                )
            }
        }
        Tool::Polyline { pts } => {
            if pts.is_empty() {
                "Specify start point".into()
            } else if pts.len() >= 3 {
                "Specify next point — click the start to close, Enter finishes".into()
            } else {
                "Specify next point — Enter/right-click finishes".into()
            }
        }
        Tool::Text { anchor, .. } => {
            if anchor.is_none() {
                "Specify text anchor point".into()
            } else {
                "Type the text, Enter to place".into()
            }
        }
        Tool::Offset { source, .. } => {
            if source.is_none() {
                "Click the curve to offset (type a distance first)".into()
            } else {
                "Click the side to offset towards".into()
            }
        }
        Tool::Trim => "Click a piece to cut it, or drag across several".into(),
        Tool::Extend => "Click the end to lengthen".into(),
        Tool::Hatch => "Click inside an area to hatch it".into(),
        Tool::Fillet { first, .. } => {
            if first.is_none() {
                "Pick the first line".into()
            } else {
                "Pick the second line".into()
            }
        }
        Tool::Chamfer { first, .. } => {
            if first.is_none() {
                "Pick the first line".into()
            } else {
                "Pick the second line".into()
            }
        }
        Tool::Blend {
            continuity,
            first,
            second,
            ..
        } => {
            if second.is_some() {
                "Adjust continuity/tension, then Apply or Enter to confirm".into()
            } else if first.is_none() {
                format!("Blend ({continuity:?}) — pick the first object")
            } else {
                "Pick the second object to blend into".into()
            }
        }
        Tool::Stretch { c1, c2, base, .. } => match (c1, c2, base) {
            (None, _, _) => "Specify first corner of crossing window".into(),
            (Some(_), None, _) => "Specify opposite corner".into(),
            (_, _, None) => "Specify base point".into(),
            _ => "Specify destination".into(),
        },
        Tool::Select => "Click an object, or drag a window".into(),
        Tool::Point => "Specify point".into(),
    }
}

pub(super) fn draw_prompt_chip(painter: &egui::Painter, rect: egui::Rect, text: &str) {
    let galley = painter.layout_no_wrap(
        text.to_string(),
        egui::FontId::proportional(crate::theme::tok::T_LABEL),
        crate::theme::TEXT,
    );
    let pad = vec2(14.0, 7.0);
    let size = galley.size() + pad * 2.0;
    let bottom_center = pos2(
        rect.center().x - size.x / 2.0,
        rect.bottom() - 56.0 - size.y,
    );
    let bg = egui::Rect::from_min_size(bottom_center, size);
    painter.rect(
        bg,
        16.0,
        Color32::from_rgba_unmultiplied(27, 34, 46, 235),
        Stroke::new(1.0, crate::theme::OUTLINE),
        egui::StrokeKind::Middle,
    );
    painter.galley(bg.min + pad, galley, crate::theme::TEXT);
}

pub(super) fn draw_grid(
    painter: &egui::Painter,
    app: &AppState,
    rect: egui::Rect,
    to_screen: &impl Fn(f64, f64) -> egui::Pos2,
) {
    {
        let top = Color32::from_rgb(17, 21, 29);
        let bot = crate::theme::CANVAS_BG;
        let mut mesh = egui::epaint::Mesh::default();
        mesh.colored_vertex(rect.left_top(), top);
        mesh.colored_vertex(rect.right_top(), top);
        mesh.colored_vertex(rect.right_bottom(), bot);
        mesh.colored_vertex(rect.left_bottom(), bot);
        mesh.add_triangle(0, 1, 2);
        mesh.add_triangle(0, 2, 3);
        painter.add(egui::Shape::mesh(mesh));
    }

    let major = app.view.grid_spacing();
    if !(major.is_finite() && major > 0.0) {
        return;
    }
    let (x0, y0, x1, y1) = app.view.visible_bounds();
    let rgb = |c: (u8, u8, u8)| Color32::from_rgb(c.0, c.1, c.2);
    let minor = Stroke::new(1.0, rgb(app.prefs.grid_minor_rgb));
    let major_line = Stroke::new(1.0, rgb(app.prefs.grid_major_rgb));
    let axis = Stroke::new(1.0, Color32::from_rgb(58, 66, 80));
    let every = app.prefs.grid_major_every.max(1) as i64;

    let ix0 = (x0 / major).floor() as i64;
    let iy0 = (y0 / major).floor() as i64;

    if app.prefs.grid_dots {
        let mut i = ix0;
        let mut gx = ix0 as f64 * major;
        while gx <= x1 {
            let major_col = i.rem_euclid(every) == 0;
            let mut j = iy0;
            let mut gy = iy0 as f64 * major;
            while gy <= y1 {
                let p = to_screen(gx, gy);
                let (r, col) = if major_col && j.rem_euclid(every) == 0 {
                    (1.4, major_line.color)
                } else {
                    (1.0, minor.color)
                };
                painter.circle_filled(p, r, col);
                j += 1;
                gy += major;
            }
            i += 1;
            gx += major;
        }
    } else {
        let mut i = ix0;
        let mut gx = ix0 as f64 * major;
        while gx <= x1 {
            let sx = to_screen(gx, y0).x;
            let stroke = if i.rem_euclid(every) == 0 {
                major_line
            } else {
                minor
            };
            painter.line_segment([pos2(sx, rect.top()), pos2(sx, rect.bottom())], stroke);
            i += 1;
            gx += major;
        }
        let mut j = iy0;
        let mut gy = iy0 as f64 * major;
        while gy <= y1 {
            let sy = to_screen(x0, gy).y;
            let stroke = if j.rem_euclid(every) == 0 {
                major_line
            } else {
                minor
            };
            painter.line_segment([pos2(rect.left(), sy), pos2(rect.right(), sy)], stroke);
            j += 1;
            gy += major;
        }
    }

    if x0 <= 0.0 && x1 >= 0.0 {
        let a = to_screen(0.0, y0);
        painter.line_segment([pos2(a.x, rect.top()), pos2(a.x, rect.bottom())], axis);
    }
    if y0 <= 0.0 && y1 >= 0.0 {
        let a = to_screen(x0, 0.0);
        painter.line_segment([pos2(rect.left(), a.y), pos2(rect.right(), a.y)], axis);
    }
}

pub(super) fn draw_scale_bar(painter: &egui::Painter, app: &AppState, rect: egui::Rect) {
    let pws = app.view.pixel_world_size();
    if !(pws.is_finite() && pws > 0.0) {
        return;
    }
    let target_px = 120.0_f64;
    let raw = target_px * pws;
    let mag = raw.log10().floor();
    let base = 10f64.powf(mag);
    let nice = if raw / base < 1.5 {
        base
    } else if raw / base < 3.5 {
        2.0 * base
    } else if raw / base < 7.5 {
        5.0 * base
    } else {
        10.0 * base
    };
    let bar_px = (nice / pws) as f32;
    if !bar_px.is_finite() || bar_px <= 0.0 {
        return;
    }

    let unit = app.document.settings.units.short_name();
    let label = format!("{} {}", format_distance(nice), unit);
    let label = label.trim_end().to_string();
    let margin = 16.0;
    let y = rect.bottom() - margin;
    let x1 = rect.right() - margin;
    let x0 = x1 - bar_px;
    let cap = 5.0;
    let bar = Stroke::new(2.0, crate::theme::ACCENT);
    let shadow = Stroke::new(3.5, Color32::from_rgba_unmultiplied(0, 0, 0, 150));
    for s in [shadow, bar] {
        painter.line_segment([pos2(x0, y), pos2(x1, y)], s);
        painter.line_segment([pos2(x0, y - cap), pos2(x0, y + cap)], s);
        painter.line_segment([pos2(x1, y - cap), pos2(x1, y + cap)], s);
    }
    let tx = (x0 + x1) / 2.0;
    let galley = painter.layout_no_wrap(
        label.clone(),
        egui::FontId::monospace(crate::theme::tok::T_BODY),
        crate::theme::TEXT,
    );
    let pad = vec2(8.0, 3.0);
    let chip = egui::Rect::from_center_size(
        pos2(tx, y - cap - 2.0 - galley.size().y / 2.0 - pad.y),
        galley.size() + pad * 2.0,
    );
    painter.rect(
        chip,
        7.0,
        crate::theme::PANEL_GLASS,
        Stroke::new(1.0, crate::theme::OUTLINE),
        egui::StrokeKind::Inside,
    );
    painter.text(
        chip.center(),
        egui::Align2::CENTER_CENTER,
        &label,
        egui::FontId::monospace(crate::theme::tok::T_BODY),
        crate::theme::ACCENT_BRIGHT,
    );
}
pub(super) fn world_to_screen_pos(
    app: &AppState,
    origin: egui::Pos2,
    wx: f64,
    wy: f64,
) -> egui::Pos2 {
    let (sx, sy) = app.view.world_to_screen(wx, wy);
    pos2(origin.x + sx as f32, origin.y + sy as f32)
}

pub(super) fn trim_decimals(v: f64, prec: usize) -> String {
    let s = format!("{v:.prec$}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

pub(super) fn format_distance(d: f64) -> String {
    if d >= 1.0 && (d.fract()).abs() < 1e-9 {
        format!("{}", d.round() as i64)
    } else {
        trim_decimals(d, 6)
    }
}

pub(super) fn draw_dashed_line(
    painter: &egui::Painter,
    start: egui::Pos2,
    end: egui::Pos2,
    stroke: Stroke,
    dash_length: f32,
    gap_length: f32,
) {
    if !start.x.is_finite() || !start.y.is_finite() || !end.x.is_finite() || !end.y.is_finite() {
        return;
    }
    let dx = end.x - start.x;
    let dy = end.y - start.y;
    let len = (dx * dx + dy * dy).sqrt();
    if !len.is_finite() || len < 1e-6 {
        return;
    }
    let ux = dx / len;
    let uy = dy / len;

    let mut dist = 0.0;
    let mut count = 0;
    while dist < len && count < 1000 {
        let next_dist = (dist + dash_length).min(len);
        let p1 = pos2(start.x + ux * dist, start.y + uy * dist);
        let p2 = pos2(start.x + ux * next_dist, start.y + uy * next_dist);
        painter.line_segment([p1, p2], stroke);
        dist += dash_length + gap_length;
        count += 1;
    }
}
pub(super) fn draw_transform_ghost(
    painter: &egui::Painter,
    app: &AppState,
    to_screen: &impl Fn(f64, f64) -> egui::Pos2,
) {
    use oxidraft_geometry::Transform2d;
    let (cx, cy) = app.cursor_world;
    let ghost = Stroke::new(1.5, crate::theme::PREVIEW);
    if let Tool::Offset {
        dist,
        source: Some(src),
    } = &app.tool
    {
        if let Some(c) = app.document.get(*src).and_then(|e| e.as_curve()) {
            let plus = oxidraft_geometry::offset_curve(c, dist.abs());
            let minus = oxidraft_geometry::offset_curve(c, -dist.abs());
            let dp = oxidraft_geometry::point_to_curve_distance(&plus, cx, cy);
            let dm = oxidraft_geometry::point_to_curve_distance(&minus, cx, cy);
            let chosen = if dp <= dm { plus } else { minus };
            draw_curve(painter, &chosen, to_screen, ghost);
        }
        return;
    }

    let (t, ids): (Transform2d, &Vec<oxidraft_document::EntityId>) = match &app.tool {
        Tool::Move { base: Some(b), ids } | Tool::Copy { base: Some(b), ids } => {
            let (bx, by) = b.to_f64();
            (Transform2d::translation(cx - bx, cy - by), ids)
        }
        Tool::Rotate { base: Some(b), ids } => {
            let (bx, by) = b.to_f64();
            (
                Transform2d::rotation_about(b, (cy - by).atan2(cx - bx)),
                ids,
            )
        }
        Tool::Scale {
            base: Some(b),
            reference: Some(r1),
            ids,
        } => {
            let factor = (b.dist_f64(&Point2d::from_f64(cx, cy)) / r1).max(1e-9);
            (Transform2d::scale_about(b, factor, factor), ids)
        }
        Tool::Mirror {
            first: Some(f),
            ids,
        } => {
            let (fx, fy) = f.to_f64();
            if (cx - fx).hypot(cy - fy) < 1e-9 {
                return;
            }
            (Transform2d::mirror_line(f, &Point2d::from_f64(cx, cy)), ids)
        }
        _ => return,
    };
    let sel = if ids.is_empty() { &app.selection } else { ids };
    for &id in sel {
        if id == app.origin_id {
            continue;
        }
        if let Some(c) = app.document.get(id).and_then(|e| e.as_curve()) {
            draw_curve(painter, &t.apply_curve(c), to_screen, ghost);
        }
    }
}
/// Ghost-previews the candidate blend curve. While only the first entity is
/// picked, the hovered entity (if any) stands in as a tentative second pick,
/// drawn faint; once both are picked, the same curve is drawn at full
/// strength as the confirm popup (`overlays::blend_confirm_hud`) is shown.
pub(super) fn draw_blend_preview(
    painter: &egui::Painter,
    app: &AppState,
    to_screen: &impl Fn(f64, f64) -> egui::Pos2,
    hovered_id: Option<EntityId>,
) {
    let Tool::Blend {
        continuity,
        tension,
        first: Some(a),
        second,
    } = app.tool
    else {
        return;
    };
    let (b, faint) = match second {
        Some(b) => (b, false),
        None => match hovered_id {
            Some(h) if h != a => (h, true),
            _ => return,
        },
    };
    let Some(curve) = oxidraft_cad::edit::blend_preview(&app.document, a, b, continuity, tension)
    else {
        return;
    };
    let stroke = if faint {
        Stroke::new(1.5, crate::theme::PREVIEW.gamma_multiply(0.45))
    } else {
        Stroke::new(1.5, crate::theme::PREVIEW)
    };
    draw_curve(painter, &curve, to_screen, stroke);
}
pub(super) fn draw_trim_extend_preview(
    painter: &egui::Painter,
    app: &AppState,
    to_screen: &impl Fn(f64, f64) -> egui::Pos2,
) {
    use crate::state::TrimExtendPreview;
    match app.trim_extend_preview() {
        Some(TrimExtendPreview::Remove(curve)) => {
            let danger = Stroke::new(2.5, crate::theme::PREVIEW);
            draw_curve(painter, &curve, to_screen, danger);
        }
        Some(TrimExtendPreview::Extension(curve)) => {
            let ghost = Stroke::new(1.5, crate::theme::PREVIEW);
            draw_curve(painter, &curve, to_screen, ghost);
        }
        None => {}
    }
}

pub(super) fn layer_visible(app: &AppState, e: &oxidraft_document::Entity) -> bool {
    app.document.layers.get(e.layer).is_none_or(|l| l.on)
}

pub(super) fn resolve_color(app: &AppState, e: &oxidraft_document::Entity) -> (u8, u8, u8) {
    match &e.color {
        Color::Rgb(r, g, b) => (*r, *g, *b),
        _ => app
            .document
            .layers
            .get(e.layer)
            .map(|l| l.color)
            .unwrap_or((220, 220, 220)),
    }
}

pub(super) fn resolve_line_weight_px(app: &AppState, e: &oxidraft_document::Entity) -> f32 {
    if !app.prefs.show_lineweights {
        return HAIRLINE_PX;
    }
    let layer_mm = app
        .document
        .layers
        .get(e.layer)
        .map(|l| l.line_weight_mm)
        .unwrap_or(0.0);
    let mm = e.line_weight.to_mm(layer_mm) as f32;
    (mm * app.prefs.lineweight_scale as f32).max(HAIRLINE_PX)
}

pub(super) fn resolve_line_pattern(app: &AppState, e: &oxidraft_document::Entity) -> Vec<f32> {
    let name = match &e.line_type {
        LineTypeRef::Named(n) => Some(n.clone()),
        LineTypeRef::ByLayer | LineTypeRef::ByBlock => {
            app.document
                .layers
                .get(e.layer)
                .and_then(|l| match &l.line_type {
                    LineTypeRef::Named(n) => Some(n.clone()),
                    _ => None,
                })
        }
    };
    let Some(name) = name else {
        return Vec::new();
    };
    if name == "Continuous" {
        return Vec::new();
    }
    let Some(def) = app.document.line_types.iter().find(|d| d.name == name) else {
        return Vec::new();
    };
    if def.pattern.is_empty() {
        return Vec::new();
    }
    let px_per_world = (1.0 / app.view.pixel_world_size()) as f32;
    def.pattern
        .iter()
        .map(|v| *v as f32 * px_per_world)
        .collect()
}

// Bucket the zoom-dependent tessellation tolerance into power-of-two steps so
// small zoom jitter doesn't thrash the caches — only crossing a bucket boundary
// invalidates. Returns (bucket for signatures, world-space tolerance).
fn tol_bucket(app: &AppState) -> (i64, f64) {
    let target = (app.view.pixel_world_size() * 0.4).max(1e-9);
    let bucket = target.log2().floor();
    (bucket as i64, 2f64.powf(bucket))
}

// The three refresh_* functions share a shape: a serial scan hashes cheap
// signatures to find stale entries, then the expensive recomputes fan out
// across cores with rayon. Bulk misses come from document loads and
// zoom-bucket crossings — exactly where the parallelism pays — while an empty
// stale list costs rayon nothing on quiet frames.
pub(super) fn refresh_hatch_cache(app: &AppState, cache: &mut super::HatchCache) {
    use rayon::prelude::*;
    use std::collections::HashSet;
    let (bucket, tol) = tol_bucket(app);
    let mut live: HashSet<EntityId> = HashSet::new();
    // Each entry is (id, sig, boundary, holes).
    let mut stale = Vec::new();
    for e in app.document.iter() {
        if let EntityKind::Hatch {
            boundary, holes, ..
        } = &e.kind
        {
            live.insert(e.id);
            let sig = hatch_signature(boundary, holes, bucket);
            if cache.get(&e.id).map(|(s, _, _)| *s) != Some(sig) {
                stale.push((e.id, sig, boundary, holes));
            }
        }
    }
    let computed: Vec<_> = stale
        .par_iter()
        .map(|&(id, sig, boundary, holes)| {
            let tris = oxidraft_cad::triangulate_hatch_with_tol(boundary, holes, tol);
            let loops = oxidraft_cad::hatch_outline_loops(boundary, holes, tol);
            (id, sig, tris, loops)
        })
        .collect();
    for (id, sig, tris, loops) in computed {
        cache.insert(id, (sig, tris, loops));
    }
    cache.retain(|id, _| live.contains(id));
}

pub(super) fn refresh_text_cache(app: &AppState, cache: &mut super::TextCache) {
    use rayon::prelude::*;
    use std::collections::HashSet;
    let (bucket, tol) = tol_bucket(app);
    let mut live: HashSet<EntityId> = HashSet::new();
    // Each entry is (id, sig, content, font, height, anchor, rotation).
    let mut stale = Vec::new();
    for e in app.document.iter() {
        if let EntityKind::Text {
            anchor,
            content,
            height,
            rotation,
            font,
        } = &e.kind
        {
            live.insert(e.id);
            let sig = text_signature(
                content,
                *height,
                *rotation,
                font.as_deref(),
                *anchor,
                bucket,
            );
            if cache.get(&e.id).map(|(s, _)| *s) != Some(sig) {
                stale.push((
                    e.id,
                    sig,
                    content,
                    font.as_deref(),
                    *height,
                    *anchor,
                    *rotation,
                ));
            }
        }
    }
    // `outline_text` only reads the OnceLock-backed font database, so it is
    // safe to call from worker threads.
    let computed: Vec<_> = stale
        .par_iter()
        .map(|&(id, sig, content, font, height, anchor, rotation)| {
            let contours = crate::fonts::outline_text(content, font, height, anchor, rotation);
            (id, sig, oxidraft_cad::triangulate_contours(&contours, tol))
        })
        .collect();
    for (id, sig, tris) in computed {
        cache.insert(id, (sig, tris));
    }
    cache.retain(|id, _| live.contains(id));
}

pub(super) fn refresh_curve_cache(app: &AppState, cache: &mut super::CurveCache) {
    use rayon::prelude::*;
    use std::collections::HashSet;
    let (bucket, tol) = tol_bucket(app);
    let mut live: HashSet<EntityId> = HashSet::new();
    let mut stale: Vec<(EntityId, u64, &Curve)> = Vec::new();
    for e in app.document.iter() {
        if e.id == app.origin_id {
            continue;
        }
        if let EntityKind::Curve(c) = &e.kind {
            // Lines are drawn with a direct two-point segment (no flattening),
            // so caching them would only bloat the map.
            if matches!(c, Curve::Line(_)) {
                continue;
            }
            live.insert(e.id);
            let sig = curve_signature(c, bucket);
            if cache.get(&e.id).map(|(s, _, _)| *s) != Some(sig) {
                stale.push((e.id, sig, c));
            }
        }
    }
    let computed: Vec<_> = stale
        .par_iter()
        .map(|&(id, sig, c)| (id, sig, flatten_curve_world(c, tol), is_closed_curve(c)))
        .collect();
    for (id, sig, pts, closed) in computed {
        cache.insert(id, (sig, pts, closed));
    }
    cache.retain(|id, _| live.contains(id));
}

// Like `hatch_signature`, this samples each curve (per sub-segment for
// polycurves) at a few parametric points, so an edit that moves none of the
// sampled points could in theory be missed — the same accepted trade-off as
// the hatch/text caches.
fn curve_signature(c: &Curve, tol_bucket: i64) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    fn feed(c: &Curve, h: &mut DefaultHasher) {
        if let Curve::Poly(p) = c {
            (p.segments.len() as u64).hash(h);
            for seg in &p.segments {
                feed(seg, h);
            }
            return;
        }
        let (t0, t1) = c.domain();
        for k in 0..=4 {
            let t = t0 + (t1 - t0) * k as f64 / 4.0;
            let (x, y) = c.evaluate_f64(t);
            x.to_bits().hash(h);
            y.to_bits().hash(h);
        }
    }
    let mut h = DefaultHasher::new();
    tol_bucket.hash(&mut h);
    feed(c, &mut h);
    h.finish()
}

fn text_signature(
    content: &str,
    height: f64,
    rotation: f64,
    font: Option<&str>,
    anchor: Point2d,
    tol_bucket: i64,
) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    tol_bucket.hash(&mut h);
    content.hash(&mut h);
    height.to_bits().hash(&mut h);
    rotation.to_bits().hash(&mut h);
    font.hash(&mut h);
    anchor.x.to_bits().hash(&mut h);
    anchor.y.to_bits().hash(&mut h);
    h.finish()
}

fn hatch_signature(boundary: &[Curve], holes: &[Vec<Curve>], tol_bucket: i64) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    fn feed(loop_: &[Curve], h: &mut DefaultHasher) {
        (loop_.len() as u64).hash(h);
        for c in loop_ {
            let (t0, t1) = c.domain();
            for k in 0..=4 {
                let t = t0 + (t1 - t0) * k as f64 / 4.0;
                let (x, y) = c.evaluate_f64(t);
                x.to_bits().hash(h);
                y.to_bits().hash(h);
            }
        }
    }
    let mut h = DefaultHasher::new();
    tol_bucket.hash(&mut h);
    feed(boundary, &mut h);
    (holes.len() as u64).hash(&mut h);
    for hole in holes {
        feed(hole, &mut h);
    }
    h.finish()
}

#[allow(clippy::too_many_arguments)]
pub(super) fn draw_entity(
    painter: &egui::Painter,
    app: &AppState,
    e: &oxidraft_document::Entity,
    origin: egui::Pos2,
    stroke: Stroke,
    selected: bool,
    hatch_tris: Option<&[[Point2d; 3]]>,
    hatch_loops: Option<&[Vec<Point2d>]>,
    text_tris: Option<&[[Point2d; 3]]>,
    curve_pts: Option<(&[Point2d], bool)>,
) {
    let to_screen = |wx: f64, wy: f64| world_to_screen_pos(app, origin, wx, wy);

    if e.id == app.origin_id {
        let origin_screen = to_screen(0.0, 0.0);
        let stroke_x = Stroke::new(1.5, Color32::from_rgb(255, 60, 60));
        let stroke_y = Stroke::new(1.5, Color32::from_rgb(60, 220, 60));
        painter.line_segment(
            [origin_screen, pos2(origin_screen.x + 18.0, origin_screen.y)],
            stroke_x,
        );
        painter.line_segment(
            [
                pos2(origin_screen.x + 18.0, origin_screen.y),
                pos2(origin_screen.x + 14.0, origin_screen.y - 3.0),
            ],
            stroke_x,
        );
        painter.line_segment(
            [
                pos2(origin_screen.x + 18.0, origin_screen.y),
                pos2(origin_screen.x + 14.0, origin_screen.y + 3.0),
            ],
            stroke_x,
        );
        painter.text(
            pos2(origin_screen.x + 24.0, origin_screen.y),
            egui::Align2::CENTER_CENTER,
            "X",
            egui::FontId::proportional(crate::theme::tok::T_CAPTION),
            stroke_x.color,
        );

        painter.line_segment(
            [origin_screen, pos2(origin_screen.x, origin_screen.y - 18.0)],
            stroke_y,
        );
        painter.line_segment(
            [
                pos2(origin_screen.x, origin_screen.y - 18.0),
                pos2(origin_screen.x - 3.0, origin_screen.y - 14.0),
            ],
            stroke_y,
        );
        painter.line_segment(
            [
                pos2(origin_screen.x, origin_screen.y - 18.0),
                pos2(origin_screen.x + 3.0, origin_screen.y - 14.0),
            ],
            stroke_y,
        );
        painter.text(
            pos2(origin_screen.x, origin_screen.y - 24.0),
            egui::Align2::CENTER_CENTER,
            "Y",
            egui::FontId::proportional(crate::theme::tok::T_CAPTION),
            stroke_y.color,
        );
        painter.circle_filled(origin_screen, 3.0, Color32::from_rgb(180, 195, 220));
        painter.circle_stroke(
            origin_screen,
            5.0,
            Stroke::new(1.0, Color32::from_rgb(80, 90, 110)),
        );
        return;
    }

    match &e.kind {
        EntityKind::Curve(c) => {
            let pattern = resolve_line_pattern(app, e);
            match (curve_pts, c) {
                // Lines are never cached (drawn as a direct segment), and a
                // cache miss falls back to the uncached screen-space path.
                (None, _) | (_, Curve::Line(_)) => {
                    draw_curve_patterned(painter, c, &to_screen, stroke, &pattern);
                }
                (Some((world_pts, closed)), _) => {
                    let mut pts: Vec<egui::Pos2> =
                        world_pts.iter().map(|p| to_screen(p.x, p.y)).collect();
                    if !pattern.is_empty() {
                        draw_patterned_polyline(painter, &pts, stroke, &pattern);
                    } else if closed {
                        if pts.len() >= 2 && (pts[0] - pts[pts.len() - 1]).length() < 0.5 {
                            pts.pop();
                        }
                        painter.add(egui::Shape::closed_line(pts, stroke));
                    } else {
                        painter.add(egui::Shape::line(pts, stroke));
                    }
                }
            }
        }
        EntityKind::Point(p) => {
            let (x, y) = p.to_f64();
            painter.circle_filled(to_screen(x, y), 2.0, stroke.color);
        }
        EntityKind::Text {
            anchor,
            content,
            height,
            rotation,
            font,
        } => {
            if let Some(tris) = text_tris.filter(|t| !t.is_empty()) {
                let mut mesh = egui::epaint::Mesh::default();
                for t in tris {
                    let base = mesh.vertices.len() as u32;
                    for v in t {
                        mesh.colored_vertex(to_screen(v.x, v.y), stroke.color);
                    }
                    mesh.add_triangle(base, base + 1, base + 2);
                }
                painter.add(egui::Shape::mesh(mesh));
            } else if !content.is_empty() {
                let (x, y) = anchor.to_f64();
                const MIN_PX: f32 = 8.0;
                const MAX_PX: f32 = 512.0;
                let target_px = (*height as f32 * app.view.zoom as f32).max(0.01);
                let raster_px = 2f32
                    .powf(target_px.clamp(MIN_PX, MAX_PX).log2().ceil())
                    .clamp(MIN_PX, MAX_PX);
                let scale = target_px / raster_px;
                let font = crate::fonts::text_font_id(painter.ctx(), font.as_deref(), raster_px);
                let galley = painter.layout_no_wrap(content.clone(), font, stroke.color);
                let angle = -(*rotation as f32);
                let baseline = galley
                    .rows
                    .first()
                    .and_then(|r| r.row.glyphs.first().map(|g| r.pos.y + g.pos.y))
                    .unwrap_or_else(|| galley.size().y);
                let off = baseline * scale;
                let (sn, cs) = angle.sin_cos();
                let pos = to_screen(x, y) + vec2(off * sn, -off * cs);
                let mut shape = egui::epaint::TextShape::new(pos, galley, stroke.color);
                shape.angle = angle;
                if (scale - 1.0).abs() > 1e-3 {
                    shape.transform(egui::emath::TSTransform {
                        scaling: scale,
                        translation: pos.to_vec2() * (1.0 - scale),
                    });
                }
                painter.add(shape);
            }
        }
        EntityKind::Hatch {
            boundary,
            holes,
            fill,
            pattern,
        } => {
            use oxidraft_document::HatchPattern;
            let fill_col = if selected {
                HATCH_SELECT.gamma_multiply(130.0 / 255.0)
            } else {
                Color32::from_rgb(fill.0, fill.1, fill.2)
            };
            match pattern {
                HatchPattern::Solid => {
                    let computed;
                    let tris: &[[Point2d; 3]] = match hatch_tris {
                        Some(t) => t,
                        None => {
                            computed = oxidraft_cad::triangulate_hatch(boundary, holes);
                            &computed
                        }
                    };
                    if !tris.is_empty() {
                        let mut mesh = egui::epaint::Mesh::default();
                        for t in tris {
                            let base = mesh.vertices.len() as u32;
                            for v in t {
                                mesh.colored_vertex(to_screen(v.x, v.y), fill_col);
                            }
                            mesh.add_triangle(base, base + 1, base + 2);
                        }
                        painter.add(egui::Shape::mesh(mesh));
                    }
                }
                HatchPattern::Lines { .. } | HatchPattern::Cross { .. } => {
                    let pat = Stroke::new(1.0, fill_col);
                    for (a, b) in oxidraft_cad::hatch_pattern_lines(boundary, holes, *pattern) {
                        painter.line_segment([to_screen(a.x, a.y), to_screen(b.x, b.y)], pat);
                    }
                }
                HatchPattern::Dots { .. } => {
                    for p in oxidraft_cad::hatch_pattern_dots(boundary, holes, *pattern) {
                        painter.circle_filled(to_screen(p.x, p.y), 1.3, fill_col);
                    }
                }
            }
            match hatch_loops {
                Some(loops) => {
                    for ring in loops {
                        if ring.len() >= 2 {
                            let pts: Vec<egui::Pos2> =
                                ring.iter().map(|p| to_screen(p.x, p.y)).collect();
                            painter.add(egui::Shape::closed_line(pts, stroke));
                        }
                    }
                }
                None => {
                    for seg in boundary {
                        draw_curve(painter, seg, &to_screen, stroke);
                    }
                    for hole in holes {
                        for seg in hole {
                            draw_curve(painter, seg, &to_screen, stroke);
                        }
                    }
                }
            }
        }
        EntityKind::Dimension {
            p1,
            p2,
            line,
            override_text,
            ..
        } => {
            draw_dimension(
                painter,
                app,
                *p1,
                *p2,
                *line,
                override_text.as_deref(),
                &to_screen,
                stroke.color,
            );
        }
        EntityKind::OrthoDim {
            p1,
            p2,
            line,
            vertical,
            override_text,
            ..
        } => {
            draw_ortho_dim(
                painter,
                app,
                *p1,
                *p2,
                *line,
                *vertical,
                override_text.as_deref(),
                &to_screen,
                stroke.color,
            );
        }
        EntityKind::AngularDim {
            center,
            p1,
            p2,
            line,
            override_text,
            ..
        } => {
            draw_angular_dim(
                painter,
                app,
                *center,
                *p1,
                *p2,
                *line,
                override_text.as_deref(),
                &to_screen,
                stroke.color,
            );
        }
        EntityKind::RadialDim {
            center,
            edge,
            diameter,
            override_text,
            ..
        } => {
            draw_radial_dim(
                painter,
                app,
                *center,
                *edge,
                *diameter,
                override_text.as_deref(),
                &to_screen,
                stroke.color,
            );
        }
        _ => {}
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn draw_dimension(
    painter: &egui::Painter,
    app: &AppState,
    p1: Point2d,
    p2: Point2d,
    line: Point2d,
    ovr: Option<&str>,
    to_screen: &impl Fn(f64, f64) -> egui::Pos2,
    color: Color32,
) {
    let (x1, y1) = p1.to_f64();
    let (x2, y2) = p2.to_f64();
    let (lx, ly) = line.to_f64();
    let (dx, dy) = (x2 - x1, y2 - y1);
    let len = (dx * dx + dy * dy).sqrt();
    if len < 1e-9 {
        return;
    }
    let (ux, uy) = (dx / len, dy / len);
    let t1 = (x1 - lx) * ux + (y1 - ly) * uy;
    let t2 = (x2 - lx) * ux + (y2 - ly) * uy;
    let d1 = (lx + t1 * ux, ly + t1 * uy);
    let d2 = (lx + t2 * ux, ly + t2 * uy);

    let s1 = to_screen(x1, y1);
    let s2 = to_screen(x2, y2);
    let sd1 = to_screen(d1.0, d1.1);
    let sd2 = to_screen(d2.0, d2.1);
    let stroke = Stroke::new(1.0, color);

    let style = &app.document.settings.dim_style;
    let zoom = app.view.zoom as f32;

    painter.line_segment([s1, sd1], stroke);
    painter.line_segment([s2, sd2], stroke);
    painter.line_segment([sd1, sd2], stroke);
    let arrow_px = (style.arrow_size as f32 * zoom).clamp(4.0, 60.0);
    arrowhead(painter, sd1, sd2, arrow_px, color);
    arrowhead(painter, sd2, sd1, arrow_px, color);

    let label = match ovr {
        Some(t) => t.to_string(),
        None => format_measure(len, app.document.settings.units, style.precision),
    };
    let text_px = (style.text_height as f32 * zoom).clamp(9.0, 200.0);
    let font_id = crate::fonts::text_font_id(painter.ctx(), style.font.as_deref(), text_px);
    let galley = painter.layout_no_wrap(label, font_id, color);
    let size = galley.size();

    let dir = (sd2 - sd1).normalized();
    let mut angle = dir.y.atan2(dir.x);
    use std::f32::consts::FRAC_PI_2;
    if !(-FRAC_PI_2..=FRAC_PI_2).contains(&angle) {
        angle += std::f32::consts::PI;
    }
    let mut perp = vec2(-dir.y, dir.x);
    let mid = pos2((sd1.x + sd2.x) * 0.5, (sd1.y + sd2.y) * 0.5);
    let mid_meas = pos2((s1.x + s2.x) * 0.5, (s1.y + s2.y) * 0.5);
    if (mid + perp - mid_meas).length_sq() < (mid - perp - mid_meas).length_sq() {
        perp = -perp;
    }
    let gap = text_px * 0.5 + 3.0;
    let center = mid + perp * gap;
    let rot = egui::emath::Rot2::from_angle(angle);
    let mut shape = egui::epaint::TextShape::new(center - rot * (size * 0.5), galley, color);
    shape.angle = angle;
    painter.add(shape);
}

#[allow(clippy::too_many_arguments)]
pub(super) fn draw_ortho_dim(
    painter: &egui::Painter,
    app: &AppState,
    p1: Point2d,
    p2: Point2d,
    line: Point2d,
    vertical: bool,
    ovr: Option<&str>,
    to_screen: &impl Fn(f64, f64) -> egui::Pos2,
    color: Color32,
) {
    let (x1, y1) = p1.to_f64();
    let (x2, y2) = p2.to_f64();
    let (lx, ly) = line.to_f64();
    let (d1, d2, measured) = if vertical {
        ((lx, y1), (lx, y2), (y2 - y1).abs())
    } else {
        ((x1, ly), (x2, ly), (x2 - x1).abs())
    };
    if measured < 1e-12 {
        return;
    }

    let s1 = to_screen(x1, y1);
    let s2 = to_screen(x2, y2);
    let sd1 = to_screen(d1.0, d1.1);
    let sd2 = to_screen(d2.0, d2.1);
    let stroke = Stroke::new(1.0, color);
    let style = &app.document.settings.dim_style;
    let zoom = app.view.zoom as f32;

    painter.line_segment([s1, sd1], stroke);
    painter.line_segment([s2, sd2], stroke);
    painter.line_segment([sd1, sd2], stroke);
    let arrow_px = (style.arrow_size as f32 * zoom).clamp(4.0, 60.0);
    arrowhead(painter, sd1, sd2, arrow_px, color);
    arrowhead(painter, sd2, sd1, arrow_px, color);

    let label = match ovr {
        Some(t) => t.to_string(),
        None => format_measure(measured, app.document.settings.units, style.precision),
    };
    let text_px = (style.text_height as f32 * zoom).clamp(9.0, 200.0);
    let font_id = crate::fonts::text_font_id(painter.ctx(), style.font.as_deref(), text_px);
    let galley = painter.layout_no_wrap(label, font_id, color);
    let mid = pos2((sd1.x + sd2.x) * 0.5, (sd1.y + sd2.y) * 0.5);
    let mid_meas = pos2((s1.x + s2.x) * 0.5, (s1.y + s2.y) * 0.5);
    let gap = text_px * 0.5 + 3.0;
    let perp = if vertical {
        let s = if mid.x >= mid_meas.x { 1.0 } else { -1.0 };
        vec2(s * (gap + galley.size().x * 0.5), 0.0)
    } else {
        let s = if mid.y >= mid_meas.y { 1.0 } else { -1.0 };
        vec2(0.0, s * gap)
    };
    let center = mid + perp;
    painter.galley(center - galley.size() * 0.5, galley, color);
}

#[allow(clippy::too_many_arguments)]
pub(super) fn draw_angular_dim(
    painter: &egui::Painter,
    app: &AppState,
    center: Point2d,
    p1: Point2d,
    p2: Point2d,
    line: Point2d,
    ovr: Option<&str>,
    to_screen: &impl Fn(f64, f64) -> egui::Pos2,
    color: Color32,
) {
    let (cx, cy) = center.to_f64();
    let sw = oxidraft_document::angular_sweep(center, p1, p2, line);
    let (start, sweep, r) = (sw.start, sw.sweep, sw.radius);

    let stroke = Stroke::new(1.0, color);
    let arc_pt = |ang: f64| (cx + r * ang.cos(), cy + r * ang.sin());
    let (e1x, e1y) = arc_pt(start);
    let (e2x, e2y) = arc_pt(start + sweep);
    painter.line_segment([to_screen(p1.x, p1.y), to_screen(e1x, e1y)], stroke);
    painter.line_segment([to_screen(p2.x, p2.y), to_screen(e2x, e2y)], stroke);

    let steps = 48.max((sweep.abs() / 0.05) as usize).min(512);
    let mut pts: Vec<egui::Pos2> = Vec::with_capacity(steps + 1);
    for i in 0..=steps {
        let a = start + sweep * (i as f64 / steps as f64);
        let (x, y) = arc_pt(a);
        pts.push(to_screen(x, y));
    }
    painter.add(egui::Shape::line(pts.clone(), stroke));

    let style = &app.document.settings.dim_style;
    let arrow_px = (style.arrow_size as f32 * app.view.zoom as f32).clamp(4.0, 60.0);
    if pts.len() >= 2 {
        arrowhead(painter, pts[0], pts[1], arrow_px, color);
        let n = pts.len();
        arrowhead(painter, pts[n - 1], pts[n - 2], arrow_px, color);
    }

    let mid_a = start + sweep * 0.5;
    let (mx, my) = (cx + r * mid_a.cos(), cy + r * mid_a.sin());
    let deg = sweep.abs().to_degrees();
    let label = match ovr {
        Some(t) => t.to_string(),
        None => format!(
            "{deg:.*}\u{00b0}",
            oxidraft_document::clamp_precision(style.precision)
        ),
    };
    let text_px = (style.text_height as f32 * app.view.zoom as f32).clamp(9.0, 200.0);
    let font_id = crate::fonts::text_font_id(painter.ctx(), style.font.as_deref(), text_px);
    let galley = painter.layout_no_wrap(label, font_id, color);
    let out = text_px * 0.5 + 3.0;
    let sc = to_screen(mx, my);
    let dir = egui::vec2(mid_a.cos() as f32, -(mid_a.sin() as f32));
    let pos = sc + dir * out - galley.size() * 0.5;
    painter.galley(pos, galley, color);
}

#[allow(clippy::too_many_arguments)]
pub(super) fn draw_radial_dim(
    painter: &egui::Painter,
    app: &AppState,
    center: Point2d,
    edge: Point2d,
    diameter: bool,
    ovr: Option<&str>,
    to_screen: &impl Fn(f64, f64) -> egui::Pos2,
    color: Color32,
) {
    let (cx, cy) = center.to_f64();
    let (ex, ey) = edge.to_f64();
    let r = center.dist_f64(&edge);
    if r < 1e-9 {
        return;
    }
    let (ux, uy) = ((ex - cx) / r, (ey - cy) / r);
    let stroke = Stroke::new(1.0, color);

    let near = if diameter {
        (cx - ux * r, cy - uy * r)
    } else {
        (cx, cy)
    };
    let s_near = to_screen(near.0, near.1);
    let s_edge = to_screen(ex, ey);
    painter.line_segment([s_near, s_edge], stroke);

    let style = &app.document.settings.dim_style;
    let arrow_px = (style.arrow_size as f32 * app.view.zoom as f32).clamp(4.0, 60.0);
    arrowhead(painter, s_edge, s_near, arrow_px, color);
    if diameter {
        arrowhead(painter, s_near, s_edge, arrow_px, color);
    }

    let value = if diameter { 2.0 * r } else { r };
    let prefix = if diameter { "\u{00d8}" } else { "R" };
    let label = match ovr {
        Some(t) => t.to_string(),
        None => format!(
            "{prefix}{}",
            format_measure(value, app.document.settings.units, style.precision)
        ),
    };
    let text_px = (style.text_height as f32 * app.view.zoom as f32).clamp(9.0, 200.0);
    let font_id = crate::fonts::text_font_id(painter.ctx(), style.font.as_deref(), text_px);
    let galley = painter.layout_no_wrap(label, font_id, color);
    let leader_dir = (s_edge - s_near).normalized();
    let anchor = s_edge + leader_dir * 6.0;
    let off = if leader_dir.x >= 0.0 {
        egui::vec2(2.0, -galley.size().y * 0.5)
    } else {
        egui::vec2(-galley.size().x - 2.0, -galley.size().y * 0.5)
    };
    painter.galley(anchor + off, galley, color);
}

fn arrowhead(
    painter: &egui::Painter,
    tip: egui::Pos2,
    from: egui::Pos2,
    size: f32,
    color: Color32,
) {
    let d = tip - from;
    let len = d.length();
    if len < 1e-3 {
        return;
    }
    let dir = d / len;
    let back = tip - dir * size;
    let perp = vec2(-dir.y, dir.x) * (size * 0.35);
    painter.add(egui::Shape::convex_polygon(
        vec![tip, back + perp, back - perp],
        color,
        Stroke::NONE,
    ));
}

fn format_measure(value: f64, units: oxidraft_document::Units, prec: usize) -> String {
    units.format_measure(value, prec)
}

pub(super) fn corner_glass_frame() -> egui::Frame {
    egui::Frame::new()
        .inner_margin(egui::Margin::symmetric(4, 3))
        .corner_radius(egui::CornerRadius::same(8))
        .fill(Color32::from_rgba_unmultiplied(26, 32, 42, 235))
        .stroke(Stroke::new(1.0, Color32::from_rgb(0, 200, 255)))
        .shadow(egui::epaint::Shadow {
            offset: [0, 3],
            blur: 14,
            spread: 0,
            color: Color32::from_black_alpha(130),
        })
}

pub(super) fn draw_corner_preview(
    painter: &egui::Painter,
    app: &AppState,
    ca: &crate::state::CornerAction,
    to_screen: &impl Fn(f64, f64) -> egui::Pos2,
) {
    let accent = Color32::from_rgb(0, 220, 255);
    let stroke = Stroke::new(2.0, Color32::from_rgba_unmultiplied(0, 220, 255, 128));
    let seg = |p: (f64, f64), q: (f64, f64)| [to_screen(p.0, p.1), to_screen(q.0, q.1)];
    let mut group = app.corner_group(&ca.geom, ca.kind);
    if group.is_empty() {
        group.push(ca.geom);
    }
    for g in &group {
        match ca.kind {
            crate::state::CornerKind::Fillet => {
                if let Some(sol) =
                    oxidraft_cad::edit::solve_fillet(g.edge_a, g.edge_b, ca.size, g.corner)
                {
                    draw_trimmed_edge(painter, &g.edge_a, g.corner, sol.ta, to_screen, stroke);
                    draw_trimmed_edge(painter, &g.edge_b, g.corner, sol.tb, to_screen, stroke);
                    draw_arc_short(
                        painter, sol.center, ca.size, sol.ta, sol.tb, to_screen, stroke,
                    );
                }
            }
            crate::state::CornerKind::Chamfer => {
                let far_a = (
                    g.corner.0 + g.dir_a.0 * g.len_a,
                    g.corner.1 + g.dir_a.1 * g.len_a,
                );
                let far_b = (
                    g.corner.0 + g.dir_b.0 * g.len_b,
                    g.corner.1 + g.dir_b.1 * g.len_b,
                );
                let p1 = (
                    g.corner.0 + g.dir_a.0 * ca.size,
                    g.corner.1 + g.dir_a.1 * ca.size,
                );
                let p2 = (
                    g.corner.0 + g.dir_b.0 * ca.size,
                    g.corner.1 + g.dir_b.1 * ca.size,
                );
                painter.line_segment(seg(far_a, p1), stroke);
                painter.line_segment(seg(far_b, p2), stroke);
                painter.line_segment(seg(p1, p2), stroke);
            }
        }
    }

    let cur = to_screen(app.cursor_world.0, app.cursor_world.1);
    painter.circle_filled(cur, 4.0, accent);
    let label = match ca.kind {
        crate::state::CornerKind::Fillet => format!("R {:.2}", ca.size),
        crate::state::CornerKind::Chamfer => format!("{:.2}", ca.size),
    };
    painter.text(
        pos2(cur.x + 9.0, cur.y - 9.0),
        egui::Align2::LEFT_BOTTOM,
        label,
        egui::FontId::monospace(crate::theme::tok::T_BODY),
        accent,
    );
}
fn draw_trimmed_edge(
    painter: &egui::Painter,
    edge: &oxidraft_cad::edit::CornerEdge,
    corner: (f64, f64),
    trim_pt: (f64, f64),
    to_screen: &impl Fn(f64, f64) -> egui::Pos2,
    stroke: Stroke,
) {
    use oxidraft_cad::edit::CornerEdge;
    let sq = |p: (f64, f64)| (p.0 - corner.0).powi(2) + (p.1 - corner.1).powi(2);
    match *edge {
        CornerEdge::Line { p0, p1 } => {
            let far = if sq(p0) > sq(p1) { p0 } else { p1 };
            painter.line_segment(
                [to_screen(far.0, far.1), to_screen(trim_pt.0, trim_pt.1)],
                stroke,
            );
        }
        CornerEdge::Arc {
            cx,
            cy,
            r,
            start,
            end,
        } => {
            let sp = (cx + r * start.cos(), cy + r * start.sin());
            let ep = (cx + r * end.cos(), cy + r * end.sin());
            let far_angle = if sq(sp) > sq(ep) { start } else { end };
            draw_arc_short(
                painter,
                (cx, cy),
                r,
                polar(cx, cy, r, far_angle),
                trim_pt,
                to_screen,
                stroke,
            );
        }
    }
}

fn polar(cx: f64, cy: f64, r: f64, angle: f64) -> (f64, f64) {
    (cx + r * angle.cos(), cy + r * angle.sin())
}

fn draw_arc_short(
    painter: &egui::Painter,
    center: (f64, f64),
    r: f64,
    a: (f64, f64),
    b: (f64, f64),
    to_screen: &impl Fn(f64, f64) -> egui::Pos2,
    stroke: Stroke,
) {
    let a0 = (a.1 - center.1).atan2(a.0 - center.0);
    let a1 = (b.1 - center.1).atan2(b.0 - center.0);
    let mut d = a1 - a0;
    while d > std::f64::consts::PI {
        d -= std::f64::consts::TAU;
    }
    while d < -std::f64::consts::PI {
        d += std::f64::consts::TAU;
    }
    let n = 28;
    let pts: Vec<_> = (0..=n)
        .map(|i| {
            let ang = a0 + d * (i as f64 / n as f64);
            to_screen(center.0 + r * ang.cos(), center.1 + r * ang.sin())
        })
        .collect();
    painter.add(egui::Shape::line(pts, stroke));
}

#[cfg(test)]
mod sig_tests {
    use super::*;
    use oxidraft_geometry::CubicBezier;

    fn bez(x0: f64) -> Curve {
        Curve::Bezier(CubicBezier::new(
            Point2d::new(x0, 0.0),
            Point2d::new(x0 + 3.0, 4.0),
            Point2d::new(x0 + 7.0, 4.0),
            Point2d::new(x0 + 10.0, 0.0),
        ))
    }

    #[test]
    fn curve_signature_stable_for_same_input() {
        assert_eq!(
            curve_signature(&bez(0.0), -3),
            curve_signature(&bez(0.0), -3)
        );
    }

    #[test]
    fn curve_signature_changes_when_geometry_moves() {
        assert_ne!(
            curve_signature(&bez(0.0), -3),
            curve_signature(&bez(0.1), -3)
        );
    }

    #[test]
    fn curve_signature_changes_across_tolerance_buckets() {
        assert_ne!(
            curve_signature(&bez(0.0), -3),
            curve_signature(&bez(0.0), -4)
        );
    }
}
