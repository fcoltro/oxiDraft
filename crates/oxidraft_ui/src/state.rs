//! [`AppState`] is the whole app's mutable state — document, selection, undo
//! history, active tool, view transform, and interaction state (grip drags,
//! bbox drags, in-progress polylines) — plus the methods everything else in
//! the UI dispatches through: command execution, constraint operations,
//! clipboard, file I/O, and grip/bbox drag handling.

use crate::command::{Command, CoordInput, parse_command, parse_coordinate};
use crate::history::History;
use crate::tools::{Tool, ToolEvent};
use crate::view_transform::ViewTransform;
use oxidraft_cad::{
    Grip, Guide, SnapPoint, SnapSettings, apply_grip, best_snap, edit, find_snaps_excluding,
    grips_for, infer_axis, pick_at,
};
use oxidraft_document::{
    ConstraintKind, Document, Entity, EntityId, EntityKind, Layer, LineTypeRef, LineWeight,
    SketchConstraint,
};
use oxidraft_geometry::{Curve, LineSeg, MinTracker, Point2d, Transform2d};

mod modify;
pub use modify::TrimExtendPreview;

mod contextual;
pub use contextual::{CornerAction, CornerGeom, CornerKind, fillet_arc};

/// The whole app's mutable state: the document, view, active tool,
/// selection, undo history, and every user-facing setting. Owned by the UI
/// shell and threaded through to every input handler and renderer.
/// One line of feedback, and whether it reports a problem.
///
/// Only the newest is ever shown, as a toast. Every message used to be drawn
/// in the same red-tinted alert frame, so "Plotted to PDF" and "Plot failed"
/// were indistinguishable — the level is what lets the toast tell them apart.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Note {
    /// Something happened, and it worked.
    Info(String),
    /// Something was refused, or failed. Says what to do next where it can.
    Problem(String),
}

impl Note {
    /// The message itself.
    pub fn text(&self) -> &str {
        match self {
            Note::Info(t) | Note::Problem(t) => t,
        }
    }

    /// Whether this reports something the user probably needs to act on.
    pub fn is_problem(&self) -> bool {
        matches!(self, Note::Problem(_))
    }
}

pub struct AppState {
    pub document: Document,
    pub view: ViewTransform,
    pub tool: Tool,
    pub selection: Vec<EntityId>,
    pub snap: SnapSettings,
    /// All persisted user preferences (snap/grid toggles, colors, scales) as
    /// one embedded value, so adding a preference is a single edit and
    /// persistence can't silently drop a field. See [`UiPrefs`].
    pub prefs: UiPrefs,
    pub last_command: Option<String>,
    pub history: History,
    pub command_log: Vec<Note>,
    pub cursor_world: (f64, f64),
    pub active_snap: Option<SnapPoint>,
    pub click_count: u32,
    pub origin_id: EntityId,
    pub interaction: InteractionState,
    pub current_file_path: Option<std::path::PathBuf>,
    pub hatch_pattern: oxidraft_document::HatchPattern,
    pub saved_revision: u64,
    pub zoom_target: Option<(f64, f64, f64)>,
    pub default_line_type: LineTypeRef,
    pub default_line_weight: LineWeight,
    pub clipboard: Vec<Entity>,
    pub hint_tool: Option<Tool>,
    pub pending_dim_edit: Option<SketchConstraint>,
    pub plot_window: Option<(f64, f64, f64, f64)>,
    pub plot_dialog_open: bool,
    pub plot_window_mode: bool,
    /// Entities to briefly pulse red after a constraint was rejected for
    /// conflicting with theirs, paired with when the flash started so the
    /// renderer can fade it out. Set on the failure path of constraint
    /// commands; cleared once it elapses.
    pub conflict_flash: Option<(Vec<EntityId>, std::time::Instant)>,
    /// Bumped on every structural change (a committed constraint, undo,
    /// redo) so the DoF status indicator recomputes only when it must,
    /// rather than solving every frame. See `dof_status`.
    pub doc_epoch: u64,
    dof_cache: Option<(u64, oxidraft_cad::DofSummary)>,
}

/// The subset of [`AppState`] that's a persisted user preference (snap/grid
/// toggles, colors, scales) rather than session/document state. Round-trips
/// through [`UiPrefs::serialize`]/[`UiPrefs::deserialize`] and is applied to
/// an `AppState` via [`AppState::apply_prefs`].
#[derive(Clone, Debug, PartialEq)]
pub struct UiPrefs {
    pub snap_on: bool,
    pub grid_on: bool,
    pub grid_snap_on: bool,
    pub polar_on: bool,
    pub track_on: bool,
    pub dyn_on: bool,
    pub comb_on: bool,
    pub comb_scale: f64,
    pub snap_px: f64,
    pub polar_step: f64,
    pub zoom_speed: f64,
    pub zoom_to_cursor: bool,
    pub invert_zoom: bool,
    pub crosshair: bool,
    pub pick_box: f64,
    pub show_lineweights: bool,
    pub lineweight_scale: f64,
    pub grid_dots: bool,
    pub grid_major_every: u32,
    pub grid_minor_rgb: (u8, u8, u8),
    pub grid_major_rgb: (u8, u8, u8),
    pub text_font: Option<String>,
    pub infer_constraints: bool,
    pub show_constraints: bool,
}

impl Default for UiPrefs {
    fn default() -> Self {
        UiPrefs {
            snap_on: true,
            grid_on: true,
            grid_snap_on: false,
            polar_on: true,
            track_on: true,
            dyn_on: true,
            comb_on: false,
            comb_scale: 5.0,
            snap_px: 12.0,
            polar_step: 45.0,
            zoom_speed: 1.0,
            zoom_to_cursor: true,
            invert_zoom: false,
            crosshair: true,
            pick_box: 11.0,
            show_lineweights: true,
            lineweight_scale: 5.0,
            grid_dots: false,
            grid_major_every: 5,
            grid_minor_rgb: (24, 28, 36),
            grid_major_rgb: (33, 39, 49),
            text_font: None,
            infer_constraints: true,
            show_constraints: true,
        }
    }
}

fn parse_rgb(s: &str) -> Option<(u8, u8, u8)> {
    let p: Vec<u8> = s.split(',').filter_map(|v| v.trim().parse().ok()).collect();
    (p.len() == 3).then(|| (p[0], p[1], p[2]))
}

impl UiPrefs {
    /// Encodes the prefs as `key=value` lines for persistence to disk.
    /// Paired with [`UiPrefs::deserialize`].
    pub fn serialize(&self) -> String {
        let b = |v: bool| if v { "1" } else { "0" };
        let rgb = |c: (u8, u8, u8)| format!("{},{},{}", c.0, c.1, c.2);
        let font = self.text_font.as_deref().unwrap_or("");
        format!(
            "snap={}\ngrid={}\ngsnap={}\npolar={}\ntrack={}\ndyn={}\ncomb={}\ncomb_scale={}\nsnap_px={}\npolar_step={}\nzoom_speed={}\nzoom_cursor={}\ninvert_zoom={}\ncrosshair={}\npick_box={}\nlw_show={}\nlw_scale={}\ngrid_dots={}\ngrid_major={}\ngrid_minor={}\ngrid_majorc={}\nfont={}\ninfer_con={}\nshow_con={}\n",
            b(self.snap_on),
            b(self.grid_on),
            b(self.grid_snap_on),
            b(self.polar_on),
            b(self.track_on),
            b(self.dyn_on),
            b(self.comb_on),
            self.comb_scale,
            self.snap_px,
            self.polar_step,
            self.zoom_speed,
            b(self.zoom_to_cursor),
            b(self.invert_zoom),
            b(self.crosshair),
            self.pick_box,
            b(self.show_lineweights),
            self.lineweight_scale,
            b(self.grid_dots),
            self.grid_major_every,
            rgb(self.grid_minor_rgb),
            rgb(self.grid_major_rgb),
            font,
            b(self.infer_constraints),
            b(self.show_constraints),
        )
    }

    /// Parses the `key=value` format written by [`UiPrefs::serialize`].
    /// Unrecognized keys and unparsable values are skipped, so a prefs file
    /// from an older version still loads with defaults for anything new.
    pub fn deserialize(s: &str) -> Self {
        let mut p = UiPrefs::default();
        for line in s.lines() {
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            let on = v == "1";
            match k.trim() {
                "snap" => p.snap_on = on,
                "grid" => p.grid_on = on,
                "gsnap" => p.grid_snap_on = on,
                "polar" => p.polar_on = on,
                "track" => p.track_on = on,
                "dyn" => p.dyn_on = on,
                "comb" => p.comb_on = on,
                "comb_scale" => {
                    if let Ok(f) = v.trim().parse::<f64>() {
                        p.comb_scale = f;
                    }
                }
                "snap_px" => {
                    if let Ok(f) = v.trim().parse::<f64>() {
                        p.snap_px = f.clamp(2.0, 40.0);
                    }
                }
                "polar_step" => {
                    if let Ok(f) = v.trim().parse::<f64>() {
                        p.polar_step = f.clamp(1.0, 90.0);
                    }
                }
                "zoom_speed" => {
                    if let Ok(f) = v.trim().parse::<f64>() {
                        p.zoom_speed = f.clamp(0.25, 4.0);
                    }
                }
                "zoom_cursor" => p.zoom_to_cursor = on,
                "invert_zoom" => p.invert_zoom = on,
                "crosshair" => p.crosshair = on,
                "pick_box" => {
                    if let Ok(f) = v.trim().parse::<f64>() {
                        p.pick_box = f.clamp(5.0, 30.0);
                    }
                }
                "lw_show" => p.show_lineweights = on,
                "lw_scale" => {
                    if let Ok(f) = v.trim().parse::<f64>() {
                        p.lineweight_scale = f.clamp(1.0, 20.0);
                    }
                }
                "grid_dots" => p.grid_dots = on,
                "grid_major" => {
                    if let Ok(n) = v.trim().parse::<u32>() {
                        p.grid_major_every = n.clamp(2, 20);
                    }
                }
                "grid_minor" => {
                    if let Some(c) = parse_rgb(v) {
                        p.grid_minor_rgb = c;
                    }
                }
                "grid_majorc" => {
                    if let Some(c) = parse_rgb(v) {
                        p.grid_major_rgb = c;
                    }
                }
                "font" => p.text_font = (!v.is_empty()).then(|| v.to_string()),
                "infer_con" => p.infer_constraints = on,
                "show_con" => p.show_constraints = on,
                _ => {}
            }
        }
        p
    }
}

impl AppState {
    /// Snapshots the current settings into a [`UiPrefs`] for persisting.
    pub fn ui_prefs(&self) -> UiPrefs {
        self.prefs.clone()
    }

    /// Loads a saved [`UiPrefs`] back into this state, e.g. on startup.
    pub fn apply_prefs(&mut self, p: &UiPrefs) {
        self.prefs = p.clone();
    }
}

/// Transient per-frame/per-drag UI state that doesn't belong in the document
/// or the undo history: an in-progress grip or bbox drag, an active corner
/// action, alignment guides, and line-chain bookkeeping for auto-inference.
#[derive(Default)]
pub struct InteractionState {
    pub grip_drag: Option<GripDrag>,
    pub bbox_drag: Option<BboxDrag>,
    pub corner_action: Option<CornerAction>,
    pub active_guide: Option<((f64, f64), f64)>,
    pub active_guides: Vec<Guide>,
    pub line_snap_prev: Option<SnapPoint>,
    pub line_chain_prev: Option<EntityId>,
    pub line_chain_first: Option<EntityId>,
}

/// An in-progress drag of a single entity grip, from [`AppState::begin_grip_drag`]
/// through to [`AppState::end_grip_drag`] or [`AppState::cancel_grip_drag`].
#[derive(Clone, Debug)]
pub struct GripDrag {
    pub entity_id: EntityId,
    pub grip: Grip,
    pub start_kind: EntityKind,
    /// Set once apply_grip_drag() has been fed a cursor position meaningfully
    /// different from the grip's starting point — lets end_grip_drag() tell
    /// a real edit apart from a click on the grip immediately followed by
    /// another click with no movement in between.
    pub moved: bool,
}

/// An in-progress drag of the selection's bounding-box handle (move, corner
/// scale, or corner rotate), from [`AppState::begin_bbox_drag`] through to
/// [`AppState::end_bbox_drag`]. Keeps each entity's original geometry so
/// [`AppState::apply_bbox_drag_transform`] can re-derive the transform from
/// scratch on every cursor move instead of compounding rounding error.
#[derive(Clone, Debug)]
pub struct BboxDrag {
    pub handle: BboxHandle,
    pub bbox_start: oxidraft_geometry::BoundingBox,
    pub cursor_start: (f64, f64),
    pub originals: Vec<(EntityId, EntityKind)>,
}

/// Which part of the selection's bounding-box widget is being dragged: the
/// body (move), a corner (scale from the opposite corner), or a rotate
/// handle beyond a corner.
#[derive(Clone, Debug, PartialEq)]
pub enum BboxHandle {
    Body,
    CornerNW,
    CornerNE,
    CornerSW,
    CornerSE,
    RotateNW,
    RotateNE,
    RotateSW,
    RotateSE,
}

fn seed_default_layers(doc: &mut oxidraft_document::Document) {
    use oxidraft_document::Layer;
    for layer in [
        Layer::new("Dimensions").with_color(46, 204, 113),
        Layer::new("Centerlines")
            .with_color(232, 134, 108)
            .with_line_type("Center"),
        Layer::new("Construction")
            .with_color(169, 140, 255)
            .with_line_type("Dotted"),
        Layer::new("Hidden")
            .with_color(150, 160, 178)
            .with_line_type("Dashed"),
    ] {
        doc.layers.add(layer);
    }
}

fn add_origin_point(doc: &mut oxidraft_document::Document) -> EntityId {
    let origin_id = doc.add(EntityKind::Point(Point2d::from_i64(0, 0)));
    doc.add_constraint(oxidraft_document::SketchConstraint::fixed(origin_id));
    origin_id
}

fn arc_or_circle(doc: &Document, id: EntityId) -> bool {
    matches!(doc.get(id).and_then(|e| e.as_curve()), Some(Curve::Arc(_)))
}

pub(crate) fn lines_parallel(doc: &Document, a: EntityId, b: EntityId) -> bool {
    let line = |id| match doc.get(id).and_then(|e| e.as_curve()) {
        Some(Curve::Line(l)) => Some(l.clone()),
        _ => None,
    };
    let (Some(la), Some(lb)) = (line(a), line(b)) else {
        return false;
    };
    let (ux, uy) = (la.p1.x - la.p0.x, la.p1.y - la.p0.y);
    let (vx, vy) = (lb.p1.x - lb.p0.x, lb.p1.y - lb.p0.y);
    let n = (ux.hypot(uy) * vx.hypot(vy)).max(1e-12);
    ((ux * vy - uy * vx) / n).abs() < 1e-7
}

fn line_endpoints(kind: &EntityKind) -> Option<((f64, f64), (f64, f64))> {
    match kind {
        EntityKind::Curve(Curve::Line(l)) => Some((l.p0.to_f64(), l.p1.to_f64())),
        _ => None,
    }
}

/// Whether a segment p0→p1 is close enough to an axis (at the given
/// pixel-world size) to auto-infer Horizontal/Vertical — the single
/// predicate shared by the post-commit capture (`infer_axis_alignment`)
/// and the live cursor preview (`inference_preview`) so the glyph shown
/// before the click always matches what the click records.
fn axis_infer_kind(p0: (f64, f64), p1: (f64, f64), px: f64) -> Option<ConstraintKind> {
    let (adx, ady) = ((p1.0 - p0.0).abs(), (p1.1 - p0.1).abs());
    let len = adx.max(ady);
    if len < px * 12.0 {
        return None;
    }
    let slack = (px * 3.0).min(len * 0.05);
    if ady <= slack && ady < adx {
        Some(ConstraintKind::Horizontal)
    } else if adx <= slack && adx < ady {
        Some(ConstraintKind::Vertical)
    } else {
        None
    }
}

fn segment_endpoints(kind: &EntityKind) -> Option<[(f64, f64); 2]> {
    match kind {
        EntityKind::Curve(Curve::Line(l)) => Some([l.p0.to_f64(), l.p1.to_f64()]),
        EntityKind::Curve(Curve::Arc(a)) => {
            if (a.end_angle - a.start_angle).abs() >= std::f64::consts::TAU - 1e-9 {
                return None;
            }
            Some([a.start_point(), a.end_point()])
        }
        _ => None,
    }
}

impl AppState {
    /// Builds a fresh, empty document at the given canvas size: default
    /// layers, an origin point fixed via a constraint, default tool/settings.
    /// The entry point for both a new-document command and app startup.
    pub fn new(canvas_w: f64, canvas_h: f64) -> Self {
        let mut document = Document::new();
        seed_default_layers(&mut document);
        let origin_id = add_origin_point(&mut document);
        AppState {
            document,
            view: ViewTransform::new(canvas_w, canvas_h),
            tool: Tool::Select,
            selection: Vec::new(),
            snap: SnapSettings::default(),
            prefs: UiPrefs::default(),
            last_command: None,
            history: History::new(),
            command_log: Vec::new(),
            cursor_world: (0.0, 0.0),
            active_snap: None,
            click_count: 0,
            origin_id,
            interaction: InteractionState::default(),
            current_file_path: None,
            hatch_pattern: oxidraft_document::HatchPattern::Solid,
            saved_revision: 0,
            zoom_target: None,
            default_line_type: LineTypeRef::ByLayer,
            default_line_weight: LineWeight::ByLayer,
            clipboard: Vec::new(),
            hint_tool: None,
            pending_dim_edit: None,
            plot_window: None,
            plot_dialog_open: false,
            plot_window_mode: false,
            conflict_flash: None,
            doc_epoch: 0,
            dof_cache: None,
        }
    }

    /// Whether the selection contains anything besides the implicit,
    /// user-invisible origin point.
    pub fn has_selection(&self) -> bool {
        self.selection.iter().any(|&id| id != self.origin_id)
    }

    /// Copies the current selection (excluding the origin point) into the
    /// clipboard, returning how many entities were copied. Leaves the
    /// clipboard untouched if the selection is empty.
    pub fn clipboard_copy(&mut self) -> usize {
        let items: Vec<Entity> = self
            .selection
            .iter()
            .filter(|&&id| id != self.origin_id)
            .filter_map(|&id| self.document.get(id).cloned())
            .collect();
        let n = items.len();
        if n > 0 {
            self.clipboard = items;
        }
        n
    }

    /// Copies the selection to the clipboard, then erases it — the CUT
    /// command. Only erases if the copy actually captured something.
    pub fn clipboard_cut(&mut self) {
        if self.clipboard_copy() > 0 {
            self.erase_selection();
        }
    }

    /// Pastes the clipboard contents centered on the cursor and selects the
    /// new copies. No-op if the clipboard is empty.
    pub fn clipboard_paste(&mut self) {
        if self.clipboard.is_empty() {
            return;
        }
        let bbox = self
            .clipboard
            .iter()
            .filter_map(|e| e.bounding_box())
            .reduce(|a, b| a.union(&b));
        let (dx, dy) = match bbox {
            Some(bb) => {
                let cx = (bb.min.x + bb.max.x) * 0.5;
                let cy = (bb.min.y + bb.max.y) * 0.5;
                (self.cursor_world.0 - cx, self.cursor_world.1 - cy)
            }
            None => (0.0, 0.0),
        };
        let t = oxidraft_geometry::Transform2d::translation(dx, dy);
        self.history.snapshot(&self.document);
        let mut pasted = Vec::with_capacity(self.clipboard.len());
        for e in &self.clipboard {
            let mut copy = e.clone();
            copy.transform(&t);
            pasted.push(self.document.add_entity(copy));
        }
        self.selection = pasted;
        self.tool = Tool::Select;
    }

    fn apply_new_entity_defaults(&mut self, id: EntityId) {
        let (lt, lw) = (
            self.default_line_type.clone(),
            self.default_line_weight.clone(),
        );
        let is_dim = matches!(
            self.document.get(id).map(|e| &e.kind),
            Some(
                oxidraft_document::EntityKind::Dimension { .. }
                    | oxidraft_document::EntityKind::OrthoDim { .. }
                    | oxidraft_document::EntityKind::AngularDim { .. }
                    | oxidraft_document::EntityKind::RadialDim { .. }
            )
        );
        let dim_layer = is_dim.then(|| {
            self.document.layers.add(
                oxidraft_document::Layer::new(oxidraft_document::DIMENSION_LAYER)
                    .with_color(46, 204, 113),
            )
        });
        if let Some(e) = self.document.get_mut(id) {
            e.line_type = lt;
            e.line_weight = lw;
            if let Some(layer) = dim_layer {
                e.layer = layer;
            }
        }
    }

    /// Recomputes `cursor_world`, `active_snap`, and the active alignment
    /// guides from a new screen-space pointer position (`sx`, `sy`). Called
    /// on every mouse-move; folds in point snapping, grid snapping, polar,
    /// and axis tracking in that priority order.
    pub fn pointer_moved(&mut self, sx: f64, sy: f64) {
        let (wx, wy) = self.view.screen_to_world(sx, sy);
        let dragged_entity = self.interaction.grip_drag.as_ref().map(|d| d.entity_id);
        let allow_snap = self.tool.wants_point_snap() || dragged_entity.is_some();
        self.active_snap = if self.prefs.snap_on && allow_snap {
            let mut s = self.snap.clone();
            s.tolerance = self.view.pixel_world_size() * self.prefs.snap_px;
            let ref_pt = self.tool.reference_point().map(|p| p.to_f64());
            let doc_snap = match dragged_entity {
                Some(ex) => find_snaps_excluding(&self.document, (wx, wy), &s, ref_pt, Some(ex))
                    .into_iter()
                    .next(),
                None => best_snap(&self.document, (wx, wy), &s, ref_pt),
            };
            let self_snap = self.nearest_self_snap((wx, wy), s.tolerance);
            match (doc_snap, self_snap) {
                (Some(a), Some(b)) => {
                    let da = (a.pos.0 - wx).hypot(a.pos.1 - wy);
                    let db = (b.pos.0 - wx).hypot(b.pos.1 - wy);
                    Some(if db < da { b } else { a })
                }
                (a, b) => a.or(b),
            }
        } else {
            None
        };
        self.interaction.active_guide = None;
        self.interaction.active_guides.clear();
        if let Some(ref sp) = self.active_snap {
            self.cursor_world = sp.pos;
        } else if self.prefs.grid_snap_on && allow_snap {
            self.cursor_world = self.view.snap_to_grid(wx, wy);
        } else {
            if let Some(ref_pt) = self.tool.reference_point() {
                let (rx, ry) = ref_pt.to_f64();
                let dx = wx - rx;
                let dy = wy - ry;
                let dist = (dx * dx + dy * dy).sqrt();
                if self.prefs.polar_on && dist > 1e-4 {
                    const POLAR_CAPTURE_DEG: f64 = 1.5;
                    let angle_rad = dy.atan2(dx);
                    let angle_deg_wrapped = oxidraft_geometry::wrap_deg360(angle_rad.to_degrees());
                    let step = self.prefs.polar_step.max(1.0);
                    let nearest = (angle_deg_wrapped / step).round() * step;
                    let diff = (angle_deg_wrapped - nearest).abs();
                    let diff = diff.min(360.0 - diff);
                    if diff <= POLAR_CAPTURE_DEG {
                        let snapped_rad = nearest.to_radians();
                        self.cursor_world =
                            (rx + dist * snapped_rad.cos(), ry + dist * snapped_rad.sin());
                        self.interaction.active_guide = Some(((rx, ry), snapped_rad));
                    } else {
                        self.cursor_world = (wx, wy);
                    }
                } else {
                    self.cursor_world = (wx, wy);
                }
            } else {
                self.cursor_world = (wx, wy);
            }
        }
        if self.prefs.track_on
            && self.active_snap.is_none()
            && let Some(drag) = self.interaction.grip_drag.as_ref()
            && let Some((a, b)) = line_endpoints(&drag.start_kind)
        {
            let tol = self.view.pixel_world_size() * 10.0;
            if let Some(res) = infer_axis(a, b, (wx, wy), tol) {
                self.cursor_world = res.point;
                self.interaction.active_guides = res.guides;
            }
        }
    }

    fn nearest_self_snap(&self, cursor: (f64, f64), tol: f64) -> Option<SnapPoint> {
        if !self
            .snap
            .enabled
            .contains(&oxidraft_cad::SnapKind::Endpoint)
        {
            return None;
        }
        let mut best = MinTracker::new();
        for p in self.tool.in_progress_points() {
            let (px, py) = p.to_f64();
            let d = (px - cursor.0).hypot(py - cursor.1);
            if d <= tol {
                best.offer(d, (px, py));
            }
        }
        best.value().map(|pos| SnapPoint {
            kind: oxidraft_cad::SnapKind::Endpoint,
            pos,
            entity: self.origin_id,
        })
    }

    /// The world-space point a click would place right now: the active snap
    /// if one is live, otherwise the raw cursor position.
    pub fn resolved_point(&self) -> Point2d {
        match &self.active_snap {
            Some(sp) => Point2d::from_f64(sp.pos.0, sp.pos.1),
            None => Point2d::from_f64(self.cursor_world.0, self.cursor_world.1),
        }
    }

    /// Handles a click at screen position (`sx`, `sy`): updates the cursor,
    /// then dispatches to modify-tool handling, text placement, selection
    /// toggling, or the active drawing tool's point-placement, depending on
    /// what's active. The main entry point for mouse clicks on the canvas.
    pub fn canvas_click(&mut self, sx: f64, sy: f64) {
        self.click_count = self.click_count.wrapping_add(1);
        self.pointer_moved(sx, sy);
        let p = self.resolved_point();
        if self.handle_modify_click(&p) {
            return;
        }
        if let Tool::Text { anchor, height } = &self.tool {
            let height = *height;
            let need_anchor = anchor.is_none();
            if need_anchor {
                self.tool = Tool::Text {
                    anchor: Some(p),
                    height,
                };
            }
            return;
        }
        if matches!(self.tool, Tool::Select) {
            if let Some(hits) = crate::view::overlays::badge_hit(self, sx, sy) {
                let hits: Vec<SketchConstraint> =
                    hits.into_iter().filter(|c| !c.kind.is_valued()).collect();
                if !hits.is_empty() {
                    self.history.snapshot(&self.document);
                    self.document.constraints.retain(|c| !hits.contains(c));
                    self.note(if hits.len() == 1 {
                        "Removed constraint via its badge".into()
                    } else {
                        format!("Removed {} constraints via their badge", hits.len())
                    });
                    return;
                }
            }
            if let Some(id) = pick_at(&self.document, p.x, p.y, self.view.pixel_world_size() * 6.0)
            {
                self.toggle_selection(id);
            } else {
                self.selection.clear();
            }
            return;
        }
        if self.try_close_on_start(p) {
            self.interaction.line_snap_prev = None;
            self.interaction.line_chain_prev = None;
            self.interaction.line_chain_first = None;
            return;
        }
        let was_line = matches!(self.tool, Tool::Line { .. });
        let was_arc = matches!(self.tool, Tool::Arc { .. });
        let snap_now = self.active_snap.clone();
        // The pointer path is the one place a pick carries a snap, so it is the
        // only caller that hands the tool anything beyond a coordinate.
        // The tool has no document, so the curve behind the snap is cloned
        // here — it is what lets two tangents and a radius be solved.
        let snapped_curve = snap_now
            .as_ref()
            .and_then(|s| self.document.get(s.entity))
            .and_then(|e| e.as_curve().cloned());
        let ev = self.tool.on_pick(crate::tools::Pick {
            pos: p,
            snap: snap_now.clone(),
            curve: snapped_curve,
        });
        let created = matches!(
            ev,
            ToolEvent::Create(_) | ToolEvent::CreateConstrained { .. }
        );
        self.apply_tool_event(ev);
        if was_line {
            self.after_line_point(created, snap_now.as_ref(), snap_now.is_some());
            self.interaction.line_snap_prev = snap_now;
        } else if was_arc && created {
            self.after_arc_create();
        }
    }

    /// Feeds a typed radius to the active tool. Does nothing for tools that
    /// have no use for one.
    pub fn place_tool_radius(&mut self, r: f64) {
        let ev = self.tool.supply_radius(r);
        self.apply_tool_event(ev);
    }

    /// Feeds an already-resolved world point directly to the active tool,
    /// bypassing screen-to-world conversion and snapping — used for
    /// programmatic point entry (typed coordinates, command-line input)
    /// rather than a raw mouse click. See [`AppState::canvas_click`] for the
    /// pointer-driven equivalent.
    pub fn place_tool_point(&mut self, p: Point2d) {
        if self.try_close_on_start(p) {
            self.interaction.line_snap_prev = None;
            self.interaction.line_chain_prev = None;
            self.interaction.line_chain_first = None;
            return;
        }
        let was_line = matches!(self.tool, Tool::Line { .. });
        let was_arc = matches!(self.tool, Tool::Arc { .. });
        // This path documents itself as bypassing snapping, and already
        // tells `after_line_point` there was none.
        let ev = self.tool.on_pick(crate::tools::Pick::bare(p));
        let created = matches!(
            ev,
            ToolEvent::Create(_) | ToolEvent::CreateConstrained { .. }
        );
        self.apply_tool_event(ev);
        if was_line {
            self.after_line_point(created, None, true);
            self.interaction.line_snap_prev = None;
        } else if was_arc && created {
            self.after_arc_create();
        }
    }

    fn after_line_point(&mut self, created: bool, end_snap: Option<&SnapPoint>, end_pinned: bool) {
        if !created {
            self.interaction.line_chain_prev = None;
            self.interaction.line_chain_first = None;
            return;
        }
        let start_snap = self.interaction.line_snap_prev.take();
        let chain_prev = self.interaction.line_chain_prev;
        let Some(&new_id) = self.document.order.last() else {
            return;
        };
        self.infer_line_coincidence(new_id, start_snap.as_ref(), end_snap);
        match chain_prev {
            Some(prev_seg) => self.weld_chain_segments(prev_seg, new_id),
            None => self.interaction.line_chain_first = Some(new_id),
        }
        let closed = self.weld_chain_closure(new_id);
        let tangent = self.infer_arc_tangency(
            new_id,
            start_snap.as_ref(),
            end_snap,
            start_snap.is_some() || chain_prev.is_some(),
            end_pinned || closed,
        );
        self.infer_axis_alignment(new_id, end_pinned || closed || tangent);
        self.interaction.line_chain_prev = Some(new_id);
    }

    fn weld_chain_segments(&mut self, prev: EntityId, new_id: EntityId) {
        if !self.prefs.infer_constraints {
            return;
        }
        let (Some(lp), Some(ln)) = (
            self.document
                .get(prev)
                .and_then(|e| line_endpoints(&e.kind)),
            self.document
                .get(new_id)
                .and_then(|e| line_endpoints(&e.kind)),
        ) else {
            return;
        };
        let ((_, p1), (n0, _)) = (lp, ln);
        if (p1.0 - n0.0).hypot(p1.1 - n0.1) > 1e-9 {
            return;
        }
        self.document
            .add_constraint(oxidraft_document::SketchConstraint::coincident(
                prev, 1, new_id, 0,
            ));
    }

    fn weld_chain_closure(&mut self, new_id: EntityId) -> bool {
        if !self.prefs.infer_constraints {
            return false;
        }
        let Some(first) = self.interaction.line_chain_first else {
            return false;
        };
        if first == new_id {
            return false;
        }
        let (Some(lf), Some(ln)) = (
            self.document
                .get(first)
                .and_then(|e| line_endpoints(&e.kind)),
            self.document
                .get(new_id)
                .and_then(|e| line_endpoints(&e.kind)),
        ) else {
            return false;
        };
        let ((f0, _), (_, n1)) = (lf, ln);
        if (f0.0 - n1.0).hypot(f0.1 - n1.1) > 1e-9 {
            return false;
        }
        if self
            .document
            .add_constraint(oxidraft_document::SketchConstraint::coincident(
                first, 0, new_id, 1,
            ))
        {
            self.note("Chain closed: corner welded coincident".into());
        }
        true
    }

    fn weld_created_loop(&mut self, ids: &[EntityId]) {
        self.weld_adjacent_segments(ids);
        if !self.prefs.infer_constraints {
            return;
        }
        match self.tool {
            Tool::Rectangle { .. } => {
                for &id in ids {
                    let Some([p0, p1]) = self
                        .document
                        .get(id)
                        .and_then(|e| segment_endpoints(&e.kind))
                    else {
                        continue;
                    };
                    let kind = if (p0.1 - p1.1).abs() < 1e-9 {
                        ConstraintKind::Horizontal
                    } else if (p0.0 - p1.0).abs() < 1e-9 {
                        ConstraintKind::Vertical
                    } else {
                        continue;
                    };
                    self.document
                        .add_constraint(SketchConstraint::single(kind, id));
                }
            }
            Tool::Polygon { .. } => {
                for &id in &ids[1..] {
                    self.document.add_constraint(SketchConstraint::pair(
                        ConstraintKind::EqualLength,
                        ids[0],
                        id,
                    ));
                }
            }
            _ => {}
        }
    }

    pub(crate) fn record_corner_constraints(
        &mut self,
        sources: [EntityId; 2],
        new_id: EntityId,
        tangent: bool,
    ) {
        if !self.prefs.infer_constraints {
            return;
        }
        let Some(new_ends) = self
            .document
            .get(new_id)
            .and_then(|e| segment_endpoints(&e.kind))
        else {
            return;
        };
        let new_is_arc = matches!(
            self.document.get(new_id).map(|e| &e.kind),
            Some(EntityKind::Curve(Curve::Arc(_)))
        );
        for src in sources {
            let Some(e) = self.document.get(src) else {
                continue;
            };
            let src_constrainable = matches!(
                &e.kind,
                EntityKind::Curve(Curve::Line(_)) | EntityKind::Curve(Curve::Arc(_))
            );
            if let Some(src_ends) = segment_endpoints(&e.kind) {
                for (si, sp) in src_ends.iter().enumerate() {
                    for (ni, np) in new_ends.iter().enumerate() {
                        if (sp.0 - np.0).hypot(sp.1 - np.1) <= 1e-6 {
                            self.document.add_constraint(
                                oxidraft_document::SketchConstraint::coincident(
                                    src, si as u8, new_id, ni as u8,
                                ),
                            );
                        }
                    }
                }
            }
            if tangent && src_constrainable && new_is_arc {
                self.document
                    .add_constraint(oxidraft_document::SketchConstraint::pair(
                        oxidraft_document::ConstraintKind::Tangent,
                        src,
                        new_id,
                    ));
            }
        }
    }

    fn infer_arc_tangency(
        &mut self,
        new_id: EntityId,
        start_snap: Option<&SnapPoint>,
        end_snap: Option<&SnapPoint>,
        start_pinned: bool,
        end_pinned: bool,
    ) -> bool {
        if !self.prefs.infer_constraints {
            return false;
        }
        let mut recorded = false;
        let cases = [(0u8, start_snap, end_pinned), (1u8, end_snap, start_pinned)];
        for (attached_end, sp, far_pinned) in cases {
            let Some(sp) = sp else { continue };
            if sp.kind != oxidraft_cad::SnapKind::Endpoint || sp.entity == new_id {
                continue;
            }
            let Some(EntityKind::Curve(Curve::Arc(arc))) =
                self.document.get(sp.entity).map(|e| &e.kind)
            else {
                continue;
            };
            if (arc.end_angle - arc.start_angle).abs() >= std::f64::consts::TAU - 1e-9 {
                continue;
            }
            let (s, e) = (arc.start_point(), arc.end_point());
            let ds = (s.0 - sp.pos.0).hypot(s.1 - sp.pos.1);
            let de = (e.0 - sp.pos.0).hypot(e.1 - sp.pos.1);
            if ds.min(de) > 1e-6 {
                continue;
            }
            let theta = if ds <= de {
                arc.start_angle
            } else {
                arc.end_angle
            };
            let arc_id = sp.entity;
            let Some((p0, p1)) = self
                .document
                .get(new_id)
                .and_then(|e| line_endpoints(&e.kind))
            else {
                return recorded;
            };
            let (att, far) = if attached_end == 0 {
                (p0, p1)
            } else {
                (p1, p0)
            };
            let anchor = if ds <= de { s } else { e };
            if (att.0 - anchor.0).hypot(att.1 - anchor.1) > 1e-6 {
                continue;
            }
            let t = (-theta.sin(), theta.cos());
            let v = (far.0 - att.0, far.1 - att.1);
            let len = v.0.hypot(v.1);
            let px = self.view.pixel_world_size();
            if len < px * 12.0 {
                continue;
            }
            let dev = (t.0 * v.1 - t.1 * v.0).abs();
            if dev > (px * 3.0).min(len * 0.05) {
                continue;
            }
            if dev > 1e-9 {
                if far_pinned {
                    continue;
                }
                let dir = if t.0 * v.0 + t.1 * v.1 >= 0.0 {
                    1.0
                } else {
                    -1.0
                };
                let new_far = (att.0 + t.0 * len * dir, att.1 + t.1 * len * dir);
                let (q0, q1) = if attached_end == 0 {
                    (att, new_far)
                } else {
                    (new_far, att)
                };
                if let Some(e) = self.document.get_mut(new_id) {
                    e.kind = EntityKind::Curve(Curve::Line(LineSeg::from_endpoints(
                        Point2d::from_f64(q0.0, q0.1),
                        Point2d::from_f64(q1.0, q1.1),
                    )));
                }
                if attached_end == 0
                    && let Tool::Line {
                        first: Some(crate::tools::TanAnchor::Point(lp)),
                    } = &mut self.tool
                {
                    *lp = Point2d::from_f64(new_far.0, new_far.1);
                }
            }
            if self
                .document
                .add_constraint(oxidraft_document::SketchConstraint::pair(
                    oxidraft_document::ConstraintKind::Tangent,
                    arc_id,
                    new_id,
                ))
            {
                self.note("Inferred tangent constraint from arc endpoint".into());
            }
            recorded = true;
        }
        recorded
    }

    fn after_arc_create(&mut self) {
        let Some(&new_id) = self.document.order.last() else {
            return;
        };
        if matches!(
            self.document.get(new_id).map(|e| &e.kind),
            Some(EntityKind::Curve(Curve::Arc(_)))
        ) {
            self.infer_arc_onset_tangency(new_id);
        }
    }

    fn infer_arc_onset_tangency(&mut self, arc_id: EntityId) {
        if !self.prefs.infer_constraints {
            return;
        }
        let Some(EntityKind::Curve(Curve::Arc(arc))) = self.document.get(arc_id).map(|e| &e.kind)
        else {
            return;
        };
        if (arc.end_angle - arc.start_angle).abs() >= std::f64::consts::TAU - 1e-9 {
            return;
        }
        let px = self.view.pixel_world_size();
        let ends = [
            (0u8, arc.start_point(), arc.start_angle),
            (1u8, arc.end_point(), arc.end_angle),
        ];
        let mut hit: Option<(EntityId, u8, u8)> = None;
        'search: for (arc_end, apos, theta) in ends {
            let t = (-theta.sin(), theta.cos());
            for &id in &self.document.order {
                if id == arc_id {
                    continue;
                }
                let Some((l0, l1)) = self.document.get(id).and_then(|e| line_endpoints(&e.kind))
                else {
                    continue;
                };
                for (line_end, near, far) in [(0u8, l0, l1), (1u8, l1, l0)] {
                    if (near.0 - apos.0).hypot(near.1 - apos.1) > 1e-6 {
                        continue;
                    }
                    let v = (far.0 - apos.0, far.1 - apos.1);
                    let len = v.0.hypot(v.1);
                    if len < px * 12.0 {
                        continue;
                    }
                    let dev = (t.0 * v.1 - t.1 * v.0).abs();
                    if dev > (px * 3.0).min(len * 0.05) {
                        continue;
                    }
                    hit = Some((id, line_end, arc_end));
                    break 'search;
                }
            }
        }
        let Some((line_id, line_end, arc_end)) = hit else {
            return;
        };
        let prev = self.document.constraints.clone();
        self.document
            .add_constraint(oxidraft_document::SketchConstraint::coincident(
                arc_id, arc_end, line_id, line_end,
            ));
        let recorded = self
            .document
            .add_constraint(oxidraft_document::SketchConstraint::pair(
                oxidraft_document::ConstraintKind::Tangent,
                arc_id,
                line_id,
            ));
        if oxidraft_cad::resolve_after_transform(&mut self.document, &[line_id]) {
            if recorded {
                self.note("Inferred tangent constraint from arc onset".into());
            }
        } else {
            self.document.constraints = prev;
        }
    }

    fn infer_axis_alignment(&mut self, new_id: EntityId, end_pinned: bool) {
        if !self.prefs.infer_constraints {
            return;
        }
        let Some((p0, p1)) = self
            .document
            .get(new_id)
            .and_then(|e| line_endpoints(&e.kind))
        else {
            return;
        };
        let px = self.view.pixel_world_size();
        let Some(kind) = axis_infer_kind(p0, p1, px) else {
            return;
        };
        let off_axis = match kind {
            oxidraft_document::ConstraintKind::Horizontal => (p1.1 - p0.1).abs(),
            _ => (p1.0 - p0.0).abs(),
        };
        if off_axis > 1e-9 {
            if end_pinned {
                return;
            }
            let new_p1 = match kind {
                oxidraft_document::ConstraintKind::Horizontal => (p1.0, p0.1),
                _ => (p0.0, p1.1),
            };
            if let Some(e) = self.document.get_mut(new_id) {
                e.kind = EntityKind::Curve(Curve::Line(LineSeg::from_endpoints(
                    Point2d::from_f64(p0.0, p0.1),
                    Point2d::from_f64(new_p1.0, new_p1.1),
                )));
            }
            if let Tool::Line {
                first: Some(crate::tools::TanAnchor::Point(lp)),
            } = &mut self.tool
            {
                *lp = Point2d::from_f64(new_p1.0, new_p1.1);
            }
        }
        if self
            .document
            .add_constraint(oxidraft_document::SketchConstraint::single(kind, new_id))
        {
            self.note(format!(
                "Inferred {} constraint on drawn line",
                kind.label()
            ));
        }
    }

    fn infer_line_coincidence(
        &mut self,
        new_id: EntityId,
        p0_snap: Option<&SnapPoint>,
        p1_snap: Option<&SnapPoint>,
    ) {
        if !self.prefs.infer_constraints {
            return;
        }
        let mut inferred = 0;
        for (my_end, sp) in [(0u8, p0_snap), (1u8, p1_snap)] {
            let Some(sp) = sp else { continue };
            if sp.kind != oxidraft_cad::SnapKind::Endpoint
                || sp.entity == new_id
                || sp.entity == self.origin_id
            {
                continue;
            }
            let Some(target) = self.document.get(sp.entity) else {
                continue;
            };
            let Some([t0, t1]) = segment_endpoints(&target.kind) else {
                continue;
            };
            let d0 = (t0.0 - sp.pos.0).hypot(t0.1 - sp.pos.1);
            let d1 = (t1.0 - sp.pos.0).hypot(t1.1 - sp.pos.1);
            if d0.min(d1) > 1e-6 {
                continue;
            }
            let their_end = if d0 <= d1 { 0u8 } else { 1u8 };
            if self
                .document
                .add_constraint(oxidraft_document::SketchConstraint::coincident(
                    sp.entity, their_end, new_id, my_end,
                ))
            {
                inferred += 1;
            }
        }
        if inferred > 0 {
            self.note(format!(
                "Inferred {inferred} coincident constraint(s) from endpoint snap"
            ));
        }
    }

    /// Commits the in-progress `Polygon` tool once both its center and
    /// radius point are set — used by an explicit confirm action (e.g. Enter
    /// key) rather than another click. No-op if the polygon isn't ready.
    pub fn confirm_pending_polygon(&mut self) {
        if !matches!(
            self.tool,
            Tool::Polygon {
                center: Some(_),
                radius_point: Some(_),
                ..
            }
        ) {
            return;
        }
        let ev = self.tool.commit();
        self.apply_tool_event(ev);
    }

    /// Clears the center/radius point of an in-progress `Polygon` tool
    /// (keeping its side count), letting the user restart placement without
    /// leaving the tool entirely.
    pub fn cancel_pending_polygon(&mut self) {
        if let Tool::Polygon { sides, .. } = self.tool.clone() {
            self.tool = Tool::Polygon {
                center: None,
                radius_point: None,
                sides,
            };
        }
    }

    fn try_close_on_start(&mut self, p: Point2d) -> bool {
        let close = match &self.tool {
            Tool::Polyline { pts } | Tool::Spline { pts } => {
                pts.len() >= 3
                    && pts[0].dist_f64(&p) <= self.view.pixel_world_size() * self.prefs.snap_px
            }
            _ => false,
        };
        if close {
            let ev = self.tool.close_and_commit();
            self.apply_tool_event(ev);
        }
        close
    }

    fn reject_nonfinite_transform(&mut self, t: &Transform2d) -> bool {
        if t.is_finite() {
            return false;
        }
        self.problem("Transform undefined for those picks — nothing changed".into());
        self.tool = Tool::Select;
        true
    }

    fn apply_tool_event(&mut self, ev: ToolEvent) {
        match ev {
            ToolEvent::Pending => {}
            ToolEvent::PlotWindow(a, b) => {
                let win = oxidraft_io::PlotWindow {
                    x0: a.x,
                    y0: a.y,
                    x1: b.x,
                    y1: b.y,
                };
                match win.normalized() {
                    Some(corners) => {
                        self.plot_window = Some(corners);
                        self.note("Plot window set".into());
                    }
                    None => self
                        .problem("The plot window has no area — pick two different corners".into()),
                }
                self.plot_dialog_open = true;
                self.plot_window_mode = true;
            }
            ToolEvent::Create(kinds) => {
                self.history.snapshot(&self.document);
                let mut created = Vec::with_capacity(kinds.len());
                for k in kinds {
                    let id = self.document.add(k);
                    self.apply_new_entity_defaults(id);
                    created.push(id);
                }
                if created.len() >= 2
                    && matches!(
                        self.tool,
                        Tool::Rectangle { .. } | Tool::Polygon { .. } | Tool::Polyline { .. }
                    )
                {
                    self.weld_created_loop(&created);
                }
            }
            ToolEvent::CreateConstrained {
                entities,
                relations,
            } => {
                self.history.snapshot(&self.document);
                let mut created = Vec::with_capacity(entities.len());
                for k in entities {
                    let id = self.document.add(k);
                    self.apply_new_entity_defaults(id);
                    created.push(id);
                }
                // The geometry is already correct — it was solved from the
                // picks. Recording the relation is what makes it *stay*
                // correct when the other entity moves later, which is the
                // whole reason to know why a circle sits where it does.
                // Behind the same switch as every other inferred constraint.
                if self.prefs.infer_constraints
                    && let Some(&new_id) = created.first()
                {
                    for r in relations {
                        // Through `constrain_lines` rather than pushing a
                        // SketchConstraint directly, so these land identically
                        // to the same relation applied from the constraint bar
                        // — same validation, same rejections.
                        if let Err(e) = oxidraft_cad::constrain_lines(
                            &mut self.document,
                            &[new_id, r.other],
                            r.kind,
                        ) {
                            self.note(format!("{:?} not recorded: {}", r.kind, e.message));
                        }
                    }
                }
            }
            ToolEvent::Transform { ids, t } => {
                if self.reject_nonfinite_transform(&t) {
                    return;
                }
                self.history.snapshot(&self.document);
                let mut moved = Vec::new();
                for id in ids {
                    if id != self.origin_id
                        && let Some(e) = self.document.get_mut(id)
                    {
                        e.transform(&t);
                        moved.push(id);
                    }
                }
                if !oxidraft_cad::resolve_after_transform_rigid(&mut self.document, &moved, &t) {
                    self.problem(
                        "Constraints not satisfiable after transform (UNCONSTRAIN to drop)".into(),
                    );
                }
                self.selection = moved;
                self.tool = Tool::Select;
            }
            ToolEvent::CopyOf { ids, t } => {
                if self.reject_nonfinite_transform(&t) {
                    return;
                }
                self.history.snapshot(&self.document);
                let mut new_ids = Vec::new();
                for id in ids {
                    if id != self.origin_id
                        && let Some(e) = self.document.get(id)
                    {
                        let copy = e.transformed(&t);
                        new_ids.push(self.document.add_entity(copy));
                    }
                }
                self.selection = new_ids;
                self.tool = Tool::Select;
            }
        }
    }

    fn toggle_selection(&mut self, id: EntityId) {
        if id == self.origin_id {
            return;
        }
        if let Some(pos) = self.selection.iter().position(|&s| s == id) {
            self.selection.remove(pos);
        } else {
            self.selection.push(id);
        }
    }

    /// Interprets a line of text typed on the command line. Tries, in
    /// order: text-tool content entry, polyline/spline close, a polygon
    /// side count, a numeric value for the active parametric tool (offset
    /// distance, fillet radius, etc.), a distance-along-reference-direction,
    /// a coordinate expression, and finally a named [`Command`] via
    /// [`parse_command`]. The single entry point for the command bar.
    pub fn run_command(&mut self, text: &str) {
        let trimmed = text.trim();
        if let Tool::Text {
            anchor: Some(p),
            height,
        } = self.tool.clone()
        {
            if !trimmed.is_empty() {
                self.history.snapshot(&self.document);
                self.document.add(EntityKind::Text {
                    anchor: p,
                    content: trimmed.replace("\\n", "\n"),
                    height,
                    rotation: 0.0,
                    font: self.prefs.text_font.clone(),
                });
            }
            self.tool = Tool::Select;
            self.note(trimmed.to_string());
            return;
        }
        if matches!(self.tool, Tool::Polyline { .. } | Tool::Spline { .. }) {
            if trimmed.is_empty() {
                let ev = self.tool.commit();
                self.apply_tool_event(ev);
                self.tool = Tool::Select;
                return;
            }
            let upper = trimmed.to_ascii_uppercase();
            if upper == "C" || upper == "CLOSE" {
                let ev = self.tool.close_and_commit();
                self.apply_tool_event(ev);
                self.tool = Tool::Select;
                self.note(trimmed.to_string());
                return;
            }
        }
        if let Tool::Polygon { center: None, .. } = self.tool
            && let Ok(n) = trimmed.parse::<usize>()
            && n >= 3
        {
            self.tool = Tool::Polygon {
                center: None,
                radius_point: None,
                sides: Some(n),
            };
            self.note(trimmed.to_string());
            return;
        }
        if let Ok(v) = trimmed.parse::<f64>()
            && v > 0.0
        {
            match &self.tool {
                Tool::Offset { source, .. } => {
                    self.tool = Tool::Offset {
                        dist: v,
                        source: *source,
                    };
                    self.note(trimmed.to_string());
                    return;
                }
                Tool::Fillet { first, .. } => {
                    self.tool = Tool::Fillet {
                        radius: v,
                        first: *first,
                    };
                    self.note(trimmed.to_string());
                    return;
                }
                Tool::Chamfer { first, .. } => {
                    self.tool = Tool::Chamfer {
                        dist: v,
                        first: *first,
                    };
                    self.note(trimmed.to_string());
                    return;
                }
                Tool::Blend {
                    continuity,
                    first,
                    second,
                    ..
                } => {
                    self.tool = Tool::Blend {
                        continuity: *continuity,
                        tension: v,
                        first: *first,
                        second: *second,
                    };
                    self.note(trimmed.to_string());
                    return;
                }
                Tool::CircleTtr { first, .. } => {
                    self.tool = Tool::CircleTtr {
                        radius: v,
                        first: *first,
                    };
                    self.note(trimmed.to_string());
                    return;
                }
                _ => {}
            }
        }
        if let Ok(dist) = trimmed.parse::<f64>()
            && let Some(ref_pt) = self.tool.reference_point()
        {
            let (rx, ry) = ref_pt.to_f64();
            let (cx, cy) = self.cursor_world;
            let dx = cx - rx;
            let dy = cy - ry;
            let len = (dx * dx + dy * dy).sqrt();
            let (ux, uy) = if len > 1e-9 {
                (dx / len, dy / len)
            } else if let Some((_, angle_rad)) = self.interaction.active_guide {
                (angle_rad.cos(), angle_rad.sin())
            } else {
                (1.0, 0.0)
            };
            let target_pt = Point2d::from_f64(rx + dist * ux, ry + dist * uy);
            let ev = self.tool.on_pick(crate::tools::Pick::bare(target_pt));
            self.apply_tool_event(ev);
            self.note(trimmed.to_string());
            return;
        }
        if let Some(coord) = parse_coordinate(trimmed) {
            let (rx, ry) = self
                .tool
                .reference_point()
                .map(|p| p.to_f64())
                .unwrap_or((0.0, 0.0));
            let (x, y) = match coord {
                CoordInput::Absolute(x, y) => (x, y),
                CoordInput::Relative(dx, dy) => (rx + dx, ry + dy),
                CoordInput::PolarAbsolute { dist, angle_deg } => {
                    let a = angle_deg.to_radians();
                    (dist * a.cos(), dist * a.sin())
                }
                CoordInput::PolarRelative { dist, angle_deg } => {
                    let a = angle_deg.to_radians();
                    (rx + dist * a.cos(), ry + dist * a.sin())
                }
            };
            let ev = self
                .tool
                .on_pick(crate::tools::Pick::bare(Point2d::from_f64(x, y)));
            self.apply_tool_event(ev);
            self.note(trimmed.to_string());
            return;
        }
        let cmd = parse_command(text);
        self.note(text.trim().to_string());
        if !matches!(cmd, Command::Cancel | Command::Unknown(_)) {
            self.last_command = Some(trimmed.to_string());
        }
        self.execute(cmd);
    }

    /// Re-runs whatever text was last accepted by [`AppState::run_command`]
    /// (e.g. pressing Enter on an empty command line to repeat the last
    /// command). No-op if nothing has been run yet.
    pub fn repeat_last_command(&mut self) {
        if let Some(cmd) = self.last_command.clone() {
            self.run_command(&cmd);
        }
    }

    /// Applies a parsed [`Command`] to the state — tool activation, undo/redo,
    /// selection edits, constraint commands, zoom, and layer changes. The
    /// non-text-parsing half of command dispatch; see [`AppState::run_command`]
    /// for the text front-end that produces a `Command`.
    pub fn execute(&mut self, cmd: Command) {
        match cmd {
            Command::Activate(mut tool) => {
                match &mut tool {
                    Tool::Move { ids, .. }
                    | Tool::Copy { ids, .. }
                    | Tool::Rotate { ids, .. }
                    | Tool::Scale { ids, .. }
                    | Tool::Mirror { ids, .. }
                    | Tool::Stretch { ids, .. } => *ids = self.selection.clone(),
                    _ => {}
                }
                self.tool = tool;
            }
            Command::Cancel => {
                self.tool.reset();
                if matches!(self.tool, Tool::Select) {
                    self.selection.clear();
                }
                self.tool = Tool::Select;
            }
            Command::Undo => self.undo(),
            Command::Redo => self.redo(),
            Command::Erase => self.erase_selection(),
            Command::Explode => self.explode_selection(),
            Command::Join => self.join_selection(),
            Command::Constrain(kind) => self.constrain_selection(kind),
            Command::ConstrainRadius(value) => self.constrain_radius_selection(value),
            Command::ConstrainDistance(value) => self.constrain_distance_selection(value),
            Command::ConstrainAngle(value) => self.constrain_angle_selection(value),
            Command::Divide(n) => self.divide_selection(n),
            Command::Measure(interval) => self.measure_selection(interval),
            Command::Unconstrain => self.unconstrain_selection(),
            Command::Fix => self.fix_selection(),
            Command::Hatch => {
                if self.selection.is_empty() {
                    self.tool = Tool::Hatch;
                } else {
                    self.hatch_selection();
                }
            }
            Command::SelectAll => {
                self.selection = self
                    .document
                    .iter()
                    .map(|e| e.id)
                    .filter(|&id| id != self.origin_id)
                    .collect();
            }
            Command::ZoomExtents => self.zoom_extents(),
            Command::ZoomScale(s) => {
                self.view.zoom = s.clamp(1e-9, 1e12);
            }
            Command::LayerSet(name) => {
                self.document.layers.set_current(&name);
            }
            Command::LayerNew(name) => {
                let idx = self.document.layers.add(Layer::new(name));
                self.document.layers.current = idx;
            }
            Command::Unknown(what) => {
                // The raw text was logged before dispatch, so on its own an
                // unrecognised command shows a toast of exactly what was typed
                // and then does nothing — which reads as confirmation. Say it
                // was not understood, and where to look.
                self.problem(format!("{what} isn't a command — press Ctrl+F to search"));
            }
        }
    }

    /// Steps the document back one entry in [`History`], clearing the
    /// selection and bumping `doc_epoch` so the DoF status recomputes.
    /// No-op at the start of history.
    pub fn undo(&mut self) {
        if let Some(prev) = self.history.undo(&self.document) {
            self.document = prev;
            self.selection.clear();
            self.doc_epoch = self.doc_epoch.wrapping_add(1);
        }
    }

    /// Steps the document forward one entry in [`History`] after an undo,
    /// clearing the selection and bumping `doc_epoch`. No-op if there's
    /// nothing to redo.
    pub fn redo(&mut self) {
        if let Some(next) = self.history.redo(&self.document) {
            self.document = next;
            self.selection.clear();
            self.doc_epoch = self.doc_epoch.wrapping_add(1);
        }
    }

    /// Deletes every selected entity (excluding the origin point) and
    /// snapshots history first. No-op if the selection is empty.
    pub fn erase_selection(&mut self) {
        if self.selection.is_empty() {
            return;
        }
        self.history.snapshot(&self.document);
        for id in std::mem::take(&mut self.selection) {
            if id != self.origin_id {
                self.document.remove(id);
            }
        }
    }

    /// Breaks every selected entity into its constituent segments (e.g. a
    /// polyline into individual lines), re-welding adjacent pieces with
    /// coincident constraints when inference is on, and selects the results.
    /// Discards the history snapshot and restores the prior selection if
    /// nothing was actually exploded.
    pub fn explode_selection(&mut self) {
        if self.selection.is_empty() {
            return;
        }
        self.history.snapshot(&self.document);
        let ids: Vec<_> = std::mem::take(&mut self.selection)
            .into_iter()
            .filter(|&id| id != self.origin_id)
            .collect();
        let mut new_ids = Vec::new();
        for &id in &ids {
            let group = edit::explode(&mut self.document, &[id]);
            if self.prefs.infer_constraints && group.len() >= 2 {
                self.weld_adjacent_segments(&group);
            }
            new_ids.extend(group);
        }
        if new_ids.is_empty() {
            self.selection = ids;
            self.history.discard_last();
            self.problem(
                "Nothing in that selection has parts to break apart — Disjoint \
                 only affects polylines, polygons, and rectangles."
                    .into(),
            );
            return;
        }
        let survived: Vec<_> = ids
            .into_iter()
            .filter(|&id| self.document.get(id).is_some())
            .collect();
        self.selection = survived.into_iter().chain(new_ids).collect();
    }

    fn weld_adjacent_segments(&mut self, ids: &[EntityId]) {
        let ends: Vec<Option<[(f64, f64); 2]>> = ids
            .iter()
            .map(|&id| {
                self.document
                    .get(id)
                    .and_then(|e| segment_endpoints(&e.kind))
            })
            .collect();
        let touch = |a: (f64, f64), b: (f64, f64)| (a.0 - b.0).hypot(a.1 - b.1) < 1e-9;
        for i in 0..ids.len() - 1 {
            if let (Some(pa), Some(pb)) = (ends[i], ends[i + 1])
                && touch(pa[1], pb[0])
            {
                self.document
                    .add_constraint(oxidraft_document::SketchConstraint::coincident(
                        ids[i],
                        1,
                        ids[i + 1],
                        0,
                    ));
            }
        }
        if ids.len() >= 3
            && let (Some(first), Some(last)) = (ends[0], ends[ids.len() - 1])
            && touch(first[0], last[1])
        {
            self.document
                .add_constraint(oxidraft_document::SketchConstraint::coincident(
                    ids[0],
                    0,
                    ids[ids.len() - 1],
                    1,
                ));
        }
    }

    /// Creates a hatch fill for each closed boundary loop found among the
    /// selected entities, on the current layer's color. Logs a hint and
    /// leaves the document unchanged if no closed boundary is selected.
    pub fn hatch_selection(&mut self) {
        if self.selection.is_empty() {
            return;
        }
        let fill = self.document.layers.current_layer().color;
        let loops: Vec<Vec<Curve>> = self
            .selection
            .iter()
            .filter(|&&id| id != self.origin_id)
            .filter_map(|&id| self.document.get(id).and_then(oxidraft_cad::boundary_loop))
            .collect();
        if loops.is_empty() {
            self.note(
                "HATCH: select a closed boundary, or run HATCH and click inside an area".into(),
            );
            return;
        }
        self.history.snapshot(&self.document);
        self.selection = loops
            .into_iter()
            .map(|b| {
                self.document.add(EntityKind::Hatch {
                    boundary: b,
                    holes: Vec::new(),
                    fill,
                    pattern: self.hatch_pattern,
                })
            })
            .collect();
    }

    /// Traces the enclosed region containing world point (`x`, `y`) and
    /// fills it as a hatch — the click-inside-an-area variant of the HATCH
    /// command, as opposed to [`AppState::hatch_selection`]'s pick-boundary
    /// variant. Returns whether a hatch was created; logs a message on
    /// failure (no enclosed area, or the boundary is too complex to trace).
    pub fn hatch_at_point(&mut self, x: f64, y: f64) -> bool {
        let (boundary, holes) = match oxidraft_cad::trace_pick_region(&self.document, x, y) {
            Ok(r) => r,
            Err(oxidraft_cad::PickRegionError::TooComplex) => {
                self.problem("HATCH: boundary too complex to trace (over 4000 segments)".into());
                return false;
            }
            Err(oxidraft_cad::PickRegionError::NotFound) => {
                self.problem("HATCH: no enclosed area found at that point".into());
                return false;
            }
        };
        let fill = self.document.layers.current_layer().color;
        self.history.snapshot(&self.document);
        let id = self.document.add(EntityKind::Hatch {
            boundary,
            holes,
            fill,
            pattern: self.hatch_pattern,
        });
        self.selection = vec![id];
        true
    }

    /// Applies a constraint result: on success snapshots history, swaps in
    /// the new document, logs the message, and bumps the DoF epoch; on
    /// failure logs the message and flashes the conflicting entities. The
    /// one place the repeated snapshot/replace/log pattern lives.
    fn commit_constraint(
        &mut self,
        doc: Document,
        res: Result<String, oxidraft_cad::ConstrainError>,
    ) -> bool {
        match res {
            Ok(msg) => {
                self.history.snapshot(&self.document);
                self.document = doc;
                self.note(msg);
                self.doc_epoch = self.doc_epoch.wrapping_add(1);
                true
            }
            Err(e) => {
                self.note(e.message);
                if !e.culprits.is_empty() {
                    self.conflict_flash = Some((e.culprits, std::time::Instant::now()));
                }
                false
            }
        }
    }

    /// DoF/redundancy of the constraint component the current selection sits
    /// in — or of the largest component in the drawing when nothing is
    /// selected — recomputed only when `doc_epoch` changes (an analyze()
    /// solves the component, too costly to run every frame). `None` when
    /// there are no constraints to report on.
    pub fn dof_status(&mut self) -> Option<&oxidraft_cad::DofSummary> {
        if self.document.constraints.is_empty() {
            self.dof_cache = None;
            return None;
        }
        if self.dof_cache.as_ref().map(|(e, _)| *e) != Some(self.doc_epoch) {
            // Seeds: the selection if it touches any constraint, else every
            // constrained entity (dof_report grows to the whole component).
            let seeds: Vec<EntityId> = if self
                .selection
                .iter()
                .any(|&id| self.document.constraints_on(id).next().is_some())
            {
                self.selection.clone()
            } else {
                let mut ids = Vec::new();
                for c in &self.document.constraints {
                    for id in [Some(c.a), c.b, c.c].into_iter().flatten() {
                        if !ids.contains(&id) {
                            ids.push(id);
                        }
                    }
                }
                ids
            };
            let summary = oxidraft_cad::dof_report(&self.document, &seeds);
            self.dof_cache = Some((self.doc_epoch, summary));
        }
        self.dof_cache.as_ref().map(|(_, s)| s)
    }

    /// The constraint the auto-inference would capture if the user clicked
    /// right now while drawing — for the live cursor glyph. `None` when
    /// inference is off, no line is in progress, or nothing would be
    /// inferred. Pure and O(1): it reads only the tool's last point, the
    /// live cursor, and the active snap, all already computed each frame.
    pub fn inference_preview(&self) -> Option<ConstraintKind> {
        if !self.prefs.infer_constraints {
            return None;
        }
        // Only the Line and Polyline tools infer as they go, and only once
        // a first point has been placed.
        let last = match &self.tool {
            Tool::Line {
                first: Some(crate::tools::TanAnchor::Point(p)),
            } => *p,
            Tool::Polyline { pts } => *pts.last()?,
            _ => return None,
        };
        // Snapping the incoming end onto an existing entity's point captures
        // a coincident weld — that takes precedence over an axis guess.
        if let Some(sp) = &self.active_snap
            && matches!(
                sp.kind,
                oxidraft_cad::SnapKind::Endpoint
                    | oxidraft_cad::SnapKind::Midpoint
                    | oxidraft_cad::SnapKind::Center
                    | oxidraft_cad::SnapKind::Node
                    | oxidraft_cad::SnapKind::Intersection
            )
        {
            return Some(ConstraintKind::Coincident);
        }
        axis_infer_kind(
            last.to_f64(),
            self.cursor_world,
            self.view.pixel_world_size(),
        )
    }

    /// Applies constraint `kind` to the current selection. Coincident
    /// without exactly two lines selected switches to the interactive `Weld`
    /// tool instead; pick-based kinds (midpoint, point-on-line, etc.)
    /// activate `ConPick` with a prompt rather than acting immediately.
    /// Otherwise solves and commits via `commit_constraint`.
    pub fn constrain_selection(&mut self, kind: oxidraft_cad::ConstraintKind) {
        use oxidraft_cad::ConstraintKind as K;
        if kind == K::Coincident {
            let lines = self
                .selection
                .iter()
                .filter(|&&id| {
                    matches!(
                        self.document.get(id).and_then(|e| e.as_curve()),
                        Some(oxidraft_geometry::Curve::Line(_))
                    )
                })
                .count();
            if lines != 2 {
                self.tool = Tool::Weld { first: None };
                self.note("WELD: pick two points to make them coincident".into());
                return;
            }
        }
        // The point-anchored relations are pick-based: activate the pick
        // tool with a prompt rather than acting on the current selection.
        if !crate::tools::con_pick_plan(kind).is_empty() {
            self.tool = Tool::ConPick {
                kind,
                picks: Vec::new(),
            };
            self.note(match kind {
                K::Midpoint => "MIDPOINT: pick a point, then a line".into(),
                K::PointOnLine => "POINT-ON-LINE: pick a point, then a line".into(),
                K::PointOnCircle => "POINT-ON-CIRCLE: pick a point, then a circle/arc".into(),
                K::Symmetric => "SYMMETRIC: pick two points, then the mirror line".into(),
                _ => format!("{}: pick its points", kind.label()),
            });
            return;
        }
        let mut doc = self.document.clone();
        let res = oxidraft_cad::constrain_lines(&mut doc, &self.selection, kind);
        self.commit_constraint(doc, res);
    }

    /// Constrains two entity anchor points (entity id + anchor index)
    /// coincident — the completion of the interactive `Weld` tool started by
    /// [`AppState::constrain_selection`] when a Coincident isn't a clean
    /// two-line selection.
    pub fn weld_points(&mut self, a: (EntityId, u8), b: (EntityId, u8)) {
        let mut doc = self.document.clone();
        let res = oxidraft_cad::constrain_coincident_points(&mut doc, a, b);
        self.commit_constraint(doc, res);
    }

    /// Applies a completed pick set from the `ConPick` tool. Each pick is
    /// (entity, anchor index, position); the count/roles match the kind's
    /// `con_pick_plan`.
    pub fn constrain_picked(
        &mut self,
        kind: ConstraintKind,
        picks: &[(EntityId, u8, oxidraft_geometry::Point2d)],
    ) {
        let mut doc = self.document.clone();
        let anchor = |i: usize| (picks[i].0, picks[i].1);
        let res = match kind {
            ConstraintKind::Midpoint
            | ConstraintKind::PointOnLine
            | ConstraintKind::PointOnCircle
                if picks.len() == 2 =>
            {
                oxidraft_cad::constrain_point_pair(&mut doc, kind, anchor(0), anchor(1))
            }
            ConstraintKind::Symmetric if picks.len() == 3 => {
                oxidraft_cad::constrain_symmetric_points(&mut doc, anchor(0), anchor(1), picks[2].0)
            }
            _ => return,
        };
        if self.commit_constraint(doc, res) {
            self.prefs.show_constraints = true;
        }
    }

    /// Constrains the selected arc/circle's radius, optionally to a fixed
    /// `value` (leave `None` to just record the current radius as-is).
    pub fn constrain_radius_selection(&mut self, value: Option<f64>) {
        let mut doc = self.document.clone();
        let res = oxidraft_cad::constrain_radius(&mut doc, &self.selection, value);
        self.commit_constraint(doc, res);
    }

    /// Constrains the selected entity's length/distance, optionally to a
    /// fixed `value` (leave `None` to record the current distance as-is).
    pub fn constrain_distance_selection(&mut self, value: Option<f64>) {
        let mut doc = self.document.clone();
        let res = oxidraft_cad::constrain_distance(&mut doc, &self.selection, value);
        self.commit_constraint(doc, res);
    }

    /// Constrains the angle between the two selected entities, optionally to
    /// a fixed `value` (leave `None` to record the current angle as-is).
    pub fn constrain_angle_selection(&mut self, value: Option<f64>) {
        let mut doc = self.document.clone();
        let res = oxidraft_cad::constrain_angle(&mut doc, &self.selection, value);
        self.commit_constraint(doc, res);
    }

    /// Edits an existing valued constraint (radius, distance, line-distance,
    /// or angle) in place to `value` — used when the user drags a dimension
    /// grip or types a new value into a dimension's edit box, as opposed to
    /// creating a fresh constraint. No-op for constraint kinds without a
    /// scalar value.
    pub fn set_constraint_value(&mut self, target: SketchConstraint, value: f64) {
        let mut doc = self.document.clone();
        let res = match target.kind {
            ConstraintKind::Radius => {
                oxidraft_cad::constrain_radius(&mut doc, &[target.a], Some(value))
            }
            ConstraintKind::Distance => {
                oxidraft_cad::constrain_distance(&mut doc, &[target.a], Some(value))
            }
            ConstraintKind::LineDistance => match target.b {
                Some(b) => {
                    oxidraft_cad::constrain_line_distance(&mut doc, &[target.a, b], Some(value))
                }
                None => return,
            },
            ConstraintKind::Angle => match target.b {
                Some(b) => oxidraft_cad::constrain_angle(&mut doc, &[target.a, b], Some(value)),
                None => return,
            },
            _ => return,
        };
        self.commit_constraint(doc, res);
    }

    /// Constrains the selected entities (excluding the origin point) fixed
    /// in place — they no longer move under the solver.
    pub fn fix_selection(&mut self) {
        let sel: Vec<EntityId> = self
            .selection
            .iter()
            .copied()
            .filter(|&id| id != self.origin_id)
            .collect();
        let mut doc = self.document.clone();
        let res = oxidraft_cad::constrain_fixed(&mut doc, &sel);
        self.commit_constraint(doc, res);
    }

    /// The SMARTDIM tool's commit: picks a dimension kind to apply based on
    /// what was picked — parallel-line distance, angle between two
    /// non-parallel entities, radius for an arc/circle, or length for a
    /// single entity — with `b` and `place` both optional (a single-entity
    /// pick and no explicit placement, respectively). Returns whether the
    /// constraint was created; on success stashes it in `pending_dim_edit`
    /// so the UI can immediately open its value editor.
    pub fn smart_dimension(
        &mut self,
        a: EntityId,
        b: Option<EntityId>,
        place: Option<(f64, f64)>,
    ) -> bool {
        let mut doc = self.document.clone();
        let (res, kind): (Result<String, oxidraft_cad::ConstrainError>, ConstraintKind) = match b {
            Some(b) if lines_parallel(&self.document, a, b) => (
                oxidraft_cad::constrain_line_distance(&mut doc, &[a, b], None),
                ConstraintKind::LineDistance,
            ),
            Some(b) => (
                oxidraft_cad::constrain_angle(&mut doc, &[a, b], None),
                ConstraintKind::Angle,
            ),
            None if arc_or_circle(&self.document, a) => (
                oxidraft_cad::constrain_radius(&mut doc, &[a], None),
                ConstraintKind::Radius,
            ),
            None => (
                oxidraft_cad::constrain_distance(&mut doc, &[a], None),
                ConstraintKind::Distance,
            ),
        };
        // Apply the placement to the fresh record before committing, so the
        // clone that gets swapped in already carries it.
        if res.is_ok()
            && place.is_some()
            && let Some(c) = doc
                .constraints
                .iter_mut()
                .rev()
                .find(|c| c.kind == kind && c.a == a && c.b == b && c.val.is_some())
        {
            c.place = place;
        }
        if self.commit_constraint(doc, res) {
            self.prefs.show_constraints = true;
            self.pending_dim_edit = self
                .document
                .constraints
                .iter()
                .rev()
                .find(|c| c.kind == kind && c.a == a && c.b == b && c.val.is_some())
                .copied();
            true
        } else {
            false
        }
    }

    /// Removes a single constraint (e.g. from clicking its dimension badge
    /// or an explicit delete action), snapshotting history first. No-op if
    /// the constraint isn't present.
    pub fn remove_constraint(&mut self, target: SketchConstraint) {
        if self.document.constraints.contains(&target) {
            self.history.snapshot(&self.document);
            self.document.constraints.retain(|c| c != &target);
            self.note(format!("Removed {} constraint", target.kind.label()));
            self.doc_epoch = self.doc_epoch.wrapping_add(1);
        }
    }

    /// Places point entities dividing each selected curve into `n` equal
    /// segments. Logs a hint if `n` is missing (the command needs a count).
    pub fn divide_selection(&mut self, n: Option<u32>) {
        let Some(n) = n else {
            self.problem("DIVIDE needs a segment count of 2 or more (DIVIDE 5)".into());
            return;
        };
        self.place_points_on_selection(
            "Select at least one curve to divide",
            "Nothing to divide on that selection",
            |doc, c| oxidraft_cad::commands::divide(doc, c, n).len(),
        );
    }

    /// Places point entities at fixed `interval` spacing along each selected
    /// curve. Logs a hint if `interval` is missing (the command needs a
    /// positive distance).
    pub fn measure_selection(&mut self, interval: Option<f64>) {
        let Some(interval) = interval else {
            self.problem("MEASURE needs a positive interval (MEASURE 2.5)".into());
            return;
        };
        self.place_points_on_selection(
            "Select at least one curve to measure",
            "The interval doesn't fit on that selection",
            |doc, c| oxidraft_cad::commands::measure(doc, c, interval).len(),
        );
    }

    fn place_points_on_selection(
        &mut self,
        empty_msg: &str,
        zero_msg: &str,
        per_curve: impl Fn(&mut Document, &oxidraft_geometry::Curve) -> usize,
    ) {
        let curves: Vec<oxidraft_geometry::Curve> = self
            .selection
            .iter()
            .filter_map(|&id| self.document.get(id).and_then(|e| e.as_curve()).cloned())
            .collect();
        if curves.is_empty() {
            self.note(empty_msg.into());
            return;
        }
        self.history.snapshot(&self.document);
        let placed: usize = curves
            .iter()
            .map(|c| per_curve(&mut self.document, c))
            .sum();
        if placed == 0 {
            self.history.discard_last();
            self.note(zero_msg.into());
        } else {
            self.note(format!("Placed {placed} point(s)"));
        }
    }

    /// Removes every constraint touching any selected entity. Logs a hint
    /// instead if the selection is empty or has no constraints on it.
    pub fn unconstrain_selection(&mut self) {
        if self.selection.is_empty() {
            self.note("Select entities to remove constraints from".into());
            return;
        }
        let count: usize = self
            .selection
            .iter()
            .map(|&id| self.document.constraints_on(id).count())
            .sum();
        if count == 0 {
            self.note("No constraints on the selection".into());
            return;
        }
        self.history.snapshot(&self.document);
        for id in self.selection.clone() {
            self.document.remove_constraints_on(id);
        }
        let remaining = self.document.constraints.len();
        self.note(format!(
            "Removed constraints touching the selection ({remaining} left in drawing)"
        ));
        self.doc_epoch = self.doc_epoch.wrapping_add(1);
    }

    /// Merges the selected entities (excluding the origin point) into
    /// combined curves where possible, e.g. touching line segments into a
    /// polyline, and selects the result. Restores the prior selection and
    /// discards the history snapshot if nothing could be joined.
    pub fn join_selection(&mut self) {
        if self.selection.is_empty() {
            return;
        }
        self.history.snapshot(&self.document);
        let ids: Vec<_> = std::mem::take(&mut self.selection)
            .into_iter()
            .filter(|&id| id != self.origin_id)
            .collect();
        let new_ids = edit::join(&mut self.document, &ids);
        if new_ids.is_empty() {
            self.selection = ids;
            self.history.discard_last();
            self.problem(
                "Nothing in that selection touches end to end — Join needs curves \
                 that share an endpoint."
                    .into(),
            );
            return;
        }
        let survived: Vec<_> = ids
            .into_iter()
            .filter(|&id| self.document.get(id).is_some())
            .collect();
        self.selection = survived.into_iter().chain(new_ids).collect();
    }

    /// Sets `zoom_target` to frame the document's full extents; the actual
    /// view animates toward it over subsequent frames via
    /// [`AppState::tick_zoom_anim`]. No-op on an empty document.
    pub fn zoom_extents(&mut self) {
        if let Some(bb) = self.document.extents() {
            let (x0, y0) = bb.min.to_f64();
            let (x1, y1) = bb.max.to_f64();
            let mut target = self.view.clone();
            target.zoom_to_bounds(x0, y0, x1, y1);
            self.zoom_target = Some((target.center.0, target.center.1, target.zoom));
        }
    }

    /// Advances the view one step toward `zoom_target` (an exponential
    /// ease), snapping exactly onto it and clearing the target once close
    /// enough. Call once per frame; returns whether the animation is still
    /// running so the caller knows whether to keep redrawing.
    pub fn tick_zoom_anim(&mut self) -> bool {
        let Some((tx, ty, tz)) = self.zoom_target else {
            return false;
        };
        let k = 0.25;
        self.view.center.0 += (tx - self.view.center.0) * k;
        self.view.center.1 += (ty - self.view.center.1) * k;
        self.view.zoom = (self.view.zoom.ln() + (tz.ln() - self.view.zoom.ln()) * k).exp();
        let dc = (tx - self.view.center.0).hypot(ty - self.view.center.1) * self.view.zoom;
        let dz = (tz / self.view.zoom).ln().abs();
        if dc < 0.5 && dz < 2e-3 {
            self.view.center = (tx, ty);
            self.view.zoom = tz;
            self.zoom_target = None;
            return false;
        }
        true
    }

    /// Snapshots history and adds a new entity directly, bypassing the tool
    /// state machine — for callers (e.g. dialogs) that construct an
    /// `EntityKind` themselves rather than driving a `Tool`.
    pub fn add_entity(&mut self, kind: EntityKind) -> EntityId {
        self.history.snapshot(&self.document);
        self.document.add(kind)
    }

    /// The single selected NURBS curve's id, control points, and weights —
    /// `None` unless exactly one NURBS curve is selected. Used to drive a
    /// single-curve control-point editor.
    pub fn selected_nurbs(&self) -> Option<(EntityId, Vec<Point2d>, Vec<f64>)> {
        if self.selection.len() != 1 {
            return None;
        }
        let id = self.selection[0];
        if let EntityKind::Curve(Curve::Nurbs(nc)) = &self.document.get(id)?.kind {
            Some((id, nc.control().to_vec(), nc.weights().to_vec()))
        } else {
            None
        }
    }

    /// Every selected NURBS curve's id, control points, and weights — the
    /// multi-selection counterpart to [`AppState::selected_nurbs`].
    pub fn selected_nurbs_all(&self) -> Vec<(EntityId, Vec<Point2d>, Vec<f64>)> {
        self.selection
            .iter()
            .filter_map(|&id| match &self.document.get(id)?.kind {
                EntityKind::Curve(Curve::Nurbs(nc)) => {
                    Some((id, nc.control().to_vec(), nc.weights().to_vec()))
                }
                _ => None,
            })
            .collect()
    }

    /// Snapshots history before an external caller mutates the document
    /// directly (e.g. a properties panel editing a field in place) — the
    /// generic pre-edit hook for callers that don't go through a dedicated
    /// `AppState` method.
    pub fn begin_edit(&mut self) {
        self.history.snapshot(&self.document);
    }

    /// The custom display-text override on a dimension entity, if any —
    /// `None` for non-dimension entities or ones using their computed text.
    pub fn dim_override(&self, id: EntityId) -> Option<String> {
        match &self.document.get(id)?.kind {
            EntityKind::Dimension { override_text, .. }
            | EntityKind::OrthoDim { override_text, .. }
            | EntityKind::AngularDim { override_text, .. }
            | EntityKind::RadialDim { override_text, .. } => override_text.clone(),
            _ => None,
        }
    }

    /// Sets or clears a dimension entity's display-text override. Blank
    /// text is treated as clearing it (falls back to the computed value).
    /// Does not snapshot history — pair with [`AppState::begin_edit`] if the
    /// change should be undoable.
    pub fn set_dim_override(&mut self, id: EntityId, text: Option<String>) {
        let text = text.filter(|t| !t.trim().is_empty());
        if let Some(e) = self.document.get_mut(id) {
            match &mut e.kind {
                EntityKind::Dimension { override_text, .. }
                | EntityKind::OrthoDim { override_text, .. }
                | EntityKind::AngularDim { override_text, .. }
                | EntityKind::RadialDim { override_text, .. } => *override_text = text,
                _ => {}
            }
        }
    }

    /// Applies edits from a text-entity editor (content, font, size) if they
    /// actually changed anything, snapshotting history first; a no-op edit
    /// doesn't pollute the undo stack. Also updates the sticky `text_font`
    /// default for the next new text entity.
    pub fn commit_text_edit(
        &mut self,
        id: EntityId,
        content: String,
        font: Option<String>,
        size: f64,
    ) {
        let size = size.max(0.1);
        let changed = matches!(
            self.document.get(id).map(| e | & e.kind), Some(EntityKind::Text { content :
            c, font : f, height : h, .. }) if * c != content || * f != font || (* h -
            size).abs() > 1e-9
        );
        if !changed {
            return;
        }
        self.history.snapshot(&self.document);
        if let Some(EntityKind::Text {
            content: c,
            font: f,
            height: h,
            ..
        }) = self.document.get_mut(id).map(|e| &mut e.kind)
        {
            *c = content;
            *f = font.clone();
            *h = size;
        }
        self.prefs.text_font = font;
    }

    /// Converts each selected text entity into curve outlines on the same
    /// layer/color and replaces the text with them, selecting the new
    /// curves. Text entities that produce no outline (e.g. empty content)
    /// are left untouched; discards the history snapshot if nothing changed.
    pub fn outline_text_selection(&mut self) {
        let texts: Vec<EntityId> = self
            .selection
            .iter()
            .copied()
            .filter(|&id| {
                matches!(
                    self.document.get(id).map(|e| &e.kind),
                    Some(EntityKind::Text { .. })
                )
            })
            .collect();
        if texts.is_empty() {
            return;
        }
        self.history.snapshot(&self.document);
        let mut new_ids = Vec::new();
        for id in texts {
            let info = match self.document.get(id) {
                Some(e) => match &e.kind {
                    EntityKind::Text {
                        content,
                        font,
                        height,
                        anchor,
                        rotation,
                    } => Some((
                        content.clone(),
                        font.clone(),
                        *height,
                        *anchor,
                        *rotation,
                        e.layer,
                        e.color.clone(),
                    )),
                    _ => None,
                },
                None => None,
            };
            let Some((content, font, height, anchor, rotation, layer, color)) = info else {
                continue;
            };
            let curves =
                crate::fonts::outline_text(&content, font.as_deref(), height, anchor, rotation);
            if curves.is_empty() {
                continue;
            }
            for c in curves {
                let cid = self.document.add_on_layer(EntityKind::Curve(c), layer);
                if let Some(e) = self.document.get_mut(cid) {
                    e.color = color.clone();
                }
                new_ids.push(cid);
            }
            self.document.remove(id);
        }
        if !new_ids.is_empty() {
            self.selection = new_ids;
        } else {
            self.history.discard_last();
            self.problem(
                "That text has no outlines to create — its font may have no glyphs \
                 for the characters used."
                    .into(),
            );
        }
    }

    /// Moves a single NURBS control point of entity `id` to `p`. Silently
    /// ignores an out-of-range `index` or a non-NURBS entity. Does not
    /// snapshot history — intended for live-drag updates.
    pub fn set_nurbs_control(&mut self, id: EntityId, index: usize, p: Point2d) {
        if let Some(e) = self.document.get_mut(id)
            && let EntityKind::Curve(Curve::Nurbs(nc)) = &mut e.kind
        {
            nc.set_control_point(index, p);
        }
    }

    /// Multiplies a single NURBS control point's weight by `factor`,
    /// clamped to a sane range, snapshotting history first. Returns whether
    /// the edit applied (false for an out-of-range index or non-NURBS
    /// entity, in which case nothing is snapshotted).
    pub fn adjust_nurbs_weight(&mut self, id: EntityId, index: usize, factor: f64) -> bool {
        // Work out the new weight and whether it is acceptable BEFORE taking a
        // history snapshot. `f64::clamp` propagates NaN, so a NaN factor
        // produced a NaN weight that `set_weight` then silently refused — and
        // this returned `true` regardless, claiming an edit that never
        // happened and leaving a no-op entry on the undo stack.
        let Some(EntityKind::Curve(Curve::Nurbs(nc))) = self.document.get(id).map(|e| &e.kind)
        else {
            return false;
        };
        let Some(&current) = nc.weights().get(index) else {
            return false;
        };
        let scaled = (current * factor).clamp(0.05, 20.0);
        if !scaled.is_finite() || scaled <= 0.0 {
            return false;
        }

        self.history.snapshot(&self.document);
        if let Some(EntityKind::Curve(Curve::Nurbs(nc))) =
            self.document.get_mut(id).map(|e| &mut e.kind)
        {
            nc.set_weight(index, scaled);
        }
        true
    }

    /// Resets to a blank document with fresh layers, origin, selection, and
    /// history, and clears the current file path — the NEW command.
    /// Unlike most edits, this is not itself undoable (it replaces history).
    pub fn new_document(&mut self) {
        self.document = Document::new();
        seed_default_layers(&mut self.document);
        self.origin_id = add_origin_point(&mut self.document);
        self.selection.clear();
        self.history = History::new();
        self.tool = Tool::Select;
        self.current_file_path = None;
        self.saved_revision = self.history.current_revision();
    }

    /// Loads a document from `path`, dispatching on its extension (DXF, SVG,
    /// or the native format; DWG is rejected with a message since oxiDRAFT
    /// can't read that proprietary format). On success replaces the
    /// document, resets history/selection, and remembers the path as the
    /// current file. On failure logs the error and leaves the state as-is.
    pub fn open_file(&mut self, path: std::path::PathBuf) {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let result = match ext.as_str() {
            "dxf" => std::fs::read_to_string(&path)
                .map(|t| oxidraft_io::import_dxf(&t))
                .map_err(|e| e.to_string()),
            "svg" => std::fs::read_to_string(&path)
                .map(|t| oxidraft_io::import_svg(&t))
                .map_err(|e| e.to_string()),
            "dwg" => Err("DWG is a proprietary binary format oxiDRAFT can't read. \
                          Re-export it as DXF from your CAD app, then open the .dxf."
                .to_string()),
            _ => oxidraft_io::load_native(&path).map_err(|e| e.to_string()),
        };
        match result {
            Ok(mut doc) => {
                let origin_id = add_origin_point(&mut doc);
                self.document = doc;
                self.origin_id = origin_id;
                self.selection.clear();
                self.history = History::new();
                self.tool = Tool::Select;
                self.current_file_path = Some(path);
                self.saved_revision = self.history.current_revision();
            }
            Err(e) => self.problem(format!("Cannot open: {e}")),
        }
    }

    /// Saves to the current file path, if one is set. Returns `false`
    /// without doing anything if there's no current path (the caller should
    /// fall back to a Save As dialog and call
    /// [`AppState::save_file_to`]).
    pub fn save_file(&mut self) -> bool {
        if let Some(path) = self.current_file_path.clone() {
            self.save_file_to(path)
        } else {
            false
        }
    }

    /// Saves the document to `path`, dispatching on its extension (DXF,
    /// SVG, or native; DWG is rejected since oxiDRAFT can't write that
    /// proprietary format). The implicit origin point is stripped before
    /// export. On success remembers `path` as current and discards any
    /// pending autosave-recovery file; on failure logs the error. Returns
    /// whether the save succeeded.
    pub fn save_file_to(&mut self, path: std::path::PathBuf) -> bool {
        let mut save_doc = self.document.clone();
        save_doc.remove(self.origin_id);
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let result = match ext.as_str() {
            "dxf" => {
                oxidraft_io::write_atomic(&path, oxidraft_io::export_dxf(&save_doc).as_bytes())
                    .map_err(|e| e.to_string())
            }
            "svg" => {
                oxidraft_io::write_atomic(&path, oxidraft_io::export_svg(&save_doc).as_bytes())
                    .map_err(|e| e.to_string())
            }
            "dwg" => Err("oxiDRAFT can't write DWG (proprietary binary). \
                          Save as DXF for CAD interchange."
                .to_string()),
            _ => oxidraft_io::save_native(&save_doc, &path).map_err(|e| e.to_string()),
        };
        match result {
            Ok(()) => {
                self.current_file_path = Some(path);
                self.saved_revision = self.history.current_revision();
                crate::autosave::discard_recovery();
                true
            }
            Err(e) => {
                self.problem(format!("Save failed: {e}"));
                false
            }
        }
    }

    /// Whether the document has unsaved changes, compared against the
    /// revision recorded at the last successful save/open/new.
    pub fn is_dirty(&self) -> bool {
        self.history.current_revision() != self.saved_revision
    }

    /// Caps the command log so a long session can't grow it without bound —
    /// only the newest entry is ever displayed (the toast), the rest exist
    /// for context. Trimmed with slack so the drain runs rarely, not on
    /// every push past the cap.
    /// Reports something that worked.
    ///
    /// Takes a `String` rather than `impl Into<String>` so the very common
    /// `"…".into()` at call sites still resolves — with the generic form there
    /// is nothing to infer the target type from.
    pub fn note(&mut self, msg: String) {
        self.command_log.push(Note::Info(msg));
    }

    /// Reports something refused or failed. Prefer saying what to do next.
    pub fn problem(&mut self, msg: String) {
        self.command_log.push(Note::Problem(msg));
    }

    pub fn trim_command_log(&mut self) {
        const CAP: usize = 400;
        const SLACK: usize = 200;
        if self.command_log.len() > CAP + SLACK {
            let cut = self.command_log.len() - CAP;
            self.command_log.drain(..cut);
        }
    }

    /// Loads a document from an autosave-recovery file at `path`, replacing
    /// the current document and resetting history/selection. Unlike
    /// [`AppState::open_file`], the recovered document isn't treated as
    /// matching any file on disk (`current_file_path` is cleared and
    /// `saved_revision` forced dirty), since the recovery file isn't itself
    /// the user's saved document. Returns whether the load succeeded.
    pub fn restore_recovery(&mut self, path: &std::path::Path) -> bool {
        match oxidraft_io::load_native(path) {
            Ok(mut doc) => {
                let origin_id = add_origin_point(&mut doc);
                self.document = doc;
                self.origin_id = origin_id;
                self.selection.clear();
                self.history = History::new();
                self.tool = Tool::Select;
                self.current_file_path = None;
                self.saved_revision = u64::MAX;
                true
            }
            Err(e) => {
                self.problem(format!("Recovery failed: {e}"));
                false
            }
        }
    }

    /// The app window's title bar text: `oxiDRAFT — <filename><*>`, with a
    /// trailing `*` when there are unsaved changes.
    pub fn window_title(&self) -> String {
        let name = self
            .current_file_path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Untitled".to_string());
        let star = if self.is_dirty() { "*" } else { "" };
        format!("oxiDraft — {name}{star}")
    }

    /// The document's short display name (filename plus a dirty-marker
    /// `*`, no `oxiDRAFT —` prefix) — for in-UI labels like a tab title, as
    /// opposed to [`AppState::window_title`]'s full window-bar text.
    pub fn document_label(&self) -> String {
        let name = self
            .current_file_path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Untitled".to_string());
        let star = if self.is_dirty() { "*" } else { "" };
        format!("{name}{star}")
    }

    /// The current cursor world position formatted for the status-bar
    /// coordinate readout.
    pub fn coord_readout(&self) -> String {
        format!("{:.4}, {:.4}", self.cursor_world.0, self.cursor_world.1)
    }

    /// The name of the document's currently-active layer.
    pub fn current_layer_name(&self) -> &str {
        &self.document.layers.current_layer().name
    }

    /// The short unit abbreviation for the document's unit system (e.g.
    /// `"mm"`), or `"none"` for a unitless document.
    pub fn units_label(&self) -> &'static str {
        match self.document.settings.units.short_name() {
            "" => "none",
            s => s,
        }
    }

    /// Updates the view's allowed zoom range from the document's unit
    /// system — call after changing units (e.g. loading a document with
    /// different settings) so zoom clamping matches the new scale.
    pub fn sync_zoom_limits(&mut self) {
        let (mn, mx) = self.document.settings.units.visible_range();
        self.view.set_visible_range(mn, mx);
    }

    /// Starts a bounding-box drag on the selection from the given `handle`,
    /// capturing each selected entity's starting geometry and snapshotting
    /// history. No-op if the selection is empty or has no bounding box.
    pub fn begin_bbox_drag(&mut self, handle: BboxHandle, cursor: (f64, f64)) {
        if self.selection.is_empty() {
            return;
        }
        let mut bbox: Option<oxidraft_geometry::BoundingBox> = None;
        for &id in &self.selection {
            if let Some(e) = self.document.get(id)
                && let Some(b) = e.bounding_box()
            {
                bbox = Some(if let Some(existing) = bbox {
                    existing.union(&b)
                } else {
                    b
                });
            }
        }
        if let Some(bbox_start) = bbox {
            let originals: Vec<(EntityId, EntityKind)> = self
                .selection
                .iter()
                .filter_map(|&id| self.document.get(id).map(|e| (id, e.kind.clone())))
                .collect();
            self.interaction.bbox_drag = Some(BboxDrag {
                handle,
                bbox_start,
                cursor_start: cursor,
                originals,
            });
            self.history.snapshot(&self.document);
        }
    }

    /// Finishes the active bounding-box drag, if any.
    pub fn end_bbox_drag(&mut self) {
        self.interaction.bbox_drag = None;
    }

    /// Re-applies the bbox drag's transform (move, scale from the opposite
    /// corner, or rotate about center) for a new `cursor` position, always
    /// starting fresh from the drag's captured original geometry so repeated
    /// calls don't compound error. Reverts to the pre-call geometry if the
    /// result fails to satisfy constraints. No-op if no bbox drag is active.
    pub fn apply_bbox_drag_transform(&mut self, cursor: (f64, f64)) {
        let Some(drag) = self.interaction.bbox_drag.as_ref() else {
            return;
        };
        let ids: Vec<_> = self.selection.clone();
        let prev: Vec<(EntityId, EntityKind)> = ids
            .iter()
            .filter_map(|&id| self.document.get(id).map(|e| (id, e.kind.clone())))
            .collect();
        for (id, kind) in &drag.originals {
            if let Some(e) = self.document.get_mut(*id) {
                e.kind = kind.clone();
            }
        }
        let (cx, cy) = cursor;
        let (sx, sy) = drag.cursor_start;
        let (dx, dy) = (cx - sx, cy - sy);
        let bbox = drag.bbox_start;
        let (bx0, by0) = (bbox.min.x, bbox.min.y);
        let (bx1, by1) = (bbox.max.x, bbox.max.y);
        match drag.handle {
            BboxHandle::Body => {
                edit::move_by(&mut self.document, &ids, dx, dy);
            }
            BboxHandle::CornerNW => {
                self.scale_bbox_from_opposite(&ids, bbox, cursor, (bx1, by1));
            }
            BboxHandle::CornerNE => {
                self.scale_bbox_from_opposite(&ids, bbox, cursor, (bx0, by1));
            }
            BboxHandle::CornerSW => {
                self.scale_bbox_from_opposite(&ids, bbox, cursor, (bx1, by0));
            }
            BboxHandle::CornerSE => {
                self.scale_bbox_from_opposite(&ids, bbox, cursor, (bx0, by0));
            }
            BboxHandle::RotateNW
            | BboxHandle::RotateNE
            | BboxHandle::RotateSW
            | BboxHandle::RotateSE => {
                let center = Point2d::from_f64((bx0 + bx1) / 2.0, (by0 + by1) / 2.0);
                let angle_start = (sy - center.y).atan2(sx - center.x);
                let angle_current = (cy - center.y).atan2(cx - center.x);
                let angle = angle_current - angle_start;
                if angle.abs() > 1e-9 {
                    edit::rotate(&mut self.document, &ids, &center, angle);
                }
            }
        }
        if !oxidraft_cad::resolve_after_transform(&mut self.document, &ids) {
            for (id, kind) in &prev {
                if let Some(e) = self.document.get_mut(*id) {
                    e.kind = kind.clone();
                }
            }
        }
    }

    fn scale_bbox_from_opposite(
        &mut self,
        ids: &[EntityId],
        bbox: oxidraft_geometry::BoundingBox,
        cursor: (f64, f64),
        opposite: (f64, f64),
    ) {
        let (cx, cy) = cursor;
        let (ox, oy) = opposite;
        let w = (cx - ox).abs();
        let h = (cy - oy).abs();
        let orig_w = (bbox.max.x - bbox.min.x).abs();
        let orig_h = (bbox.max.y - bbox.min.y).abs();
        if orig_w > 1e-9 && orig_h > 1e-9 {
            let sx = w / orig_w;
            let sy = h / orig_h;
            let s = sx.max(sy);
            if (s - 1.0).abs() > 1e-6 {
                let base = Point2d::from_f64(ox, oy);
                edit::scale(&mut self.document, ids, &base, s);
            }
        }
    }

    /// Starts a grip drag on entity `id`'s `grip`, capturing the entity's
    /// starting geometry and snapshotting history. No-op if the entity
    /// doesn't exist.
    pub fn begin_grip_drag(&mut self, id: EntityId, grip: Grip) {
        if let Some(e) = self.document.get(id) {
            self.history.snapshot(&self.document);
            self.interaction.grip_drag = Some(GripDrag {
                entity_id: id,
                grip,
                start_kind: e.kind.clone(),
                moved: false,
            });
        }
    }

    /// Re-applies the active grip drag's edit for a new `cursor` position:
    /// derives the edited geometry from the grip's starting kind, marks the
    /// drag as having moved once the cursor departs the grip's start point,
    /// re-solves dependent tangency and constraints, and reverts the entity
    /// and constraints if the result isn't solvable. No-op if no grip drag
    /// is active.
    pub fn apply_grip_drag(&mut self, cursor: (f64, f64)) {
        let Some(drag) = self.interaction.grip_drag.as_ref() else {
            return;
        };
        let to = Point2d::from_f64(cursor.0, cursor.1);
        let edited = apply_grip(&drag.start_kind, &drag.grip, to);
        let id = drag.entity_id;
        if drag.grip.world.dist_f64(&to) > 1e-9
            && let Some(d) = self.interaction.grip_drag.as_mut()
        {
            d.moved = true;
        }
        let prev_kind = self.document.get(id).map(|e| e.kind.clone());
        let prev_constraints = self.document.constraints.clone();
        if let Some(e) = self.document.get_mut(id) {
            e.kind = edited;
        }
        self.reconstrain_tangency(id);
        if !self.resolve_constraints_after(id) {
            if let Some(k) = prev_kind
                && let Some(e) = self.document.get_mut(id)
            {
                e.kind = k;
            }
            self.document.constraints = prev_constraints;
        }
    }

    fn resolve_constraints_after(&mut self, id: EntityId) -> bool {
        let role = self.interaction.grip_drag.as_ref().map(|d| d.grip.role);
        self.retarget_driven_dimension(id, role);
        let pinned = match role {
            Some(oxidraft_cad::GripRole::Endpoint(i)) => Some(i),
            _ => None,
        };
        oxidraft_cad::resolve_after_direct_edit(&mut self.document, id, pinned)
    }

    fn retarget_driven_dimension(&mut self, id: EntityId, role: Option<oxidraft_cad::GripRole>) {
        let retarget = match (role, self.document.get(id).and_then(|e| e.as_curve())) {
            (Some(oxidraft_cad::GripRole::Radius), Some(Curve::Arc(a))) => {
                oxidraft_document::SketchConstraint::radius(id, a.radius)
            }
            (Some(oxidraft_cad::GripRole::Endpoint(_)), Some(Curve::Line(l))) => {
                let len = (l.p1.x - l.p0.x).hypot(l.p1.y - l.p0.y);
                oxidraft_document::SketchConstraint::distance(id, len)
            }
            _ => return,
        };
        if self
            .document
            .constraints
            .iter()
            .any(|c| c.same_relation(&retarget))
        {
            self.document.add_constraint(retarget);
        }
    }

    fn reconstrain_tangency(&mut self, id: EntityId) {
        let Some(e) = self.document.get(id) else {
            return;
        };
        if e.tangents.is_empty() {
            return;
        }
        let Some(Curve::Arc(arc)) = e.as_curve() else {
            return;
        };
        let (center, radius) = (arc.center, arc.radius);
        let tangents = e.tangents.clone();
        let curves: Vec<Curve> = tangents
            .iter()
            .filter_map(|tr| {
                self.document
                    .get(tr.target)
                    .and_then(|t| t.as_curve())
                    .cloned()
            })
            .collect();
        if curves.len() != tangents.len() {
            return;
        }
        let solved = match curves.len() {
            3 => oxidraft_geometry::tangent_circle_ttt(&curves[0], &curves[1], &curves[2], center),
            2 => oxidraft_geometry::tangent_circle_ttr(&curves[0], &curves[1], radius, center),
            1 => {
                let r = oxidraft_geometry::point_to_curve_distance(&curves[0], center.x, center.y);
                (r > 1e-9).then_some((center, r))
            }
            _ => None,
        };
        if let Some((c, r)) = solved
            && r > 1e-9
            && let Some(e) = self.document.get_mut(id)
        {
            e.kind = EntityKind::Curve(Curve::Arc(oxidraft_geometry::CircularArc::new(
                c,
                r,
                0.0,
                std::f64::consts::TAU,
            )));
        }
    }

    /// Detaches the `which`-th tangency relation from arc/circle `id`,
    /// snapshotting history first. Silently ignores an out-of-range index.
    pub fn remove_tangent(&mut self, id: EntityId, which: usize) {
        self.history.snapshot(&self.document);
        if let Some(e) = self.document.get_mut(id)
            && which < e.tangents.len()
        {
            e.tangents.remove(which);
        }
    }

    /// World-space marker positions (index into the entity's tangent list,
    /// plus the point on the arc closest to each tangent target) for
    /// rendering arc/circle `id`'s tangency badges. Empty for a non-arc
    /// entity or one with no tangents.
    pub fn tangent_markers(&self, id: EntityId) -> Vec<(usize, Point2d)> {
        let Some(e) = self.document.get(id) else {
            return vec![];
        };
        let Some(Curve::Arc(arc)) = e.as_curve() else {
            return vec![];
        };
        e.tangents
            .iter()
            .enumerate()
            .filter_map(|(i, tr)| {
                let target = self.document.get(tr.target)?.as_curve()?;
                let (cx, cy) = arc.center.to_f64();
                let foot = oxidraft_geometry::project_point_onto_curve(target, cx, cy).point;
                let (fx, fy) = foot;
                let (dx, dy) = (fx - cx, fy - cy);
                let len = (dx * dx + dy * dy).sqrt();
                let tp = if len > 1e-9 {
                    Point2d::from_f64(cx + dx / len * arc.radius, cy + dy / len * arc.radius)
                } else {
                    Point2d::from_f64(fx, fy)
                };
                Some((i, tp))
            })
            .collect()
    }

    /// Finishes the active grip drag. If the grip never actually moved
    /// (e.g. a click immediately followed by release with no drag), discards
    /// the history snapshot [`AppState::begin_grip_drag`] took so a no-op
    /// click doesn't pollute the undo stack.
    pub fn end_grip_drag(&mut self) {
        if let Some(drag) = self.interaction.grip_drag.take()
            && !drag.moved
        {
            self.history.discard_last();
        }
    }

    /// Aborts the active grip drag and rolls the document back to its state
    /// before the drag started (e.g. on Escape mid-drag). No-op if no grip
    /// drag is active.
    pub fn cancel_grip_drag(&mut self) {
        if self.interaction.grip_drag.take().is_some()
            && let Some(prev) = self.history.rollback()
        {
            self.document = prev;
        }
    }

    /// Whether a grip drag is currently in progress.
    pub fn grip_editing(&self) -> bool {
        self.interaction.grip_drag.is_some()
    }

    /// The role of the grip currently being dragged (endpoint, radius,
    /// etc.), if a drag is active.
    pub fn grip_role(&self) -> Option<oxidraft_cad::GripRole> {
        self.interaction.grip_drag.as_ref().map(|d| d.grip.role)
    }

    /// Applies the active grip drag with an exact numeric `value` instead of
    /// a cursor position — used when the user types a value into a grip's
    /// inline edit box rather than dragging it — and ends the drag. Reverts
    /// the entity and constraints if the result isn't solvable. No-op if no
    /// grip drag is active.
    pub fn commit_grip_value(&mut self, value: f64) {
        let Some(drag) = self.interaction.grip_drag.as_ref() else {
            return;
        };
        let to = Point2d::from_f64(self.cursor_world.0, self.cursor_world.1);
        let edited = oxidraft_cad::apply_grip_value(&drag.start_kind, &drag.grip, value, to);
        let id = drag.entity_id;
        let prev_kind = self.document.get(id).map(|e| e.kind.clone());
        let prev_constraints = self.document.constraints.clone();
        if let Some(e) = self.document.get_mut(id) {
            e.kind = edited;
        }
        self.reconstrain_tangency(id);
        if !self.resolve_constraints_after(id) {
            if let Some(k) = prev_kind
                && let Some(e) = self.document.get_mut(id)
            {
                e.kind = k;
            }
            self.document.constraints = prev_constraints;
        }
        self.interaction.grip_drag = None;
    }

    /// Every draggable grip on the current selection, for rendering — empty
    /// unless the Select tool is active and no corner action is in progress
    /// (grips and corner-rounding handles don't coexist).
    pub fn selection_grips(&self) -> Vec<(EntityId, Grip)> {
        if !matches!(self.tool, Tool::Select) || self.interaction.corner_action.is_some() {
            return Vec::new();
        }
        let mut out = Vec::new();
        for &id in &self.selection {
            if let Some(e) = self.document.get(id) {
                for g in grips_for(&e.kind) {
                    out.push((id, g));
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxidraft_geometry::{Curve, LineSeg};

    fn pt(x: i64, y: i64) -> Point2d {
        Point2d::from_i64(x, y)
    }

    fn app() -> AppState {
        AppState::new(800.0, 600.0)
    }

    /// Drives the circle tool the way the pointer path does: a pick that
    /// snapped to `kind` on `id`.
    fn circle_pick_snapped(
        a: &mut AppState,
        kind: oxidraft_cad::SnapKind,
        id: EntityId,
        p: Point2d,
    ) {
        let ev = a.tool.on_pick(crate::tools::Pick {
            pos: p,
            curve: a.document.get(id).and_then(|e| e.as_curve().cloned()),
            snap: Some(oxidraft_cad::SnapPoint {
                kind,
                pos: p.to_f64(),
                entity: id,
            }),
        });
        a.apply_tool_event(ev);
    }

    #[test]
    fn a_tangent_circle_records_the_tangency_rather_than_baking_it() {
        // The point of knowing *why* a circle sits where it does. Baking the
        // radius produces the right picture once; recording the relation keeps
        // it right when the line later moves.
        let mut a = app();
        a.prefs.infer_constraints = true;
        let l = a.add_entity(line(-10, 3, 10, 3));

        a.tool = crate::tools::Tool::circle();
        a.place_tool_point(pt(0, 0));
        circle_pick_snapped(&mut a, oxidraft_cad::SnapKind::Tangent, l, pt(0, 3));

        let circle = a
            .document
            .iter()
            .find(|e| matches!(&e.kind, EntityKind::Curve(Curve::Arc(_))))
            .map(|e| e.id)
            .expect("a circle should have been created");
        assert!(
            a.document
                .constraints_on(circle)
                .any(|c| c.kind == oxidraft_document::ConstraintKind::Tangent),
            "the tangency should be recorded, not just drawn: {:?}",
            a.document.constraints_on(circle).collect::<Vec<_>>()
        );
    }

    #[test]
    fn recording_is_off_when_inference_is_off() {
        // Same picks, same geometry, no relation — `infer_constraints` is the
        // existing switch for "do not put things in my document that I did not
        // ask for", and this obeys it like every other inferred constraint.
        let mut a = app();
        a.prefs.infer_constraints = false;
        let l = a.add_entity(line(-10, 3, 10, 3));

        a.tool = crate::tools::Tool::circle();
        a.place_tool_point(pt(0, 0));
        circle_pick_snapped(&mut a, oxidraft_cad::SnapKind::Tangent, l, pt(0, 3));

        let before = a.document.len();
        assert!(before >= 2, "the circle should still be created");
        assert!(
            a.document
                .constraints
                .iter()
                .all(|c| c.kind != oxidraft_document::ConstraintKind::Tangent),
            "no tangency should be recorded with inference off"
        );
    }

    fn line(x0: i64, y0: i64, x1: i64, y1: i64) -> EntityKind {
        EntityKind::Curve(Curve::Line(LineSeg::from_endpoints(pt(x0, y0), pt(x1, y1))))
    }

    fn user_constraints(a: &AppState) -> Vec<oxidraft_document::SketchConstraint> {
        a.document
            .constraints
            .iter()
            .filter(|c| c.kind != oxidraft_document::ConstraintKind::Fixed)
            .copied()
            .collect()
    }

    #[test]
    fn polyline_closes_when_clicking_start_vertex() {
        let mut a = app();
        a.tool = Tool::Polyline { pts: Vec::new() };
        a.place_tool_point(pt(0, 0));
        a.place_tool_point(pt(10, 0));
        a.place_tool_point(pt(5, 8));
        a.place_tool_point(pt(0, 0));
        let lines: Vec<_> = a
            .document
            .iter()
            .filter_map(|e| match &e.kind {
                EntityKind::Curve(Curve::Line(l)) => Some(l.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(lines.len(), 3, "closed triangle = three line entities");
        assert!(
            lines[0].p0.dist_f64(&lines[2].p1) < 1e-9,
            "ends must coincide (closed)"
        );
        let welds = a
            .document
            .constraints
            .iter()
            .filter(|c| c.kind == oxidraft_document::ConstraintKind::Coincident)
            .count();
        assert_eq!(welds, 3, "every corner of the closed chain is welded");
        assert!(matches!(a.tool, Tool::Polyline { ref pts } if pts.is_empty()));
    }

    #[test]
    fn rectangle_creates_welded_lines_with_axis_constraints() {
        let mut a = app();
        a.prefs.infer_constraints = true;
        a.tool = Tool::Rectangle { first: None };
        a.place_tool_point(pt(0, 0));
        a.place_tool_point(pt(8, 5));
        let lines = a
            .document
            .iter()
            .filter(|e| matches!(e.kind, EntityKind::Curve(Curve::Line(_))))
            .count();
        assert_eq!(lines, 4, "four individual sides");
        let count = |k: oxidraft_document::ConstraintKind| {
            a.document
                .constraints
                .iter()
                .filter(|c| c.kind == k)
                .count()
        };
        assert_eq!(
            count(oxidraft_document::ConstraintKind::Coincident),
            4,
            "all four corners welded"
        );
        assert_eq!(count(oxidraft_document::ConstraintKind::Horizontal), 2);
        assert_eq!(count(oxidraft_document::ConstraintKind::Vertical), 2);
    }

    #[test]
    fn rectangle_welds_corners_even_without_auto_constrain() {
        let mut a = app();
        a.prefs.infer_constraints = false;
        a.tool = Tool::Rectangle { first: None };
        a.place_tool_point(pt(0, 0));
        a.place_tool_point(pt(8, 5));
        let count = |k: oxidraft_document::ConstraintKind| {
            a.document
                .constraints
                .iter()
                .filter(|c| c.kind == k)
                .count()
        };
        assert_eq!(
            count(oxidraft_document::ConstraintKind::Coincident),
            4,
            "the welds are structural, not inferred — always recorded"
        );
        assert_eq!(
            count(oxidraft_document::ConstraintKind::Horizontal)
                + count(oxidraft_document::ConstraintKind::Vertical),
            0,
            "the axis constraints are inferred extras, gated on the toggle"
        );
    }

    #[test]
    fn hexagon_side_dimension_drives_all_sides() {
        let mut a = app();
        a.prefs.infer_constraints = true;
        a.prefs.snap_on = false;
        a.tool = Tool::Polygon {
            center: None,
            radius_point: None,
            sides: Some(6),
        };
        a.place_tool_point(pt(0, 0));
        a.place_tool_point(pt(10, 0));
        a.confirm_pending_polygon();
        let sides: Vec<EntityId> = a
            .document
            .iter()
            .filter(|e| matches!(e.kind, EntityKind::Curve(Curve::Line(_))))
            .map(|e| e.id)
            .collect();
        assert_eq!(sides.len(), 6, "six individual sides");
        let count = |k: oxidraft_document::ConstraintKind| {
            a.document
                .constraints
                .iter()
                .filter(|c| c.kind == k)
                .count()
        };
        assert_eq!(count(oxidraft_document::ConstraintKind::Coincident), 6);
        assert_eq!(
            count(oxidraft_document::ConstraintKind::EqualLength),
            5,
            "every side equal to the first"
        );
        assert!(a.smart_dimension(sides[0], None, None));
        let dim = a
            .document
            .constraints
            .iter()
            .find(|c| c.kind == oxidraft_document::ConstraintKind::Distance && c.a == sides[0])
            .copied()
            .expect("a driving length landed on the picked side");
        a.set_constraint_value(dim, 5.0);
        for &id in &sides {
            let Some(EntityKind::Curve(Curve::Line(l))) = a.document.get(id).map(|e| &e.kind)
            else {
                panic!("side is still a line");
            };
            let len = (l.p1.x - l.p0.x).hypot(l.p1.y - l.p0.y);
            assert!(
                (len - 5.0).abs() < 1e-4,
                "every side follows the dimension: got {len}"
            );
        }
    }

    #[test]
    fn exploding_a_polycurve_welds_segments_when_auto_constrain_is_on() {
        let mut a = app();
        a.prefs.infer_constraints = true;
        let tri = oxidraft_geometry::PolyCurve::new(vec![
            Curve::Line(LineSeg::from_endpoints(pt(0, 0), pt(4, 0))),
            Curve::Line(LineSeg::from_endpoints(pt(4, 0), pt(4, 3))),
            Curve::Line(LineSeg::from_endpoints(pt(4, 3), pt(0, 0))),
        ]);
        let id = a.add_entity(EntityKind::Curve(Curve::Poly(Box::new(tri))));
        a.selection = vec![id];
        a.explode_selection();
        assert!(a.document.get(id).is_none(), "the polycurve is gone");
        let lines = a
            .document
            .iter()
            .filter(|e| matches!(e.kind, EntityKind::Curve(Curve::Line(_))))
            .count();
        assert_eq!(lines, 3);
        let welds = a
            .document
            .constraints
            .iter()
            .filter(|c| c.kind == oxidraft_document::ConstraintKind::Coincident)
            .count();
        assert_eq!(welds, 3, "explode weds the closed loop's corners");
    }

    #[test]
    fn exploding_stays_loose_with_auto_constrain_off() {
        let mut a = app();
        a.prefs.infer_constraints = false;
        let chain = oxidraft_geometry::PolyCurve::new(vec![
            Curve::Line(LineSeg::from_endpoints(pt(0, 0), pt(4, 0))),
            Curve::Line(LineSeg::from_endpoints(pt(4, 0), pt(4, 3))),
        ]);
        let id = a.add_entity(EntityKind::Curve(Curve::Poly(Box::new(chain))));
        a.selection = vec![id];
        a.explode_selection();
        let welds = a
            .document
            .constraints
            .iter()
            .filter(|c| c.kind == oxidraft_document::ConstraintKind::Coincident)
            .count();
        assert_eq!(welds, 0, "DISJOINT keeps its word when inference is off");
    }

    #[test]
    fn polyline_does_not_close_on_a_non_start_point() {
        let mut a = app();
        a.tool = Tool::Polyline { pts: Vec::new() };
        a.place_tool_point(pt(0, 0));
        a.place_tool_point(pt(10, 0));
        a.place_tool_point(pt(5, 8));
        a.place_tool_point(pt(12, 8));
        assert!(matches!(a.tool, Tool::Polyline { ref pts } if pts.len() == 4));
        assert!(
            !a.document
                .iter()
                .any(|e| matches!(&e.kind, EntityKind::Curve(Curve::Poly(_))))
        );
    }

    #[test]
    fn tangent_markers_and_removal() {
        use oxidraft_geometry::CircularArc;
        let mut a = app();
        a.prefs.snap_on = false;
        let l1 = a.document.add(line(0, 0, 10, 0));
        let l2 = a.document.add(line(0, 0, 0, 10));
        let cid = a
            .document
            .add(EntityKind::Curve(Curve::Arc(CircularArc::new(
                Point2d::from_f64(2.0, 2.0),
                2.0,
                0.0,
                std::f64::consts::TAU,
            ))));
        if let Some(e) = a.document.get_mut(cid) {
            e.tangents = vec![
                oxidraft_document::TangentRef {
                    target: l1,
                    near: Point2d::from_f64(2.0, 0.0),
                },
                oxidraft_document::TangentRef {
                    target: l2,
                    near: Point2d::from_f64(0.0, 2.0),
                },
            ];
        }
        a.selection = vec![cid];
        let markers = a.tangent_markers(cid);
        assert_eq!(markers.len(), 2);
        for (_, p) in &markers {
            assert!((p.dist_f64(&Point2d::from_f64(2.0, 2.0)) - 2.0).abs() < 1e-6);
        }
        a.remove_tangent(cid, 0);
        assert_eq!(a.tangent_markers(cid).len(), 1);
    }

    #[test]
    fn clipboard_copy_paste_duplicates_at_cursor() {
        let mut a = app();
        let id = a.document.add(line(0, 0, 10, 0));
        a.selection = vec![id];
        assert_eq!(a.clipboard_copy(), 1);
        a.cursor_world = (50.0, 20.0);
        let before = a.document.len();
        a.clipboard_paste();
        assert_eq!(a.document.len(), before + 1);
        assert_eq!(a.selection.len(), 1, "pasted entity becomes the selection");
        let pasted = a.document.get(a.selection[0]).unwrap();
        if let EntityKind::Curve(Curve::Line(l)) = &pasted.kind {
            assert!((l.p0.x - 45.0).abs() < 1e-9 && (l.p0.y - 20.0).abs() < 1e-9);
            assert!((l.p1.x - 55.0).abs() < 1e-9 && (l.p1.y - 20.0).abs() < 1e-9);
        } else {
            panic!("expected a pasted line");
        }
    }

    #[test]
    fn clipboard_cut_removes_then_pastes() {
        let mut a = app();
        let id = a.document.add(line(0, 0, 2, 2));
        a.selection = vec![id];
        let with_entity = a.document.len();
        a.clipboard_cut();
        assert_eq!(a.document.len(), with_entity - 1, "cut removes the entity");
        a.cursor_world = (0.0, 0.0);
        a.clipboard_paste();
        assert_eq!(a.document.len(), with_entity, "paste restores one entity");
    }

    #[test]
    fn paste_with_empty_clipboard_is_noop() {
        let mut a = app();
        let before = a.document.len();
        a.clipboard_paste();
        assert_eq!(a.document.len(), before);
    }

    #[test]
    fn ui_prefs_round_trip() {
        let p = UiPrefs {
            snap_on: false,
            grid_on: true,
            grid_snap_on: true,
            polar_on: false,
            track_on: false,
            dyn_on: true,
            comb_on: true,
            comb_scale: 7.5,
            snap_px: 8.0,
            polar_step: 30.0,
            zoom_speed: 1.5,
            zoom_to_cursor: false,
            invert_zoom: true,
            crosshair: false,
            pick_box: 14.0,
            show_lineweights: false,
            lineweight_scale: 3.0,
            grid_dots: true,
            grid_major_every: 4,
            grid_minor_rgb: (20, 30, 40),
            grid_major_rgb: (50, 60, 70),
            text_font: Some("Arial".into()),
            infer_constraints: false,
            show_constraints: false,
        };
        assert_eq!(UiPrefs::deserialize(&p.serialize()), p);
        let q = UiPrefs {
            text_font: None,
            ..Default::default()
        };
        assert_eq!(UiPrefs::deserialize(&q.serialize()).text_font, None);
    }

    #[test]
    fn dimension_lands_on_dimension_layer() {
        let mut a = app();
        a.tool = crate::tools::Tool::Dimension { subject: None };
        a.place_tool_point(Point2d::from_f64(0.0, 0.0));
        a.place_tool_point(Point2d::from_f64(10.0, 0.0));
        a.place_tool_point(Point2d::from_f64(0.0, 3.0));
        let dim = a
            .document
            .iter()
            .find(|e| matches!(e.kind, oxidraft_document::EntityKind::Dimension { .. }))
            .expect("a dimension entity");
        let layer = a.document.layers.get(dim.layer).expect("its layer");
        assert_eq!(layer.name, oxidraft_document::DIMENSION_LAYER);
    }

    #[test]
    fn weld_tool_welds_origin_to_a_line_midpoint() {
        let mut a = app();
        a.prefs.snap_on = false;
        let l = a
            .document
            .add(EntityKind::Curve(Curve::Line(LineSeg::from_endpoints(
                Point2d::from_f64(2.0, 3.0),
                Point2d::from_f64(6.0, 5.0),
            ))));
        a.tool = crate::tools::Tool::Weld { first: None };
        let (ox, oy) = a.view.world_to_screen(0.0, 0.0);
        a.canvas_click(ox, oy);
        assert!(
            matches!(a.tool, crate ::tools::Tool::Weld { first : Some((id, 0, _)) } if id
            == a.origin_id),
            "origin picked as the first anchor: {:?}",
            a.tool
        );
        let (mx, my) = a.view.world_to_screen(4.0, 4.0);
        a.canvas_click(mx, my);
        let c = a
            .document
            .constraints
            .iter()
            .find(|c| c.kind == oxidraft_document::ConstraintKind::Coincident)
            .expect("the weld was recorded");
        assert_eq!(
            c.pts,
            Some((0, oxidraft_document::ANCHOR_DERIVED)),
            "origin anchor 0 welded to the line's midpoint anchor"
        );
        let ls = match a.document.get(l).and_then(|e| e.as_curve()) {
            Some(Curve::Line(ls)) => ls.clone(),
            other => panic!("expected the line, got {other:?}"),
        };
        let mid = ((ls.p0.x + ls.p1.x) * 0.5, (ls.p0.y + ls.p1.y) * 0.5);
        assert!(
            mid.0.abs() < 1e-6 && mid.1.abs() < 1e-6,
            "the line slid so its midpoint sits on the origin: {mid:?}"
        );
        assert!(
            matches!(a.tool, crate::tools::Tool::Weld { first: None }),
            "the tool reset for the next weld"
        );
    }

    #[test]
    fn radial_dimension_tool_dimensions_a_circle() {
        let mut a = app();
        a.prefs.snap_on = false;
        let circle = a.document.add(EntityKind::Curve(Curve::Arc(
            oxidraft_geometry::CircularArc::new(
                Point2d::from_f64(0.0, 0.0),
                5.0,
                0.0,
                std::f64::consts::TAU,
            ),
        )));
        a.tool = crate::tools::Tool::Dimension { subject: None };
        let (sx, sy) = a.view.world_to_screen(5.0, 0.0);
        a.canvas_click(sx, sy);
        assert!(
            matches!(
                a.tool,
                crate::tools::Tool::Dimension {
                    subject: Some(crate::tools::DimSubject::Radial { radius, .. })
                } if (radius - 5.0).abs() < 1e-9
            ),
            "circle pick set centre+radius, got {:?}",
            a.tool
        );
        let (lx, ly) = a.view.world_to_screen(0.0, 6.0);
        a.canvas_click(lx, ly);
        let made = a.document.iter().any(|e| {
            matches!(
                & e.kind, EntityKind::RadialDim { center, .. } if * center ==
                Point2d::from_f64(0.0, 0.0)
            )
        });
        assert!(made, "radial dimension created on the circle");
        let _ = circle;
    }

    #[test]
    fn angular_from_two_lines_creates_dim_at_intersection() {
        let mut a = app();
        a.prefs.snap_on = false;
        a.document
            .add(EntityKind::Curve(Curve::Line(LineSeg::from_endpoints(
                Point2d::from_f64(0.0, 0.0),
                Point2d::from_f64(10.0, 0.0),
            ))));
        a.document
            .add(EntityKind::Curve(Curve::Line(LineSeg::from_endpoints(
                Point2d::from_f64(0.0, 0.0),
                Point2d::from_f64(0.0, 10.0),
            ))));
        a.tool = crate::tools::Tool::Dimension { subject: None };
        let (s1x, s1y) = a.view.world_to_screen(5.0, 0.0);
        a.canvas_click(s1x, s1y);
        let (s2x, s2y) = a.view.world_to_screen(0.0, 5.0);
        a.canvas_click(s2x, s2y);
        assert!(
            matches!(
                a.tool,
                crate::tools::Tool::Dimension {
                    subject: Some(crate::tools::DimSubject::LinePair(..))
                }
            ),
            "two line picks produced the angle geometry, got {:?}",
            a.tool
        );
        let (lx, ly) = a.view.world_to_screen(3.0, 3.0);
        a.canvas_click(lx, ly);
        let dim = a
            .document
            .iter()
            .find_map(|e| match &e.kind {
                EntityKind::AngularDim { center, .. } => Some(*center),
                _ => None,
            })
            .expect("an angular dimension");
        assert!(
            dim.dist_f64(&Point2d::from_f64(0.0, 0.0)) < 1e-6,
            "vertex at intersection"
        );
    }

    #[test]
    fn save_open_dispatches_by_extension() {
        let _guard = crate::autosave::RECOVERY_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for ext in ["o2d", "dxf", "svg"] {
            let mut a = app();
            a.document
                .add(EntityKind::Curve(Curve::Line(LineSeg::from_endpoints(
                    pt(0, 0),
                    pt(10, 5),
                ))));
            a.document.add(EntityKind::Curve(Curve::Arc(
                oxidraft_geometry::CircularArc::new(pt(3, 4), 5.0, 0.0, std::f64::consts::TAU),
            )));
            let want = a.document.iter().filter(|e| e.id != a.origin_id).count();
            let path = std::env::temp_dir()
                .join(format!("o2d_io_test_{}_{ext}.{ext}", std::process::id()));
            assert!(a.save_file_to(path.clone()), "save .{ext} should succeed");
            let mut b = app();
            b.open_file(path.clone());
            let got = b.document.iter().filter(|e| e.id != b.origin_id).count();
            assert_eq!(
                got, want,
                ".{ext} round-trip lost entities: {want} -> {got}"
            );
            let _ = std::fs::remove_file(path);
        }
    }

    #[test]
    fn opening_a_legacy_e2d_file_still_works() {
        let path = std::env::temp_dir().join(format!("o2d_legacy_test_{}.e2d", std::process::id()));
        std::fs::write(&path, "E2D 1\nE LINE 0 bylayer 0;0 4;0 ByLayer bylayer\n").unwrap();
        let mut a = app();
        a.open_file(path.clone());
        let _ = std::fs::remove_file(path);
        assert!(
            a.command_log.is_empty(),
            "opening a legacy .e2d file should not log an error: {:?}",
            a.command_log
        );
        assert_eq!(a.document.iter().filter(|e| e.id != a.origin_id).count(), 1);
    }

    #[test]
    fn line_command_then_two_clicks_creates_segment() {
        let mut a = app();
        a.run_command("LINE");
        assert_eq!(a.tool.name(), "LINE");
        let (s1x, s1y) = a.view.world_to_screen(0.0, 0.0);
        let (s2x, s2y) = a.view.world_to_screen(5.0, 0.0);
        a.prefs.snap_on = false;
        a.canvas_click(s1x, s1y);
        assert_eq!(a.document.len(), 1);
        a.canvas_click(s2x, s2y);
        assert_eq!(a.document.len(), 2);
    }

    #[test]
    fn undo_redo_through_state() {
        let mut a = app();
        a.add_entity(EntityKind::Curve(Curve::Line(LineSeg::from_endpoints(
            pt(0, 0),
            pt(1, 1),
        ))));
        assert_eq!(a.document.len(), 2);
        a.undo();
        assert_eq!(a.document.len(), 1);
        a.redo();
        assert_eq!(a.document.len(), 2);
    }

    #[test]
    fn erase_removes_selection() {
        let mut a = app();
        let id = a.add_entity(EntityKind::Curve(Curve::Line(LineSeg::from_endpoints(
            pt(0, 0),
            pt(2, 2),
        ))));
        a.selection = vec![id];
        a.run_command("ERASE");
        assert_eq!(a.document.len(), 1);
    }

    #[test]
    fn select_all_then_erase() {
        let mut a = app();
        a.add_entity(EntityKind::Curve(Curve::Line(LineSeg::from_endpoints(
            pt(0, 0),
            pt(1, 0),
        ))));
        a.add_entity(EntityKind::Curve(Curve::Line(LineSeg::from_endpoints(
            pt(0, 0),
            pt(0, 1),
        ))));
        a.run_command("ALL");
        assert_eq!(a.selection.len(), 2);
        a.run_command("ERASE");
        assert_eq!(a.document.len(), 1);
    }

    #[test]
    fn layer_commands() {
        let mut a = app();
        a.run_command("LAYER NEW walls");
        assert_eq!(a.current_layer_name(), "walls");
        a.run_command("LAYER SET 0");
        assert_eq!(a.current_layer_name(), "0");
    }

    #[test]
    fn move_command_uses_selection() {
        let mut a = app();
        let id = a.add_entity(EntityKind::Curve(Curve::Line(LineSeg::from_endpoints(
            pt(0, 0),
            pt(2, 0),
        ))));
        a.selection = vec![id];
        a.run_command("MOVE");
        a.prefs.snap_on = false;
        let (b1x, b1y) = a.view.world_to_screen(0.0, 0.0);
        let (b2x, b2y) = a.view.world_to_screen(10.0, 5.0);
        a.canvas_click(b1x, b1y);
        a.canvas_click(b2x, b2y);
        if let Some(Curve::Line(l)) = a.document.get(id).unwrap().as_curve() {
            assert!((l.p0.x - 10.0).abs() < 1e-4);
            assert!((l.p0.y - 5.0).abs() < 1e-4);
        } else {
            panic!()
        }
    }

    #[test]
    fn zoom_extents_frames_geometry() {
        let mut a = app();
        a.add_entity(EntityKind::Curve(Curve::Line(LineSeg::from_endpoints(
            pt(0, 0),
            pt(100, 80),
        ))));
        a.run_command("ZOOM E");
        for _ in 0..200 {
            if !a.tick_zoom_anim() {
                break;
            }
        }
        let (x0, y0, x1, y1) = a.view.visible_bounds();
        assert!(x0 <= 0.0 && x1 >= 100.0 && y0 <= 0.0 && y1 >= 80.0);
    }

    #[test]
    fn coord_readout_tracks_cursor() {
        let mut a = app();
        let (sx, sy) = a.view.world_to_screen(3.0, 7.0);
        a.pointer_moved(sx, sy);
        let r = a.coord_readout();
        assert!(r.starts_with("3.0000, 7.0000"));
    }

    #[test]
    fn perpendicular_snapping_uses_tool_reference_point() {
        let mut a = app();
        a.add_entity(EntityKind::Curve(Curve::Line(LineSeg::from_endpoints(
            pt(0, 0),
            pt(10, 0),
        ))));
        a.snap.enabled = vec![oxidraft_cad::SnapKind::Perpendicular];
        a.prefs.snap_on = true;
        a.run_command("LINE");
        let (s1x, s1y) = a.view.world_to_screen(3.0, 5.0);
        a.canvas_click(s1x, s1y);
        let (s2x, s2y) = a.view.world_to_screen(3.1, 0.1);
        a.pointer_moved(s2x, s2y);
        assert!(a.active_snap.is_some());
        let sp = a.active_snap.as_ref().unwrap();
        assert_eq!(sp.kind, oxidraft_cad::SnapKind::Perpendicular);
        assert!((sp.pos.0 - 3.0).abs() < 1e-4);
        assert!(sp.pos.1.abs() < 1e-4);
    }

    #[test]
    fn grid_snap_locks_cursor_to_grid_intersection() {
        let mut a = app();
        a.prefs.snap_on = false;
        a.prefs.grid_snap_on = true;
        a.run_command("LINE");
        let g = a.view.grid_spacing();
        let (sx, sy) = a.view.world_to_screen(2.0 * g + g * 0.2, -g - g * 0.1);
        a.pointer_moved(sx, sy);
        assert!(
            (a.cursor_world.0 - 2.0 * g).abs() < 1e-6,
            "x={}",
            a.cursor_world.0
        );
        assert!(
            (a.cursor_world.1 - (-g)).abs() < 1e-6,
            "y={}",
            a.cursor_world.1
        );
    }

    #[test]
    fn grip_drag_snaps_to_other_entity() {
        let mut a = app();
        a.prefs.snap_on = true;
        let l1 = a.add_entity(EntityKind::Curve(Curve::Line(LineSeg::from_endpoints(
            pt(0, 0),
            pt(10, 0),
        ))));
        a.add_entity(EntityKind::Curve(Curve::Line(LineSeg::from_endpoints(
            pt(5, 5),
            pt(20, 5),
        ))));
        a.selection = vec![l1];
        let grip = a
            .selection_grips()
            .into_iter()
            .find(|(id, _)| *id == l1)
            .map(|(_, g)| g)
            .expect("line should expose grips");
        a.begin_grip_drag(l1, grip);
        let (sx, sy) = a.view.world_to_screen(5.0, 5.0);
        a.pointer_moved(sx, sy);
        assert!(
            a.active_snap.is_some(),
            "expected a snap while grip-dragging"
        );
        assert!(
            (a.cursor_world.0 - 5.0).abs() < 1e-6 && (a.cursor_world.1 - 5.0).abs() < 1e-6,
            "cursor did not snap to the other entity: {:?}",
            a.cursor_world
        );
    }

    fn perpendicular_pair(a: &mut AppState) -> (EntityId, EntityId) {
        let l1 = a.add_entity(EntityKind::Curve(Curve::Line(LineSeg::from_endpoints(
            pt(0, 0),
            pt(5, 0),
        ))));
        let l2 = a.add_entity(EntityKind::Curve(Curve::Line(LineSeg::from_endpoints(
            pt(0, 0),
            pt(0, 3),
        ))));
        a.selection = vec![l1, l2];
        a.constrain_selection(oxidraft_cad::ConstraintKind::Perpendicular);
        assert_eq!(user_constraints(a).len(), 1, "constraint recorded");
        (l1, l2)
    }

    fn line_of(a: &AppState, id: EntityId) -> LineSeg {
        match a.document.get(id).unwrap().as_curve().unwrap() {
            Curve::Line(l) => l.clone(),
            other => panic!("expected line, got {other:?}"),
        }
    }

    #[test]
    fn grip_drag_maintains_perpendicular_constraint() {
        let mut a = app();
        a.prefs.snap_on = false;
        let (l1, l2) = perpendicular_pair(&mut a);
        let grip = oxidraft_cad::grips_for(&a.document.get(l1).unwrap().kind)[1];
        a.begin_grip_drag(l1, grip);
        a.apply_grip_drag((4.0, 3.0));
        a.end_grip_drag();
        let la = line_of(&a, l1);
        let lb = line_of(&a, l2);
        assert!(
            (la.p1.x - 4.0).abs() < 1e-6 && (la.p1.y - 3.0).abs() < 1e-6,
            "dragged endpoint follows the cursor: {la:?}"
        );
        let dot =
            (la.p1.x - la.p0.x) * (lb.p1.x - lb.p0.x) + (la.p1.y - la.p0.y) * (lb.p1.y - lb.p0.y);
        assert!(dot.abs() < 1e-6, "partner re-solved, dot={dot}");
    }

    #[test]
    fn cancel_grip_drag_restores_constrained_partners() {
        let mut a = app();
        a.prefs.snap_on = false;
        let (l1, l2) = perpendicular_pair(&mut a);
        let before_a = line_of(&a, l1);
        let before_b = line_of(&a, l2);
        let grip = oxidraft_cad::grips_for(&a.document.get(l1).unwrap().kind)[1];
        a.begin_grip_drag(l1, grip);
        a.apply_grip_drag((4.0, 3.0));
        let moved_b = line_of(&a, l2);
        assert!(
            (moved_b.p1.x - before_b.p1.x).abs() > 1e-3
                || (moved_b.p1.y - before_b.p1.y).abs() > 1e-3,
            "partner moved during the drag"
        );
        a.cancel_grip_drag();
        let la = line_of(&a, l1);
        let lb = line_of(&a, l2);
        assert_eq!(
            (la.p0, la.p1),
            (before_a.p0, before_a.p1),
            "dragged line restored"
        );
        assert_eq!(
            (lb.p0, lb.p1),
            (before_b.p0, before_b.p1),
            "partner restored"
        );
        assert_eq!(user_constraints(&a).len(), 1, "constraint survives cancel");
    }

    #[test]
    fn infeasible_grip_drag_holds_the_last_solvable_state() {
        let mut a = app();
        a.prefs.snap_on = false;
        let id = a.add_entity(EntityKind::Curve(Curve::Line(LineSeg::from_endpoints(
            pt(0, 0),
            pt(5, 0),
        ))));
        a.document
            .constraints
            .push(oxidraft_document::SketchConstraint::distance(id, 5.0));
        a.document
            .constraints
            .push(oxidraft_document::SketchConstraint::distance(id, 3.0));
        let before = line_of(&a, id);
        let grip = oxidraft_cad::grips_for(&a.document.get(id).unwrap().kind)[1];
        a.begin_grip_drag(id, grip);
        a.apply_grip_drag((9.0, 9.0));
        let after = line_of(&a, id);
        assert_eq!(
            (before.p0, before.p1),
            (after.p0, after.p1),
            "an unsatisfiable drag must hold the last solvable geometry, not tear it: {after:?}"
        );
        assert_eq!(
            user_constraints(&a).len(),
            2,
            "the rolled-back step must not leave a retargeted constraint behind"
        );
    }

    #[test]
    fn dragging_an_endpoint_retargets_a_driven_length() {
        let mut a = app();
        a.prefs.snap_on = false;
        let id = a.add_entity(EntityKind::Curve(Curve::Line(LineSeg::from_endpoints(
            pt(0, 0),
            pt(4, 0),
        ))));
        a.selection = vec![id];
        a.constrain_distance_selection(Some(4.0));
        let grip = oxidraft_cad::grips_for(&a.document.get(id).unwrap().kind)[1];
        a.begin_grip_drag(id, grip);
        a.apply_grip_drag((7.0, 0.0));
        a.end_grip_drag();
        let l = line_of(&a, id);
        assert!(
            (l.p1.x - 7.0).abs() < 1e-6 && l.p0.x.abs() < 1e-6,
            "drag wins, the untouched end stays put: {l:?}"
        );
        let c = a
            .document
            .constraints
            .iter()
            .find(|c| c.kind == oxidraft_document::ConstraintKind::Distance && c.a == id)
            .expect("length constraint survives");
        assert_eq!(c.val, Some(7.0), "the length dimension followed the drag");
    }

    #[test]
    fn dragging_the_radius_grip_retargets_a_driven_radius() {
        use oxidraft_geometry::CircularArc;
        let mut a = app();
        a.prefs.snap_on = false;
        let circle = a.add_entity(EntityKind::Curve(Curve::Arc(CircularArc::new(
            Point2d::from_f64(0.0, 0.0),
            2.0,
            0.0,
            std::f64::consts::TAU,
        ))));
        a.selection = vec![circle];
        a.constrain_radius_selection(Some(2.0));
        let grip = oxidraft_cad::grips_for(&a.document.get(circle).unwrap().kind)
            .into_iter()
            .find(|g| g.role == oxidraft_cad::GripRole::Radius)
            .expect("a full circle exposes radius grips");
        a.begin_grip_drag(circle, grip);
        a.apply_grip_drag((0.0, 3.0));
        a.end_grip_drag();
        let r = match a.document.get(circle).unwrap().as_curve().unwrap() {
            Curve::Arc(arc) => arc.radius,
            other => panic!("expected arc, got {other:?}"),
        };
        assert!((r - 3.0).abs() < 1e-6, "drag resized the circle: {r}");
        let c = a
            .document
            .constraints
            .iter()
            .find(|c| c.kind == oxidraft_document::ConstraintKind::Radius && c.a == circle)
            .expect("radius constraint survives");
        assert_eq!(c.val, Some(3.0), "the radius dimension followed the drag");
    }

    #[test]
    fn chained_line_segments_weld_and_stay_attached() {
        let mut a = app();
        a.prefs.snap_on = false;
        a.prefs.infer_constraints = true;
        a.run_command("LINE");
        for (x, y) in [(0.0, 0.0), (8.0, 0.0), (8.0, 6.0)] {
            let (sx, sy) = a.view.world_to_screen(x, y);
            a.canvas_click(sx, sy);
        }
        let l1 = a.document.order[1];
        let l2 = a.document.order[2];
        let welds: Vec<_> = a
            .document
            .constraints
            .iter()
            .filter(|c| c.kind == oxidraft_document::ConstraintKind::Coincident)
            .collect();
        assert_eq!(welds.len(), 1, "chain weld recorded");
        let c = welds[0];
        assert_eq!((c.a, c.b), (l1, Some(l2)));
        assert_eq!(c.pts, Some((1, 0)));
        let grip = oxidraft_cad::grips_for(&a.document.get(l1).unwrap().kind)[1];
        a.begin_grip_drag(l1, grip);
        a.apply_grip_drag((9.0, 1.0));
        a.end_grip_drag();
        let s2 = line_of(&a, l2);
        assert!(
            (s2.p0.x - 9.0).abs() < 1e-6 && (s2.p0.y - 1.0).abs() < 1e-6,
            "welded corner followed: {s2:?}"
        );
    }

    #[test]
    fn closing_a_line_chain_welds_the_loop_corner() {
        let mut a = app();
        a.prefs.snap_on = false;
        a.prefs.infer_constraints = true;
        a.run_command("LINE");
        for (x, y) in [(0.0, 0.0), (8.0, 0.0), (8.0, 6.0), (0.0, 0.0)] {
            a.place_tool_point(Point2d::from_f64(x, y));
        }
        let l1 = a.document.order[1];
        let l3 = a.document.order[3];
        let closure = a
            .document
            .constraints
            .iter()
            .find(|c| {
                c.kind == oxidraft_document::ConstraintKind::Coincident
                    && (c.a, c.b) == (l1, Some(l3))
            })
            .expect("closing segment welded to the chain start");
        assert_eq!(closure.pts, Some((0, 1)));
        let welds = a
            .document
            .constraints
            .iter()
            .filter(|c| c.kind == oxidraft_document::ConstraintKind::Coincident)
            .count();
        assert_eq!(welds, 3, "two chain welds plus the closure");
    }

    #[test]
    fn near_axis_drawn_line_is_leveled_and_constrained_horizontal() {
        let mut a = app();
        a.prefs.snap_on = false;
        a.prefs.infer_constraints = true;
        a.prefs.polar_on = false;
        a.prefs.track_on = false;
        a.run_command("LINE");
        let (s1x, s1y) = a.view.world_to_screen(0.0, 0.0);
        a.canvas_click(s1x, s1y);
        let (s2x, s2y) = a.view.world_to_screen(8.0, 0.04);
        a.canvas_click(s2x, s2y);
        let id = *a.document.order.last().unwrap();
        let l = line_of(&a, id);
        assert!((l.p1.y - l.p0.y).abs() < 1e-12, "line snapped level: {l:?}");
        assert!(
            a.document
                .constraints
                .iter()
                .any(|c| c.kind == oxidraft_document::ConstraintKind::Horizontal && c.a == id),
            "horizontal constraint recorded"
        );
        match &a.tool {
            Tool::Line {
                first: Some(crate::tools::TanAnchor::Point(p)),
            } => {
                assert!((p.to_f64().1 - l.p1.y).abs() < 1e-12, "chain follows level")
            }
            other => panic!("line tool still active, got {other:?}"),
        }
    }

    #[test]
    fn near_axis_drawn_line_is_leveled_and_constrained_vertical() {
        let mut a = app();
        a.prefs.snap_on = false;
        a.prefs.infer_constraints = true;
        a.prefs.polar_on = false;
        a.prefs.track_on = false;
        a.run_command("LINE");
        let (s1x, s1y) = a.view.world_to_screen(2.0, 1.0);
        a.canvas_click(s1x, s1y);
        let (s2x, s2y) = a.view.world_to_screen(2.04, 7.0);
        a.canvas_click(s2x, s2y);
        let id = *a.document.order.last().unwrap();
        let l = line_of(&a, id);
        assert!((l.p1.x - l.p0.x).abs() < 1e-12, "line snapped plumb: {l:?}");
        assert!(
            a.document
                .constraints
                .iter()
                .any(|c| c.kind == oxidraft_document::ConstraintKind::Vertical && c.a == id),
            "vertical constraint recorded"
        );
    }

    #[test]
    fn typed_near_axis_end_is_not_leveled() {
        let mut a = app();
        a.prefs.snap_on = false;
        a.run_command("LINE");
        a.place_tool_point(Point2d::from_f64(0.0, 0.0));
        a.place_tool_point(Point2d::from_f64(8.0, 0.04));
        let id = *a.document.order.last().unwrap();
        let l = line_of(&a, id);
        assert!((l.p1.y - 0.04).abs() < 1e-12, "typed end untouched: {l:?}");
        assert!(
            user_constraints(&a).is_empty(),
            "no constraint on a deliberately off-axis typed line"
        );
    }

    #[test]
    fn exact_axis_lines_record_constraints_without_moving() {
        let mut a = app();
        a.prefs.snap_on = false;
        a.prefs.infer_constraints = true;
        a.run_command("LINE");
        for (x, y) in [(0.0, 0.0), (10.0, 0.0), (10.0, 4.0)] {
            a.place_tool_point(Point2d::from_f64(x, y));
        }
        let l1 = a.document.order[1];
        let l2 = a.document.order[2];
        assert!(
            a.document
                .constraints
                .iter()
                .any(|c| c.kind == oxidraft_document::ConstraintKind::Horizontal && c.a == l1),
            "exact horizontal typed line recorded"
        );
        assert!(
            a.document
                .constraints
                .iter()
                .any(|c| c.kind == oxidraft_document::ConstraintKind::Vertical && c.a == l2),
            "exact vertical typed line recorded"
        );
        let s1 = line_of(&a, l1);
        assert_eq!((s1.p0.x, s1.p0.y, s1.p1.x, s1.p1.y), (0.0, 0.0, 10.0, 0.0));
    }

    #[test]
    fn endpoint_snap_infers_coincident_on_drawn_line() {
        let mut a = app();
        a.prefs.snap_on = true;
        a.prefs.infer_constraints = true;
        let base = a.add_entity(EntityKind::Curve(Curve::Line(LineSeg::from_endpoints(
            pt(0, 0),
            pt(10, 0),
        ))));
        a.run_command("LINE");
        let (sx, sy) = a.view.world_to_screen(10.0, 0.0);
        a.canvas_click(sx, sy);
        let (sx2, sy2) = a.view.world_to_screen(15.0, 5.0);
        a.canvas_click(sx2, sy2);
        let new_id = *a.document.order.last().unwrap();
        let uc = user_constraints(&a);
        assert_eq!(uc.len(), 1, "snap coincidence recorded");
        let c = uc[0];
        assert_eq!((c.a, c.b, c.pts), (base, Some(new_id), Some((1, 0))));
    }

    fn quarter_arc(a: &mut AppState) -> (EntityId, (f64, f64), (f64, f64)) {
        let q = std::f64::consts::FRAC_PI_4;
        let arc = oxidraft_geometry::CircularArc::new(pt(0, 0), 5.0, -q, q);
        let end = arc.end_point();
        let id = a.add_entity(EntityKind::Curve(Curve::Arc(arc)));
        (id, end, (-q.sin(), q.cos()))
    }

    #[test]
    fn line_drawn_off_an_arc_endpoint_snaps_tangent_and_records() {
        let mut a = app();
        a.prefs.snap_on = true;
        a.prefs.infer_constraints = true;
        a.prefs.polar_on = false;
        a.prefs.track_on = false;
        let (arc_id, att, t) = quarter_arc(&mut a);
        a.run_command("LINE");
        let (sx, sy) = a.view.world_to_screen(att.0, att.1);
        a.canvas_click(sx, sy);
        let click = (
            att.0 + 6.0 * t.0 - 0.04 * t.1,
            att.1 + 6.0 * t.1 + 0.04 * t.0,
        );
        let (sx2, sy2) = a.view.world_to_screen(click.0, click.1);
        a.canvas_click(sx2, sy2);
        let line_id = *a.document.order.last().unwrap();
        let l = line_of(&a, line_id);
        let cross = t.0 * (l.p1.y - l.p0.y) - t.1 * (l.p1.x - l.p0.x);
        assert!(
            cross.abs() < 1e-9,
            "line rotated onto the tangent ray: {cross}"
        );
        assert!(
            a.document.constraints.iter().any(|c| {
                c.kind == oxidraft_document::ConstraintKind::Tangent
                    && (c.a, c.b) == (arc_id, Some(line_id))
            }),
            "tangent constraint recorded"
        );
        assert!(
            a.document.constraints.iter().any(|c| {
                c.kind == oxidraft_document::ConstraintKind::Coincident
                    && (c.a, c.b, c.pts) == (arc_id, Some(line_id), Some((1, 0)))
            }),
            "weld to the arc endpoint recorded"
        );
        match &a.tool {
            Tool::Line {
                first: Some(crate::tools::TanAnchor::Point(p)),
            } => {
                assert!(
                    (p.x - l.p1.x).abs() < 1e-12 && (p.y - l.p1.y).abs() < 1e-12,
                    "chain follows the rotated end"
                );
            }
            other => panic!("line tool still active, got {other:?}"),
        }
    }

    #[test]
    fn inferred_tangency_survives_a_grip_drag() {
        let mut a = app();
        a.prefs.snap_on = true;
        a.prefs.infer_constraints = true;
        a.prefs.polar_on = false;
        a.prefs.track_on = false;
        let (_, att, t) = quarter_arc(&mut a);
        a.run_command("LINE");
        let (sx, sy) = a.view.world_to_screen(att.0, att.1);
        a.canvas_click(sx, sy);
        let click = (att.0 + 6.0 * t.0, att.1 + 6.0 * t.1);
        let (sx2, sy2) = a.view.world_to_screen(click.0, click.1);
        a.canvas_click(sx2, sy2);
        let line_id = *a.document.order.last().unwrap();
        a.run_command("");
        let grips = oxidraft_cad::grips_for(&a.document.get(line_id).unwrap().kind);
        let grip = *grips
            .iter()
            .find(|g| {
                matches!(g.role, oxidraft_cad::GripRole::Endpoint(_))
                    && (g.world.x - att.0).hypot(g.world.y - att.1) > 1.0
            })
            .expect("free endpoint grip");
        a.begin_grip_drag(line_id, grip);
        a.apply_grip_drag((-2.0, 6.0));
        a.end_grip_drag();
        let l = line_of(&a, line_id);
        let arc = a
            .document
            .iter()
            .find_map(|e| match &e.kind {
                EntityKind::Curve(Curve::Arc(arc)) => Some(*arc),
                _ => None,
            })
            .expect("arc still present");
        let (ux, uy) = (l.p1.x - l.p0.x, l.p1.y - l.p0.y);
        let d = (ux * (arc.center.y - l.p0.y) - uy * (arc.center.x - l.p0.x)) / ux.hypot(uy);
        assert!(
            (d.abs() - arc.radius).abs() < 1e-6,
            "arc re-solved tangent to the dragged line: gap {}",
            d.abs() - arc.radius
        );
    }

    #[test]
    fn typed_near_tangent_end_is_not_rotated() {
        let mut a = app();
        a.prefs.snap_on = true;
        a.prefs.polar_on = false;
        a.prefs.track_on = false;
        let (_, att, t) = quarter_arc(&mut a);
        a.run_command("LINE");
        let (sx, sy) = a.view.world_to_screen(att.0, att.1);
        a.canvas_click(sx, sy);
        let typed = (
            att.0 + 6.0 * t.0 - 0.04 * t.1,
            att.1 + 6.0 * t.1 + 0.04 * t.0,
        );
        a.place_tool_point(Point2d::from_f64(typed.0, typed.1));
        let line_id = *a.document.order.last().unwrap();
        let l = line_of(&a, line_id);
        assert!(
            (l.p1.x - typed.0).abs() < 1e-12 && (l.p1.y - typed.1).abs() < 1e-12,
            "typed end untouched: {l:?}"
        );
        assert!(
            !a.document
                .constraints
                .iter()
                .any(|c| c.kind == oxidraft_document::ConstraintKind::Tangent),
            "no tangent on a deliberately off-tangent typed line"
        );
    }

    #[test]
    fn typed_exact_tangent_records_without_moving() {
        let mut a = app();
        a.prefs.snap_on = true;
        a.prefs.infer_constraints = true;
        a.prefs.polar_on = false;
        a.prefs.track_on = false;
        let (arc_id, att, t) = quarter_arc(&mut a);
        a.run_command("LINE");
        let (sx, sy) = a.view.world_to_screen(att.0, att.1);
        a.canvas_click(sx, sy);
        let typed = (att.0 + 6.0 * t.0, att.1 + 6.0 * t.1);
        a.place_tool_point(Point2d::from_f64(typed.0, typed.1));
        let line_id = *a.document.order.last().unwrap();
        let l = line_of(&a, line_id);
        assert!(
            (l.p1.x - typed.0).abs() < 1e-12 && (l.p1.y - typed.1).abs() < 1e-12,
            "exact end untouched: {l:?}"
        );
        assert!(
            a.document.constraints.iter().any(|c| {
                c.kind == oxidraft_document::ConstraintKind::Tangent
                    && (c.a, c.b) == (arc_id, Some(line_id))
            }),
            "exact tangency recorded on a pinned end"
        );
    }

    #[test]
    fn line_drawn_into_an_arc_endpoint_rotates_its_free_start() {
        let mut a = app();
        a.prefs.snap_on = true;
        a.prefs.infer_constraints = true;
        a.prefs.polar_on = false;
        a.prefs.track_on = false;
        let (arc_id, att, t) = quarter_arc(&mut a);
        a.run_command("LINE");
        let start = (
            att.0 + 6.0 * t.0 - 0.04 * t.1,
            att.1 + 6.0 * t.1 + 0.04 * t.0,
        );
        let (sx, sy) = a.view.world_to_screen(start.0, start.1);
        a.canvas_click(sx, sy);
        let (sx2, sy2) = a.view.world_to_screen(att.0, att.1);
        a.canvas_click(sx2, sy2);
        let line_id = *a.document.order.last().unwrap();
        let l = line_of(&a, line_id);
        assert!(
            (l.p1.x - att.0).abs() < 1e-12 && (l.p1.y - att.1).abs() < 1e-12,
            "snapped end stays on the arc endpoint: {l:?}"
        );
        let cross = t.0 * (l.p1.y - l.p0.y) - t.1 * (l.p1.x - l.p0.x);
        assert!(
            cross.abs() < 1e-9,
            "free start rotated onto the ray: {cross}"
        );
        assert!(
            a.document.constraints.iter().any(|c| {
                c.kind == oxidraft_document::ConstraintKind::Tangent
                    && (c.a, c.b) == (arc_id, Some(line_id))
            }),
            "tangent constraint recorded"
        );
    }

    #[test]
    fn arc_drawn_off_a_line_endpoint_pulls_tangent_and_welds() {
        let mut a = app();
        a.prefs.snap_on = false;
        a.prefs.infer_constraints = true;
        let line_id = a.add_entity(line(0, 0, -5, 0));
        let (cx, cy) = (0.04_f64, 5.0_f64);
        let r = (cx * cx + cy * cy).sqrt();
        let start_angle = (0.0 - cy).atan2(0.0 - cx);
        let arc =
            oxidraft_geometry::CircularArc::new(Point2d::from_f64(cx, cy), r, start_angle, 0.0);
        let arc_id = a.add_entity(EntityKind::Curve(Curve::Arc(arc)));
        a.infer_arc_onset_tangency(arc_id);
        assert!(
            a.document.constraints.iter().any(|c| {
                c.kind == oxidraft_document::ConstraintKind::Tangent
                    && (c.a, c.b) == (arc_id, Some(line_id))
            }),
            "tangent inferred between the drawn arc and the line"
        );
        assert!(
            a.document.constraints.iter().any(|c| {
                c.kind == oxidraft_document::ConstraintKind::Coincident
                    && c.a == arc_id
                    && c.b == Some(line_id)
            }),
            "shared corner welded coincident"
        );
        let solved = a
            .document
            .get(arc_id)
            .and_then(|e| match &e.kind {
                EntityKind::Curve(Curve::Arc(arc)) => Some(*arc),
                _ => None,
            })
            .expect("arc still present");
        let l = line_of(&a, line_id);
        let (ux, uy) = (l.p1.x - l.p0.x, l.p1.y - l.p0.y);
        let d = (ux * (solved.center.y - l.p0.y) - uy * (solved.center.x - l.p0.x)).abs()
            / ux.hypot(uy);
        assert!(
            (d - solved.radius).abs() < 1e-6,
            "arc pulled exactly tangent to the line: gap {}",
            d - solved.radius
        );
    }

    #[test]
    fn arc_tool_off_a_line_endpoint_infers_tangency() {
        let mut a = app();
        a.prefs.snap_on = false;
        a.prefs.infer_constraints = true;
        a.run_command("LINE");
        a.place_tool_point(pt(-5, 0));
        a.place_tool_point(pt(0, 0));
        let line_id = *a.document.order.last().unwrap();
        a.run_command("ARC");
        a.place_tool_point(pt(0, 0));
        a.place_tool_point(Point2d::from_f64(3.5355339, 1.4644661));
        a.place_tool_point(pt(5, 5));
        let arc_id = *a.document.order.last().unwrap();
        assert!(
            matches!(
                a.document.get(arc_id).map(|e| &e.kind),
                Some(EntityKind::Curve(Curve::Arc(_)))
            ),
            "the three points made an arc"
        );
        assert!(
            a.document.constraints.iter().any(|c| {
                c.kind == oxidraft_document::ConstraintKind::Tangent
                    && (c.a, c.b) == (arc_id, Some(line_id))
            }),
            "arc-onset tangency inferred through the Arc tool"
        );
    }

    #[test]
    fn arc_onset_tangency_respects_the_infer_toggle() {
        let mut a = app();
        a.prefs.snap_on = false;
        a.prefs.infer_constraints = false;
        let _line_id = a.add_entity(line(0, 0, -5, 0));
        let arc = oxidraft_geometry::CircularArc::new(
            Point2d::from_f64(0.0, 5.0),
            5.0,
            -std::f64::consts::FRAC_PI_2,
            0.0,
        );
        let arc_id = a.add_entity(EntityKind::Curve(Curve::Arc(arc)));
        a.infer_arc_onset_tangency(arc_id);
        assert!(
            user_constraints(&a).is_empty(),
            "auto-constrain off: no tangency inferred"
        );
    }

    #[test]
    fn radcon_command_drives_the_selected_circle() {
        let mut a = app();
        a.prefs.snap_on = false;
        let c = a.add_entity(EntityKind::Curve(Curve::Arc(
            oxidraft_geometry::CircularArc::new(pt(0, 0), 2.0, 0.0, std::f64::consts::TAU),
        )));
        a.selection = vec![c];
        a.run_command("RADCON 3.5");
        let radius_of = |a: &AppState| match a.document.get(c).unwrap().as_curve().unwrap() {
            Curve::Arc(arc) => arc.radius,
            other => panic!("expected arc, got {other:?}"),
        };
        assert!((radius_of(&a) - 3.5).abs() < 1e-7, "circle resized");
        let uc = user_constraints(&a);
        assert_eq!(uc.len(), 1);
        assert_eq!(uc[0].val, Some(3.5));
        a.undo();
        assert!((radius_of(&a) - 2.0).abs() < 1e-9, "undo restores the size");
        assert!(user_constraints(&a).is_empty(), "and drops the record");
    }

    #[test]
    fn inference_respects_the_toggle() {
        let mut a = app();
        a.prefs.snap_on = false;
        a.prefs.infer_constraints = false;
        a.run_command("LINE");
        for (x, y) in [(0.0, 0.0), (8.0, 0.0), (8.0, 6.0)] {
            let (sx, sy) = a.view.world_to_screen(x, y);
            a.canvas_click(sx, sy);
        }
        assert!(user_constraints(&a).is_empty(), "toggle off, no welds");
    }

    #[test]
    fn moving_a_welded_line_drags_its_neighbour() {
        let mut a = app();
        a.prefs.snap_on = false;
        a.prefs.infer_constraints = true;
        a.run_command("LINE");
        for (x, y) in [(0.0, 0.0), (8.0, 0.0), (8.0, 6.0)] {
            let (sx, sy) = a.view.world_to_screen(x, y);
            a.canvas_click(sx, sy);
        }
        let l1 = a.document.order[1];
        let l2 = a.document.order[2];
        a.run_command("");
        a.selection = vec![l1];
        let t = oxidraft_geometry::Transform2d::translation(2.0, 1.0);
        a.apply_tool_event(ToolEvent::Transform { ids: vec![l1], t });
        let s1 = line_of(&a, l1);
        let s2 = line_of(&a, l2);
        assert!(
            (s1.p1.x - 10.0).abs() < 1e-6 && (s1.p1.y - 1.0).abs() < 1e-6,
            "l1 moved: {s1:?}"
        );
        assert!(
            (s2.p0.x - 10.0).abs() < 1e-6 && (s2.p0.y - 1.0).abs() < 1e-6,
            "welded neighbour reattached: {s2:?}"
        );
    }

    #[test]
    fn divide_and_measure_place_points_on_the_selection() {
        let mut a = app();
        a.run_command("LINE");
        a.canvas_click(400.0, 300.0);
        a.canvas_click(650.0, 300.0);
        a.run_command("");
        let line_id = *a.document.order.last().unwrap();
        a.selection = vec![line_id];
        let before = a.document.len();
        a.run_command("DIVIDE 5");
        assert_eq!(a.document.len(), before + 4, "4 division points");
        a.execute(Command::Undo);
        assert_eq!(a.document.len(), before, "divide is one undo step");
        a.selection = vec![line_id];
        a.run_command("MEASURE 2");
        assert_eq!(a.document.len(), before + 2, "points at 2 and 4");
        let depth = a.history.undo_depth();
        a.run_command("DIVIDE 1");
        a.run_command("MEASURE 0");
        a.selection.clear();
        a.run_command("DIVIDE 4");
        assert_eq!(
            a.history.undo_depth(),
            depth,
            "declined commands snapshot nothing"
        );
    }

    #[test]
    fn fillet_with_non_positive_radius_does_not_leave_a_phantom_undo_entry() {
        let mut a = app();
        a.run_command("LINE");
        a.canvas_click(400.0, 300.0);
        a.canvas_click(500.0, 300.0);
        a.run_command("");
        a.run_command("LINE");
        a.canvas_click(500.0, 300.0);
        a.canvas_click(500.0, 400.0);
        a.run_command("");
        let before = a.document.len();
        let depth = a.history.undo_depth();

        a.run_command("FILLET 0");
        a.canvas_click(450.0, 300.0);
        a.canvas_click(500.0, 350.0);

        assert_eq!(
            a.document.len(),
            before,
            "a non-positive radius fillets nothing"
        );
        assert_eq!(
            a.history.undo_depth(),
            depth,
            "a rejected fillet must not leave a phantom undo entry"
        );
        assert!(
            a.command_log.last().is_some_and(Note::is_problem),
            "a rejected fillet must say so, got {:?}",
            a.command_log.last()
        );
    }

    #[test]
    fn chamfer_of_parallel_lines_does_not_leave_a_phantom_undo_entry() {
        let mut a = app();
        // Parallel lines have no corner to chamfer — solve_chamfer must
        // decline regardless of the requested distance.
        a.run_command("LINE");
        a.canvas_click(400.0, 300.0);
        a.canvas_click(500.0, 300.0);
        a.run_command("");
        a.run_command("LINE");
        a.canvas_click(400.0, 350.0);
        a.canvas_click(500.0, 350.0);
        a.run_command("");
        let before = a.document.len();
        let depth = a.history.undo_depth();

        a.run_command("CHAMFER 3");
        a.canvas_click(450.0, 300.0);
        a.canvas_click(450.0, 350.0);

        assert_eq!(
            a.document.len(),
            before,
            "parallel lines have no corner to chamfer"
        );
        assert_eq!(
            a.history.undo_depth(),
            depth,
            "a rejected chamfer must not leave a phantom undo entry"
        );
        assert!(
            a.command_log.last().is_some_and(Note::is_problem),
            "a rejected chamfer must say so, got {:?}",
            a.command_log.last()
        );
    }

    #[test]
    fn extend_with_nothing_ahead_does_not_leave_a_phantom_undo_entry() {
        let mut a = app();
        // A short line with nothing beyond either end: no boundary to reach.
        a.run_command("LINE");
        a.canvas_click(400.0, 300.0);
        a.canvas_click(500.0, 300.0);
        a.run_command("");
        let before = a.document.len();
        let depth = a.history.undo_depth();

        a.run_command("EXTEND");
        a.canvas_click(500.0, 300.0);

        assert_eq!(a.document.len(), before, "nothing was there to extend to");
        assert_eq!(
            a.history.undo_depth(),
            depth,
            "a rejected extend must not leave a phantom undo entry"
        );
        assert!(
            a.command_log.last().is_some_and(Note::is_problem),
            "a rejected extend must say so, got {:?}",
            a.command_log.last()
        );
    }

    #[test]
    fn join_of_untouching_curves_does_not_leave_a_phantom_undo_entry() {
        let mut a = app();
        let l1 = a.add_entity(line(0, 0, 10, 0));
        let l2 = a.add_entity(line(0, 100, 10, 100));
        a.selection = vec![l1, l2];
        let before = a.document.len();
        let depth = a.history.undo_depth();

        a.join_selection();

        assert_eq!(a.document.len(), before, "the lines do not touch");
        assert_eq!(
            a.history.undo_depth(),
            depth,
            "a no-op join must not leave a phantom undo entry"
        );
        assert!(
            a.command_log.last().is_some_and(Note::is_problem),
            "a no-op join must say so, got {:?}",
            a.command_log.last()
        );
    }

    #[test]
    fn command_log_is_capped_and_keeps_the_newest_entries() {
        let mut a = app();
        for i in 0..1000 {
            a.note(format!("entry {i}"));
            a.trim_command_log();
        }
        assert!(
            a.command_log.len() <= 600,
            "the log must stay bounded: {}",
            a.command_log.len()
        );
        assert_eq!(
            a.command_log.last().map(Note::text),
            Some("entry 999"),
            "trimming drops the oldest entries, never the newest"
        );
    }

    #[test]
    fn a_trim_sweep_cuts_every_edge_it_crosses() {
        // Power trim: one stroke across a ladder cuts every rung it passes
        // through, instead of picking them off a click at a time.
        let mut a = app();
        a.add_entity(line(0, -5, 0, 25));
        a.add_entity(line(10, -5, 10, 25));
        let rungs: Vec<EntityId> = (0..3)
            .map(|i| a.add_entity(line(-5, i * 10, 15, i * 10)))
            .collect();

        a.tool = Tool::Trim;
        let mut cut = 0;
        for step in 0..20 {
            if a.trim_across((5.0, f64::from(step)), (5.0, f64::from(step + 1))) {
                cut += 1;
            }
        }
        assert!(
            cut >= 3,
            "the stroke should have cut all three rungs, cut {cut}"
        );
        for &r in &rungs {
            let spans_the_rails = matches!(
                a.document.get(r).and_then(|e| e.as_curve()),
                Some(Curve::Line(l)) if l.p0.to_f64().0 <= -5.0 && l.p1.to_f64().0 >= 15.0
            );
            assert!(
                !spans_the_rails,
                "rung {r:?} came through the stroke untouched"
            );
        }
    }

    #[test]
    fn a_trim_sweep_along_an_edge_leaves_it_alone() {
        // Proximity is not crossing. Asking what lay *under* each sample meant
        // a stroke run down the length of a rung kept finding it and kept
        // eating it a span at a time, until the whole rung was gone. A stroke
        // that runs alongside geometry never crosses it.
        let mut a = app();
        a.add_entity(line(0, -5, 0, 25));
        a.add_entity(line(10, -5, 10, 25));
        let rung = a.add_entity(line(-5, 10, 15, 10));

        a.tool = Tool::Trim;
        for step in -5..15 {
            a.trim_across((f64::from(step), 10.0), (f64::from(step + 1), 10.0));
        }
        // The rails are a different matter — a stroke running along the rung
        // does cross both of them, and cutting those is exactly right. What
        // has to survive is the edge the stroke was travelling on.
        let survived = matches!(
            a.document.get(rung).and_then(|e| e.as_curve()),
            Some(Curve::Line(l)) if l.p0.to_f64().0 <= -5.0 && l.p1.to_f64().0 >= 15.0
        );
        assert!(
            survived,
            "sweeping along the rung ate it: {:?}",
            a.document.get(rung).and_then(|e| e.as_curve())
        );
    }

    #[test]
    fn a_trim_sweep_over_empty_space_cuts_nothing() {
        // `trim_across` reporting false is what tells the caller to drop the
        // snapshot, so a stroke through thin air leaves no undo step.
        let mut a = app();
        a.add_entity(line(0, 0, 10, 0));
        a.tool = Tool::Trim;
        for step in 0..10 {
            assert!(
                !a.trim_across((f64::from(step), 50.0), (f64::from(step + 1), 50.0)),
                "nothing is up there to cut"
            );
        }
    }

    #[test]
    fn success_and_failure_are_told_apart() {
        // Every message used to render in the same red alert frame, so a
        // finished export looked exactly like a failed one. The toast picks
        // its frame from this, so the levels have to be right.
        let mut a = app();
        a.note("Plotted to PDF".into());
        assert!(
            !a.command_log.last().expect("logged").is_problem(),
            "something that worked must not be dressed as a failure"
        );
        a.problem("Couldn't plot to PDF — disk full".into());
        assert!(
            a.command_log.last().expect("logged").is_problem(),
            "a failure must read as one"
        );
    }

    #[test]
    fn an_unrecognised_command_says_so() {
        // The raw text is logged before dispatch, so without this the toast
        // shows exactly what was typed and nothing happens — which reads as
        // confirmation. Typing FILLETT looked like a fillet had been applied.
        let mut a = app();
        a.run_command("FILLETT");
        let last = a.command_log.last().expect("something was logged");
        assert!(
            last.text().contains("isn't a command"),
            "an unknown command must say so, got {last:?}"
        );
        assert!(
            last.text().contains("FILLETT"),
            "and it should quote what was typed, got {last:?}"
        );
        assert!(
            last.is_problem(),
            "an unrecognised command is a refusal, not news: {last:?}"
        );
    }

    #[test]
    fn a_real_command_does_not_get_the_unknown_message() {
        let mut a = app();
        a.run_command("LINE");
        let last = a.command_log.last().expect("something was logged");
        assert!(
            !last.text().contains("isn't a command"),
            "LINE is a command, got {last:?}"
        );
    }

    #[test]
    fn trim_no_op_does_not_leave_a_phantom_undo_entry() {
        let mut a = app();
        a.run_command("LINE");
        a.canvas_click(400.0, 300.0);
        a.canvas_click(500.0, 300.0);
        a.run_command("");
        let before = a.document.len();
        let depth = a.history.undo_depth();

        // A lone line has no cutters to trim against, so picking it is a no-op.
        a.run_command("TRIM");
        a.canvas_click(450.0, 300.0);

        assert_eq!(
            a.document.len(),
            before,
            "trim with no cutters removes nothing"
        );
        assert_eq!(
            a.history.undo_depth(),
            depth,
            "a no-op trim must not leave a phantom undo entry"
        );
        let last = a.command_log.last().expect("something was logged");
        assert!(last.is_problem(), "a no-op trim must say so, got {last:?}");
        assert!(
            last.text().contains("trim"),
            "and say what the click was for, got {last:?}"
        );
    }

    #[test]
    fn exploding_a_non_polyline_does_not_leave_a_phantom_undo_entry() {
        let mut a = app();
        let id = a.add_entity(line(0, 0, 10, 0));
        a.selection = vec![id];
        let before = a.document.len();
        let depth = a.history.undo_depth();

        a.explode_selection();

        assert_eq!(
            a.document.len(),
            before,
            "a plain line has nothing to explode"
        );
        assert_eq!(
            a.history.undo_depth(),
            depth,
            "a no-op explode must not leave a phantom undo entry"
        );
        let last = a.command_log.last().expect("something was logged");
        assert!(
            last.is_problem(),
            "a no-op explode must say so, got {last:?}"
        );
    }

    #[test]
    fn outline_text_of_blank_content_does_not_leave_a_phantom_undo_entry() {
        let mut a = app();
        let id = a.add_entity(EntityKind::Text {
            anchor: pt(0, 0),
            content: "   ".into(),
            height: 2.5,
            rotation: 0.0,
            font: None,
        });
        a.selection = vec![id];
        let before = a.document.len();
        let depth = a.history.undo_depth();

        a.outline_text_selection();

        assert_eq!(a.document.len(), before, "blank text has no glyph outlines");
        assert_eq!(
            a.history.undo_depth(),
            depth,
            "a no-op outline must not leave a phantom undo entry"
        );
        let last = a.command_log.last().expect("something was logged");
        assert!(
            last.is_problem(),
            "a no-op outline must say so, got {last:?}"
        );
    }

    #[test]
    fn stretch_missing_the_selection_does_not_leave_a_phantom_undo_entry() {
        let mut a = app();
        a.run_command("LINE");
        a.canvas_click(400.0, 300.0);
        a.canvas_click(500.0, 300.0);
        a.run_command("");
        let line_id = *a.document.order.last().unwrap();
        a.selection = vec![line_id];
        let before = line_of(&a, line_id);
        let depth = a.history.undo_depth();

        // A crossing window nowhere near the line's endpoints (c1, c2), then
        // a base point and a non-zero drag (base, final): STRETCH must
        // decline rather than record a no-op edit.
        a.run_command("STRETCH");
        a.canvas_click(10.0, 10.0);
        a.canvas_click(30.0, 30.0);
        a.canvas_click(50.0, 50.0);
        a.canvas_click(60.0, 60.0);

        assert_eq!(
            (line_of(&a, line_id).p0, line_of(&a, line_id).p1),
            (before.p0, before.p1),
            "the window enclosed no endpoint"
        );
        assert_eq!(
            a.history.undo_depth(),
            depth,
            "a no-op stretch must not leave a phantom undo entry"
        );
    }

    #[test]
    fn grip_drag_with_no_movement_does_not_leave_a_phantom_undo_entry() {
        let mut a = app();
        let id = a.add_entity(line(0, 0, 10, 0));
        let grip = oxidraft_cad::grips_for(&a.document.get(id).unwrap().kind)[0];
        let depth = a.history.undo_depth();

        a.begin_grip_drag(id, grip);
        a.end_grip_drag();

        assert_eq!(
            a.history.undo_depth(),
            depth,
            "picking up and releasing a grip with no movement must not leave a phantom undo entry"
        );
    }

    #[test]
    fn grip_drag_that_moves_records_one_undo_step() {
        let mut a = app();
        let id = a.add_entity(line(0, 0, 10, 0));
        let grip = oxidraft_cad::grips_for(&a.document.get(id).unwrap().kind)[0];
        let depth = a.history.undo_depth();

        a.begin_grip_drag(id, grip);
        a.apply_grip_drag((3.0, 4.0));
        a.end_grip_drag();

        assert_eq!(
            a.history.undo_depth(),
            depth + 1,
            "a real drag must still record its undo step"
        );
    }

    #[test]
    fn dragging_a_fixed_point_does_not_move_it() {
        let mut a = app();
        let id = a.add_entity(EntityKind::Point(Point2d::from_i64(0, 0)));
        a.selection = vec![id];
        a.fix_selection();
        assert!(
            a.document
                .constraints
                .iter()
                .any(|c| c.kind == oxidraft_document::ConstraintKind::Fixed && c.a == id),
            "Fix must have recorded the constraint"
        );

        let grip = oxidraft_cad::grips_for(&a.document.get(id).unwrap().kind)[0];
        a.begin_grip_drag(id, grip);
        a.apply_grip_drag((9.0, 9.0));
        a.end_grip_drag();

        let EntityKind::Point(p) = a.document.get(id).unwrap().kind else {
            panic!("expected a point");
        };
        assert_eq!(
            p.to_f64(),
            (0.0, 0.0),
            "a fixed point must not move when dragged"
        );
    }

    #[test]
    fn plot_window_pick_stores_the_rect_and_reopens_the_dialog() {
        let mut a = AppState::new(800.0, 600.0);
        a.prefs.snap_on = false;
        a.prefs.grid_snap_on = false;
        a.tool = Tool::PlotWindow { first: None };
        a.canvas_click(300.0, 350.0);
        a.canvas_click(500.0, 250.0);
        let (x0, y0, x1, y1) = a.plot_window.expect("window stored");
        assert!(
            (x0 + 2.0).abs() < 1e-9
                && (y0 + 1.0).abs() < 1e-9
                && (x1 - 2.0).abs() < 1e-9
                && (y1 - 1.0).abs() < 1e-9,
            "corners sorted: ({x0},{y0})..({x1},{y1})"
        );
        assert!(
            a.plot_dialog_open && a.plot_window_mode,
            "the dialog reopens in Window mode after the pick"
        );
        assert!(matches!(a.tool, Tool::Select));
        a.plot_dialog_open = false;
        a.plot_window = None;
        a.tool = Tool::PlotWindow { first: None };
        a.canvas_click(400.0, 300.0);
        a.canvas_click(400.0, 300.0);
        assert_eq!(a.plot_window, None, "no area, no window");
        assert!(a.plot_dialog_open && a.plot_window_mode);
    }

    #[test]
    fn perpendicular_snapping_triggers_anywhere_near_line() {
        let mut a = app();
        a.add_entity(EntityKind::Curve(Curve::Line(LineSeg::from_endpoints(
            pt(0, 0),
            pt(10, 0),
        ))));
        a.snap.enabled = vec![oxidraft_cad::SnapKind::Perpendicular];
        a.prefs.snap_on = true;
        a.run_command("LINE");
        let (s1x, s1y) = a.view.world_to_screen(5.0, 5.0);
        a.canvas_click(s1x, s1y);
        let (s2x, s2y) = a.view.world_to_screen(5.3, 0.1);
        a.pointer_moved(s2x, s2y);
        assert!(a.active_snap.is_some());
        let sp = a.active_snap.as_ref().unwrap();
        assert_eq!(sp.kind, oxidraft_cad::SnapKind::Perpendicular);
        assert!((sp.pos.0 - 5.0).abs() < 1e-4);
        assert!(sp.pos.1.abs() < 1e-4);
    }

    #[test]
    fn direct_distance_entry_projects_along_cursor() {
        let mut a = app();
        a.prefs.snap_on = false;
        a.run_command("LINE");
        let (s1x, s1y) = a.view.world_to_screen(0.0, 0.0);
        a.canvas_click(s1x, s1y);
        let (s2x, s2y) = a.view.world_to_screen(3.0, 4.0);
        a.pointer_moved(s2x, s2y);
        a.run_command("10.0");
        assert_eq!(a.document.len(), 2);
        let first = a.document.iter().find(|e| e.id != a.origin_id).unwrap();
        if let EntityKind::Curve(Curve::Line(l)) = &first.kind {
            assert!((l.p0.x - 0.0).abs() < 1e-4);
            assert!((l.p0.y - 0.0).abs() < 1e-4);
            assert!((l.p1.x - 6.0).abs() < 1e-4);
            assert!((l.p1.y - 8.0).abs() < 1e-4);
        } else {
            panic!("expected line");
        }
    }

    #[test]
    fn typed_coordinates_build_a_line() {
        let mut a = app();
        a.prefs.snap_on = false;
        a.run_command("LINE");
        a.run_command("0,0");
        a.run_command("@10,0");
        assert_eq!(a.document.len(), 2);
        let line = a.document.iter().find(|e| e.id != a.origin_id).unwrap();
        if let EntityKind::Curve(Curve::Line(l)) = &line.kind {
            assert!((l.p0.x).abs() < 1e-9 && (l.p0.y).abs() < 1e-9);
            assert!((l.p1.x - 10.0).abs() < 1e-9 && (l.p1.y).abs() < 1e-9);
        } else {
            panic!("expected line");
        }
    }

    #[test]
    fn relative_polar_coordinate_places_point() {
        let mut a = app();
        a.prefs.snap_on = false;
        a.run_command("LINE");
        a.run_command("0,0");
        a.run_command("@5<90");
        let line = a.document.iter().find(|e| e.id != a.origin_id).unwrap();
        if let EntityKind::Curve(Curve::Line(l)) = &line.kind {
            assert!((l.p1.x).abs() < 1e-6, "x should be ~0, got {}", l.p1.x);
            assert!(
                (l.p1.y - 5.0).abs() < 1e-6,
                "y should be ~5, got {}",
                l.p1.y
            );
        } else {
            panic!("expected line");
        }
    }

    #[test]
    fn right_click_repeat_reactivates_last_command() {
        let mut a = app();
        a.run_command("CIRCLE");
        assert!(matches!(a.tool, Tool::Circle { .. }));
        assert_eq!(a.last_command.as_deref(), Some("CIRCLE"));
        a.run_command("");
        assert!(matches!(a.tool, Tool::Select));
        a.repeat_last_command();
        assert!(matches!(a.tool, Tool::Circle { .. }));
    }

    #[test]
    fn polygon_command_allows_side_update() {
        let mut a = app();
        a.run_command("POLYGON");
        assert!(matches!(
            a.tool,
            Tool::Polygon {
                center: None,
                radius_point: None,
                sides: None
            }
        ));
        a.run_command("6");
        assert!(matches!(
            a.tool,
            Tool::Polygon {
                center: None,
                radius_point: None,
                sides: Some(6)
            }
        ));
        let (s1x, s1y) = a.view.world_to_screen(0.0, 0.0);
        a.canvas_click(s1x, s1y);
        let (s2x, s2y) = a.view.world_to_screen(10.0, 0.0);
        a.canvas_click(s2x, s2y);
        assert_eq!(a.document.len(), 1);
        assert!(matches!(
            a.tool,
            Tool::Polygon {
                center: Some(_),
                radius_point: Some(_),
                sides: Some(6)
            }
        ));
        a.confirm_pending_polygon();
        assert_eq!(a.document.len(), 7);
        let welds = a
            .document
            .constraints
            .iter()
            .filter(|c| c.kind == oxidraft_document::ConstraintKind::Coincident)
            .count();
        assert_eq!(welds, 6, "every hexagon corner is welded");
        assert!(matches!(
            a.tool,
            Tool::Polygon {
                center: None,
                radius_point: None,
                ..
            }
        ));
    }

    #[test]
    fn polygon_cancel_pending_drops_without_committing() {
        let mut a = app();
        a.run_command("POLYGON");
        let (s1x, s1y) = a.view.world_to_screen(0.0, 0.0);
        a.canvas_click(s1x, s1y);
        let (s2x, s2y) = a.view.world_to_screen(10.0, 0.0);
        a.canvas_click(s2x, s2y);
        assert_eq!(a.document.len(), 1);
        a.cancel_pending_polygon();
        assert_eq!(a.document.len(), 1, "cancel must not create anything");
        assert!(matches!(
            a.tool,
            Tool::Polygon {
                center: None,
                radius_point: None,
                sides: Some(6)
            }
        ));
    }

    #[test]
    fn polyline_command_commits_on_empty_command() {
        let mut a = app();
        a.run_command("PL");
        assert!(matches!(a.tool, Tool::Polyline { .. }));
        let (s1x, s1y) = a.view.world_to_screen(0.0, 0.0);
        a.canvas_click(s1x, s1y);
        let (s2x, s2y) = a.view.world_to_screen(5.0, 5.0);
        a.canvas_click(s2x, s2y);
        let (s3x, s3y) = a.view.world_to_screen(10.0, 0.0);
        a.canvas_click(s3x, s3y);
        a.run_command("");
        assert!(matches!(a.tool, Tool::Select));
        assert_eq!(a.document.len(), 3);
        let welds = a
            .document
            .constraints
            .iter()
            .filter(|c| c.kind == oxidraft_document::ConstraintKind::Coincident)
            .count();
        assert_eq!(welds, 1, "the open chain's shared corner is welded");
    }

    #[test]
    fn cv_spline_command_commits_to_nurbs() {
        let mut a = app();
        a.run_command("SPLINE");
        assert!(matches!(a.tool, Tool::Spline { .. }));
        for (wx, wy) in [(0.0, 0.0), (5.0, 8.0), (10.0, -4.0), (15.0, 0.0)] {
            let (sx, sy) = a.view.world_to_screen(wx, wy);
            a.canvas_click(sx, sy);
        }
        a.run_command("");
        assert!(matches!(a.tool, Tool::Select));
        assert_eq!(a.document.len(), 2);
        let entity = a.document.iter().find(|e| e.id != a.origin_id).unwrap();
        match &entity.kind {
            EntityKind::Curve(Curve::Nurbs(nc)) => assert_eq!(nc.control().len(), 4),
            other => panic!("expected a NURBS curve, got {:?}", other),
        }
    }

    #[test]
    fn nurbs_grip_edit_moves_control_and_weight() {
        let mut a = app();
        let nc = oxidraft_geometry::NurbsCurve::uniform(vec![
            Point2d::from_i64(0, 0),
            Point2d::from_i64(2, 4),
            Point2d::from_i64(6, 4),
            Point2d::from_i64(8, 0),
            Point2d::from_i64(10, 4),
        ]);
        let id = a.add_entity(EntityKind::Curve(Curve::Nurbs(nc)));
        a.selection = vec![id];
        let (sid, control, weights) = a.selected_nurbs().expect("a NURBS is selected");
        assert_eq!(sid, id);
        assert_eq!(control.len(), 5);
        assert!(weights.iter().all(|&w| w == 1.0));
        a.begin_edit();
        a.set_nurbs_control(id, 2, Point2d::from_f64(6.0, 9.0));
        let weight_at = |a: &AppState, i: usize| {
            if let EntityKind::Curve(Curve::Nurbs(nc)) = &a.document.get(id).unwrap().kind {
                (nc.control()[i], nc.weights()[i])
            } else {
                panic!("expected NURBS")
            }
        };
        assert_eq!(weight_at(&a, 2).0, Point2d::from_f64(6.0, 9.0));
        assert!(a.adjust_nurbs_weight(id, 2, 5.0));
        assert!((weight_at(&a, 2).1 - 5.0).abs() < 1e-9);
        a.adjust_nurbs_weight(id, 2, 100.0);
        assert!(weight_at(&a, 2).1 <= 20.0 + 1e-9);
        a.undo();
        assert!(
            (weight_at(&a, 2).1 - 5.0).abs() < 1e-9,
            "undo restores the prior weight"
        );

        // A rejected edit must report failure and leave the undo stack alone.
        // `f64::clamp` propagates NaN, so a NaN factor produced a NaN weight
        // the setter silently refused while this still returned true — an
        // undo entry that restores nothing.
        let before = weight_at(&a, 2).1;
        let depth = a.history.undo_depth();
        assert!(
            !a.adjust_nurbs_weight(id, 2, f64::NAN),
            "a NaN factor must report failure"
        );
        assert!(
            (weight_at(&a, 2).1 - before).abs() < 1e-12,
            "weight unchanged"
        );
        assert_eq!(a.history.undo_depth(), depth, "no phantom undo entry");
        assert!(
            !a.adjust_nurbs_weight(id, 99, 2.0),
            "out-of-range index must report failure"
        );
        assert_eq!(a.history.undo_depth(), depth, "still no phantom undo entry");
    }

    #[test]
    fn polyline_command_closes_on_c_command() {
        let mut a = app();
        a.run_command("PL");
        let (s1x, s1y) = a.view.world_to_screen(0.0, 0.0);
        a.canvas_click(s1x, s1y);
        let (s2x, s2y) = a.view.world_to_screen(5.0, 5.0);
        a.canvas_click(s2x, s2y);
        let (s3x, s3y) = a.view.world_to_screen(10.0, 0.0);
        a.canvas_click(s3x, s3y);
        a.run_command("c");
        assert!(matches!(a.tool, Tool::Select));
        assert_eq!(a.document.len(), 4);
        let lines = a
            .document
            .iter()
            .filter(|e| matches!(e.kind, EntityKind::Curve(Curve::Line(_))))
            .count();
        assert_eq!(lines, 3);
        let welds = a
            .document
            .constraints
            .iter()
            .filter(|c| c.kind == oxidraft_document::ConstraintKind::Coincident)
            .count();
        assert_eq!(welds, 3, "closed chain welds all three corners");
    }

    #[test]
    fn fixed_origin_test() {
        let mut a = app();
        if let Some(EntityKind::Point(p)) = a.document.get(a.origin_id).map(|e| &e.kind) {
            assert_eq!(p.to_f64(), (0.0, 0.0));
        } else {
            panic!("expected origin point");
        }
        a.toggle_selection(a.origin_id);
        assert!(!a.selection.contains(&a.origin_id));
        a.selection = vec![a.origin_id];
        a.erase_selection();
        assert!(a.document.get(a.origin_id).is_some());
        let t = oxidraft_geometry::Transform2d::translation(10.0, 10.0);
        let ev = ToolEvent::Transform {
            ids: vec![a.origin_id],
            t,
        };
        a.apply_tool_event(ev);
        if let Some(EntityKind::Point(p)) = a.document.get(a.origin_id).map(|e| &e.kind) {
            assert_eq!(p.to_f64(), (0.0, 0.0));
        } else {
            panic!("expected origin point");
        }
    }

    #[test]
    fn text_tool_places_text_entity() {
        let mut a = app();
        a.run_command("TEXT");
        assert!(matches!(a.tool, Tool::Text { anchor: None, .. }));
        let (sx, sy) = a.view.world_to_screen(2.0, 3.0);
        a.canvas_click(sx, sy);
        assert!(matches!(
            a.tool,
            Tool::Text {
                anchor: Some(_),
                ..
            }
        ));
        a.run_command("Hello\\nWorld");
        assert!(matches!(a.tool, Tool::Select));
        let content = a
            .document
            .iter()
            .find_map(|e| match &e.kind {
                EntityKind::Text { content, .. } => Some(content.clone()),
                _ => None,
            })
            .expect("a Text entity should be created");
        assert_eq!(
            content, "Hello\nWorld",
            "single unified tool handles multi-line via \\n"
        );
    }

    #[test]
    fn reconstrain_tangency_tolerates_deleted_target() {
        use oxidraft_document::TangentRef;
        use oxidraft_geometry::CircularArc;
        let mut a = app();
        let t1 = a.document.add(line(0, 0, 10, 0));
        let t2 = a.document.add(line(0, 10, 10, 10));
        let arc = a
            .document
            .add(EntityKind::Curve(Curve::Arc(CircularArc::new(
                pt(5, 5),
                1.0,
                0.0,
                std::f64::consts::TAU,
            ))));
        if let Some(e) = a.document.get_mut(arc) {
            e.tangents = vec![
                TangentRef {
                    target: t1,
                    near: pt(5, 0),
                },
                TangentRef {
                    target: t2,
                    near: pt(5, 10),
                },
            ];
        }
        a.document.remove(t2);
        a.reconstrain_tangency(arc);
        assert!(matches!(
            a.document.get(arc).and_then(|e| e.as_curve()),
            Some(Curve::Arc(_))
        ));
    }

    #[test]
    fn smart_dimension_reads_a_centre_snap_as_an_anchor() {
        // The whole point of the feature: clicking a circle's centre must
        // mean "this point", while clicking its rim still means "this
        // circle's radius". `weld_anchor_at` is what separates them -- the
        // same helper Weld and ConPick already classify picks with.
        let mut a = app();
        let c = a.add_entity(EntityKind::Curve(Curve::Arc(
            oxidraft_geometry::CircularArc::new(
                Point2d::from_f64(0.0, 0.0),
                5.0,
                0.0,
                std::f64::consts::TAU,
            ),
        )));
        a.tool = crate::tools::Tool::DimConstraint {
            first: None,
            pending: None,
        };

        let (sx, sy) = a.view.world_to_screen(0.0, 0.0);
        a.canvas_click(sx, sy);
        assert!(
            matches!(
                a.tool,
                crate::tools::Tool::DimConstraint {
                    first: Some(crate::tools::DimTarget::Anchor(id, _, _)),
                    ..
                } if id == c
            ),
            "a centre click must bank an Anchor, got {:?}",
            a.tool
        );
    }

    #[test]
    fn smart_dimension_still_reads_a_rim_click_as_the_whole_circle() {
        // Guards the existing behaviour the new pick model must not break.
        let mut a = app();
        let c = a.add_entity(EntityKind::Curve(Curve::Arc(
            oxidraft_geometry::CircularArc::new(
                Point2d::from_f64(0.0, 0.0),
                5.0,
                0.0,
                std::f64::consts::TAU,
            ),
        )));
        a.tool = crate::tools::Tool::DimConstraint {
            first: None,
            pending: None,
        };

        let (sx, sy) = a.view.world_to_screen(5.0, 0.0);
        a.canvas_click(sx, sy);
        assert!(
            matches!(
                a.tool,
                crate::tools::Tool::DimConstraint {
                    pending: Some((crate::tools::DimTarget::Entity(id), None)),
                    ..
                } if id == c
            ),
            "a rim click must still mean the whole circle, got {:?}",
            a.tool
        );
    }

    #[test]
    fn a_held_arc_anchor_still_places_its_radius_rather_than_pairing_with_a_line() {
        // Pairing is a line-with-line gesture — the only pair `smart_dimension`
        // can record. `first` could only ever hold a line before, so that went
        // without saying; it can hold a point anchor now, and an arc endpoint
        // banked there must still fall through to the arc's own radius, which
        // is what the same two clicks produced before the pick model changed.
        let mut a = app();
        let arc = a.add_entity(EntityKind::Curve(Curve::Arc(
            oxidraft_geometry::CircularArc::new(
                Point2d::from_f64(0.0, 0.0),
                5.0,
                0.0,
                std::f64::consts::FRAC_PI_2,
            ),
        )));
        a.add_entity(EntityKind::Curve(Curve::Line(LineSeg::from_endpoints(
            Point2d::from_f64(10.0, -5.0),
            Point2d::from_f64(10.0, 5.0),
        ))));
        a.tool = crate::tools::Tool::DimConstraint {
            first: None,
            pending: None,
        };

        // The arc's start endpoint: a point on the arc, so it banks as one.
        let (sx, sy) = a.view.world_to_screen(5.0, 0.0);
        a.canvas_click(sx, sy);
        assert!(
            matches!(
                a.tool,
                crate::tools::Tool::DimConstraint {
                    first: Some(crate::tools::DimTarget::Anchor(id, _, _)),
                    ..
                } if id == arc
            ),
            "an arc endpoint banks as an anchor, got {:?}",
            a.tool
        );

        // A line's body, clear of its own anchors — nothing to pair with.
        let (lx, ly) = a.view.world_to_screen(10.0, 2.0);
        a.canvas_click(lx, ly);
        assert!(
            a.document
                .constraints
                .iter()
                .any(|c| c.kind == ConstraintKind::Radius && c.a == arc),
            "the second click placed the arc's radius: {:?}",
            a.document.constraints
        );
        assert!(
            !a.document
                .constraints
                .iter()
                .any(|c| c.kind == ConstraintKind::Angle),
            "an arc and a line are not an angle this tool can record: {:?}",
            a.document.constraints
        );
    }
}
