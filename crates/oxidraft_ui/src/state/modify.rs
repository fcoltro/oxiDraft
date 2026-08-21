//! Click handling for the modify (edit) tools: dispatches a canvas click to
//! the active tool's next step (move/copy/rotate/scale/mirror/trim/extend/
//! fillet/chamfer/blend/stretch/…), building up multi-click interactions and
//! committing them to the document through [`AppState`]'s undo history.

use super::AppState;
use crate::tools::{DimTarget, Tool};
use oxidraft_cad::pick_at;
use oxidraft_document::{ANCHOR_DERIVED, EntityId, EntityKind};
use oxidraft_geometry::{Curve, Point2d};

impl AppState {
    pub(crate) fn handle_modify_click(&mut self, p: &Point2d) -> bool {
        use oxidraft_cad::edit;
        let px = p.x;
        let py = p.y;
        let tol = self.view.pixel_world_size() * 6.0;
        let pick = |s: &Self| pick_at(&s.document, px, py, tol).filter(|&id| id != s.origin_id);

        match self.tool.clone() {
            Tool::Trim => {
                if let Some(id) = pick(self) {
                    self.history.snapshot(&self.document);
                    let cutters: Vec<EntityId> = self
                        .document
                        .iter()
                        .map(|e| e.id)
                        .filter(|&i| i != id && i != self.origin_id)
                        .collect();
                    // edit::trim returns `vec![id]` unchanged exactly when the
                    // pick found no valid span to remove (TrimOutcome::NoOp);
                    // every other outcome removes or replaces `id`.
                    if edit::trim(&mut self.document, id, &cutters, px, py) == vec![id] {
                        self.history.discard_last();
                        self.problem(
                            "Nothing crosses there to trim against — the piece you click \
                             has to be bounded by something."
                                .into(),
                        );
                    }
                    self.selection.clear();
                }
                true
            }
            Tool::Hatch => {
                self.hatch_at_point(px, py);
                true
            }
            Tool::Extend => {
                if let Some(id) = pick(self) {
                    let boundaries: Vec<EntityId> = self
                        .document
                        .iter()
                        .map(|e| e.id)
                        .filter(|&i| i != id && i != self.origin_id)
                        .collect();
                    self.history.snapshot(&self.document);
                    if !edit::extend(&mut self.document, id, &boundaries, px, py) {
                        self.history.discard_last();
                        self.problem(
                            "Nothing to extend to in that direction — there has to be an \
                             edge ahead of the end you pick."
                                .into(),
                        );
                    }
                }
                true
            }
            Tool::Offset { dist, source } => {
                match source {
                    None => {
                        if let Some(id) = pick(self) {
                            self.tool = Tool::Offset {
                                dist,
                                source: Some(id),
                            };
                        }
                    }
                    Some(src) => {
                        if let Some(c) = self.document.get(src).and_then(|e| e.as_curve()).cloned()
                        {
                            let plus = oxidraft_geometry::offset_curve(&c, dist.abs());
                            let minus = oxidraft_geometry::offset_curve(&c, -dist.abs());
                            let dp = oxidraft_geometry::point_to_curve_distance(&plus, px, py);
                            let dm = oxidraft_geometry::point_to_curve_distance(&minus, px, py);
                            let signed = if dp <= dm { dist.abs() } else { -dist.abs() };
                            self.history.snapshot(&self.document);
                            edit::offset(&mut self.document, &[src], signed);
                        }
                        self.tool = Tool::Offset { dist, source: None };
                    }
                }
                true
            }
            Tool::Dimension { mut subject } => {
                use crate::tools::DimSubject;
                let hit = pick(self);
                let click = Point2d::from_f64(px, py);
                match (&subject, hit) {
                    // A second line → the angle geometry follows the cursor
                    // until placed. Checked before the generic `Line`
                    // fallback below so a second line pick is never
                    // swallowed as "place this line's own length" instead.
                    (Some(DimSubject::Line(a, ..)), Some(id))
                        if id != *a && line_endpoints_of(self, id).is_some() =>
                    {
                        if let Some((vertex, p1, p2)) = angular_from_lines(self, *a, id) {
                            subject = Some(DimSubject::LinePair(vertex, p1, p2));
                        }
                    }
                    // First pick: classify what's under the cursor. A line
                    // may still pair with a second line, so it waits as
                    // `Line` (endpoints resolved now, so placing its own
                    // length later needs no document); a circle/arc has
                    // nothing to pair with, so its radius preview starts
                    // following the cursor right away.
                    (None, Some(id)) if is_dimensionable(self, id) => {
                        subject = if let Some((p0, p1)) = line_endpoints_of(self, id) {
                            Some(DimSubject::Line(id, p0, p1))
                        } else if let Some((c, r)) = self
                            .document
                            .get(id)
                            .and_then(|e| e.as_curve())
                            .and_then(circle_center_radius)
                        {
                            Some(DimSubject::Radial {
                                center: c,
                                radius: r,
                                diameter: false,
                            })
                        } else {
                            None
                        };
                    }
                    (None, Some(id)) if is_polycurve(self, id) => {
                        self.problem(
                            "Polylines can't take dimensions. Run Disjoint (Shift+X) to break \
                             it into welded lines first."
                                .into(),
                        );
                    }
                    // Everything else — a fully resolved subject following
                    // the cursor to its placement click, a held line falling
                    // back to its own length (same-line/non-line/empty-space
                    // second pick), nothing dimensionable under the cursor
                    // starting the free two-point case, or a first free
                    // point completing its pair — needs nothing the
                    // document can add, so `on_pick`'s own dispatch handles
                    // it too.
                    _ => {
                        let ev = crate::tools::dimension_pick(&mut subject, click);
                        self.apply_tool_event(ev);
                    }
                }
                self.tool = Tool::Dimension { subject };
                true
            }
            Tool::DimConstraint { first, pending, .. } => {
                // An anchor within tolerance means the click was aimed at a
                // point on the entity, not the entity itself — the same rule
                // Weld and ConPick already use, through the same helper.
                // Unlike them this tool takes no object snaps (see
                // `Tool::wants_point_snap`), so nothing nudges the click onto
                // the anchor first: `tol` is doing the whole job, and it is
                // the same tolerance that decided what was picked at all.
                let classify = |s: &Self, id: EntityId| match weld_anchor_at(s, id, px, py, tol) {
                    Some((idx, pos)) => DimTarget::Anchor(id, idx, pos),
                    None => DimTarget::Entity(id),
                };
                // `pick` finds an entity by its *body*, so a click on a
                // circle's centre reads as nothing to it — a whole radius
                // from the only part of the circle it looks at. The widened
                // search is what lets that click reach the circle at all.
                //
                // It runs only where `pick` came back empty, which keeps a
                // click that resolved to an entity resolving to that same
                // entity — but "empty" is not the same as "empty space".
                // `pick` also comes back empty when the nearest thing was
                // the origin point, which it filters out, and a loaded
                // drawing puts the origin last in `order`, so `pick_at`'s
                // `.rev()` reaches it first. Within tolerance of the world
                // origin, then, a click that used to resolve to nothing can
                // now find an anchor. That is the path the centre of a
                // circle drawn on the origin travels, so it is the feature
                // working rather than an edge case to suppress.
                let hit = pick(self)
                    .map(|id| classify(self, id))
                    .or_else(|| dim_anchor_near(self, px, py, tol));
                match (first, pending, hit) {
                    // A fully picked dimension is following the cursor —
                    // this click drops it wherever it lands, entity or not.
                    (_, Some((a, b)), _) => {
                        self.smart_dimension_targets(a, b, Some((px, py)));
                        self.tool = Tool::DimConstraint {
                            first: None,
                            pending: None,
                            last_anchor: None,
                        };
                    }
                    // First pick: anything that could still pair with a
                    // second pick waits in `first` — a line (with another
                    // line), or a point anchor (with another point). A whole
                    // circle/arc pairs with nothing, so its radius preview
                    // starts following the cursor right away.
                    //
                    // An anchor is accepted whatever it sits on, where an
                    // entity pick still has to be dimensionable in its own
                    // right. `weld_anchor_at` only ever yields anchors on
                    // points, lines and arcs, so the one thing this newly
                    // admits is a standalone point entity — which now has a
                    // dimension it can take part in, and did not before.
                    (None, None, Some(t))
                        if matches!(t, DimTarget::Anchor(..))
                            || is_dimensionable(self, t.entity()) =>
                    {
                        let pairable = matches!(t, DimTarget::Anchor(..))
                            || line_endpoints_of(self, t.entity()).is_some();
                        self.tool = if pairable {
                            Tool::DimConstraint {
                                first: Some(t),
                                pending: None,
                                last_anchor: None,
                            }
                        } else {
                            Tool::DimConstraint {
                                first: None,
                                pending: Some((t, None)),
                                last_anchor: None,
                            }
                        };
                    }
                    // A polyline pick is a dead end today — say how to fix
                    // it instead of silently ignoring the click.
                    (None, None, Some(t)) if is_polycurve(self, t.entity()) => {
                        self.problem(
                            "Polylines can't take dimensions. Run Disjoint (Shift+X) to break \
                             it into welded lines first."
                                .into(),
                        );
                    }
                    // A second point → the two anchors follow the cursor
                    // until placed, and the placement click is what chooses
                    // among aligned, horizontal and vertical. Two anchors on
                    // the same entity are still two points (a line's own
                    // endpoints, an arc's chord), so only naming the very
                    // same point twice falls through — that is a pick this
                    // tool cannot dimension, and it drops to the held pick's
                    // own dimension below.
                    (
                        Some(a @ DimTarget::Anchor(ia, ea, _)),
                        None,
                        Some(t @ DimTarget::Anchor(ib, eb, _)),
                    ) if (ia, ea) != (ib, eb) => {
                        self.tool = Tool::DimConstraint {
                            first: None,
                            pending: Some((a, Some(t))),
                            last_anchor: None,
                        };
                    }
                    // A second line → the pair (angle, or width when
                    // parallel) follows the cursor until placed. Both sides
                    // have to be lines, which used to go without saying:
                    // `first` could only ever hold one. It can hold a point
                    // anchor now, and a point pairs with a line as nothing
                    // this tool can yet record, so the invariant is spelled
                    // out rather than assumed.
                    (Some(a), None, Some(t))
                        if t.entity() != a.entity()
                            && line_endpoints_of(self, a.entity()).is_some()
                            && line_endpoints_of(self, t.entity()).is_some() =>
                    {
                        self.tool = Tool::DimConstraint {
                            first: None,
                            pending: Some((a, Some(t))),
                            last_anchor: None,
                        };
                    }
                    // Empty space, the same line, or a second pick that
                    // cannot pair → place the held pick's own dimension here.
                    (Some(a), None, _) => {
                        self.smart_dimension_targets(a, None, Some((px, py)));
                        self.tool = Tool::DimConstraint {
                            first: None,
                            pending: None,
                            last_anchor: None,
                        };
                    }
                    (None, None, _) => {}
                }
                true
            }
            Tool::Weld { first } => {
                // Welds happily target the origin, so no origin filter here.
                let Some(id) = pick_at(&self.document, px, py, tol) else {
                    // Empty space drops a held first pick.
                    self.tool = Tool::Weld { first: None };
                    return true;
                };
                let Some((anchor, pos)) = weld_anchor_at(self, id, px, py, tol) else {
                    self.note(
                        "No weldable point there — pick an endpoint, midpoint, center, or point"
                            .into(),
                    );
                    return true;
                };
                match first {
                    None => {
                        self.tool = Tool::Weld {
                            first: Some((id, anchor, pos)),
                        };
                    }
                    Some((fid, fa, _)) => {
                        self.weld_points((fid, fa), (id, anchor));
                        self.tool = Tool::Weld { first: None };
                    }
                }
                true
            }
            Tool::ConPick { kind, mut picks } => {
                let plan = crate::tools::con_pick_plan(kind);
                let step = plan.get(picks.len()).copied();
                let Some(step) = step else {
                    // Shouldn't happen — a full pick set is applied below on
                    // the click that completes it — but reset defensively.
                    self.tool = Tool::ConPick {
                        kind,
                        picks: Vec::new(),
                    };
                    return true;
                };
                let Some(id) = pick_at(&self.document, px, py, tol) else {
                    // Empty space cancels the in-progress pick set.
                    self.tool = Tool::ConPick {
                        kind,
                        picks: Vec::new(),
                    };
                    return true;
                };
                // What the second pick lands on decides the relation, so the
                // kind the tool was started with is only a starting guess.
                let mut applied = kind;
                let resolved = match step {
                    crate::tools::ConPickStep::Anything => self
                        .document
                        .get(id)
                        .and_then(|e| e.as_curve())
                        .and_then(|c| {
                            crate::tools::point_on_kind(c, Point2d::from_f64(px, py), tol).map(
                                |k| {
                                    applied = k;
                                    (id, 0u8, Point2d::from_f64(px, py))
                                },
                            )
                        }),
                    crate::tools::ConPickStep::Point => {
                        weld_anchor_at(self, id, px, py, tol).map(|(a, p)| (id, a, p))
                    }
                    crate::tools::ConPickStep::Line => line_endpoints_of(self, id)
                        .is_some()
                        .then(|| (id, 0u8, Point2d::from_f64(px, py))),
                    crate::tools::ConPickStep::Arc => self
                        .document
                        .get(id)
                        .and_then(|e| e.as_curve())
                        .and_then(circle_center_radius)
                        .map(|(c, _)| (id, 0u8, c)),
                };
                let Some(pick) = resolved else {
                    self.note(match step {
                        crate::tools::ConPickStep::Point => {
                            "Pick an endpoint, midpoint, center, or point".into()
                        }
                        crate::tools::ConPickStep::Line => "Pick a line".into(),
                        crate::tools::ConPickStep::Arc => "Pick a circle or arc".into(),
                        crate::tools::ConPickStep::Anything => {
                            "Pick a line or a circle/arc to hold the point against".into()
                        }
                    });
                    return true;
                };
                picks.push(pick);
                if picks.len() == plan.len() {
                    self.constrain_picked(applied, &picks);
                    self.tool = Tool::ConPick {
                        kind,
                        picks: Vec::new(),
                    };
                } else {
                    self.tool = Tool::ConPick { kind, picks };
                }
                true
            }
            Tool::Fillet { radius, first } => {
                if let Some(id) = pick(self) {
                    match first {
                        None => {
                            self.tool = Tool::Fillet {
                                radius,
                                first: Some(id),
                            }
                        }
                        Some(a) => {
                            if a != id {
                                self.history.snapshot(&self.document);
                                if let Some(arc) =
                                    edit::fillet(&mut self.document, a, id, radius, px, py)
                                {
                                    self.record_corner_constraints([a, id], arc, true);
                                } else {
                                    self.history.discard_last();
                                    self.problem(format!(
                                        "No fillet fits there at radius {radius} — try a \
                                         smaller one, or pick two edges that meet."
                                    ));
                                }
                            }
                            self.tool = Tool::Fillet {
                                radius,
                                first: None,
                            };
                        }
                    }
                }
                true
            }
            Tool::Chamfer { dist, first } => {
                if let Some(id) = pick(self) {
                    match first {
                        None => {
                            self.tool = Tool::Chamfer {
                                dist,
                                first: Some(id),
                            }
                        }
                        Some(a) => {
                            if a != id {
                                self.history.snapshot(&self.document);
                                if let Some(conn) =
                                    edit::chamfer(&mut self.document, a, id, dist, dist)
                                {
                                    self.record_corner_constraints([a, id], conn, false);
                                } else {
                                    self.history.discard_last();
                                    self.problem(format!(
                                        "No chamfer fits there at distance {dist} — try a \
                                         smaller one, or pick two edges that meet."
                                    ));
                                }
                            }
                            self.tool = Tool::Chamfer { dist, first: None };
                        }
                    }
                }
                true
            }
            Tool::Blend {
                continuity,
                tension,
                first,
                second,
            } => {
                if second.is_some() {
                    // Both entities are picked and the live-preview popup is showing;
                    // absorb further canvas clicks until Apply/Enter or Escape.
                    return true;
                }
                if let Some(id) = pick(self) {
                    match first {
                        None => {
                            self.tool = Tool::Blend {
                                continuity,
                                tension,
                                first: Some(id),
                                second: None,
                            }
                        }
                        Some(a) => {
                            self.tool = Tool::Blend {
                                continuity,
                                tension,
                                first: if a == id { None } else { Some(a) },
                                second: if a == id { None } else { Some(id) },
                            };
                        }
                    }
                }
                true
            }
            Tool::CircleTtr { radius, first } => {
                if let Some(id) = pick(self) {
                    match first {
                        None => {
                            self.tool = Tool::CircleTtr {
                                radius,
                                first: Some(id),
                            }
                        }
                        Some(a) => {
                            if a != id {
                                self.add_tangent_circle_ttr(a, id, radius, *p);
                            }
                            self.tool = Tool::CircleTtr {
                                radius,
                                first: None,
                            };
                        }
                    }
                }
                true
            }
            Tool::CircleTtt { mut picks } => {
                if let Some(id) = pick(self)
                    && !picks.contains(&id)
                {
                    picks.push(id);
                    if picks.len() == 3 {
                        self.add_tangent_circle_ttt([picks[0], picks[1], picks[2]], *p);
                        self.tool = Tool::CircleTtt { picks: Vec::new() };
                    } else {
                        self.tool = Tool::CircleTtt { picks };
                    }
                }
                true
            }
            Tool::Stretch { c1, c2, base, ids } => {
                match (c1, c2, base) {
                    (None, _, _) => {
                        let ids = if self.selection.is_empty() {
                            self.document
                                .iter()
                                .map(|e| e.id)
                                .filter(|&i| i != self.origin_id)
                                .collect()
                        } else {
                            self.selection.clone()
                        };
                        self.tool = Tool::Stretch {
                            c1: Some(*p),
                            c2: None,
                            base: None,
                            ids,
                        };
                    }
                    (Some(a), None, _) => {
                        self.tool = Tool::Stretch {
                            c1: Some(a),
                            c2: Some(*p),
                            base: None,
                            ids,
                        }
                    }
                    (Some(a), Some(b), None) => {
                        self.tool = Tool::Stretch {
                            c1: Some(a),
                            c2: Some(b),
                            base: Some(*p),
                            ids,
                        }
                    }
                    (Some(a), Some(b), Some(bp)) => {
                        let (ax, ay) = a.to_f64();
                        let (bx, by) = b.to_f64();
                        let window = (ax.min(bx), ay.min(by), ax.max(bx), ay.max(by));
                        let dx = px - bp.x;
                        let dy = py - bp.y;
                        self.history.snapshot(&self.document);
                        if edit::stretch(&mut self.document, &ids, window, dx, dy) {
                            oxidraft_cad::resolve_after_transform(&mut self.document, &ids);
                        } else {
                            self.history.discard_last();
                        }
                        self.tool = Tool::Stretch {
                            c1: None,
                            c2: None,
                            base: None,
                            ids: vec![],
                        };
                    }
                }
                true
            }
            _ => false,
        }
    }

    /// Commits the pending blend (both entities picked, popup showing) with
    /// whatever continuity/tension is currently set, then resets the tool to
    /// pick a fresh pair. No-op if the tool isn't in the pending-confirm state.
    pub fn confirm_pending_blend(&mut self) {
        let Tool::Blend {
            continuity,
            tension,
            first: Some(a),
            second: Some(b),
        } = self.tool.clone()
        else {
            return;
        };
        self.history.snapshot(&self.document);
        if oxidraft_cad::edit::blend(&mut self.document, a, b, continuity, tension).is_none() {
            self.history.discard_last();
        }
        self.tool = Tool::Blend {
            continuity,
            tension,
            first: None,
            second: None,
        };
    }

    /// Drops the pending blend pick (both entities chosen, popup showing)
    /// without committing, returning to "pick the first entity".
    pub fn cancel_pending_blend(&mut self) {
        if let Tool::Blend {
            continuity,
            tension,
            ..
        } = self.tool.clone()
        {
            self.tool = Tool::Blend {
                continuity,
                tension,
                first: None,
                second: None,
            };
        }
    }

    fn add_tangent_circle_ttr(&mut self, a: EntityId, b: EntityId, radius: f64, near: Point2d) {
        let (Some(c1), Some(c2)) = (
            self.document.get(a).and_then(|e| e.as_curve()).cloned(),
            self.document.get(b).and_then(|e| e.as_curve()).cloned(),
        ) else {
            return;
        };
        match oxidraft_geometry::tangent_circle_ttr(&c1, &c2, radius, near) {
            Some((center, r)) => {
                if let Some(id) = self.create_full_circle(center, r) {
                    self.set_tangents(
                        id,
                        vec![
                            oxidraft_document::TangentRef { target: a, near },
                            oxidraft_document::TangentRef { target: b, near },
                        ],
                    );
                }
            }
            None => self.problem(format!(
                "No circle of radius {radius} is tangent to both — try a \
                 different radius, or pick nearer where you want it."
            )),
        }
    }

    fn add_tangent_circle_ttt(&mut self, ids: [EntityId; 3], near: Point2d) {
        let curves: Vec<_> = ids
            .iter()
            .filter_map(|&id| self.document.get(id).and_then(|e| e.as_curve()).cloned())
            .collect();
        if curves.len() != 3 {
            return;
        }
        match oxidraft_geometry::tangent_circle_ttt(&curves[0], &curves[1], &curves[2], near) {
            Some((center, r)) => {
                if let Some(id) = self.create_full_circle(center, r) {
                    self.set_tangents(
                        id,
                        ids.iter()
                            .map(|&t| oxidraft_document::TangentRef { target: t, near })
                            .collect(),
                    );
                }
            }
            None => self.problem(
                "No circle is tangent to all three — pick nearer where you want \
                 it, or a different set of edges."
                    .into(),
            ),
        }
    }

    fn set_tangents(&mut self, id: EntityId, tangents: Vec<oxidraft_document::TangentRef>) {
        if let Some(e) = self.document.get_mut(id) {
            e.tangents = tangents;
        }
    }

    fn create_full_circle(&mut self, center: Point2d, r: f64) -> Option<EntityId> {
        if r <= 1e-9 {
            return None;
        }
        let arc = oxidraft_geometry::CircularArc::new(center, r, 0.0, std::f64::consts::TAU);
        self.history.snapshot(&self.document);
        let id = self.document.add(oxidraft_document::EntityKind::Curve(
            oxidraft_geometry::Curve::Arc(arc),
        ));
        self.apply_new_entity_defaults(id);
        Some(id)
    }

    /// The live preview for the Trim/Extend tool's entity under the cursor:
    /// what would be removed (Trim) or added (Extend) if clicked now.
    /// Trims everything the stroke from `from` to `to` genuinely crosses,
    /// reporting whether it cut anything.
    ///
    /// One step of a power-trim sweep. Crossing is the rule, not proximity:
    /// asking what lies *under* each sample meant a stroke run along an edge
    /// kept finding it and kept eating it a span at a time — sweeping the
    /// length of a rung between two rails consumed the whole rung. A stroke
    /// that runs alongside geometry never crosses it, so now it leaves it be.
    ///
    /// History is deliberately untouched: the caller snapshots once for the
    /// whole stroke, so dragging across twenty edges is one undo, not twenty.
    pub fn trim_across(&mut self, from: (f64, f64), to: (f64, f64)) -> bool {
        use oxidraft_geometry::{Curve, LineSeg, Point2d};
        let stroke = Curve::Line(LineSeg::from_endpoints(
            Point2d::from_f64(from.0, from.1),
            Point2d::from_f64(to.0, to.1),
        ));
        let ids: Vec<EntityId> = self
            .document
            .iter()
            .map(|e| e.id)
            .filter(|&i| i != self.origin_id)
            .collect();
        let mut cut = false;
        for id in ids {
            let Some(curve) = self.document.get(id).and_then(|e| e.as_curve()).cloned() else {
                continue;
            };
            // The first crossing is enough: one step of the stroke takes one
            // bite out of any given edge, so a slow drag does not chew through
            // an edge it is merely resting on.
            let Some(hit) = oxidraft_geometry::intersect(&stroke, &curve)
                .into_iter()
                .next()
            else {
                continue;
            };
            let cutters: Vec<EntityId> = self
                .document
                .iter()
                .map(|e| e.id)
                .filter(|&i| i != id && i != self.origin_id)
                .collect();
            // `trim` hands back the target unchanged exactly when the point
            // found no span worth removing — the same signal the click path
            // reads, and what keeps an edge nothing else crosses intact.
            if oxidraft_cad::edit::trim(&mut self.document, id, &cutters, hit.point.0, hit.point.1)
                != vec![id]
            {
                cut = true;
            }
        }
        cut
    }

    pub fn trim_extend_preview(&self) -> Option<TrimExtendPreview> {
        use oxidraft_cad::edit;
        let (px, py) = self.cursor_world;
        let tol = self.view.pixel_world_size() * 6.0;
        let id = pick_at(&self.document, px, py, tol)?;
        match self.tool {
            Tool::Trim => {
                let cutters: Vec<EntityId> = self
                    .document
                    .iter()
                    .map(|e| e.id)
                    .filter(|&i| i != id)
                    .collect();
                edit::trim_preview(&self.document, id, &cutters, px, py)
                    .map(TrimExtendPreview::Remove)
            }
            Tool::Extend => {
                let boundaries: Vec<EntityId> = self
                    .document
                    .iter()
                    .map(|e| e.id)
                    .filter(|&i| i != id)
                    .collect();
                edit::extend_preview(&self.document, id, &boundaries, px, py)
                    .map(TrimExtendPreview::Extension)
            }
            _ => None,
        }
    }
}

/// What [`AppState::trim_extend_preview`] would do to the hovered entity.
pub enum TrimExtendPreview {
    /// The curve segment that would be cut away.
    Remove(oxidraft_geometry::Curve),
    /// The curve as it would look after extending to the target boundary.
    Extension(oxidraft_geometry::Curve),
}

/// Nearest weldable anchor on `id` to the click, within `tol`: 0/1 an
/// endpoint, ANCHOR_DERIVED a line's midpoint or an arc's center; a point
/// entity is anchor 0 at its own position. With snapping on, the click
/// arrives exactly on the snapped anchor, so "nearest" is exact.
fn weld_anchor_at(
    app: &AppState,
    id: EntityId,
    px: f64,
    py: f64,
    tol: f64,
) -> Option<(u8, Point2d)> {
    let mut cands: Vec<(u8, (f64, f64))> = Vec::new();
    match &app.document.get(id)?.kind {
        EntityKind::Point(p) => cands.push((0, p.to_f64())),
        EntityKind::Curve(Curve::Line(l)) => {
            let (x0, y0) = l.p0.to_f64();
            let (x1, y1) = l.p1.to_f64();
            cands.push((0, (x0, y0)));
            cands.push((1, (x1, y1)));
            cands.push((ANCHOR_DERIVED, ((x0 + x1) * 0.5, (y0 + y1) * 0.5)));
        }
        EntityKind::Curve(Curve::Arc(a)) => {
            let full = (a.end_angle - a.start_angle).abs() >= std::f64::consts::TAU - 1e-9;
            if !full {
                let (cx, cy) = a.center.to_f64();
                let at = |th: f64| (cx + a.radius * th.cos(), cy + a.radius * th.sin());
                cands.push((0, at(a.start_angle)));
                cands.push((1, at(a.end_angle)));
            }
            cands.push((ANCHOR_DERIVED, a.center.to_f64()));
        }
        _ => return None,
    }
    let mut best: Option<(f64, u8, (f64, f64))> = None;
    for (i, (x, y)) in cands {
        let d = (x - px).hypot(y - py);
        if d <= tol && best.is_none_or(|(bd, _, _)| d < bd) {
            best = Some((d, i, (x, y)));
        }
    }
    best.map(|(_, i, (x, y))| (i, Point2d::from_f64(x, y)))
}

/// Nearest weldable anchor to the click on anything Smart Dimension can
/// dimension, searched across the drawing rather than on one known entity.
///
/// [`weld_anchor_at`] answers "which anchor of *this* entity", which needs
/// the entity to have been found first — and `pick_at` finds an entity by
/// its body, so a circle's centre is a whole radius away from anything it
/// looks at. Without this, the one pick the anchored dimension exists for
/// would read as empty space. Restricted to dimensionable entities, so it
/// can only ever hand the tool something it could already have been given.
fn dim_anchor_near(app: &AppState, px: f64, py: f64, tol: f64) -> Option<DimTarget> {
    let click = Point2d::from_f64(px, py);
    let mut best: Option<(f64, DimTarget)> = None;
    for id in app.document.editable_entities().map(|e| e.id) {
        if !is_dimensionable(app, id) {
            continue;
        }
        let Some((idx, pos)) = weld_anchor_at(app, id, px, py, tol) else {
            continue;
        };
        let d = pos.dist_f64(&click);
        if best.as_ref().is_none_or(|(bd, _)| d < *bd) {
            best = Some((d, DimTarget::Anchor(id, idx, pos)));
        }
    }
    best.map(|(_, t)| t)
}

fn circle_center_radius(c: &oxidraft_geometry::Curve) -> Option<(Point2d, f64)> {
    match c {
        oxidraft_geometry::Curve::Arc(a) => Some((a.center, a.radius)),
        _ => None,
    }
}

/// Whether the entity can carry a driving dimension the smart-dimension tool
/// understands: a line (length or, paired, angle) or a circle/arc (radius).
fn is_dimensionable(app: &AppState, id: EntityId) -> bool {
    matches!(
        app.document.get(id).and_then(|e| e.as_curve()),
        Some(oxidraft_geometry::Curve::Line(_)) | Some(oxidraft_geometry::Curve::Arc(_))
    )
}

/// Whether the entity is a multi-segment polyline — undimensionable as-is,
/// but one EXPLODE away from welded, dimensionable lines.
fn is_polycurve(app: &AppState, id: EntityId) -> bool {
    matches!(
        app.document.get(id).and_then(|e| e.as_curve()),
        Some(oxidraft_geometry::Curve::Poly(_))
    )
}

fn line_endpoints_of(app: &AppState, id: EntityId) -> Option<(Point2d, Point2d)> {
    match app.document.get(id)?.as_curve()? {
        oxidraft_geometry::Curve::Line(l) => Some((l.p0, l.p1)),
        _ => None,
    }
}

fn angular_from_lines(
    app: &AppState,
    a: EntityId,
    b: EntityId,
) -> Option<(Point2d, Point2d, Point2d)> {
    let (a0, a1) = line_endpoints_of(app, a)?;
    let (b0, b1) = line_endpoints_of(app, b)?;
    let vertex = oxidraft_geometry::intersect_lines_unbounded(
        &oxidraft_geometry::LineSeg::from_endpoints(a0, a1),
        &oxidraft_geometry::LineSeg::from_endpoints(b0, b1),
    )?;
    let far = |p: Point2d, q: Point2d| {
        if vertex.dist_f64(&p) >= vertex.dist_f64(&q) {
            p
        } else {
            q
        }
    };
    Some((vertex, far(a0, a1), far(b0, b1)))
}
