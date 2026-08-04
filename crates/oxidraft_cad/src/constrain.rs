//! Geometric constraints on line entities. Applying a constraint solves it
//! once — the numeric solver in `oxidraft_constraint` moves endpoints the
//! *minimum* amount that satisfies the requested relation, so a nearly
//! horizontal line snaps flat about its own midpoint instead of jumping —
//! and records it on the document so later edits can re-satisfy it via
//! [`resolve_after_edit`] / [`resolve_after_transform`]. Reference geometry
//! (the first pick of a pair) is pinned during the initial solve and never
//! moves.

use oxidraft_constraint::{Constraint, PointVar, ScalarVar, Sketch};
use oxidraft_document::{
    ANCHOR_DERIVED, ConstraintKind, Document, EntityId, EntityKind, SketchConstraint,
    normalize_angle_deg,
};
use oxidraft_geometry::{CircularArc, Curve, LineSeg, Point2d};
use std::collections::HashMap;
use std::f64::consts::TAU;

/// A rejected constraint action: the user-facing message, plus the entities
/// carrying the conflicting constraints (when a conflict was diagnosed) so
/// the UI can highlight them. Errors with nothing to highlight (bad
/// selection, non-finite value) carry an empty culprit list.
#[derive(Debug, Clone)]
pub struct ConstrainError {
    /// The user-facing explanation of why the action was rejected.
    pub message: String,
    /// Entities carrying the conflicting constraints, for the UI to highlight
    /// (empty when there's nothing specific to point at).
    pub culprits: Vec<EntityId>,
}

impl From<String> for ConstrainError {
    fn from(message: String) -> Self {
        ConstrainError {
            message,
            culprits: Vec::new(),
        }
    }
}

impl From<&str> for ConstrainError {
    fn from(message: &str) -> Self {
        ConstrainError {
            message: message.to_owned(),
            culprits: Vec::new(),
        }
    }
}

impl std::fmt::Display for ConstrainError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// The entities the given `doc.constraints` indices are recorded on —
/// what a conflict highlight should point the user at.
fn culprit_entities(doc: &Document, indices: &[usize]) -> Vec<EntityId> {
    let mut out = Vec::new();
    for &i in indices {
        let Some(c) = doc.constraints.get(i) else {
            continue;
        };
        for id in [Some(c.a), c.b, c.c].into_iter().flatten() {
            if !out.contains(&id) {
                out.push(id);
            }
        }
    }
    out
}

fn line_of(doc: &Document, id: EntityId) -> Option<LineSeg> {
    match &doc.get(id)?.kind {
        EntityKind::Curve(Curve::Line(l)) => Some(l.clone()),
        _ => None,
    }
}

fn arc_of(doc: &Document, id: EntityId) -> Option<CircularArc> {
    match &doc.get(id)?.kind {
        EntityKind::Curve(Curve::Arc(a)) => Some(*a),
        _ => None,
    }
}

fn point_of(doc: &Document, id: EntityId) -> Option<Point2d> {
    match &doc.get(id)?.kind {
        EntityKind::Point(p) => Some(*p),
        _ => None,
    }
}

fn arc_is_full(a: &CircularArc) -> bool {
    (a.end_angle - a.start_angle).abs() >= TAU - 1e-9
}

fn arc_end_pos(a: &CircularArc, i: u8) -> (f64, f64) {
    let th = if i == 0 { a.start_angle } else { a.end_angle };
    (
        a.center.x + a.radius * th.cos(),
        a.center.y + a.radius * th.sin(),
    )
}

fn len(l: &LineSeg) -> f64 {
    (l.p1.x - l.p0.x).hypot(l.p1.y - l.p0.y)
}

fn endpoint(l: &LineSeg, i: u8) -> (f64, f64) {
    if i == 0 {
        (l.p0.x, l.p0.y)
    } else {
        (l.p1.x, l.p1.y)
    }
}

/// Rotates the sketch's initial guess for the b-line slightly about its
/// midpoint. The parallel/perpendicular/horizontal residuals have a saddle
/// when the line starts exactly 90° from the target orientation; a small
/// rotation breaks the symmetry so LM can descend.
fn perturb_line(s: &mut Sketch, b0: PointVar, b1: PointVar, l: &LineSeg) {
    let (mx, my) = ((l.p0.x + l.p1.x) * 0.5, (l.p0.y + l.p1.y) * 0.5);
    let th = 0.05f64;
    let (c, sn) = (th.cos(), th.sin());
    let rot = |x: f64, y: f64| {
        (
            mx + c * (x - mx) - sn * (y - my),
            my + sn * (x - mx) + c * (y - my),
        )
    };
    let (x0, y0) = rot(l.p0.x, l.p0.y);
    let (x1, y1) = rot(l.p1.x, l.p1.y);
    s.set_point(b0, x0, y0);
    s.set_point(b1, x1, y1);
}

/// An actionable suffix for "nothing eligible in the selection" errors:
/// when the selection holds a polyline (a multi-segment PolyCurve entity),
/// the fix is to EXPLODE it — its segments then become individual welded
/// lines the constraint system can hold onto.
fn polyline_hint(doc: &Document, selection: &[EntityId]) -> &'static str {
    let has_poly = selection
        .iter()
        .any(|&id| matches!(doc.get(id).and_then(|e| e.as_curve()), Some(Curve::Poly(_))));
    if has_poly {
        " — polylines can't take constraints; EXPLODE (X) into welded lines first"
    } else {
        ""
    }
}

/// Applies the constraint to the selected line entities and records it on
/// the document. Single-line kinds (horizontal/vertical) accept any number
/// of lines; pair kinds require exactly two, with the first selected line
/// acting as the fixed reference. Coincident joins the nearest endpoints of
/// the two lines.
pub fn constrain_lines(
    doc: &mut Document,
    selection: &[EntityId],
    kind: ConstraintKind,
) -> Result<String, ConstrainError> {
    let lines: Vec<(EntityId, LineSeg)> = selection
        .iter()
        .filter_map(|&id| line_of(doc, id).map(|l| (id, l)))
        .collect();

    match kind {
        ConstraintKind::Fixed => {
            Err("Fixed is set automatically on structural anchors, not a selectable command".into())
        }
        ConstraintKind::Tangent => constrain_tangent(doc, selection),
        ConstraintKind::Radius => constrain_radius(doc, selection, None),
        ConstraintKind::Distance => constrain_distance(doc, selection, None),
        ConstraintKind::LineDistance => constrain_line_distance(doc, selection, None),
        ConstraintKind::Angle => constrain_angle(doc, selection, None),
        ConstraintKind::Concentric => constrain_concentric(doc, selection),
        ConstraintKind::EqualRadius => constrain_equal_radius(doc, selection),
        ConstraintKind::Block => constrain_block(doc, selection),
        ConstraintKind::Midpoint
        | ConstraintKind::PointOnLine
        | ConstraintKind::PointOnCircle
        | ConstraintKind::PointDistance
        | ConstraintKind::HDistance
        | ConstraintKind::VDistance
        | ConstraintKind::PointLineDistance
        | ConstraintKind::Symmetric => {
            Err(format!("{} is pick-based — pick its points on canvas", kind.label()).into())
        }
        ConstraintKind::Horizontal | ConstraintKind::Vertical => {
            if lines.is_empty() {
                return Err(format!(
                    "Select at least one line to make {}{}",
                    kind.label(),
                    polyline_hint(doc, selection)
                )
                .into());
            }
            let mut count = 0;
            for (id, l) in &lines {
                // Record first and validate against the FULL connected
                // component (component_sketch pulls in whatever's already
                // recorded on this entity) before touching geometry — this
                // is what catches e.g. a line already Horizontal being
                // asked to go Vertical, instead of silently leaving both
                // (mutually exclusive) records on it while only the new
                // one's geometry actually holds.
                let candidate = SketchConstraint::single(kind, *id);
                let added = doc.add_constraint(candidate);
                if added && let Err(conflict) = validate_recorded(doc, &[*id], &candidate, true) {
                    doc.constraints.retain(|c| !c.same_relation(&candidate));
                    return Err(ConstrainError {
                        message: format!(
                            "Could not make the line {} against its existing constraints{}",
                            kind.label(),
                            conflict.message
                        ),
                        culprits: conflict.culprits,
                    });
                }
                let mut s = Sketch::new();
                let a = s.add_point(l.p0.x, l.p0.y);
                let b = s.add_point(l.p1.x, l.p1.y);
                s.constrain(match kind {
                    ConstraintKind::Horizontal => Constraint::Horizontal(a, b),
                    _ => Constraint::Vertical(a, b),
                });
                s.constrain(Constraint::Distance(a, b, len(l)));
                let mut res = s.solve();
                if !res.converged {
                    perturb_line(&mut s, a, b, l);
                    res = s.solve();
                }
                if !res.converged {
                    return Err(format!(
                        "Could not make the line {} (residual {:.2e})",
                        kind.label(),
                        res.residual
                    )
                    .into());
                }
                write_line(doc, *id, s.point(a), s.point(b));
                // The solve above only ever looks at `id` in isolation, so a
                // coincident (or otherwise linked) neighbour elsewhere in the
                // component is still sitting at the pre-solve position —
                // drag it back into place the same way a live edit would.
                resolve_after_transform(doc, &[*id]);
                count += 1;
            }
            Ok(format!("Made {count} line(s) {}", kind.label()))
        }
        ConstraintKind::Parallel
        | ConstraintKind::Perpendicular
        | ConstraintKind::EqualLength
        | ConstraintKind::Collinear
        | ConstraintKind::Coincident => {
            if lines.len() != 2 {
                return Err(format!(
                    "Select exactly two lines to make them {} (got {}){}",
                    kind.label(),
                    lines.len(),
                    polyline_hint(doc, selection)
                )
                .into());
            }
            let (ref_id, ref_line) = lines[0].clone();
            let (mov_id, mov_line) = lines[1].clone();

            // Join the endpoint pair that is already closest — computed up
            // front since Coincident's candidate record needs it either way.
            let endpoints = (kind == ConstraintKind::Coincident).then(|| {
                let mut best = (f64::INFINITY, 0u8, 0u8);
                for ea in 0..2u8 {
                    for eb in 0..2u8 {
                        let (ax, ay) = endpoint(&ref_line, ea);
                        let (bx, by) = endpoint(&mov_line, eb);
                        let d = (ax - bx).hypot(ay - by);
                        if d < best.0 {
                            best = (d, ea, eb);
                        }
                    }
                }
                (best.1, best.2)
            });
            let candidate = match endpoints {
                Some((ea, eb)) => SketchConstraint::coincident(ref_id, ea, mov_id, eb),
                None => SketchConstraint::pair(kind, ref_id, mov_id),
            };

            // Record first and validate against the FULL connected
            // component (whatever's already recorded on either line) before
            // touching geometry — this is what catches e.g. the mover
            // already being Perpendicular to a third line that this new
            // Parallel relation can't also hold, instead of silently
            // leaving both records with only the new one geometrically true.
            // Parallel/Perpendicular need the length-collapse safeguard
            // Horizontal/Vertical use (their real solve below also anchors
            // the mover's length); EqualLength's whole point is to change a
            // length, so it must NOT be anchored, and Coincident doesn't
            // involve length at all.
            let anchor = matches!(
                kind,
                ConstraintKind::Parallel
                    | ConstraintKind::Perpendicular
                    | ConstraintKind::Collinear
            );
            let added = doc.add_constraint(candidate);
            if added
                && let Err(conflict) = validate_recorded(doc, &[ref_id, mov_id], &candidate, anchor)
            {
                doc.constraints.retain(|c| !c.same_relation(&candidate));
                return Err(ConstrainError {
                    message: format!(
                        "Could not make the lines {} against their existing constraints{}",
                        kind.label(),
                        conflict.message
                    ),
                    culprits: conflict.culprits,
                });
            }

            let mut s = Sketch::new();
            let a0 = s.add_point(ref_line.p0.x, ref_line.p0.y);
            let a1 = s.add_point(ref_line.p1.x, ref_line.p1.y);
            let b0 = s.add_point(mov_line.p0.x, mov_line.p0.y);
            let b1 = s.add_point(mov_line.p1.x, mov_line.p1.y);
            s.constrain(Constraint::Fixed(a0, ref_line.p0.x, ref_line.p0.y));
            s.constrain(Constraint::Fixed(a1, ref_line.p1.x, ref_line.p1.y));
            match kind {
                ConstraintKind::Parallel => {
                    s.constrain(Constraint::Parallel(a0, a1, b0, b1));
                    s.constrain(Constraint::Distance(b0, b1, len(&mov_line)));
                }
                ConstraintKind::Perpendicular => {
                    s.constrain(Constraint::Perpendicular(a0, a1, b0, b1));
                    s.constrain(Constraint::Distance(b0, b1, len(&mov_line)));
                }
                ConstraintKind::EqualLength => {
                    s.constrain(Constraint::EqualLength(a0, a1, b0, b1));
                }
                ConstraintKind::Collinear => {
                    s.constrain(Constraint::PointOnLine(b0, a0, a1));
                    s.constrain(Constraint::PointOnLine(b1, a0, a1));
                    s.constrain(Constraint::Distance(b0, b1, len(&mov_line)));
                }
                ConstraintKind::Coincident => {
                    let (ea, eb) = endpoints.expect("computed above for Coincident");
                    let pa = if ea == 0 { a0 } else { a1 };
                    let pb = if eb == 0 { b0 } else { b1 };
                    s.constrain(Constraint::Coincident(pa, pb));
                }
                _ => unreachable!(),
            }
            let mut res = s.solve();
            if !res.converged {
                s.set_point(a0, ref_line.p0.x, ref_line.p0.y);
                s.set_point(a1, ref_line.p1.x, ref_line.p1.y);
                perturb_line(&mut s, b0, b1, &mov_line);
                res = s.solve();
            }
            if !res.converged {
                return Err(format!(
                    "Could not make the lines {} (residual {:.2e})",
                    kind.label(),
                    res.residual
                )
                .into());
            }
            write_line(doc, mov_id, s.point(b0), s.point(b1));
            // Same as above: this solve only ever looked at ref_id/mov_id,
            // so a third entity coincident (or otherwise linked) to mov_id
            // is still sitting at mov_id's pre-solve position — drag it
            // back into place the same way a live edit would.
            resolve_after_transform(doc, &[mov_id]);
            Ok(format!(
                "Made the second line {} to the first",
                kind.label()
            ))
        }
    }
}

/// Makes a line and a circular arc tangent. The first-selected entity is
/// the fixed reference: tangent-to-line slides the circle onto the line,
/// tangent-to-circle rotates/translates the line (length kept) until it
/// touches. Records the relation for re-solving on later edits.
fn constrain_tangent(doc: &mut Document, selection: &[EntityId]) -> Result<String, ConstrainError> {
    if selection.len() != 2 {
        return Err(format!(
            "Select one line and one arc/circle to make them tangent (got {})",
            selection.len()
        )
        .into());
    }
    let (first, second) = (selection[0], selection[1]);
    let (line_id, arc_id) = match (
        line_of(doc, first).is_some(),
        arc_of(doc, first).is_some(),
        line_of(doc, second).is_some(),
        arc_of(doc, second).is_some(),
    ) {
        (_, true, _, true) => return constrain_tangent_circles(doc, first, second),
        (true, _, _, true) => (first, second),
        (_, true, true, _) => (second, first),
        _ => return Err("Tangent needs a line and an arc, or two arcs".into()),
    };
    let l = line_of(doc, line_id).expect("classified as line");
    let a = arc_of(doc, arc_id).expect("classified as arc");
    let mut s = Sketch::new();
    let l0 = s.add_point(l.p0.x, l.p0.y);
    let l1 = s.add_point(l.p1.x, l.p1.y);
    let av = add_arc_vars(&mut s, &a);
    let (c, r) = av.circle().expect("arc vars carry a circle");
    if first == line_id {
        for p in [l0, l1] {
            let (x, y) = s.point(p);
            s.constrain(Constraint::Fixed(p, x, y));
        }
    } else {
        pin_shape(&mut s, &av);
        // The line is the mover: keep its length so it slides and rotates
        // rather than collapsing onto the rim.
        s.constrain(Constraint::Distance(l0, l1, len(&l)));
    }
    s.constrain(Constraint::TangentLineCircle(l0, l1, c, r));
    let res = s.solve();
    if !res.converged {
        return Err(format!(
            "Could not make the entities tangent (residual {:.2e})",
            res.residual
        )
        .into());
    }
    // Write back only the mover: the pinned reference is a least-squares
    // residual, within ~1e-10 of its coordinates but not bit-exact.
    let mut vars = HashMap::new();
    if first == line_id {
        vars.insert(arc_id, av);
    } else {
        vars.insert(line_id, ShapeVars::Line(l0, l1));
    }
    write_back(doc, &s, &vars);
    doc.add_constraint(SketchConstraint::pair(
        ConstraintKind::Tangent,
        first,
        second,
    ));
    Ok("Made the second entity tangent to the first".into())
}

/// Makes two circular arcs tangent, first pick pinned. The mover keeps its
/// radius and translates into contact; internal vs external tangency is
/// whichever the current drawing is closer to.
fn constrain_tangent_circles(
    doc: &mut Document,
    first: EntityId,
    second: EntityId,
) -> Result<String, ConstrainError> {
    let a = arc_of(doc, first).expect("classified as arc");
    let b = arc_of(doc, second).expect("classified as arc");
    let mut s = Sketch::new();
    let va = add_arc_vars(&mut s, &a);
    let vb = add_arc_vars(&mut s, &b);
    pin_shape(&mut s, &va);
    let (c1, r1) = va.circle().expect("arc vars carry a circle");
    let (c2, r2) = vb.circle().expect("arc vars carry a circle");
    s.constrain(Constraint::FixedScalar(r2, b.radius));
    s.constrain(Constraint::TangentCircleCircle {
        c1,
        r1,
        c2,
        r2,
        internal: tangency_is_internal(&a, &b),
    });
    let res = s.solve();
    if !res.converged {
        return Err(format!(
            "Could not make the circles tangent (residual {:.2e})",
            res.residual
        )
        .into());
    }
    let mut vars = HashMap::new();
    vars.insert(second, vb);
    write_back(doc, &s, &vars);
    doc.add_constraint(SketchConstraint::pair(
        ConstraintKind::Tangent,
        first,
        second,
    ));
    Ok("Made the second circle tangent to the first".into())
}

/// Pins the selected entities where they currently sit — a driving "fix"
/// that records a `Fixed` constraint on each selected line, circle/arc, or
/// point so the solver holds it in place while its neighbours move. Pinning
/// at the current position is trivially consistent with everything already
/// satisfied, so no re-solve is needed. Idempotent per entity.
pub fn constrain_fixed(
    doc: &mut Document,
    selection: &[EntityId],
) -> Result<String, ConstrainError> {
    let targets: Vec<EntityId> = selection
        .iter()
        .copied()
        .filter(|&id| {
            line_of(doc, id).is_some() || arc_of(doc, id).is_some() || point_of(doc, id).is_some()
        })
        .collect();
    if targets.is_empty() {
        return Err(format!(
            "Select a line, circle/arc, or point to fix it in place{}",
            polyline_hint(doc, selection)
        )
        .into());
    }
    let mut count = 0;
    for id in targets {
        if doc.add_constraint(SketchConstraint::fixed(id)) {
            count += 1;
        }
    }
    if count == 0 {
        return Err("That selection is already fixed".into());
    }
    Ok(format!("Fixed {count} object(s) in place"))
}

/// Welds two picked anchor points together — endpoint, line midpoint,
/// arc/circle center, or a point entity — recording a coincident constraint
/// and re-solving the whole component so the geometry actually meets. This
/// is the pick-based weld: the origin onto a line's midpoint, a circle's
/// center onto a corner, two midpoints onto each other.
pub fn constrain_coincident_points(
    doc: &mut Document,
    a: (EntityId, u8),
    b: (EntityId, u8),
) -> Result<String, ConstrainError> {
    constrain_point_pair(doc, ConstraintKind::Coincident, a, b)
}

/// Whether anchor index `idx` names a real pick target on the entity: a
/// point entity is index 0 only; a line takes endpoints and its midpoint;
/// an arc takes endpoints and its center, a full circle only its center.
fn anchor_ok(doc: &Document, id: EntityId, idx: u8) -> bool {
    if point_of(doc, id).is_some() {
        return idx == 0;
    }
    if line_of(doc, id).is_some() {
        return idx <= ANCHOR_DERIVED;
    }
    if let Some(arc) = arc_of(doc, id) {
        let full = (arc.end_angle - arc.start_angle).abs() >= TAU - 1e-9;
        return if full {
            idx == ANCHOR_DERIVED
        } else {
            idx <= ANCHOR_DERIVED
        };
    }
    false
}

/// Applies a pick-based point relation between two picked anchors:
/// Coincident (weld), Midpoint (point onto a line's midpoint), PointOnLine
/// (point onto a line's infinite carrier), or PointOnCircle (point onto an
/// arc's rim). `a` is the picked point anchor; for the on-curve kinds `b`
/// names the target curve entity (its anchor index is ignored).
pub fn constrain_point_pair(
    doc: &mut Document,
    kind: ConstraintKind,
    a: (EntityId, u8),
    b: (EntityId, u8),
) -> Result<String, ConstrainError> {
    let (a_id, ea) = a;
    let (b_id, eb) = b;
    let verb = match kind {
        ConstraintKind::Coincident => "weld",
        ConstraintKind::Midpoint => "hold at the midpoint",
        ConstraintKind::PointOnLine => "hold on the line",
        ConstraintKind::PointOnCircle => "hold on the circle",
        _ => return Err("Not a pick-based point relation".into()),
    };
    if a_id == b_id {
        return Err(format!("Pick points on two different objects to {verb}").into());
    }
    let (candidate, seeds) = match kind {
        ConstraintKind::Coincident => {
            if !anchor_ok(doc, a_id, ea) || !anchor_ok(doc, b_id, eb) {
                return Err(format!(
                    "Pick an endpoint, midpoint, center, or point to weld{}",
                    polyline_hint(doc, &[a_id, b_id])
                )
                .into());
            }
            (
                SketchConstraint::coincident(a_id, ea, b_id, eb),
                [a_id, b_id],
            )
        }
        ConstraintKind::Midpoint => {
            if !anchor_ok(doc, a_id, ea) {
                return Err("Pick an endpoint, center, or point first".into());
            }
            if line_of(doc, b_id).is_none() {
                return Err("Midpoint needs a line to take the midpoint of".into());
            }
            (
                SketchConstraint::anchored(kind, a_id, ea, b_id, ANCHOR_DERIVED),
                [a_id, b_id],
            )
        }
        ConstraintKind::PointOnLine => {
            if !anchor_ok(doc, a_id, ea) {
                return Err("Pick an endpoint, center, or point first".into());
            }
            if line_of(doc, b_id).is_none() {
                return Err("Point-on-line needs a line to hold the point on".into());
            }
            (
                SketchConstraint::anchored(kind, a_id, ea, b_id, 0),
                [a_id, b_id],
            )
        }
        ConstraintKind::PointOnCircle => {
            if !anchor_ok(doc, a_id, ea) {
                return Err("Pick an endpoint, center, or point first".into());
            }
            if arc_of(doc, b_id).is_none() {
                return Err("Point-on-circle needs a circle or arc to hold the point on".into());
            }
            (
                SketchConstraint::anchored(kind, a_id, ea, b_id, 0),
                [a_id, b_id],
            )
        }
        _ => unreachable!(),
    };
    if !doc.add_constraint(candidate) {
        return Err(format!("That {} relation already exists", kind.label()).into());
    }
    if let Err(conflict) = validate_recorded(doc, &seeds, &candidate, false) {
        doc.constraints.retain(|c| !c.same_relation(&candidate));
        return Err(ConstrainError {
            message: format!(
                "Could not {verb} against the existing constraints{}",
                conflict.message
            ),
            culprits: conflict.culprits,
        });
    }
    // Validated consistent — re-solve the component and write everything
    // back so the relation takes hold visibly.
    let CompSketch { mut s, vars, .. } = component_sketch(doc, &seeds);
    if !s.solve_robust().converged {
        doc.constraints.retain(|c| !c.same_relation(&candidate));
        return Err(format!("Could not {verb}").into());
    }
    write_back(doc, &s, &vars);
    Ok(match kind {
        ConstraintKind::Coincident => "Welded the points together".into(),
        _ => format!("Added the {} relation", kind.label()),
    })
}

/// Drives the radius of the selected circles/arcs, recording it so later
/// edits re-satisfy it. `value: None` locks each arc's current radius in
/// place. The whole constraint component of each arc re-solves with the
/// new radius as the only added target, so welded and tangent neighbours
/// follow the resize.
pub fn constrain_radius(
    doc: &mut Document,
    selection: &[EntityId],
    value: Option<f64>,
) -> Result<String, ConstrainError> {
    let arcs: Vec<EntityId> = selection
        .iter()
        .copied()
        .filter(|&id| arc_of(doc, id).is_some())
        .collect();
    if arcs.is_empty() {
        return Err(format!(
            "Select at least one circle or arc to constrain its radius{}",
            polyline_hint(doc, selection)
        )
        .into());
    }
    if let Some(v) = value
        && (!v.is_finite() || v <= 0.0)
    {
        return Err("Radius must be a positive number".into());
    }
    let mut count = 0;
    for id in arcs {
        let target = match value {
            Some(v) => v,
            None => arc_of(doc, id).expect("classified as arc").radius,
        };
        // Record (or retarget) first so the component lowering itself
        // carries the new value — adding a second scalar target for the
        // same radius would fight the old record.
        let prev = doc
            .constraints
            .iter()
            .find(|c| c.kind == ConstraintKind::Radius && c.a == id)
            .copied();
        doc.add_constraint(SketchConstraint::radius(id, target));
        let CompSketch { mut s, vars, .. } = component_sketch(doc, &[id]);
        let res = s.solve();
        if !res.converged {
            let touched = doc
                .constraints
                .iter()
                .position(|c| c.kind == ConstraintKind::Radius && c.a == id);
            let culprit_idx: Vec<usize> = diagnose_conflict(doc, &[id])
                .into_iter()
                .filter(|&i| Some(i) != touched)
                .collect();
            let conflict = describe_conflict(doc, &culprit_idx, touched);
            let culprits = culprit_entities(doc, &culprit_idx);
            restore_or_remove(doc, prev, |c| c.kind == ConstraintKind::Radius && c.a == id);
            return Err(ConstrainError {
                message: format!(
                    "Could not solve radius {target} against the existing constraints (residual {:.2e}){conflict}",
                    res.residual
                ),
                culprits,
            });
        }
        write_back(doc, &s, &vars);
        count += 1;
    }
    Ok(format!(
        "Constrained the radius of {count} circle(s)/arc(s)"
    ))
}

/// Drives the length of the selected lines, recording it so later edits
/// re-satisfy it. `value: None` locks each line's current length in place.
/// The whole constraint component of each line re-solves with the new
/// length as the only added target, so the line scales about its midpoint
/// and coincident/parallel neighbours follow.
pub fn constrain_distance(
    doc: &mut Document,
    selection: &[EntityId],
    value: Option<f64>,
) -> Result<String, ConstrainError> {
    let lines: Vec<EntityId> = selection
        .iter()
        .copied()
        .filter(|&id| line_of(doc, id).is_some())
        .collect();
    if lines.is_empty() {
        return Err(format!(
            "Select at least one line to constrain its length{}",
            polyline_hint(doc, selection)
        )
        .into());
    }
    if let Some(v) = value
        && (!v.is_finite() || v <= 0.0)
    {
        return Err("Length must be a positive number".into());
    }
    let mut count = 0;
    for id in lines {
        let target = match value {
            Some(v) => v,
            None => len(&line_of(doc, id).expect("classified as line")),
        };
        // Record (or retarget) first so the component lowering itself
        // carries the new value — adding a second distance target for the
        // same line would fight the old record.
        let prev = doc
            .constraints
            .iter()
            .find(|c| c.kind == ConstraintKind::Distance && c.a == id)
            .copied();
        doc.add_constraint(SketchConstraint::distance(id, target));
        let CompSketch { mut s, vars, .. } = component_sketch(doc, &[id]);
        let res = s.solve();
        if !res.converged {
            let touched = doc
                .constraints
                .iter()
                .position(|c| c.kind == ConstraintKind::Distance && c.a == id);
            let culprit_idx: Vec<usize> = diagnose_conflict(doc, &[id])
                .into_iter()
                .filter(|&i| Some(i) != touched)
                .collect();
            let conflict = describe_conflict(doc, &culprit_idx, touched);
            let culprits = culprit_entities(doc, &culprit_idx);
            restore_or_remove(doc, prev, |c| {
                c.kind == ConstraintKind::Distance && c.a == id
            });
            return Err(ConstrainError {
                message: format!(
                    "Could not solve length {target} against the existing constraints (residual {:.2e}){conflict}",
                    res.residual
                ),
                culprits,
            });
        }
        write_back(doc, &s, &vars);
        count += 1;
    }
    Ok(format!("Constrained the length of {count} line(s)"))
}

/// Holds two lines at a driving angle (degrees), recorded so later edits
/// re-satisfy it. The first-selected line is the fixed reference; the
/// second rotates (length kept) to meet the target. `value: None` locks
/// the current angle in place. Lines are undirected, so the recorded
/// angle lives in (0, 180].
pub fn constrain_angle(
    doc: &mut Document,
    selection: &[EntityId],
    value: Option<f64>,
) -> Result<String, ConstrainError> {
    let lines: Vec<(EntityId, LineSeg)> = selection
        .iter()
        .filter_map(|&id| line_of(doc, id).map(|l| (id, l)))
        .collect();
    if lines.len() != 2 {
        return Err(format!(
            "Select exactly two lines to constrain their angle (got {}){}",
            lines.len(),
            polyline_hint(doc, selection)
        )
        .into());
    }
    if let Some(v) = value
        && !v.is_finite()
    {
        return Err("Angle must be a finite number of degrees".into());
    }
    let (ref_id, ref_line) = lines[0].clone();
    let (mov_id, mov_line) = lines[1].clone();
    if ref_id == mov_id {
        return Err("Select two different lines to constrain their angle".into());
    }
    let dir = |l: &LineSeg| (l.p1.y - l.p0.y).atan2(l.p1.x - l.p0.x);
    let target = normalize_angle_deg(match value {
        Some(v) => v,
        None => (dir(&mov_line) - dir(&ref_line)).to_degrees(),
    });

    // Record (or retarget) first and validate against the FULL connected
    // component, like the other pair kinds; restore the previous record on
    // failure — the relation may already have existed with another value.
    let candidate = SketchConstraint::angle(ref_id, mov_id, target);
    let prev = doc
        .constraints
        .iter()
        .find(|c| c.same_relation(&candidate))
        .copied();
    if doc.add_constraint(candidate)
        && let Err(conflict) = validate_recorded(doc, &[ref_id, mov_id], &candidate, true)
    {
        restore_or_remove(doc, prev, |c| c.same_relation(&candidate));
        return Err(ConstrainError {
            message: format!(
                "Could not hold the lines at {target}° against their existing constraints{}",
                conflict.message
            ),
            culprits: conflict.culprits,
        });
    }

    let mut s = Sketch::new();
    let a0 = s.add_point(ref_line.p0.x, ref_line.p0.y);
    let a1 = s.add_point(ref_line.p1.x, ref_line.p1.y);
    let b0 = s.add_point(mov_line.p0.x, mov_line.p0.y);
    let b1 = s.add_point(mov_line.p1.x, mov_line.p1.y);
    s.constrain(Constraint::Fixed(a0, ref_line.p0.x, ref_line.p0.y));
    s.constrain(Constraint::Fixed(a1, ref_line.p1.x, ref_line.p1.y));
    // The mover keeps its length so it rotates rather than collapsing.
    s.constrain(Constraint::Distance(b0, b1, len(&mov_line)));
    s.constrain(Constraint::Angle(a0, a1, b0, b1, target.to_radians()));
    let mut res = s.solve();
    if !res.converged {
        s.set_point(a0, ref_line.p0.x, ref_line.p0.y);
        s.set_point(a1, ref_line.p1.x, ref_line.p1.y);
        perturb_line(&mut s, b0, b1, &mov_line);
        res = s.solve();
    }
    if !res.converged {
        return Err(format!(
            "Could not hold the lines at {target}° (residual {:.2e})",
            res.residual
        )
        .into());
    }
    write_line(doc, mov_id, s.point(b0), s.point(b1));
    // Same as the other pair solves: linked neighbours of the mover are
    // still at their pre-solve positions — drag them back into place.
    resolve_after_transform(doc, &[mov_id]);
    Ok(format!("Held the angle between the lines at {target}°"))
}

/// Holds two parallel lines at a driving perpendicular distance (a width),
/// recorded so later edits re-satisfy it. The first-selected line is the
/// fixed reference; the second slides (direction and length kept) to meet
/// the target. `value: None` locks the current width in place. The lines
/// must already be parallel — dimensioning two crossing lines means an
/// angle, not a width.
pub fn constrain_line_distance(
    doc: &mut Document,
    selection: &[EntityId],
    value: Option<f64>,
) -> Result<String, ConstrainError> {
    let lines: Vec<(EntityId, LineSeg)> = selection
        .iter()
        .filter_map(|&id| line_of(doc, id).map(|l| (id, l)))
        .collect();
    if lines.len() != 2 {
        return Err(format!(
            "Select exactly two parallel lines to constrain their distance (got {}){}",
            lines.len(),
            polyline_hint(doc, selection)
        )
        .into());
    }
    let (ref_id, ref_line) = lines[0].clone();
    let (mov_id, mov_line) = lines[1].clone();
    if ref_id == mov_id {
        return Err("Select two different lines to constrain their distance".into());
    }
    let (ux, uy) = (ref_line.p1.x - ref_line.p0.x, ref_line.p1.y - ref_line.p0.y);
    let (vx, vy) = (mov_line.p1.x - mov_line.p0.x, mov_line.p1.y - mov_line.p0.y);
    let (nu, nv) = (ux.hypot(uy).max(1e-12), vx.hypot(vy).max(1e-12));
    if ((ux * vy - uy * vx) / (nu * nv)).abs() > 1e-7 {
        return Err("The two lines must be parallel to hold a distance between them".into());
    }
    // Perpendicular distance from the mover's midpoint to the reference's
    // infinite line — with parallel lines any point gives the same value.
    let (mx, my) = (
        (mov_line.p0.x + mov_line.p1.x) * 0.5,
        (mov_line.p0.y + mov_line.p1.y) * 0.5,
    );
    let current = ((ux * (my - ref_line.p0.y) - uy * (mx - ref_line.p0.x)) / nu).abs();
    let target = match value {
        Some(v) => v,
        None => current,
    };
    if !target.is_finite() || target <= 0.0 {
        return Err("Distance must be a positive number".into());
    }

    // Record (or retarget) first and validate against the FULL connected
    // component, like the other pair kinds; restore the previous record on
    // failure — the relation may already have existed with another value.
    let candidate = SketchConstraint::line_distance(ref_id, mov_id, target);
    let prev = doc
        .constraints
        .iter()
        .find(|c| c.same_relation(&candidate))
        .copied();
    if doc.add_constraint(candidate)
        && let Err(conflict) = validate_recorded(doc, &[ref_id, mov_id], &candidate, true)
    {
        restore_or_remove(doc, prev, |c| c.same_relation(&candidate));
        return Err(ConstrainError {
            message: format!(
                "Could not hold the lines {target} apart against their existing constraints{}",
                conflict.message
            ),
            culprits: conflict.culprits,
        });
    }

    let mut s = Sketch::new();
    let a0 = s.add_point(ref_line.p0.x, ref_line.p0.y);
    let a1 = s.add_point(ref_line.p1.x, ref_line.p1.y);
    let b0 = s.add_point(mov_line.p0.x, mov_line.p0.y);
    let b1 = s.add_point(mov_line.p1.x, mov_line.p1.y);
    s.constrain(Constraint::Fixed(a0, ref_line.p0.x, ref_line.p0.y));
    s.constrain(Constraint::Fixed(a1, ref_line.p1.x, ref_line.p1.y));
    // The mover keeps its length so it slides rather than collapsing.
    s.constrain(Constraint::Distance(b0, b1, len(&mov_line)));
    s.constrain(Constraint::PointLineDistance(b0, a0, a1, target));
    s.constrain(Constraint::PointLineDistance(b1, a0, a1, target));
    let res = s.solve();
    if !res.converged {
        return Err(format!(
            "Could not hold the lines {target} apart (residual {:.2e})",
            res.residual
        )
        .into());
    }
    write_line(doc, mov_id, s.point(b0), s.point(b1));
    // Same as the other pair solves: linked neighbours of the mover are
    // still at their pre-solve positions — drag them back into place.
    resolve_after_transform(doc, &[mov_id]);
    Ok(format!("Held the lines {target} apart"))
}

/// Welds the centers of two circles/arcs. The first-selected is the fixed
/// reference; the second translates (radius kept) onto its center.
pub fn constrain_concentric(
    doc: &mut Document,
    selection: &[EntityId],
) -> Result<String, ConstrainError> {
    let arcs: Vec<EntityId> = selection
        .iter()
        .copied()
        .filter(|&id| arc_of(doc, id).is_some())
        .collect();
    if arcs.len() != 2 {
        return Err(format!(
            "Select exactly two circles/arcs to make them concentric (got {})",
            arcs.len()
        )
        .into());
    }
    let (ref_id, mov_id) = (arcs[0], arcs[1]);
    let candidate = SketchConstraint::pair(ConstraintKind::Concentric, ref_id, mov_id);
    if !doc.add_constraint(candidate) {
        return Err("Those circles are already concentric".into());
    }
    if let Err(conflict) = validate_recorded(doc, &[ref_id, mov_id], &candidate, false) {
        doc.constraints.retain(|c| !c.same_relation(&candidate));
        return Err(ConstrainError {
            message: format!(
                "Could not make the circles concentric against their existing constraints{}",
                conflict.message
            ),
            culprits: conflict.culprits,
        });
    }
    let a = arc_of(doc, ref_id).expect("classified as arc");
    let b = arc_of(doc, mov_id).expect("classified as arc");
    let mut s = Sketch::new();
    let va = add_arc_vars(&mut s, &a);
    let vb = add_arc_vars(&mut s, &b);
    pin_shape(&mut s, &va);
    let (c1, _) = va.circle().expect("arc vars carry a circle");
    let (c2, r2) = vb.circle().expect("arc vars carry a circle");
    s.constrain(Constraint::FixedScalar(r2, b.radius));
    s.constrain(Constraint::Coincident(c1, c2));
    let res = s.solve();
    if !res.converged {
        doc.constraints.retain(|c| !c.same_relation(&candidate));
        return Err(format!(
            "Could not make the circles concentric (residual {:.2e})",
            res.residual
        )
        .into());
    }
    let mut vars = HashMap::new();
    vars.insert(mov_id, vb);
    write_back(doc, &s, &vars);
    resolve_after_transform(doc, &[mov_id]);
    Ok("Made the second circle concentric with the first".into())
}

/// Holds two circles/arcs at equal radii. The first-selected is the fixed
/// reference; the second resizes about its own center to match.
pub fn constrain_equal_radius(
    doc: &mut Document,
    selection: &[EntityId],
) -> Result<String, ConstrainError> {
    let arcs: Vec<EntityId> = selection
        .iter()
        .copied()
        .filter(|&id| arc_of(doc, id).is_some())
        .collect();
    if arcs.len() != 2 {
        return Err(format!(
            "Select exactly two circles/arcs to equalize their radii (got {})",
            arcs.len()
        )
        .into());
    }
    let (ref_id, mov_id) = (arcs[0], arcs[1]);
    let candidate = SketchConstraint::pair(ConstraintKind::EqualRadius, ref_id, mov_id);
    if !doc.add_constraint(candidate) {
        return Err("Those radii are already held equal".into());
    }
    if let Err(conflict) = validate_recorded(doc, &[ref_id, mov_id], &candidate, false) {
        doc.constraints.retain(|c| !c.same_relation(&candidate));
        return Err(ConstrainError {
            message: format!(
                "Could not equalize the radii against the existing constraints{}",
                conflict.message
            ),
            culprits: conflict.culprits,
        });
    }
    let a = arc_of(doc, ref_id).expect("classified as arc");
    let b = arc_of(doc, mov_id).expect("classified as arc");
    let mut s = Sketch::new();
    let va = add_arc_vars(&mut s, &a);
    let vb = add_arc_vars(&mut s, &b);
    pin_shape(&mut s, &va);
    let (_, r1) = va.circle().expect("arc vars carry a circle");
    let (c2, r2) = vb.circle().expect("arc vars carry a circle");
    let (cx, cy) = s.point(c2);
    s.constrain(Constraint::Fixed(c2, cx, cy));
    s.constrain(Constraint::EqualScalar(r1, r2));
    let res = s.solve();
    if !res.converged {
        doc.constraints.retain(|c| !c.same_relation(&candidate));
        return Err(format!(
            "Could not equalize the radii (residual {:.2e})",
            res.residual
        )
        .into());
    }
    let mut vars = HashMap::new();
    vars.insert(mov_id, vb);
    write_back(doc, &s, &vars);
    resolve_after_transform(doc, &[mov_id]);
    Ok("Made the second radius equal to the first".into())
}

/// Holds two picked point anchors at a driving distance: straight-line
/// ([`ConstraintKind::PointDistance`]) or axis-projected (HDistance /
/// VDistance). `value: None` locks the current separation in place;
/// `place` stores where the dimension annotation was dropped.
pub fn constrain_point_distance(
    doc: &mut Document,
    kind: ConstraintKind,
    a: (EntityId, u8),
    b: (EntityId, u8),
    value: Option<f64>,
    place: Option<(f64, f64)>,
) -> Result<String, ConstrainError> {
    if !matches!(
        kind,
        ConstraintKind::PointDistance | ConstraintKind::HDistance | ConstraintKind::VDistance
    ) {
        return Err("Not a point-distance kind".into());
    }
    let (a_id, ea) = a;
    let (b_id, eb) = b;
    if a_id == b_id && ea == eb {
        return Err("Pick two different points to hold a distance between them".into());
    }
    if !anchor_ok(doc, a_id, ea) || !anchor_ok(doc, b_id, eb) {
        return Err(format!(
            "Pick an endpoint, midpoint, center, or point{}",
            polyline_hint(doc, &[a_id, b_id])
        )
        .into());
    }
    let (ax, ay) = anchor_pos(doc, a_id, ea).ok_or("Could not resolve the first pick")?;
    let (bx, by) = anchor_pos(doc, b_id, eb).ok_or("Could not resolve the second pick")?;
    let current = match kind {
        ConstraintKind::HDistance => (bx - ax).abs(),
        ConstraintKind::VDistance => (by - ay).abs(),
        _ => (bx - ax).hypot(by - ay),
    };
    let target = value.unwrap_or(current);
    if !target.is_finite() || target <= 0.0 {
        return Err(
            "Distance must be a positive number (use horizontal/vertical for zero separation)"
                .into(),
        );
    }
    let mut candidate = SketchConstraint::point_distance(kind, a_id, ea, b_id, eb, target);
    candidate.place = place;
    let prev = doc
        .constraints
        .iter()
        .find(|c| c.same_relation(&candidate))
        .copied();
    if doc.add_constraint(candidate)
        && let Err(conflict) = validate_recorded(doc, &[a_id, b_id], &candidate, false)
    {
        restore_or_remove(doc, prev, |c| c.same_relation(&candidate));
        return Err(ConstrainError {
            message: format!(
                "Could not hold the points {target} apart against their existing constraints{}",
                conflict.message
            ),
            culprits: conflict.culprits,
        });
    }
    let CompSketch { mut s, vars, .. } = component_sketch(doc, &[a_id, b_id]);
    if !s.solve_robust().converged {
        restore_or_remove(doc, prev, |c| c.same_relation(&candidate));
        return Err("Could not hold the points at that distance".into());
    }
    write_back(doc, &s, &vars);
    Ok(format!("Held the points {target} apart"))
}

/// Holds a picked point anchor at a driving perpendicular distance from a
/// line entity. `value: None` locks the current separation in place;
/// `place` stores where the dimension annotation was dropped.
///
/// Distinct from [`constrain_point_distance`] because the second pick is a
/// whole line, not an anchor on one: the distance is to the line's infinite
/// carrier, so which end of it was clicked is irrelevant.
pub fn constrain_point_line_distance(
    doc: &mut Document,
    anchor: (EntityId, u8),
    line: EntityId,
    value: Option<f64>,
    place: Option<(f64, f64)>,
) -> Result<String, ConstrainError> {
    let (a_id, ea) = anchor;
    if a_id == line {
        return Err("Pick a point and a different line to hold it off".into());
    }
    if !anchor_ok(doc, a_id, ea) {
        return Err(format!(
            "Pick an endpoint, midpoint, center, or point{}",
            polyline_hint(doc, &[a_id])
        )
        .into());
    }
    let Some(l) = line_of(doc, line) else {
        return Err(format!(
            "Point-line distance needs a line to measure to{}",
            polyline_hint(doc, &[line])
        )
        .into());
    };
    let (ax, ay) = anchor_pos(doc, a_id, ea).ok_or("Could not resolve the picked point")?;
    let (ux, uy) = (l.p1.x - l.p0.x, l.p1.y - l.p0.y);
    let n = ux.hypot(uy);
    if n <= 1e-9 {
        return Err("That line is too short to measure from".into());
    }
    let current = ((ux * (ay - l.p0.y) - uy * (ax - l.p0.x)) / n).abs();
    let target = value.unwrap_or(current);
    if !target.is_finite() || target <= 0.0 {
        return Err(
            "Distance must be a positive number (use Point on line for zero separation)".into(),
        );
    }
    let mut candidate = SketchConstraint::point_distance(
        ConstraintKind::PointLineDistance,
        a_id,
        ea,
        line,
        0,
        target,
    );
    candidate.place = place;
    let prev = doc
        .constraints
        .iter()
        .find(|c| c.same_relation(&candidate))
        .copied();
    if doc.add_constraint(candidate)
        && let Err(conflict) = validate_recorded(doc, &[a_id, line], &candidate, false)
    {
        restore_or_remove(doc, prev, |c| c.same_relation(&candidate));
        return Err(ConstrainError {
            message: format!(
                "Could not hold the point {target} from the line against its existing constraints{}",
                conflict.message
            ),
            culprits: conflict.culprits,
        });
    }
    let CompSketch { mut s, vars, .. } = component_sketch(doc, &[a_id, line]);
    if !s.solve_robust().converged {
        restore_or_remove(doc, prev, |c| c.same_relation(&candidate));
        return Err("Could not hold the point at that distance from the line".into());
    }
    write_back(doc, &s, &vars);
    Ok(format!("Held the point {target} from the line"))
}

/// Mirrors two picked point anchors about a line: their midpoint is held on
/// the mirror's infinite carrier and their segment perpendicular to it.
/// Both sides move minimally, like the other pick-based relations.
pub fn constrain_symmetric_points(
    doc: &mut Document,
    a: (EntityId, u8),
    b: (EntityId, u8),
    mirror: EntityId,
) -> Result<String, ConstrainError> {
    let (a_id, ea) = a;
    let (b_id, eb) = b;
    if line_of(doc, mirror).is_none() {
        return Err("Symmetric needs a line to mirror about".into());
    }
    if (a_id == b_id && ea == eb) || a_id == mirror || b_id == mirror {
        return Err("Pick two different points, then the mirror line".into());
    }
    if !anchor_ok(doc, a_id, ea) || !anchor_ok(doc, b_id, eb) {
        return Err(format!(
            "Pick an endpoint, midpoint, center, or point{}",
            polyline_hint(doc, &[a_id, b_id])
        )
        .into());
    }
    let candidate = SketchConstraint::symmetric(a_id, ea, b_id, eb, mirror);
    if !doc.add_constraint(candidate) {
        return Err("Those points are already symmetric about that line".into());
    }
    if let Err(conflict) = validate_recorded(doc, &[a_id, b_id, mirror], &candidate, false) {
        doc.constraints.retain(|c| !c.same_relation(&candidate));
        return Err(ConstrainError {
            message: format!(
                "Could not make the points symmetric against their existing constraints{}",
                conflict.message
            ),
            culprits: conflict.culprits,
        });
    }
    let CompSketch { mut s, vars, .. } = component_sketch(doc, &[a_id, b_id, mirror]);
    if !s.solve_robust().converged {
        doc.constraints.retain(|c| !c.same_relation(&candidate));
        return Err("Could not make the points symmetric".into());
    }
    write_back(doc, &s, &vars);
    Ok("Made the points symmetric about the line".into())
}

/// Locks the selection into a rigid group: every member holds its shape and
/// its pose relative to the first-selected entity (the reference), leaving
/// the group only its translate/rotate freedom. Recorded as one Block pair
/// per member; UNCON on any member releases its record.
pub fn constrain_block(
    doc: &mut Document,
    selection: &[EntityId],
) -> Result<String, ConstrainError> {
    let members: Vec<EntityId> = selection
        .iter()
        .copied()
        .filter(|&id| {
            line_of(doc, id).is_some() || arc_of(doc, id).is_some() || point_of(doc, id).is_some()
        })
        .collect();
    if members.len() < 2 {
        return Err(format!(
            "Select at least two lines/arcs/points to block together{}",
            polyline_hint(doc, selection)
        )
        .into());
    }
    let reference = members[0];
    let mut count = 0;
    for &m in &members[1..] {
        let candidate = SketchConstraint::block(m, reference);
        if !doc.add_constraint(candidate) {
            continue;
        }
        if let Err(conflict) = validate_recorded(doc, &[m, reference], &candidate, false) {
            doc.constraints.retain(|c| !c.same_relation(&candidate));
            return Err(ConstrainError {
                message: format!(
                    "Could not block the selection against its existing constraints{}",
                    conflict.message
                ),
                culprits: conflict.culprits,
            });
        }
        count += 1;
    }
    if count == 0 {
        return Err("That selection is already blocked together".into());
    }
    // Blocking freezes everything exactly where it sits, so like Fixed no
    // re-solve is needed — the records are trivially consistent by
    // construction (validated above against pre-existing constraints).
    Ok(format!("Blocked {} object(s) as a rigid group", count + 1))
}

/// World position of anchor `idx` on entity `id`: 0/1 an endpoint,
/// [`ANCHOR_DERIVED`] a line's midpoint or an arc's centre; a point entity
/// is its own anchor at any index. `None` when the entity is neither a
/// point, a line, nor an arc.
pub fn anchor_pos(doc: &Document, id: EntityId, idx: u8) -> Option<(f64, f64)> {
    if let Some(p) = point_of(doc, id) {
        return Some((p.x, p.y));
    }
    if let Some(l) = line_of(doc, id) {
        return Some(if idx == ANCHOR_DERIVED {
            ((l.p0.x + l.p1.x) * 0.5, (l.p0.y + l.p1.y) * 0.5)
        } else {
            endpoint(&l, idx)
        });
    }
    if let Some(a) = arc_of(doc, id) {
        return Some(if idx == ANCHOR_DERIVED {
            (a.center.x, a.center.y)
        } else {
            arc_end_pos(&a, idx)
        });
    }
    None
}

/// Whether the current selection is a valid target for applying `kind`
/// directly — the shared gate between the constraint bar's button enabling
/// and the dispatch itself, so both give the same answer. Pick-based kinds
/// are always `Ok` (they open a pick tool regardless of selection). The
/// `Err` string states what the kind needs.
pub fn selection_validity(
    doc: &Document,
    selection: &[EntityId],
    kind: ConstraintKind,
) -> Result<(), &'static str> {
    let lines = selection
        .iter()
        .filter(|&&id| line_of(doc, id).is_some())
        .count();
    let arcs = selection
        .iter()
        .filter(|&&id| arc_of(doc, id).is_some())
        .count();
    let points = selection
        .iter()
        .filter(|&&id| point_of(doc, id).is_some())
        .count();
    match kind {
        ConstraintKind::Horizontal | ConstraintKind::Vertical | ConstraintKind::Distance => {
            if lines >= 1 {
                Ok(())
            } else {
                Err("Select one or more lines")
            }
        }
        ConstraintKind::Parallel
        | ConstraintKind::Perpendicular
        | ConstraintKind::EqualLength
        | ConstraintKind::Collinear
        | ConstraintKind::Angle
        | ConstraintKind::LineDistance => {
            if lines == 2 {
                Ok(())
            } else {
                Err("Select exactly two lines")
            }
        }
        ConstraintKind::Tangent => {
            if (lines == 1 && arcs == 1) || arcs == 2 {
                Ok(())
            } else {
                Err("Select a line and an arc, or two arcs")
            }
        }
        ConstraintKind::Concentric | ConstraintKind::EqualRadius => {
            if arcs == 2 {
                Ok(())
            } else {
                Err("Select exactly two circles or arcs")
            }
        }
        ConstraintKind::Radius => {
            if arcs >= 1 {
                Ok(())
            } else {
                Err("Select one or more circles or arcs")
            }
        }
        ConstraintKind::Fixed => {
            if lines + arcs + points >= 1 {
                Ok(())
            } else {
                Err("Select lines, arcs, or points")
            }
        }
        ConstraintKind::Block => {
            if lines + arcs + points >= 2 {
                Ok(())
            } else {
                Err("Select two or more lines, arcs, or points")
            }
        }
        // Pick-based kinds open a pick tool; any selection state is fine.
        ConstraintKind::Coincident
        | ConstraintKind::Midpoint
        | ConstraintKind::PointOnLine
        | ConstraintKind::PointOnCircle
        | ConstraintKind::PointDistance
        | ConstraintKind::HDistance
        | ConstraintKind::VDistance
        | ConstraintKind::PointLineDistance
        | ConstraintKind::Symmetric => Ok(()),
    }
}

/// Rolls back a failed valued-constraint record: the previous record is
/// restored when one existed, otherwise the freshly added one (matched by
/// `added`) is removed. Shared by the radius/length/angle commands.
fn restore_or_remove(
    doc: &mut Document,
    prev: Option<SketchConstraint>,
    added: impl Fn(&SketchConstraint) -> bool,
) {
    match prev {
        Some(p) => {
            doc.add_constraint(p);
        }
        None => doc.constraints.retain(|c| !added(c)),
    }
}

/// Solver variables for one entity: a line's two endpoints, or an arc's
/// center + radius (+ endpoint points kept on the rim for partial arcs).
enum ShapeVars {
    Line(PointVar, PointVar),
    Point(PointVar),
    Arc {
        c: PointVar,
        r: ScalarVar,
        ends: Option<(PointVar, PointVar)>,
        orig: CircularArc,
    },
}

/// Solver variables behind one coincident anchor: a point that exists as a
/// variable (`At`), or a line midpoint derived from its two endpoint
/// variables (`Mid`).
#[derive(Clone, Copy)]
enum AnchorVars {
    At(PointVar),
    Mid(PointVar, PointVar),
}

impl ShapeVars {
    fn line(&self) -> Option<(PointVar, PointVar)> {
        match self {
            ShapeVars::Line(a, b) => Some((*a, *b)),
            _ => None,
        }
    }

    fn endpoint(&self, i: u8) -> Option<PointVar> {
        match self {
            ShapeVars::Line(a, b) => Some(if i == 0 { *a } else { *b }),
            ShapeVars::Point(p) => Some(*p),
            ShapeVars::Arc {
                ends: Some((ps, pe)),
                ..
            } => Some(if i == 0 { *ps } else { *pe }),
            _ => None,
        }
    }

    /// The anchor `i` names on this entity: 0/1 endpoints, ANCHOR_DERIVED a
    /// line's midpoint or an arc's center. A point entity is its own anchor
    /// at any index; a full circle has no endpoints, only its center.
    fn anchor(&self, i: u8) -> Option<AnchorVars> {
        match (self, i) {
            (ShapeVars::Point(p), _) => Some(AnchorVars::At(*p)),
            (ShapeVars::Line(a, b), ANCHOR_DERIVED) => Some(AnchorVars::Mid(*a, *b)),
            (ShapeVars::Arc { c, .. }, ANCHOR_DERIVED) => Some(AnchorVars::At(*c)),
            _ if i <= 1 => self.endpoint(i).map(AnchorVars::At),
            _ => None,
        }
    }

    fn circle(&self) -> Option<(PointVar, ScalarVar)> {
        match self {
            ShapeVars::Arc { c, r, .. } => Some((*c, *r)),
            _ => None,
        }
    }

    fn arc_orig(&self) -> Option<&CircularArc> {
        match self {
            ShapeVars::Arc { orig, .. } => Some(orig),
            _ => None,
        }
    }
}

/// Whether two circles, as currently drawn, are closer to internal (nested)
/// than external tangency — the mode the solver should maintain.
fn tangency_is_internal(a: &CircularArc, b: &CircularArc) -> bool {
    let d = (a.center.x - b.center.x).hypot(a.center.y - b.center.y);
    (d - (a.radius - b.radius).abs()).abs() < (d - (a.radius + b.radius)).abs()
}

fn add_arc_vars(s: &mut Sketch, a: &CircularArc) -> ShapeVars {
    let c = s.add_point(a.center.x, a.center.y);
    let r = s.add_scalar(a.radius);
    let ends = (!arc_is_full(a)).then(|| {
        let (sx, sy) = arc_end_pos(a, 0);
        let (ex, ey) = arc_end_pos(a, 1);
        let ps = s.add_point(sx, sy);
        let pe = s.add_point(ex, ey);
        s.constrain(Constraint::PointOnCircle(ps, c, r));
        s.constrain(Constraint::PointOnCircle(pe, c, r));
        (ps, pe)
    });
    ShapeVars::Arc {
        c,
        r,
        ends,
        orig: *a,
    }
}

/// Pins every degree of freedom of the entity where it currently sits.
fn pin_shape(s: &mut Sketch, sv: &ShapeVars) {
    match sv {
        ShapeVars::Line(p0, p1) => {
            for p in [*p0, *p1] {
                let (x, y) = s.point(p);
                s.constrain(Constraint::Fixed(p, x, y));
            }
        }
        ShapeVars::Point(p) => {
            let (x, y) = s.point(*p);
            s.constrain(Constraint::Fixed(*p, x, y));
        }
        ShapeVars::Arc { c, r, ends, .. } => {
            let (cx, cy) = s.point(*c);
            s.constrain(Constraint::Fixed(*c, cx, cy));
            let rv = s.scalar(*r);
            s.constrain(Constraint::FixedScalar(*r, rv));
            if let Some((ps, pe)) = ends {
                for p in [*ps, *pe] {
                    let (x, y) = s.point(p);
                    s.constrain(Constraint::Fixed(p, x, y));
                }
            }
        }
    }
}

/// Anchors every line entity's CURRENT length in `s`, using its
/// `component_sketch` vars. A validation solve checking whether a new
/// pure-angle relation (Horizontal/Vertical/Parallel/Perpendicular) is
/// consistent with everything already recorded must not be allowed to
/// "succeed" merely by collapsing some line in the component to a
/// zero-length point — a numeric solver otherwise treats that as a
/// perfectly valid solution (a degenerate one, but the residuals really
/// are zero), silently hiding a genuine conflict a real geometric solution
/// never has this escape from. Not used for EqualLength, whose whole point
/// is to change a length.
fn anchor_line_lengths(s: &mut Sketch, doc: &Document, vars: &HashMap<EntityId, ShapeVars>) {
    // Every line, unconditionally — this is the validation sketch, where the
    // point is to detect a solve that "succeeded" only by degenerating some
    // line. Distinct from `anchor_line_lengths_except`, which is selective
    // because it runs on sketches that then WRITE geometry back.
    for (&id, sv) in vars {
        if let (Some((a, b)), Some(l)) = (sv.line(), line_of(doc, id)) {
            s.constrain(Constraint::Distance(a, b, len(&l)));
        }
    }
}

/// [`anchor_line_lengths`], skipping the entities the user is directly
/// editing. Their length is legitimately free — dragging one endpoint of a
/// line is meant to change its length — so anchoring them would fight the
/// edit. Only the *followers*, which should rotate to satisfy an angular
/// relation rather than shorten, get pinned lengths.
fn anchor_line_lengths_except(
    s: &mut Sketch,
    doc: &Document,
    vars: &HashMap<EntityId, ShapeVars>,
    edited: &[EntityId],
) {
    for (&id, sv) in vars {
        if edited.contains(&id) {
            continue;
        }
        // Only lines held by a purely ANGULAR relation get their length
        // pinned. Those are the ones whose residual is minimised by
        // projection, so they shorten instead of rotating. A line positioned
        // by Coincident/Midpoint/PointOnLine has no such failure mode, and its
        // length must stay free — it is welded to geometry that is moving, and
        // stretching to follow is the correct behaviour.
        let angular = doc.constraints_on(id).any(|c| {
            matches!(
                c.kind,
                ConstraintKind::Parallel
                    | ConstraintKind::Perpendicular
                    | ConstraintKind::Collinear
                    | ConstraintKind::Angle
            )
        });
        if !angular {
            continue;
        }
        if let (Some((a, b)), Some(l)) = (sv.line(), line_of(doc, id)) {
            s.constrain(Constraint::Distance(a, b, len(&l)));
        }
    }
}

/// Whether a validation solve reached its answer by degenerating some line:
/// any line now under a thousandth of its recorded length counts as the
/// zero-length escape, never as a real geometric solution.
fn any_line_collapsed(s: &Sketch, doc: &Document, vars: &HashMap<EntityId, ShapeVars>) -> bool {
    vars.iter().any(|(&id, sv)| {
        let (Some((a, b)), Some(l)) = (sv.line(), line_of(doc, id)) else {
            return false;
        };
        let (ax, ay) = s.point(a);
        let (bx, by) = s.point(b);
        (bx - ax).hypot(by - ay) < len(&l) * 1e-3
    })
}

/// Validates a just-recorded `candidate` against the full connected
/// component of everything it touches, deciding "consistent" vs "genuine
/// conflict" in two passes:
///
/// 1. With every line's current length anchored (`anchor: true`, the
///    pure-angle and width kinds) — the strict check; when it solves,
///    nothing even needed to stretch. The anchors exist because a numeric
///    solver otherwise treats collapsing a line to a zero-length point as a
///    perfectly valid solution, silently hiding a real conflict.
/// 2. Without the anchors — a relation that failed pass 1 only because it
///    legitimately needs some line's length to change (a slanted edge welded
///    between two rails must shorten to become vertical; retargeting a width
///    stretches the sides that span it) is accepted here, provided the
///    solver didn't take the zero-length escape.
///
/// On a genuine conflict, returns the diagnosed culprit clause (as the
/// error's message, for the caller to fold into its own) plus the entities
/// carrying the conflicting constraints; the caller unwinds its own record
/// (remove, or restore the previous value it displaced).
fn validate_recorded(
    doc: &Document,
    seeds: &[EntityId],
    candidate: &SketchConstraint,
    anchor: bool,
) -> Result<(), ConstrainError> {
    let CompSketch {
        s,
        vars,
        intrinsic,
        constraint_doc_idx,
    } = component_sketch(doc, seeds);
    let mut strict = s.clone();
    if anchor {
        anchor_line_lengths(&mut strict, doc, &vars);
    }
    let initial = strict.snapshot();
    if strict.solve_robust().converged {
        // The anchored pass can't collapse anything by construction; an
        // un-anchored pass (EqualLength/Coincident) must still be checked.
        if anchor || !any_line_collapsed(&strict, doc, &vars) {
            return Ok(());
        }
    } else if anchor {
        let mut free = s;
        if free.solve_robust().converged && !any_line_collapsed(&free, doc, &vars) {
            return Ok(());
        }
    }
    let touched = doc
        .constraints
        .iter()
        .position(|c| c.same_relation(candidate));
    let culprit_idx: Vec<usize> = strict
        .diagnose_conflict(&initial)
        .culprits
        .into_iter()
        .filter_map(|i| {
            i.checked_sub(intrinsic)
                .and_then(|i| constraint_doc_idx.get(i).copied())
        })
        .filter(|&i| Some(i) != touched)
        .collect();
    Err(ConstrainError {
        message: describe_conflict(doc, &culprit_idx, touched),
        culprits: culprit_entities(doc, &culprit_idx),
    })
}

struct CompSketch {
    s: Sketch,
    vars: HashMap<EntityId, ShapeVars>,
    /// Number of intrinsic solver constraints emitted while building the
    /// shape variables themselves (an arc's endpoints riding its circle),
    /// before any document constraint was lowered. Solver constraint `i`
    /// maps to `constraint_doc_idx[i - intrinsic]`; indices below
    /// `intrinsic` have no document counterpart.
    intrinsic: usize,
    /// Parallel to `s`'s constraint insertion order *after* the intrinsic
    /// prefix: `constraint_doc_idx[i]` is the index into `doc.constraints`
    /// that solver constraint `intrinsic + i` came from. Lets diagnostics
    /// computed on `s` (redundant rows, conflict culprits) name the
    /// document constraint responsible.
    constraint_doc_idx: Vec<usize>,
}

/// Builds one sketch covering the connected component(s) of the constraint
/// graph reachable from the seeds, with every recorded constraint on those
/// entities lowered into solver form.
fn component_sketch(doc: &Document, seeds: &[EntityId]) -> CompSketch {
    let mut comp: Vec<EntityId> = Vec::new();
    for &id in seeds {
        if !comp.contains(&id) {
            comp.push(id);
        }
    }
    let mut grew = true;
    while grew {
        grew = false;
        for c in &doc.constraints {
            // A constraint is an edge over every entity it references —
            // {a, b} for pairs, plus the mirror line for Symmetric — so one
            // member being in pulls the rest in.
            let members: Vec<EntityId> = [Some(c.a), c.b, c.c].into_iter().flatten().collect();
            if members.len() < 2 {
                continue;
            }
            if members.iter().any(|m| comp.contains(m)) {
                for &m in &members {
                    if !comp.contains(&m) {
                        comp.push(m);
                        grew = true;
                    }
                }
            }
        }
    }

    let mut s = Sketch::new();
    let mut vars: HashMap<EntityId, ShapeVars> = HashMap::new();
    for &id in &comp {
        if let Some(l) = line_of(doc, id) {
            let p0 = s.add_point(l.p0.x, l.p0.y);
            let p1 = s.add_point(l.p1.x, l.p1.y);
            vars.insert(id, ShapeVars::Line(p0, p1));
        } else if let Some(a) = arc_of(doc, id) {
            let sv = add_arc_vars(&mut s, &a);
            vars.insert(id, sv);
        } else if let Some(p) = point_of(doc, id) {
            let pv = s.add_point(p.x, p.y);
            vars.insert(id, ShapeVars::Point(pv));
        }
    }

    // Everything constrained so far is intrinsic to the shapes (arc
    // endpoints on their circles) — document constraints start after it.
    let intrinsic = s.constraint_count();
    let mut constraint_doc_idx = Vec::new();
    for (doc_idx, c) in doc.constraints.iter().enumerate() {
        let Some(sa) = vars.get(&c.a) else {
            continue;
        };
        let sb = c.b.and_then(|b| vars.get(&b));
        match c.kind {
            ConstraintKind::Fixed => {
                // Pin every degree of freedom of the entity where it sits — a
                // user Fix locks a whole line/arc, not just one endpoint (the
                // origin, a point, pins to a single Fixed either way). One doc
                // constraint lowers to several solver rows, so record `doc_idx`
                // for each so diagnostics stay aligned.
                let before = s.constraint_count();
                pin_shape(&mut s, sa);
                for _ in before..s.constraint_count() {
                    constraint_doc_idx.push(doc_idx);
                }
            }
            ConstraintKind::Horizontal | ConstraintKind::Vertical => {
                let Some((a0, a1)) = sa.line() else { continue };
                s.constrain(match c.kind {
                    ConstraintKind::Horizontal => Constraint::Horizontal(a0, a1),
                    _ => Constraint::Vertical(a0, a1),
                });
                constraint_doc_idx.push(doc_idx);
            }
            ConstraintKind::Parallel
            | ConstraintKind::Perpendicular
            | ConstraintKind::EqualLength => {
                let (Some((a0, a1)), Some((b0, b1))) = (sa.line(), sb.and_then(|v| v.line()))
                else {
                    continue;
                };
                s.constrain(match c.kind {
                    ConstraintKind::Parallel => Constraint::Parallel(a0, a1, b0, b1),
                    ConstraintKind::Perpendicular => Constraint::Perpendicular(a0, a1, b0, b1),
                    _ => Constraint::EqualLength(a0, a1, b0, b1),
                });
                constraint_doc_idx.push(doc_idx);
            }
            ConstraintKind::Coincident => {
                let Some((ea, eb)) = c.pts else { continue };
                let (Some(aa), Some(ab)) = (sa.anchor(ea), sb.and_then(|v| v.anchor(eb))) else {
                    continue;
                };
                s.constrain(match (aa, ab) {
                    (AnchorVars::At(pa), AnchorVars::At(pb)) => Constraint::Coincident(pa, pb),
                    (AnchorVars::At(pa), AnchorVars::Mid(b0, b1)) => {
                        Constraint::MidpointsCoincident(pa, pa, b0, b1)
                    }
                    (AnchorVars::Mid(a0, a1), AnchorVars::At(pb)) => {
                        Constraint::MidpointsCoincident(a0, a1, pb, pb)
                    }
                    (AnchorVars::Mid(a0, a1), AnchorVars::Mid(b0, b1)) => {
                        Constraint::MidpointsCoincident(a0, a1, b0, b1)
                    }
                });
                constraint_doc_idx.push(doc_idx);
            }
            ConstraintKind::Radius => {
                let (Some((_, r)), Some(v)) = (sa.circle(), c.val) else {
                    continue;
                };
                s.constrain(Constraint::FixedScalar(r, v));
                constraint_doc_idx.push(doc_idx);
            }
            ConstraintKind::Distance => {
                let (Some((a0, a1)), Some(v)) = (sa.line(), c.val) else {
                    continue;
                };
                s.constrain(Constraint::Distance(a0, a1, v));
                constraint_doc_idx.push(doc_idx);
            }
            ConstraintKind::LineDistance => {
                // Both of b's endpoints held at the width from a's infinite
                // line. The distance residual is |signed distance| − v, so
                // the two rows alone have a second satisfied state with the
                // endpoints on OPPOSITE sides of a (b crossing it) — an
                // explicit Parallel row excludes it. In the intended
                // same-side state that row is linearly dependent, so the
                // DOF/rank accounting is unchanged. Three solver rows for
                // one record, so log doc_idx three times.
                let (Some((a0, a1)), Some((b0, b1)), Some(v)) =
                    (sa.line(), sb.and_then(|s| s.line()), c.val)
                else {
                    continue;
                };
                s.constrain(Constraint::PointLineDistance(b0, a0, a1, v));
                s.constrain(Constraint::PointLineDistance(b1, a0, a1, v));
                s.constrain(Constraint::Parallel(a0, a1, b0, b1));
                constraint_doc_idx.push(doc_idx);
                constraint_doc_idx.push(doc_idx);
                constraint_doc_idx.push(doc_idx);
            }
            ConstraintKind::Angle => {
                let (Some((a0, a1)), Some((b0, b1)), Some(v)) =
                    (sa.line(), sb.and_then(|s| s.line()), c.val)
                else {
                    continue;
                };
                // Document::add_constraint already folded this into (0, 180],
                // so the value is safe to hand straight to the solver.
                s.constrain(Constraint::Angle(a0, a1, b0, b1, v.to_radians()));
                constraint_doc_idx.push(doc_idx);
            }
            ConstraintKind::Tangent => {
                let Some(sb) = sb else { continue };
                match (sa.line(), sb.line(), sa.circle(), sb.circle()) {
                    (Some(l), _, _, Some(k)) | (_, Some(l), Some(k), _) => {
                        s.constrain(Constraint::TangentLineCircle(l.0, l.1, k.0, k.1));
                    }
                    (_, _, Some(k1), Some(k2)) => {
                        let (Some(oa), Some(ob)) = (sa.arc_orig(), sb.arc_orig()) else {
                            continue;
                        };
                        s.constrain(Constraint::TangentCircleCircle {
                            c1: k1.0,
                            r1: k1.1,
                            c2: k2.0,
                            r2: k2.1,
                            internal: tangency_is_internal(oa, ob),
                        });
                    }
                    _ => continue,
                }
                constraint_doc_idx.push(doc_idx);
            }
            ConstraintKind::Concentric => {
                let (Some((ca, _)), Some((cb, _))) = (sa.circle(), sb.and_then(|v| v.circle()))
                else {
                    continue;
                };
                // One entry per `constrain` call, NOT per residual row.
                // Coincident does emit two rows, but `analyze().redundant` and
                // `diagnose_conflict().culprits` index by constraint, so an
                // extra entry here shifted every later constraint's mapping —
                // the properties panel then put the "redundant" badge on the
                // wrong row and conflicts named the wrong kind.
                s.constrain(Constraint::Coincident(ca, cb));
                constraint_doc_idx.push(doc_idx);
            }
            ConstraintKind::Collinear => {
                let (Some((a0, a1)), Some((b0, b1))) = (sa.line(), sb.and_then(|v| v.line()))
                else {
                    continue;
                };
                s.constrain(Constraint::PointOnLine(b0, a0, a1));
                s.constrain(Constraint::PointOnLine(b1, a0, a1));
                constraint_doc_idx.push(doc_idx);
                constraint_doc_idx.push(doc_idx);
            }
            ConstraintKind::EqualRadius => {
                let (Some((_, ra)), Some((_, rb))) = (sa.circle(), sb.and_then(|v| v.circle()))
                else {
                    continue;
                };
                s.constrain(Constraint::EqualScalar(ra, rb));
                constraint_doc_idx.push(doc_idx);
            }
            ConstraintKind::Midpoint => {
                // A point anchor welded to a line's midpoint — the same
                // relation Coincident-with-derived-anchor lowers to; the
                // distinct kind exists for its own label and badge.
                let Some((ea, _)) = c.pts else { continue };
                let (Some(aa), Some((b0, b1))) = (sa.anchor(ea), sb.and_then(|v| v.line())) else {
                    continue;
                };
                s.constrain(match aa {
                    AnchorVars::At(pa) => Constraint::MidpointsCoincident(pa, pa, b0, b1),
                    AnchorVars::Mid(a0, a1) => Constraint::MidpointsCoincident(a0, a1, b0, b1),
                });
                // One entry per `constrain` call — see the Coincident arm.
                constraint_doc_idx.push(doc_idx);
            }
            ConstraintKind::PointOnLine => {
                let Some((ea, _)) = c.pts else { continue };
                let (Some(pa), Some((b0, b1))) = (
                    anchor_point_var(&mut s, sa, ea, doc_idx, &mut constraint_doc_idx),
                    sb.and_then(|v| v.line()),
                ) else {
                    continue;
                };
                s.constrain(Constraint::PointOnLine(pa, b0, b1));
                constraint_doc_idx.push(doc_idx);
            }
            ConstraintKind::PointOnCircle => {
                let Some((ea, _)) = c.pts else { continue };
                let (Some(pa), Some((cb, rb))) = (
                    anchor_point_var(&mut s, sa, ea, doc_idx, &mut constraint_doc_idx),
                    sb.and_then(|v| v.circle()),
                ) else {
                    continue;
                };
                s.constrain(Constraint::PointOnCircle(pa, cb, rb));
                constraint_doc_idx.push(doc_idx);
            }
            ConstraintKind::PointDistance
            | ConstraintKind::HDistance
            | ConstraintKind::VDistance => {
                let (Some((ea, eb)), Some(v)) = (c.pts, c.val) else {
                    continue;
                };
                let (Some(pa), Some(pb)) = (
                    anchor_point_var(&mut s, sa, ea, doc_idx, &mut constraint_doc_idx),
                    sb.and_then(|svb| {
                        anchor_point_var(&mut s, svb, eb, doc_idx, &mut constraint_doc_idx)
                    }),
                ) else {
                    continue;
                };
                s.constrain(match c.kind {
                    ConstraintKind::HDistance => Constraint::HorizontalDistance(pa, pb, v),
                    ConstraintKind::VDistance => Constraint::VerticalDistance(pa, pb, v),
                    _ => Constraint::Distance(pa, pb, v),
                });
                constraint_doc_idx.push(doc_idx);
            }
            ConstraintKind::Symmetric => {
                let Some((ea, eb)) = c.pts else { continue };
                let sm = c.c.and_then(|m| vars.get(&m));
                let (Some(pa), Some(pb), Some((m0, m1))) = (
                    anchor_point_var(&mut s, sa, ea, doc_idx, &mut constraint_doc_idx),
                    sb.and_then(|svb| {
                        anchor_point_var(&mut s, svb, eb, doc_idx, &mut constraint_doc_idx)
                    }),
                    sm.and_then(|v| v.line()),
                ) else {
                    continue;
                };
                // Midpoint of the pair on the mirror line, and the pair's
                // segment perpendicular to it: reflection, in two rows.
                s.constrain(Constraint::MidpointOnLine(pa, pb, m0, m1));
                s.constrain(Constraint::Perpendicular(pa, pb, m0, m1));
                constraint_doc_idx.push(doc_idx);
                constraint_doc_idx.push(doc_idx);
            }
            ConstraintKind::Block => {
                let Some(sb) = sb else { continue };
                let before = s.constraint_count();
                lower_block(&mut s, sa, sb);
                for _ in before..s.constraint_count() {
                    constraint_doc_idx.push(doc_idx);
                }
            }
            ConstraintKind::PointLineDistance => {
                // PointOnLine with a non-zero gap. `PointLineDistance` is the
                // same primitive `LineDistance` is built from -- there it is
                // applied to both endpoints of a line, here to one anchor.
                let (Some((ea, _)), Some(v)) = (c.pts, c.val) else {
                    continue;
                };
                let (Some(pa), Some((b0, b1))) = (
                    anchor_point_var(&mut s, sa, ea, doc_idx, &mut constraint_doc_idx),
                    sb.and_then(|v| v.line()),
                ) else {
                    continue;
                };
                s.constrain(Constraint::PointLineDistance(pa, b0, b1, v));
                constraint_doc_idx.push(doc_idx);
            }
        }
    }
    CompSketch {
        s,
        vars,
        intrinsic,
        constraint_doc_idx,
    }
}

/// Up to two representative points of an entity for the Block lowering:
/// enough, together with the entity's own rigidity, to pin its pose
/// relative to another entity's pair.
fn block_points(sv: &ShapeVars) -> Vec<PointVar> {
    match sv {
        ShapeVars::Line(p0, p1) => vec![*p0, *p1],
        ShapeVars::Point(p) => vec![*p],
        ShapeVars::Arc { c, ends, .. } => match ends {
            Some((ps, _)) => vec![*c, *ps],
            None => vec![*c],
        },
    }
}

/// Freezes an entity's own shape at its current dimensions (not its pose):
/// a line keeps its length, an arc its radius and chord.
fn rigidify(s: &mut Sketch, sv: &ShapeVars) {
    match sv {
        ShapeVars::Line(p0, p1) => {
            let (x0, y0) = s.point(*p0);
            let (x1, y1) = s.point(*p1);
            s.constrain(Constraint::Distance(*p0, *p1, (x1 - x0).hypot(y1 - y0)));
        }
        ShapeVars::Point(_) => {}
        ShapeVars::Arc { r, ends, .. } => {
            let rv = s.scalar(*r);
            s.constrain(Constraint::FixedScalar(*r, rv));
            if let Some((ps, pe)) = ends {
                let (sx, sy) = s.point(*ps);
                let (ex, ey) = s.point(*pe);
                s.constrain(Constraint::Distance(*ps, *pe, (ex - sx).hypot(ey - sy)));
            }
        }
    }
}

/// Lowers one Block record: both entities rigid in themselves, plus every
/// pairwise distance between their representative points frozen at its
/// current value — the pair moves as one rigid body (all members share the
/// same reference, so the whole group does). Targets are read from the
/// current geometry, the same "as it currently sits" semantics `Fixed`
/// uses; every converged solve preserves them exactly.
fn lower_block(s: &mut Sketch, sa: &ShapeVars, sb: &ShapeVars) {
    rigidify(s, sa);
    rigidify(s, sb);
    for &p in &block_points(sa) {
        for &q in &block_points(sb) {
            let (px, py) = s.point(p);
            let (qx, qy) = s.point(q);
            s.constrain(Constraint::Distance(p, q, (qx - px).hypot(qy - py)));
        }
    }
}

/// The solver point behind anchor `idx` of `sv`. An `At` anchor is its var
/// directly; a `Mid` anchor (a line's midpoint) has no var of its own, so
/// an auxiliary point is created and welded to the midpoint (two extra
/// rows, logged against the same `doc_idx` so diagnostics stay aligned).
fn anchor_point_var(
    s: &mut Sketch,
    sv: &ShapeVars,
    idx: u8,
    doc_idx: usize,
    constraint_doc_idx: &mut Vec<usize>,
) -> Option<PointVar> {
    match sv.anchor(idx)? {
        AnchorVars::At(p) => Some(p),
        AnchorVars::Mid(a0, a1) => {
            let (x0, y0) = s.point(a0);
            let (x1, y1) = s.point(a1);
            let aux = s.add_point((x0 + x1) * 0.5, (y0 + y1) * 0.5);
            s.constrain(Constraint::MidpointsCoincident(aux, aux, a0, a1));
            // One entry per `constrain` call — see the Coincident arm. This
            // auxiliary is why a midpoint-anchored constraint (a PointOnLine
            // onto a line's midpoint, say) shifted every later diagnostic.
            constraint_doc_idx.push(doc_idx);
            Some(aux)
        }
    }
}

/// Degrees of freedom remaining, and which recorded constraints are
/// numerically redundant, for the connected constraint component containing
/// `seeds`. Indices in `redundant` are into `doc.constraints`.
pub struct DofSummary {
    pub dof: usize,
    pub redundant: Vec<usize>,
    /// Geometric entities (curves and points) in the document that no
    /// constraint references at all. They are invisible to `dof` — the
    /// component sketch never includes them — so a caller reporting
    /// "fully constrained" must also check that this is zero, or the claim
    /// silently ignores entirely free geometry.
    pub free_entities: usize,
}

/// Reports the DOF/redundancy state of the constraint component reachable
/// from `seeds` — e.g. for a selection, "how much freedom does this shape
/// still have, and is anything on it redundant."
pub fn dof_report(doc: &Document, seeds: &[EntityId]) -> DofSummary {
    let CompSketch {
        s,
        intrinsic,
        constraint_doc_idx,
        ..
    } = component_sketch(doc, seeds);
    let report = s.analyze();
    let redundant = report
        .redundant
        .into_iter()
        .filter_map(|i| {
            i.checked_sub(intrinsic)
                .and_then(|i| constraint_doc_idx.get(i).copied())
        })
        .collect();
    let free_entities = doc
        .iter()
        .filter(|e| {
            matches!(e.kind, EntityKind::Curve(_) | EntityKind::Point(_))
                && doc.constraints_on(e.id).next().is_none()
        })
        .count();
    DofSummary {
        dof: report.dof,
        redundant,
        free_entities,
    }
}

/// Only useful right after an action on `seeds` failed to solve (before any
/// rollback restores prior geometry — this rebuilds the component from
/// whatever `doc` holds right now and replays the same starting point).
/// Returns the `doc.constraints` indices whose removal alone would let the
/// rest converge: the leading suspects in a contradictory constraint set.
/// Empty when the component isn't actually failing (nothing to diagnose).
pub fn diagnose_conflict(doc: &Document, seeds: &[EntityId]) -> Vec<usize> {
    let CompSketch {
        mut s,
        intrinsic,
        constraint_doc_idx,
        ..
    } = component_sketch(doc, seeds);
    let initial = s.snapshot();
    if s.solve().converged {
        return Vec::new();
    }
    s.diagnose_conflict(&initial)
        .culprits
        .into_iter()
        .filter_map(|i| {
            i.checked_sub(intrinsic)
                .and_then(|i| constraint_doc_idx.get(i).copied())
        })
        .collect()
}

/// Turns `diagnose_conflict`'s culprit indices into a short clause to append
/// to an error message, e.g. "; conflicts with its existing perpendicular
/// constraint". `exclude` drops the constraint whose own addition triggered
/// the diagnosis (it always shows up as a trivial culprit — removing what
/// you just tried to add naturally "fixes" the solve — and isn't useful to
/// name). Empty once that's excluded, and there's nothing else to blame.
fn describe_conflict(doc: &Document, culprits: &[usize], exclude: Option<usize>) -> String {
    let mut labels: Vec<&str> = culprits
        .iter()
        .filter(|&&i| Some(i) != exclude)
        .filter_map(|&i| doc.constraints.get(i))
        .map(|c| c.kind.label())
        .collect();
    labels.sort_unstable();
    labels.dedup();
    if labels.is_empty() {
        return String::new();
    }
    format!(
        "; conflicts with its existing {} constraint{}",
        labels.join("/"),
        if labels.len() > 1 { "s" } else { "" }
    )
}

/// Re-satisfies the recorded constraints after `moved` was edited, solving
/// the connected component of the constraint graph that contains it. The
/// dragged endpoint (or the whole entity when `pinned_endpoint` is `None`,
/// and always for arcs) is pinned where the user put it; everything else
/// moves minimally.
///
/// Returns `false` when the component could not be solved, in which case no
/// geometry beyond the caller's original edit is touched.
pub fn resolve_after_edit(
    doc: &mut Document,
    moved: EntityId,
    pinned_endpoint: Option<usize>,
) -> bool {
    if doc.constraints_on(moved).next().is_none() {
        return true;
    }
    let CompSketch { mut s, vars, .. } = component_sketch(doc, &[moved]);
    let Some(sv) = vars.get(&moved) else {
        return true;
    };
    match (sv, pinned_endpoint) {
        (ShapeVars::Line(m0, m1), Some(i)) => {
            let pv = if i == 1 { *m1 } else { *m0 };
            let (x, y) = s.point(pv);
            s.constrain(Constraint::Fixed(pv, x, y));
        }
        _ => pin_shape(&mut s, sv),
    }
    // Without this the purely angular relations (Parallel, Perpendicular,
    // Collinear, Angle) are least-squares-minimised by the ORTHOGONAL
    // PROJECTION of the follower onto the target direction, not by rotating
    // it — so every re-solve shortened it by |v|·(1−cos θ) and a 90° edit
    // collapsed it to a zero-length segment at its own midpoint, while this
    // returned `true` and wrote it to the document. Record time already pairs
    // each angular row with a Distance row for exactly this reason; the
    // re-solve path lowered the angular row alone.
    anchor_line_lengths_except(&mut s, doc, &vars, &[moved]);
    if !s.solve_robust().converged {
        return false;
    }
    write_back(doc, &s, &vars);
    true
}

/// Like [`resolve_after_edit`], for callers that have just directly
/// rewritten `moved`'s own geometry (a grip drag, a typed coordinate) rather
/// than passing it in as a stable anchor for some *other* entity's edit —
/// `resolve_after_edit` itself can't tell those two cases apart, since both
/// look identical from inside it: `moved`'s current position, whatever put
/// it there. That's exactly right for an anchor, which never actually
/// moved, and exactly wrong when `moved` is itself `Fixed` and is the thing
/// that just moved — `component_sketch` would seed the solver from the
/// freshly edited position and `Fixed` would trivially satisfy itself right
/// there, letting a fixed point, line, or arc drag itself anywhere. Reject
/// up front instead, before the solve ever runs.
pub fn resolve_after_direct_edit(
    doc: &mut Document,
    moved: EntityId,
    pinned_endpoint: Option<usize>,
) -> bool {
    if doc
        .constraints_on(moved)
        .any(|c| c.kind == ConstraintKind::Fixed)
    {
        return false;
    }
    resolve_after_edit(doc, moved, pinned_endpoint)
}

/// Re-satisfies constraints after a whole-entity transform (move/rotate/
/// scale) of `moved`. Every moved entity is pinned where the user put it;
/// constrained neighbours outside the moved set follow. Returns `false`
/// when the constraints cannot be satisfied with the moved geometry pinned
/// (for example rotating a horizontal-constrained line); the transform is
/// kept as the user made it.
/// [`resolve_after_transform`] for a transform the caller still has in hand,
/// carrying `Block` groups along with it.
///
/// `Block` freezes its members' relative arrangement, but the solver rows for
/// that are derived from the geometry *as it stands when the sketch is built*
/// — which, on this path, is already after the user's edit. Every residual is
/// therefore zero on arrival, the solve ends in no iterations, and the
/// un-moved members stay put while the moved one walks away: the group
/// silently deforms, and is then permanently redefined around the deformed
/// shape. `SketchConstraint` stores a single `Option<f64>`, so there is
/// nowhere to have recorded the original arrangement to restore to.
///
/// Applying `xf` to the rest of the group first is what "rigid" actually
/// means, and needs no stored geometry. The solve then runs as usual, so any
/// other constraints on those entities still get their say.
pub fn resolve_after_transform_rigid(
    doc: &mut Document,
    moved: &[EntityId],
    xf: &oxidraft_geometry::Transform2d,
) -> bool {
    if xf.is_finite() {
        let followers = rigid_group_followers(doc, moved);
        for id in followers {
            if let Some(e) = doc.get_mut(id) {
                e.transform(xf);
            }
        }
    }
    resolve_after_transform(doc, moved)
}

/// Entities that must travel with `moved` because a `Block` ties them to it.
///
/// The walk is transitive, which is not a refinement but the whole job:
/// `constrain_lines` records an N-entity Block as a *star* of pair
/// constraints hubbed on the first entity, so from any member except that hub
/// the rest of the group is two hops away. A single-hop search driven from
/// the middle of a three-line group moves the hub and abandons the third
/// line — the same silent deformation this function exists to prevent.
///
/// `Fixed` is deliberately not a source of followers, and acts as a wall: it
/// is a single-entity pin, and `pin_shape` freezes the entity wherever it
/// currently sits, so moving it would not fight the transform — it would
/// silently re-pin it at the new place and lose the user's anchor. Nothing
/// beyond it travels either, which keeps the pin meaningful locally rather
/// than tearing the group open across it.
fn rigid_group_followers(doc: &Document, moved: &[EntityId]) -> Vec<EntityId> {
    let pinned = |id: EntityId| {
        doc.constraints_on(id)
            .any(|p| p.kind == ConstraintKind::Fixed && p.a == id)
    };
    let mut out: Vec<EntityId> = Vec::new();
    let mut queue: Vec<EntityId> = moved.to_vec();
    let mut seen: Vec<EntityId> = moved.to_vec();
    while let Some(id) = queue.pop() {
        for c in doc.constraints_on(id) {
            if c.kind != ConstraintKind::Block {
                continue;
            }
            for other in [Some(c.a), c.b, c.c].into_iter().flatten() {
                if seen.contains(&other) {
                    continue;
                }
                seen.push(other);
                if !pinned(other) {
                    out.push(other);
                    queue.push(other);
                }
            }
        }
    }
    out
}

pub fn resolve_after_transform(doc: &mut Document, moved: &[EntityId]) -> bool {
    let seeds: Vec<EntityId> = moved
        .iter()
        .copied()
        .filter(|&id| doc.constraints_on(id).next().is_some())
        .collect();
    if seeds.is_empty() {
        return true;
    }
    let CompSketch { mut s, vars, .. } = component_sketch(doc, &seeds);
    for id in moved {
        if let Some(sv) = vars.get(id) {
            pin_shape(&mut s, sv);
        }
    }
    // solve_robust, not solve: pure-angle residuals (Parallel,
    // Perpendicular, ...) have an exact saddle 90° from the target, and a
    // transform can land geometry exactly there (axis-aligned drawings do
    // this routinely). Plain solve() then reports failure and the recorded
    // constraint is silently left violated with no user signal. Every
    // record-time sibling already retries from a perturbed start.
    //
    // Anchor lengths for the same reason as `resolve_after_edit`: an angular
    // residual alone is minimised by projecting the follower onto the target
    // direction, which shortens it every solve and annihilates it outright at
    // 90°. Pinned entities are already fully constrained, so anchoring their
    // length is redundant rather than conflicting.
    anchor_line_lengths_except(&mut s, doc, &vars, moved);
    if !s.solve_robust().converged {
        return false;
    }
    write_back(doc, &s, &vars);
    true
}

fn write_back(doc: &mut Document, s: &Sketch, vars: &HashMap<EntityId, ShapeVars>) {
    for (&id, sv) in vars {
        match sv {
            ShapeVars::Line(p0, p1) => write_line(doc, id, s.point(*p0), s.point(*p1)),
            ShapeVars::Point(p) => {
                let (x, y) = s.point(*p);
                if let Some(e) = doc.get_mut(id) {
                    e.kind = EntityKind::Point(Point2d::from_f64(x, y));
                }
            }
            ShapeVars::Arc { c, r, ends, orig } => {
                let (cx, cy) = s.point(*c);
                let radius = s.scalar(*r);
                if radius <= 1e-9 {
                    continue;
                }
                let center = Point2d::from_f64(cx, cy);
                let arc = match ends {
                    None => CircularArc::new(center, radius, orig.start_angle, orig.end_angle),
                    Some((ps, pe)) => {
                        // Rebuild the angular span from the solved endpoints,
                        // keeping the original sweep direction and winding.
                        let (sx, sy) = s.point(*ps);
                        let (ex, ey) = s.point(*pe);
                        let th_s = (sy - cy).atan2(sx - cx);
                        let sweep_old = orig.end_angle - orig.start_angle;
                        let raw = (ey - cy).atan2(ex - cx) - th_s;
                        let sweep = sweep_old + oxidraft_geometry::wrap_pi(raw - sweep_old);
                        CircularArc::new(center, radius, th_s, th_s + sweep)
                    }
                };
                if let Some(e) = doc.get_mut(id) {
                    e.kind = EntityKind::Curve(Curve::Arc(arc));
                }
            }
        }
    }
}

fn write_line(doc: &mut Document, id: EntityId, p0: (f64, f64), p1: (f64, f64)) {
    if let Some(e) = doc.get_mut(id) {
        e.kind = EntityKind::Curve(Curve::Line(LineSeg::from_endpoints(
            Point2d::from_f64(p0.0, p0.1),
            Point2d::from_f64(p1.0, p1.1),
        )));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::PI;

    fn add_line(doc: &mut Document, x0: f64, y0: f64, x1: f64, y1: f64) -> EntityId {
        doc.add(EntityKind::Curve(Curve::Line(LineSeg::from_endpoints(
            Point2d::from_f64(x0, y0),
            Point2d::from_f64(x1, y1),
        ))))
    }

    fn set_line(doc: &mut Document, id: EntityId, x0: f64, y0: f64, x1: f64, y1: f64) {
        write_line(doc, id, (x0, y0), (x1, y1));
    }

    fn line_angle_deg(l: &LineSeg) -> f64 {
        (l.p1.y - l.p0.y).atan2(l.p1.x - l.p0.x).to_degrees()
    }

    #[test]
    fn angle_rotates_only_the_second_line_to_the_target() {
        let mut doc = Document::new();
        let a = add_line(&mut doc, 0.0, 0.0, 5.0, 0.0);
        let b = add_line(&mut doc, 2.0, 1.0, 6.0, 1.5);
        constrain_angle(&mut doc, &[a, b], Some(60.0)).expect("must solve");
        let la = line_of(&doc, a).unwrap();
        let lb = line_of(&doc, b).unwrap();
        assert!(
            (la.p0.x, la.p0.y, la.p1.x, la.p1.y) == (0.0, 0.0, 5.0, 0.0),
            "the first pick is the fixed reference: {la:?}"
        );
        let diff = (line_angle_deg(&lb) - 60.0).rem_euclid(180.0);
        assert!(
            diff.min(180.0 - diff) < 1e-6,
            "mover settled at {}°",
            line_angle_deg(&lb)
        );
        assert!(
            (len(&lb) - 4.0f64.hypot(0.5)).abs() < 1e-7,
            "mover length kept"
        );
        assert_eq!(doc.constraints.len(), 1);
        assert_eq!(doc.constraints[0].kind, ConstraintKind::Angle);
        assert_eq!(doc.constraints[0].val, Some(60.0));

        // Re-constraining the same pair retargets the record in place.
        constrain_angle(&mut doc, &[a, b], Some(30.0)).expect("must solve");
        assert_eq!(doc.constraints.len(), 1);
        assert_eq!(doc.constraints[0].val, Some(30.0));
        let lb = line_of(&doc, b).unwrap();
        let diff = (line_angle_deg(&lb) - 30.0).rem_euclid(180.0);
        assert!(diff.min(180.0 - diff) < 1e-6);
    }

    #[test]
    fn angle_none_locks_the_current_angle() {
        let mut doc = Document::new();
        let a = add_line(&mut doc, 0.0, 0.0, 4.0, 0.0);
        let b = add_line(&mut doc, 0.0, 0.0, 3.0, 3.0);
        let before = line_of(&doc, b).unwrap();
        constrain_angle(&mut doc, &[a, b], None).expect("must solve");
        let c = doc.constraints[0];
        assert_eq!(c.kind, ConstraintKind::Angle);
        assert!((c.val.unwrap() - 45.0).abs() < 1e-9, "locked {:?}", c.val);
        let after = line_of(&doc, b).unwrap();
        assert!(
            after.p0.dist_f64(&before.p0) < 1e-7 && after.p1.dist_f64(&before.p1) < 1e-7,
            "locking the current angle must not move the line"
        );
        // The record now drives later edits: re-solve after moving the
        // reference keeps the pair at 45°.
        set_line(&mut doc, a, 0.0, 0.0, 4.0, 2.0);
        resolve_after_edit(&mut doc, a, None);
        let la = line_of(&doc, a).unwrap();
        let lb = line_of(&doc, b).unwrap();
        let rel = (line_angle_deg(&lb) - line_angle_deg(&la) - 45.0).rem_euclid(180.0);
        assert!(
            rel.min(180.0 - rel) < 1e-5,
            "relative angle re-solved to {}°",
            line_angle_deg(&lb) - line_angle_deg(&la)
        );
    }

    #[test]
    fn wild_recorded_angle_values_are_folded_at_lowering() {
        let mut doc = Document::new();
        let a = add_line(&mut doc, 0.0, 0.0, 4.0, 0.0);
        let b = add_line(&mut doc, 0.0, 1.0, 4.0, 1.0);
        // A hand-edited file can carry any positive degrees; 36045° must
        // lower as 45°, not as 629 radians fed to the residual.
        doc.add_constraint(SketchConstraint::angle(a, b, 36045.0));
        assert!(resolve_after_edit(&mut doc, a, None), "must re-solve");
        let la = line_of(&doc, a).unwrap();
        let lb = line_of(&doc, b).unwrap();
        let rel = (line_angle_deg(&lb) - line_angle_deg(&la) - 45.0).rem_euclid(180.0);
        assert!(
            rel.min(180.0 - rel) < 1e-5,
            "relative angle solved to {}°",
            line_angle_deg(&lb) - line_angle_deg(&la)
        );
    }

    #[test]
    fn angle_declines_hostile_values_and_selections() {
        let mut doc = Document::new();
        let a = add_line(&mut doc, 0.0, 0.0, 4.0, 0.0);
        let b = add_line(&mut doc, 0.0, 1.0, 4.0, 2.0);
        assert!(constrain_angle(&mut doc, &[a, b], Some(f64::NAN)).is_err());
        assert!(constrain_angle(&mut doc, &[a, b], Some(f64::INFINITY)).is_err());
        assert!(constrain_angle(&mut doc, &[a], Some(45.0)).is_err());
        assert!(constrain_angle(&mut doc, &[a, a], Some(45.0)).is_err());
        assert!(constrain_angle(&mut doc, &[], None).is_err());
        assert!(
            doc.constraints.is_empty(),
            "declined commands record nothing"
        );
        // 0° and negative degrees are legal picks, folded into (0, 180].
        constrain_angle(&mut doc, &[a, b], Some(0.0)).expect("0° is parallel");
        assert_eq!(doc.constraints[0].val, Some(180.0));
        constrain_angle(&mut doc, &[a, b], Some(-45.0)).expect("negative folds");
        assert_eq!(doc.constraints[0].val, Some(135.0));
    }

    #[test]
    fn horizontal_levels_a_sloped_line_about_its_midpoint() {
        let mut doc = Document::new();
        let id = add_line(&mut doc, 0.0, 0.0, 4.0, 0.6);
        constrain_lines(&mut doc, &[id], ConstraintKind::Horizontal).expect("must solve");
        let l = line_of(&doc, id).unwrap();
        assert!((l.p0.y - l.p1.y).abs() < 1e-8, "level: {:?}", l);
        assert!((len(&l) - (4.0f64.hypot(0.6))).abs() < 1e-7, "length kept");
        // Minimal motion: both endpoints moved, in opposite directions.
        assert!(l.p0.y > 0.0 && l.p1.y < 0.6, "levelled about the middle");
    }

    #[test]
    fn perpendicular_rotates_only_the_second_line() {
        let mut doc = Document::new();
        let a = add_line(&mut doc, 0.0, 0.0, 5.0, 0.0);
        let b = add_line(&mut doc, 2.0, 1.0, 4.5, 3.5);
        constrain_lines(&mut doc, &[a, b], ConstraintKind::Perpendicular).expect("must solve");
        let la = line_of(&doc, a).unwrap();
        let lb = line_of(&doc, b).unwrap();
        assert!(
            (la.p0.x, la.p0.y, la.p1.x, la.p1.y) == (0.0, 0.0, 5.0, 0.0),
            "reference untouched"
        );
        let dot =
            (la.p1.x - la.p0.x) * (lb.p1.x - lb.p0.x) + (la.p1.y - la.p0.y) * (lb.p1.y - lb.p0.y);
        assert!(dot.abs() < 1e-6, "perpendicular, dot={dot}");
    }

    #[test]
    fn parallel_preserves_the_moved_lines_length() {
        let mut doc = Document::new();
        let a = add_line(&mut doc, 0.0, 0.0, 6.0, 0.0);
        let b = add_line(&mut doc, 1.0, 2.0, 3.0, 4.5);
        let before = len(&line_of(&doc, b).unwrap());
        constrain_lines(&mut doc, &[a, b], ConstraintKind::Parallel).expect("must solve");
        let la = line_of(&doc, a).unwrap();
        let lb = line_of(&doc, b).unwrap();
        let cross =
            (la.p1.x - la.p0.x) * (lb.p1.y - lb.p0.y) - (la.p1.y - la.p0.y) * (lb.p1.x - lb.p0.x);
        assert!(cross.abs() < 1e-6, "parallel, cross={cross}");
        assert!((len(&lb) - before).abs() < 1e-7, "length preserved");
    }

    #[test]
    fn parallel_works_from_an_exactly_perpendicular_start() {
        // 90° apart is a saddle of the cross-product residual; the
        // perturbation retry must get past it.
        let mut doc = Document::new();
        let a = add_line(&mut doc, 0.0, 0.0, 5.0, 0.0);
        let b = add_line(&mut doc, 1.0, 1.0, 1.0, 4.0);
        constrain_lines(&mut doc, &[a, b], ConstraintKind::Parallel).expect("must solve");
        let lb = line_of(&doc, b).unwrap();
        assert!((lb.p0.y - lb.p1.y).abs() < 1e-6, "b is horizontal: {lb:?}");
        assert!((len(&lb) - 3.0).abs() < 1e-6, "length preserved");
    }

    #[test]
    fn vertical_works_on_an_exactly_horizontal_line() {
        let mut doc = Document::new();
        let id = add_line(&mut doc, 0.0, 0.0, 4.0, 0.0);
        constrain_lines(&mut doc, &[id], ConstraintKind::Vertical).expect("must solve");
        let l = line_of(&doc, id).unwrap();
        assert!((l.p0.x - l.p1.x).abs() < 1e-6, "vertical: {l:?}");
        assert!((len(&l) - 4.0).abs() < 1e-6, "length preserved");
    }

    #[test]
    fn equal_length_stretches_the_second_line() {
        let mut doc = Document::new();
        let a = add_line(&mut doc, 0.0, 0.0, 8.0, 0.0);
        let b = add_line(&mut doc, 1.0, 2.0, 4.0, 2.0);
        constrain_lines(&mut doc, &[a, b], ConstraintKind::EqualLength).expect("must solve");
        let lb = line_of(&doc, b).unwrap();
        assert!(
            (len(&lb) - 8.0).abs() < 1e-7,
            "stretched to 8, got {}",
            len(&lb)
        );
    }

    #[test]
    fn coincident_joins_the_nearest_endpoints() {
        let mut doc = Document::new();
        let a = add_line(&mut doc, 0.0, 0.0, 4.0, 0.0);
        let b = add_line(&mut doc, 4.3, 0.2, 8.0, 3.0);
        constrain_lines(&mut doc, &[a, b], ConstraintKind::Coincident).expect("must solve");
        let la = line_of(&doc, a).unwrap();
        let lb = line_of(&doc, b).unwrap();
        assert!(
            (la.p1.x - lb.p0.x).abs() < 1e-8 && (la.p1.y - lb.p0.y).abs() < 1e-8,
            "endpoints joined: {la:?} {lb:?}"
        );
        assert_eq!(
            (la.p0.x, la.p0.y, la.p1.x, la.p1.y),
            (0.0, 0.0, 4.0, 0.0),
            "reference untouched"
        );
        assert_eq!(doc.constraints.len(), 1);
        assert_eq!(doc.constraints[0].pts, Some((1, 0)));
    }

    #[test]
    fn pair_kinds_reject_wrong_selection_counts() {
        let mut doc = Document::new();
        let a = add_line(&mut doc, 0.0, 0.0, 5.0, 0.0);
        assert!(constrain_lines(&mut doc, &[a], ConstraintKind::Parallel).is_err());
        assert!(constrain_lines(&mut doc, &[], ConstraintKind::Horizontal).is_err());
    }

    #[test]
    fn applying_a_constraint_records_it_once() {
        let mut doc = Document::new();
        let a = add_line(&mut doc, 0.0, 0.0, 5.0, 0.0);
        let b = add_line(&mut doc, 1.0, 1.0, 1.2, 4.0);
        constrain_lines(&mut doc, &[a, b], ConstraintKind::Perpendicular).unwrap();
        constrain_lines(&mut doc, &[a, b], ConstraintKind::Perpendicular).unwrap();
        constrain_lines(&mut doc, &[b, a], ConstraintKind::Perpendicular).unwrap();
        assert_eq!(doc.constraints.len(), 1, "symmetric duplicates collapse");
        constrain_lines(&mut doc, &[a], ConstraintKind::Horizontal).unwrap();
        assert_eq!(doc.constraints.len(), 2);
    }

    #[test]
    fn vertical_conflicts_with_an_existing_horizontal_and_is_rejected() {
        // Horizontal and Vertical on the same line force it to a single
        // point — the only way to reproduce a bug where this silently
        // "succeeded" (rotated the line vertical) while leaving BOTH
        // records behind, the Horizontal one now lying about the geometry.
        let mut doc = Document::new();
        let id = add_line(&mut doc, 0.0, 0.0, 4.0, 0.0);
        constrain_lines(&mut doc, &[id], ConstraintKind::Horizontal).unwrap();
        let before = line_of(&doc, id).unwrap();
        let err = constrain_lines(&mut doc, &[id], ConstraintKind::Vertical).unwrap_err();
        assert!(
            err.message
                .contains("conflicts with its existing horizontal constraint"),
            "names the conflict: {err}"
        );
        assert_eq!(
            doc.constraints,
            vec![SketchConstraint::single(ConstraintKind::Horizontal, id)],
            "the rejected Vertical must not be left recorded alongside Horizontal"
        );
        let after = line_of(&doc, id).unwrap();
        assert_eq!(
            (before.p0, before.p1),
            (after.p0, after.p1),
            "geometry untouched by the rejected attempt"
        );
    }

    #[test]
    fn parallel_conflicts_with_an_existing_parallel_to_a_vertical_line_and_is_rejected() {
        // a is Horizontal (an absolute angle anchor), b is Parallel to a
        // (so b is horizontal too), c is Vertical (another absolute
        // anchor). Pure orientation relations among free-floating lines are
        // never truly contradictory on their own — angle is relative, so
        // the group can just rotate together — genuine conflict needs an
        // absolute anchor like Horizontal/Vertical pinning two DIFFERENT
        // angles into the same component. Asking b to ALSO be parallel to
        // c is exactly that: horizontal can't be parallel to vertical.
        let mut doc = Document::new();
        let a = add_line(&mut doc, 0.0, 0.0, 5.0, 0.0);
        let b = add_line(&mut doc, 0.0, 1.0, 5.0, 1.2);
        let c = add_line(&mut doc, 2.0, 2.0, 2.3, 6.0);
        constrain_lines(&mut doc, &[a], ConstraintKind::Horizontal).unwrap();
        constrain_lines(&mut doc, &[a, b], ConstraintKind::Parallel).unwrap();
        constrain_lines(&mut doc, &[c], ConstraintKind::Vertical).unwrap();
        let before = line_of(&doc, b).unwrap();
        let count_before = doc.constraints.len();
        let err = constrain_lines(&mut doc, &[c, b], ConstraintKind::Parallel).unwrap_err();
        assert!(
            err.message.contains("conflicts with its existing"),
            "names a conflict: {err}"
        );
        assert_eq!(
            doc.constraints.len(),
            count_before,
            "the rejected Parallel must not be left recorded"
        );
        let after = line_of(&doc, b).unwrap();
        assert_eq!(
            (before.p0, before.p1),
            (after.p0, after.p1),
            "geometry untouched by the rejected attempt"
        );
    }

    #[test]
    fn deleting_an_entity_prunes_its_constraints() {
        let mut doc = Document::new();
        let a = add_line(&mut doc, 0.0, 0.0, 5.0, 0.0);
        let b = add_line(&mut doc, 1.0, 1.0, 4.0, 2.0);
        constrain_lines(&mut doc, &[a, b], ConstraintKind::Parallel).unwrap();
        constrain_lines(&mut doc, &[a], ConstraintKind::Horizontal).unwrap();
        assert_eq!(doc.constraints.len(), 2);
        doc.remove(b);
        assert_eq!(doc.constraints.len(), 1, "only the pair is pruned");
        doc.remove(a);
        assert!(doc.constraints.is_empty());
    }

    #[test]
    fn drag_keeps_a_horizontal_line_horizontal() {
        let mut doc = Document::new();
        let id = add_line(&mut doc, 0.0, 0.0, 4.0, 0.0);
        constrain_lines(&mut doc, &[id], ConstraintKind::Horizontal).unwrap();
        // Simulate a grip drag of endpoint 1 off-axis.
        set_line(&mut doc, id, 0.0, 0.0, 5.0, 2.0);
        assert!(resolve_after_edit(&mut doc, id, Some(1)));
        let l = line_of(&doc, id).unwrap();
        assert!(
            (l.p1.x - 5.0).abs() < 1e-8 && (l.p1.y - 2.0).abs() < 1e-8,
            "drag wins: {l:?}"
        );
        assert!((l.p0.y - l.p1.y).abs() < 1e-8, "still horizontal: {l:?}");
        assert!(l.p0.x.abs() < 1e-6, "free endpoint x stays near start");
    }

    #[test]
    fn drag_on_a_perpendicular_pair_rotates_the_partner() {
        let mut doc = Document::new();
        let a = add_line(&mut doc, 0.0, 0.0, 5.0, 0.0);
        let b = add_line(&mut doc, 0.0, 0.0, 0.0, 3.0);
        constrain_lines(&mut doc, &[a, b], ConstraintKind::Perpendicular).unwrap();
        // Rotate line a by dragging its far endpoint upward.
        set_line(&mut doc, a, 0.0, 0.0, 4.0, 3.0);
        assert!(resolve_after_edit(&mut doc, a, Some(1)));
        let la = line_of(&doc, a).unwrap();
        let lb = line_of(&doc, b).unwrap();
        assert!((la.p1.x - 4.0).abs() < 1e-8 && (la.p1.y - 3.0).abs() < 1e-8);
        let dot =
            (la.p1.x - la.p0.x) * (lb.p1.x - lb.p0.x) + (la.p1.y - la.p0.y) * (lb.p1.y - lb.p0.y);
        assert!(dot.abs() < 1e-6, "partner stayed perpendicular, dot={dot}");
    }

    #[test]
    fn drag_reaches_through_a_constraint_chain() {
        let mut doc = Document::new();
        let a = add_line(&mut doc, 0.0, 0.0, 4.0, 0.0);
        let b = add_line(&mut doc, 0.0, 1.0, 4.0, 1.0);
        let c = add_line(&mut doc, 0.0, 2.0, 4.0, 2.0);
        constrain_lines(&mut doc, &[a, b], ConstraintKind::Parallel).unwrap();
        constrain_lines(&mut doc, &[b, c], ConstraintKind::Parallel).unwrap();
        set_line(&mut doc, a, 0.0, 0.0, 4.0, 2.0);
        assert!(resolve_after_edit(&mut doc, a, Some(1)));
        let la = line_of(&doc, a).unwrap();
        let lc = line_of(&doc, c).unwrap();
        let cross =
            (la.p1.x - la.p0.x) * (lc.p1.y - lc.p0.y) - (la.p1.y - la.p0.y) * (lc.p1.x - lc.p0.x);
        assert!(cross.abs() < 1e-6, "c follows a through b, cross={cross}");
    }

    #[test]
    fn drag_pulls_a_coincident_corner_along() {
        let mut doc = Document::new();
        let a = add_line(&mut doc, 0.0, 0.0, 4.0, 0.0);
        let b = add_line(&mut doc, 4.0, 0.0, 4.0, 3.0);
        doc.add_constraint(SketchConstraint::coincident(a, 1, b, 0));
        // Drag a's shared endpoint away.
        set_line(&mut doc, a, 0.0, 0.0, 5.0, 1.0);
        assert!(resolve_after_edit(&mut doc, a, Some(1)));
        let lb = line_of(&doc, b).unwrap();
        assert!(
            (lb.p0.x - 5.0).abs() < 1e-8 && (lb.p0.y - 1.0).abs() < 1e-8,
            "b's corner followed: {lb:?}"
        );
        assert!(
            (lb.p1.x - 4.0).abs() < 0.5 && (lb.p1.y - 3.0).abs() < 0.5,
            "b's far end stayed near where it was: {lb:?}"
        );
    }

    #[test]
    fn fixed_point_anchors_a_coincident_line_end_under_a_drag() {
        // Mirrors how the origin is wired up in oxidraft_ui::add_origin_point:
        // a Point entity carrying a `Fixed` constraint, welded to a line's
        // endpoint. Dragging the line's far end must re-solve the near end
        // back onto the fixed point, not let it drift with the drag.
        let mut doc = Document::new();
        let origin = doc.add(EntityKind::Point(Point2d::from_f64(0.0, 0.0)));
        doc.add_constraint(SketchConstraint::fixed(origin));
        let a = add_line(&mut doc, 0.0, 0.0, 4.0, 0.0);
        doc.add_constraint(SketchConstraint::coincident(origin, 0, a, 0));

        set_line(&mut doc, a, 0.2, 0.3, 9.0, 2.0);
        assert!(resolve_after_edit(&mut doc, a, Some(1)));
        let la = line_of(&doc, a).unwrap();
        assert!(
            (la.p0.x).abs() < 1e-6 && (la.p0.y).abs() < 1e-6,
            "near end pulled back onto the fixed origin: {la:?}"
        );
        assert!(
            (la.p1.x - 9.0).abs() < 1e-6 && (la.p1.y - 2.0).abs() < 1e-6,
            "dragged end kept the user's placement: {la:?}"
        );
        if let Some(EntityKind::Point(p)) = doc.get(origin).map(|e| &e.kind) {
            let (ox, oy) = p.to_f64();
            assert!(
                ox.abs() < 1e-6 && oy.abs() < 1e-6,
                "the origin itself never moved: ({ox}, {oy})"
            );
        } else {
            panic!("expected the origin to still be a point");
        }
    }

    #[test]
    fn line_distance_slides_the_second_line_to_the_width() {
        let mut doc = Document::new();
        let a = add_line(&mut doc, 0.0, 0.0, 6.0, 0.0);
        let b = add_line(&mut doc, 1.0, 2.0, 5.0, 2.0);
        constrain_line_distance(&mut doc, &[a, b], Some(5.0)).expect("must solve");
        let la = line_of(&doc, a).unwrap();
        assert!(
            (la.p0.x, la.p0.y, la.p1.x, la.p1.y) == (0.0, 0.0, 6.0, 0.0),
            "the first pick is the fixed reference: {la:?}"
        );
        let lb = line_of(&doc, b).unwrap();
        assert!(
            (lb.p0.y - 5.0).abs() < 1e-6 && (lb.p1.y - 5.0).abs() < 1e-6,
            "mover slid to the width: {lb:?}"
        );
        assert!((len(&lb) - 4.0).abs() < 1e-6, "mover length kept");
        assert_eq!(doc.constraints.len(), 1);
        assert_eq!(doc.constraints[0].kind, ConstraintKind::LineDistance);
        assert_eq!(doc.constraints[0].val, Some(5.0));

        // Re-constraining the same pair retargets the record in place.
        constrain_line_distance(&mut doc, &[a, b], Some(3.0)).expect("must solve");
        assert_eq!(doc.constraints.len(), 1);
        assert_eq!(doc.constraints[0].val, Some(3.0));
        let lb = line_of(&doc, b).unwrap();
        assert!((lb.p0.y - 3.0).abs() < 1e-6, "retarget moved it: {lb:?}");
    }

    #[test]
    fn line_distance_rejects_crossing_lines() {
        let mut doc = Document::new();
        let a = add_line(&mut doc, 0.0, 0.0, 6.0, 0.0);
        let b = add_line(&mut doc, 0.0, 0.0, 4.0, 4.0);
        assert!(
            constrain_line_distance(&mut doc, &[a, b], Some(2.0)).is_err(),
            "crossing lines have an angle, not a width"
        );
        assert!(doc.constraints.is_empty(), "nothing recorded on failure");
    }

    #[test]
    fn fix_pins_a_whole_line_against_a_neighbours_resolve() {
        let mut doc = Document::new();
        let a = add_line(&mut doc, 0.0, 0.0, 4.0, 0.0);
        let b = add_line(&mut doc, 4.0, 0.0, 4.0, 3.0);
        doc.add_constraint(SketchConstraint::coincident(a, 1, b, 0));

        constrain_fixed(&mut doc, &[a]).expect("a line is fixable");
        assert!(
            doc.constraints
                .iter()
                .any(|c| c.kind == ConstraintKind::Fixed && c.a == a),
            "a Fixed constraint was recorded on the line"
        );
        // Idempotent: fixing an already-fixed selection is a no-op error.
        assert!(constrain_fixed(&mut doc, &[a]).is_err());

        // Drag b's free end far away and re-solve its component. Because a is
        // fully pinned (both endpoints), it must not drift, and b's welded end
        // stays on the fixed corner.
        set_line(&mut doc, b, 4.0, 0.0, 7.0, 5.0);
        assert!(resolve_after_edit(&mut doc, b, Some(1)));
        let la = line_of(&doc, a).unwrap();
        for (got, want) in [
            (la.p0.x, 0.0),
            (la.p0.y, 0.0),
            (la.p1.x, 4.0),
            (la.p1.y, 0.0),
        ] {
            assert!(
                (got - want).abs() < 1e-6,
                "fixed line a never moved: {la:?}"
            );
        }
        let lb = line_of(&doc, b).unwrap();
        assert!(
            (lb.p0.x - 4.0).abs() < 1e-6 && lb.p0.y.abs() < 1e-6,
            "b's welded end stayed on the fixed corner: {lb:?}"
        );
    }

    #[test]
    fn fixed_point_dragged_directly_rejects_the_edit() {
        // `constrain_fixed` pins a point at "wherever it currently sits" —
        // fine when `moved` is an unchanged anchor, but a grip drag (or a
        // typed coordinate edit) mutates the entity's own position *before*
        // calling into the solver (see `AppState::apply_grip_drag`), so by
        // the time this runs the point's "current" position already is the
        // dragged one. Plain `resolve_after_edit` can't tell the two cases
        // apart and would trivially satisfy `Fixed` right there — that's
        // exactly what `resolve_after_direct_edit` exists to reject.
        let mut doc = Document::new();
        let p = doc.add(EntityKind::Point(Point2d::from_f64(0.0, 0.0)));
        constrain_fixed(&mut doc, &[p]).expect("a point is fixable");

        // Simulate the drag: the caller mutates the entity first, exactly
        // like `apply_grip_drag` and the Properties-inspector coordinate
        // fields both do, then asks the solver to resolve it.
        if let Some(e) = doc.get_mut(p) {
            e.kind = EntityKind::Point(Point2d::from_f64(9.0, 9.0));
        }
        assert!(
            !resolve_after_direct_edit(&mut doc, p, Some(0)),
            "a point can't drag itself off its own Fixed position"
        );
    }

    #[test]
    fn fixed_line_dragged_by_one_endpoint_rejects_the_edit() {
        // The same gap as `fixed_point_dragged_directly_rejects_the_edit`,
        // for a line: `constrain_fixed` locks the *whole* line, both
        // endpoints, not just the one being dragged.
        let mut doc = Document::new();
        let a = add_line(&mut doc, 0.0, 0.0, 10.0, 0.0);
        constrain_fixed(&mut doc, &[a]).expect("a line is fixable");

        set_line(&mut doc, a, 0.0, 0.0, 10.0, 8.0);
        assert!(
            !resolve_after_direct_edit(&mut doc, a, Some(1)),
            "a fixed line can't drag its own endpoint away either"
        );
    }

    #[test]
    fn moving_a_line_drags_coincident_neighbours() {
        let mut doc = Document::new();
        let a = add_line(&mut doc, 0.0, 0.0, 4.0, 0.0);
        let b = add_line(&mut doc, 4.0, 0.0, 4.0, 3.0);
        doc.add_constraint(SketchConstraint::coincident(a, 1, b, 0));
        // Translate the whole of line a.
        set_line(&mut doc, a, 1.0, 2.0, 5.0, 2.0);
        assert!(resolve_after_transform(&mut doc, &[a]));
        let la = line_of(&doc, a).unwrap();
        let lb = line_of(&doc, b).unwrap();
        for (got, want) in [
            (la.p0.x, 1.0),
            (la.p0.y, 2.0),
            (la.p1.x, 5.0),
            (la.p1.y, 2.0),
        ] {
            assert!((got - want).abs() < 1e-8, "moved line pinned: {la:?}");
        }
        assert!(
            (lb.p0.x - 5.0).abs() < 1e-8 && (lb.p0.y - 2.0).abs() < 1e-8,
            "neighbour corner reattached: {lb:?}"
        );
    }

    #[test]
    fn moving_both_members_of_a_pair_keeps_them_put() {
        let mut doc = Document::new();
        let a = add_line(&mut doc, 0.0, 0.0, 4.0, 0.0);
        let b = add_line(&mut doc, 0.0, 1.0, 4.0, 1.0);
        constrain_lines(&mut doc, &[a, b], ConstraintKind::Parallel).unwrap();
        // Translate both together: the relation still holds, nothing moves.
        set_line(&mut doc, a, 10.0, 0.0, 14.0, 0.0);
        set_line(&mut doc, b, 10.0, 1.0, 14.0, 1.0);
        assert!(resolve_after_transform(&mut doc, &[a, b]));
        let la = line_of(&doc, a).unwrap();
        let lb = line_of(&doc, b).unwrap();
        assert_eq!((la.p0.x, la.p0.y), (10.0, 0.0));
        assert_eq!((lb.p0.x, lb.p0.y), (10.0, 1.0));
    }

    #[test]
    fn transform_that_breaks_a_constraint_reports_failure() {
        let mut doc = Document::new();
        let a = add_line(&mut doc, 0.0, 0.0, 4.0, 0.0);
        constrain_lines(&mut doc, &[a], ConstraintKind::Horizontal).unwrap();
        // "Rotate" the line 30°: pinned endpoints conflict with Horizontal.
        set_line(&mut doc, a, 0.0, 0.0, 3.46, 2.0);
        assert!(!resolve_after_transform(&mut doc, &[a]));
        let la = line_of(&doc, a).unwrap();
        assert!(
            (la.p1.y - 2.0).abs() < 1e-9,
            "user's transform left alone: {la:?}"
        );
    }

    #[test]
    fn perpendicular_on_a_coincident_pair_keeps_the_joint_welded() {
        // a-b share a corner (Coincident). Squaring b up to a third line c
        // rotates b about that corner; the corner must follow, not gap.
        let mut doc = Document::new();
        let a = add_line(&mut doc, 0.0, 0.0, 4.0, 0.0);
        let b = add_line(&mut doc, 4.0, 0.0, 6.0, 1.5);
        let c = add_line(&mut doc, -3.0, -3.0, -3.0, 3.0);
        constrain_lines(&mut doc, &[a, b], ConstraintKind::Coincident).unwrap();
        constrain_lines(&mut doc, &[c, b], ConstraintKind::Perpendicular).unwrap();
        let la = line_of(&doc, a).unwrap();
        let lb = line_of(&doc, b).unwrap();
        assert!(
            (la.p1.x - lb.p0.x).abs() < 1e-6 && (la.p1.y - lb.p0.y).abs() < 1e-6,
            "corner stayed welded after b rotated: a={la:?} b={lb:?}"
        );
    }

    #[test]
    fn perpendicular_still_solves_on_a_tight_slot_after_a_small_nudge() {
        // A "slot" — two vertical legs tangent to a shared bottom arc, a
        // horizontal top, and welded corners — is a numerically stiff
        // component: normal-equations LM's achievable precision is roughly
        // the *square* of the system's condition number times machine
        // epsilon, and this combination of tangency + welds + H/V anchors
        // is ill-conditioned enough to plateau around 1e-8, just above the
        // solver's old 1e-10 tolerance. That made a perfectly legitimate,
        // barely-off-vertical `right` (as a small grip drag would leave it)
        // get rejected as "conflicts with its existing constraints" even
        // though a solution a few nanometres away clearly exists.
        use oxidraft_document::SketchConstraint;
        let mut doc = Document::new();
        let top = add_line(&mut doc, 0.0, 5.0, 4.0, 5.0);
        let left = add_line(&mut doc, 0.0, 5.0, 0.0, 0.0);
        let right = add_line(&mut doc, 4.0, 5.0, 4.0, 0.0);
        let arc = doc.add(EntityKind::Curve(Curve::Arc(CircularArc::new(
            Point2d::from_f64(2.0, 0.0),
            2.0,
            PI,
            TAU,
        ))));
        constrain_lines(&mut doc, &[top], ConstraintKind::Horizontal).expect("top horizontal");
        constrain_lines(&mut doc, &[left], ConstraintKind::Vertical).expect("left vertical");
        constrain_tangent(&mut doc, &[arc, left]).expect("left tangent");
        constrain_tangent(&mut doc, &[arc, right]).expect("right tangent");
        doc.add_constraint(SketchConstraint::coincident(top, 0, left, 0));
        doc.add_constraint(SketchConstraint::coincident(top, 1, right, 0));
        doc.add_constraint(SketchConstraint::coincident(left, 1, arc, 0));
        doc.add_constraint(SketchConstraint::coincident(right, 1, arc, 1));

        // Nudge right's top endpoint sideways a little, as a small manual
        // drag would, breaking exact verticality/tangency slightly.
        let r = line_of(&doc, right).unwrap();
        set_line(&mut doc, right, 4.1, 5.0, r.p1.x, r.p1.y);

        constrain_lines(&mut doc, &[top, right], ConstraintKind::Perpendicular)
            .expect("a barely-off-vertical leg must still solve, not be rejected as conflicting");
        let lr = line_of(&doc, right).unwrap();
        assert!(
            (lr.p0.x - lr.p1.x).abs() < 1e-6,
            "right ended up vertical: {lr:?}"
        );
    }

    fn add_circle(doc: &mut Document, cx: f64, cy: f64, r: f64) -> EntityId {
        doc.add(EntityKind::Curve(Curve::Arc(CircularArc::new(
            Point2d::from_f64(cx, cy),
            r,
            0.0,
            TAU,
        ))))
    }

    fn line_circle_gap(l: &LineSeg, a: &CircularArc) -> f64 {
        let (ux, uy) = (l.p1.x - l.p0.x, l.p1.y - l.p0.y);
        let n = ux.hypot(uy);
        let d = (ux * (a.center.y - l.p0.y) - uy * (a.center.x - l.p0.x)) / n;
        d.abs() - a.radius
    }

    #[test]
    fn tangent_slides_the_line_onto_a_pinned_circle() {
        let mut doc = Document::new();
        let circle = add_circle(&mut doc, 0.0, 0.0, 2.0);
        let line = add_line(&mut doc, -3.0, 3.0, 3.0, 3.4);
        let before = len(&line_of(&doc, line).unwrap());
        constrain_lines(&mut doc, &[circle, line], ConstraintKind::Tangent).expect("must solve");
        let l = line_of(&doc, line).unwrap();
        let a = arc_of(&doc, circle).unwrap();
        assert!(
            (a.center.x, a.center.y) == (0.0, 0.0) && a.radius == 2.0,
            "reference pinned"
        );
        assert!(line_circle_gap(&l, &a).abs() < 1e-7, "line touches the rim");
        assert!((len(&l) - before).abs() < 1e-7, "line length kept");
        assert_eq!(doc.constraints.len(), 1);
        assert_eq!(doc.constraints[0].kind, ConstraintKind::Tangent);
    }

    #[test]
    fn tangent_pulls_the_circle_onto_a_pinned_line() {
        let mut doc = Document::new();
        let line = add_line(&mut doc, -4.0, 0.0, 4.0, 0.0);
        let circle = add_circle(&mut doc, 0.5, 3.1, 2.0);
        constrain_lines(&mut doc, &[line, circle], ConstraintKind::Tangent).expect("must solve");
        let l = line_of(&doc, line).unwrap();
        let a = arc_of(&doc, circle).unwrap();
        assert_eq!((l.p0.x, l.p0.y, l.p1.x, l.p1.y), (-4.0, 0.0, 4.0, 0.0));
        assert!(
            line_circle_gap(&l, &a).abs() < 1e-7,
            "circle touches the line"
        );
        assert!(a.center.y > 1.0, "circle stayed on its side");
    }

    #[test]
    fn dragging_a_tangent_line_pulls_the_circle_along() {
        let mut doc = Document::new();
        let circle = add_circle(&mut doc, 0.0, 0.0, 2.0);
        let line = add_line(&mut doc, -3.0, 2.0, 3.0, 2.0);
        constrain_lines(&mut doc, &[circle, line], ConstraintKind::Tangent).unwrap();
        // Tilt the line by dragging its right end up.
        set_line(&mut doc, line, -3.0, 2.0, 3.0, 4.0);
        assert!(resolve_after_edit(&mut doc, line, Some(1)));
        let l = line_of(&doc, line).unwrap();
        let a = arc_of(&doc, circle).unwrap();
        assert!(
            (l.p1.x - 3.0).abs() < 1e-8 && (l.p1.y - 4.0).abs() < 1e-8,
            "drag wins"
        );
        assert!(
            line_circle_gap(&l, &a).abs() < 1e-7,
            "circle re-attached: {a:?}"
        );
    }

    #[test]
    fn radius_resizes_a_circle_and_keeps_its_tangent_line() {
        let mut doc = Document::new();
        let circle = add_circle(&mut doc, 0.0, 0.0, 2.0);
        let line = add_line(&mut doc, -3.0, 2.0, 3.0, 2.0);
        constrain_lines(&mut doc, &[circle, line], ConstraintKind::Tangent).unwrap();
        constrain_radius(&mut doc, &[circle], Some(3.0)).expect("must solve");
        let a = arc_of(&doc, circle).unwrap();
        let l = line_of(&doc, line).unwrap();
        assert!((a.radius - 3.0).abs() < 1e-6, "resized: {}", a.radius);
        assert!(line_circle_gap(&l, &a).abs() < 1e-6, "still tangent");
        assert!(
            doc.constraints
                .iter()
                .any(|c| c.kind == ConstraintKind::Radius && c.a == circle && c.val == Some(3.0)),
            "radius recorded"
        );
    }

    #[test]
    fn radius_reapplied_retargets_the_same_constraint() {
        let mut doc = Document::new();
        let circle = add_circle(&mut doc, 0.0, 0.0, 2.0);
        constrain_radius(&mut doc, &[circle], Some(3.0)).unwrap();
        constrain_radius(&mut doc, &[circle], Some(4.0)).unwrap();
        assert_eq!(doc.constraints.len(), 1, "one constraint, updated in place");
        assert_eq!(doc.constraints[0].val, Some(4.0));
        assert!((arc_of(&doc, circle).unwrap().radius - 4.0).abs() < 1e-6);
    }

    #[test]
    fn bare_radius_locks_the_current_value() {
        let mut doc = Document::new();
        let circle = add_circle(&mut doc, 1.0, 1.0, 2.5);
        constrain_radius(&mut doc, &[circle], None).unwrap();
        let a = arc_of(&doc, circle).unwrap();
        assert!((a.radius - 2.5).abs() < 1e-9, "geometry untouched");
        assert_eq!(doc.constraints[0].val, Some(2.5));
    }

    #[test]
    fn dragging_a_tangent_line_respects_a_driven_radius() {
        let mut doc = Document::new();
        let circle = add_circle(&mut doc, 0.0, 0.0, 2.0);
        let line = add_line(&mut doc, -3.0, 2.0, 3.0, 2.0);
        constrain_lines(&mut doc, &[circle, line], ConstraintKind::Tangent).unwrap();
        constrain_radius(&mut doc, &[circle], Some(2.0)).unwrap();
        // Tilt the line by dragging its right end up; the circle must
        // follow without resizing.
        set_line(&mut doc, line, -3.0, 2.0, 3.0, 4.0);
        assert!(resolve_after_edit(&mut doc, line, Some(1)));
        let a = arc_of(&doc, circle).unwrap();
        let l = line_of(&doc, line).unwrap();
        assert!((a.radius - 2.0).abs() < 1e-6, "radius held: {}", a.radius);
        assert!(line_circle_gap(&l, &a).abs() < 1e-6, "still tangent");
    }

    #[test]
    fn radius_rejects_nonsense() {
        let mut doc = Document::new();
        let circle = add_circle(&mut doc, 0.0, 0.0, 2.0);
        assert!(constrain_radius(&mut doc, &[circle], Some(-1.0)).is_err());
        let l = add_line(&mut doc, 0.0, 0.0, 1.0, 0.0);
        assert!(
            constrain_radius(&mut doc, &[l], Some(1.0)).is_err(),
            "lines have no radius"
        );
        assert!(doc.constraints.is_empty());
    }

    #[test]
    fn distance_resizes_a_line_about_its_midpoint() {
        let mut doc = Document::new();
        let id = add_line(&mut doc, 0.0, 0.0, 4.0, 0.0);
        constrain_distance(&mut doc, &[id], Some(6.0)).expect("must solve");
        let l = line_of(&doc, id).unwrap();
        assert!((len(&l) - 6.0).abs() < 1e-6, "resized: {}", len(&l));
        // Minimal motion scales about the midpoint (x = 2), symmetrically.
        assert!(
            (l.p0.x - -1.0).abs() < 1e-6 && (l.p1.x - 5.0).abs() < 1e-6,
            "{l:?}"
        );
        assert!(
            doc.constraints
                .iter()
                .any(|c| c.kind == ConstraintKind::Distance && c.a == id && c.val == Some(6.0)),
            "length recorded"
        );
    }

    #[test]
    fn distance_reapplied_retargets_the_same_constraint() {
        let mut doc = Document::new();
        let id = add_line(&mut doc, 0.0, 0.0, 3.0, 0.0);
        constrain_distance(&mut doc, &[id], Some(5.0)).unwrap();
        constrain_distance(&mut doc, &[id], Some(7.0)).unwrap();
        assert_eq!(doc.constraints.len(), 1, "one constraint, updated in place");
        assert_eq!(doc.constraints[0].val, Some(7.0));
        assert!((len(&line_of(&doc, id).unwrap()) - 7.0).abs() < 1e-6);
    }

    #[test]
    fn bare_distance_locks_the_current_length() {
        let mut doc = Document::new();
        let id = add_line(&mut doc, 1.0, 1.0, 4.0, 5.0);
        constrain_distance(&mut doc, &[id], None).unwrap();
        let l = line_of(&doc, id).unwrap();
        assert_eq!(
            (l.p0.x, l.p0.y, l.p1.x, l.p1.y),
            (1.0, 1.0, 4.0, 5.0),
            "geometry untouched"
        );
        assert_eq!(doc.constraints[0].val, Some(5.0), "3-4-5 length locked");
    }

    #[test]
    fn distance_drags_a_coincident_neighbour_when_driven() {
        let mut doc = Document::new();
        let a = add_line(&mut doc, 0.0, 0.0, 4.0, 0.0);
        let b = add_line(&mut doc, 4.0, 0.0, 4.0, 3.0);
        doc.add_constraint(SketchConstraint::coincident(a, 1, b, 0));
        // Lengthening a about its midpoint pushes the shared corner out, and
        // b's joined endpoint must follow.
        constrain_distance(&mut doc, &[a], Some(6.0)).expect("must solve");
        let la = line_of(&doc, a).unwrap();
        let lb = line_of(&doc, b).unwrap();
        assert!((len(&la) - 6.0).abs() < 1e-6, "a resized: {}", len(&la));
        assert!(
            (lb.p0.x - la.p1.x).abs() < 1e-6 && (lb.p0.y - la.p1.y).abs() < 1e-6,
            "corner stayed welded: {la:?} {lb:?}"
        );
    }

    #[test]
    fn distance_rejects_nonsense() {
        let mut doc = Document::new();
        let l = add_line(&mut doc, 0.0, 0.0, 4.0, 0.0);
        assert!(constrain_distance(&mut doc, &[l], Some(-1.0)).is_err());
        let circle = add_circle(&mut doc, 0.0, 0.0, 2.0);
        assert!(
            constrain_distance(&mut doc, &[circle], Some(1.0)).is_err(),
            "arcs have no length"
        );
        assert!(doc.constraints.is_empty());
    }

    #[test]
    fn moving_a_circle_drags_its_tangent_line() {
        let mut doc = Document::new();
        let line = add_line(&mut doc, -4.0, 0.0, 4.0, 0.0);
        let circle = add_circle(&mut doc, 0.0, 2.0, 2.0);
        constrain_lines(&mut doc, &[line, circle], ConstraintKind::Tangent).unwrap();
        // Translate the circle up; the line must follow to stay tangent.
        if let Some(e) = doc.get_mut(circle) {
            e.kind = EntityKind::Curve(Curve::Arc(CircularArc::new(
                Point2d::from_f64(0.0, 3.0),
                2.0,
                0.0,
                TAU,
            )));
        }
        assert!(resolve_after_transform(&mut doc, &[circle]));
        let l = line_of(&doc, line).unwrap();
        let a = arc_of(&doc, circle).unwrap();
        assert!((a.center.y - 3.0).abs() < 1e-8, "moved circle pinned");
        assert!(line_circle_gap(&l, &a).abs() < 1e-7, "line followed");
    }

    #[test]
    fn tangent_circles_touch_and_follow_each_other() {
        let mut doc = Document::new();
        let a = add_circle(&mut doc, 0.0, 0.0, 2.0);
        let b = add_circle(&mut doc, 5.5, 0.0, 1.0);
        constrain_lines(&mut doc, &[a, b], ConstraintKind::Tangent).expect("must solve");
        let ca = arc_of(&doc, a).unwrap();
        let cb = arc_of(&doc, b).unwrap();
        assert_eq!((ca.center.x, ca.center.y, ca.radius), (0.0, 0.0, 2.0));
        let d = (cb.center.x - ca.center.x).hypot(cb.center.y - ca.center.y);
        assert!((d - 3.0).abs() < 1e-7, "externally tangent, d={d}");
        assert!((cb.radius - 1.0).abs() < 1e-6, "mover kept its radius");

        // Move the reference circle; the partner must re-attach.
        if let Some(e) = doc.get_mut(a) {
            e.kind = EntityKind::Curve(Curve::Arc(CircularArc::new(
                Point2d::from_f64(0.0, 2.0),
                2.0,
                0.0,
                TAU,
            )));
        }
        assert!(resolve_after_transform(&mut doc, &[a]));
        let ca = arc_of(&doc, a).unwrap();
        let cb = arc_of(&doc, b).unwrap();
        assert!((ca.center.y - 2.0).abs() < 1e-8, "moved circle pinned");
        let d = (cb.center.x - ca.center.x).hypot(cb.center.y - ca.center.y);
        assert!(
            (d - (ca.radius + cb.radius)).abs() < 1e-7,
            "still tangent after move, d={d}"
        );
    }

    #[test]
    fn welded_tangent_fillet_survives_dragging_a_leg() {
        // Horizontal leg into a quarter fillet into a vertical leg, welded
        // and tangent like the fillet tool records. Dragging the far end of
        // the vertical leg must keep the corner smooth.
        let mut doc = Document::new();
        let leg_a = add_line(&mut doc, 0.0, 0.0, 3.0, 0.0);
        let leg_b = add_line(&mut doc, 4.0, 1.0, 4.0, 4.0);
        let arc = doc.add(EntityKind::Curve(Curve::Arc(CircularArc::new(
            Point2d::from_f64(3.0, 1.0),
            1.0,
            -std::f64::consts::FRAC_PI_2,
            0.0,
        ))));
        doc.add_constraint(SketchConstraint::coincident(leg_a, 1, arc, 0));
        doc.add_constraint(SketchConstraint::coincident(leg_b, 0, arc, 1));
        doc.add_constraint(SketchConstraint::pair(ConstraintKind::Tangent, leg_a, arc));
        doc.add_constraint(SketchConstraint::pair(ConstraintKind::Tangent, leg_b, arc));
        set_line(&mut doc, leg_b, 4.0, 1.0, 5.5, 4.0);
        assert!(resolve_after_edit(&mut doc, leg_b, Some(1)));
        let la = line_of(&doc, leg_a).unwrap();
        let lb = line_of(&doc, leg_b).unwrap();
        let a = arc_of(&doc, arc).unwrap();
        assert!(
            (lb.p1.x - 5.5).abs() < 1e-8 && (lb.p1.y - 4.0).abs() < 1e-8,
            "drag wins"
        );
        assert!(line_circle_gap(&la, &a).abs() < 1e-6, "leg a still tangent");
        assert!(line_circle_gap(&lb, &a).abs() < 1e-6, "leg b still tangent");
        let (s0, s1) = (arc_end_pos(&a, 0), arc_end_pos(&a, 1));
        assert!(
            (la.p1.x - s0.0).abs() < 1e-6 && (la.p1.y - s0.1).abs() < 1e-6,
            "arc start welded to leg a: {s0:?} vs {la:?}"
        );
        assert!(
            (lb.p0.x - s1.0).abs() < 1e-6 && (lb.p0.y - s1.1).abs() < 1e-6,
            "arc end welded to leg b: {s1:?} vs {lb:?}"
        );
    }

    #[test]
    fn resolve_ignores_unconstrained_entities() {
        let mut doc = Document::new();
        let a = add_line(&mut doc, 0.0, 0.0, 4.0, 0.0);
        let free = add_line(&mut doc, 9.0, 9.0, 10.0, 10.0);
        constrain_lines(&mut doc, &[a], ConstraintKind::Horizontal).unwrap();
        set_line(&mut doc, free, 9.0, 9.0, 11.0, 12.0);
        assert!(resolve_after_edit(&mut doc, free, Some(1)));
        assert!(resolve_after_transform(&mut doc, &[free]));
        let l = line_of(&doc, free).unwrap();
        assert!(
            (l.p1.x - 11.0).abs() < 1e-12 && (l.p1.y - 12.0).abs() < 1e-12,
            "untouched"
        );
    }

    #[test]
    fn dof_report_counts_a_plain_and_a_horizontal_line() {
        let mut doc = Document::new();
        let id = add_line(&mut doc, 0.0, 0.0, 4.0, 0.0);
        assert_eq!(
            dof_report(&doc, &[id]).dof,
            4,
            "two free endpoints, unconstrained"
        );
        constrain_lines(&mut doc, &[id], ConstraintKind::Horizontal).unwrap();
        let report = dof_report(&doc, &[id]);
        assert_eq!(report.dof, 3, "one row pinned by Horizontal");
        assert!(report.redundant.is_empty());
    }

    #[test]
    fn redundancy_blame_stays_aligned_with_an_arc_in_the_component() {
        // A partial arc adds two intrinsic PointOnCircle rows to the solver
        // BEFORE any document constraint is lowered. The redundant/culprit
        // indices must be mapped past that prefix, or the blame lands on the
        // wrong document constraint (or is silently dropped past the end).
        let mut doc = Document::new();
        let a = add_line(&mut doc, 0.0, 0.0, 4.0, 0.0);
        let b = add_line(&mut doc, 0.0, 1.0, 4.0, 1.3);
        let c = add_line(&mut doc, 0.0, 2.0, 4.0, 2.2);
        let arc = doc.add(EntityKind::Curve(Curve::Arc(CircularArc::new(
            Point2d::from_f64(0.0, 0.0),
            2.0,
            0.0,
            std::f64::consts::FRAC_PI_2,
        ))));
        // Weld the arc's start to a's start so it joins the same component.
        constrain_coincident_points(&mut doc, (arc, 0), (a, 0)).unwrap();
        constrain_lines(&mut doc, &[a, b], ConstraintKind::Parallel).unwrap();
        constrain_lines(&mut doc, &[b, c], ConstraintKind::Parallel).unwrap();
        constrain_lines(&mut doc, &[a, c], ConstraintKind::Parallel).unwrap();
        let report = dof_report(&doc, &[a]);
        assert_eq!(
            report.redundant.len(),
            1,
            "exactly one Parallel is redundant: {:?}",
            report.redundant
        );
        let blamed = &doc.constraints[report.redundant[0]];
        assert_eq!(
            blamed.kind,
            ConstraintKind::Parallel,
            "the blame must land on a Parallel record, not shift onto {blamed:?}"
        );
    }

    #[test]
    fn dof_report_counts_entities_no_constraint_touches() {
        let mut doc = Document::new();
        let a = add_line(&mut doc, 0.0, 0.0, 4.0, 0.0);
        doc.add_constraint(SketchConstraint::fixed(a));
        add_line(&mut doc, 10.0, 10.0, 14.0, 12.0);
        let report = dof_report(&doc, &[a]);
        assert_eq!(report.dof, 0, "the fixed line itself is rigid");
        assert_eq!(
            report.free_entities, 1,
            "the untouched line must be reported, or 'fully constrained' lies"
        );
    }

    #[test]
    fn redundancy_is_blamed_on_the_right_constraint() {
        // `constraint_doc_idx` maps solver constraints back to document ones.
        // Arms that emit two residual ROWS from one `constrain` call used to
        // push two entries, but `analyze().redundant` indexes by CONSTRAINT —
        // so everything recorded after one was shifted by one and the badge
        // landed on the wrong row.
        //
        // A Concentric ahead of the group is enough to expose it, because it
        // lowers through the Coincident arm. Note the component must be seeded
        // with the circles too: seeding only the lines leaves the Concentric
        // out of the sketch entirely, and then nothing shifts and the test
        // proves nothing (which is exactly how an earlier version of this test
        // passed against the bug).
        let tau = std::f64::consts::TAU;
        let mut doc = Document::new();
        let c1 = doc.add(EntityKind::Curve(Curve::Arc(
            oxidraft_geometry::CircularArc::new(Point2d::from_f64(0.0, 0.0), 1.0, 0.0, tau),
        )));
        let c2 = doc.add(EntityKind::Curve(Curve::Arc(
            oxidraft_geometry::CircularArc::new(Point2d::from_f64(3.0, 0.0), 2.0, 0.0, tau),
        )));
        constrain_lines(&mut doc, &[c1, c2], ConstraintKind::Concentric).expect("concentric");

        let l = doc.add(EntityKind::Curve(Curve::Line(LineSeg::from_endpoints(
            Point2d::from_f64(0.0, 0.0),
            Point2d::from_f64(4.0, 0.0),
        ))));
        let m = doc.add(EntityKind::Curve(Curve::Line(LineSeg::from_endpoints(
            Point2d::from_f64(0.0, 2.0),
            Point2d::from_f64(4.0, 2.0),
        ))));
        let _ = constrain_lines(&mut doc, &[l], ConstraintKind::Horizontal);
        let _ = constrain_lines(&mut doc, &[l, m], ConstraintKind::Parallel);
        // Redundant: Parallel to an already-horizontal line implies this.
        let _ = constrain_lines(&mut doc, &[m], ConstraintKind::Horizontal);

        let report = dof_report(&doc, &[c1, c2, l, m]);
        assert!(
            !report.redundant.is_empty(),
            "the trailing Horizontal is redundant and must be reported — an \
             empty list would make every assertion below vacuous"
        );
        for &i in &report.redundant {
            assert_eq!(
                doc.constraints[i].kind,
                ConstraintKind::Horizontal,
                "blamed doc[{i}] {:?}; the redundant constraint is the second \
                 Horizontal, so anything else means the mapping is shifted",
                doc.constraints[i].kind
            );
        }
    }

    #[test]
    fn angular_constraints_rotate_the_follower_instead_of_shrinking_it() {
        // A purely angular residual is least-squares-minimised by projecting
        // the follower onto the target direction, which costs it
        // |v|·(1−cos θ) on EVERY re-solve. Dragging the reference round to
        // 90° collapsed the follower to a zero-length segment at its own
        // midpoint — and `resolve_after_transform` returned true and wrote
        // that to the document.
        for kind in [
            ConstraintKind::Parallel,
            ConstraintKind::Perpendicular,
            ConstraintKind::Collinear,
        ] {
            let mut doc = Document::new();
            let a = doc.add(EntityKind::Curve(Curve::Line(LineSeg::from_endpoints(
                Point2d::from_f64(0.0, 0.0),
                Point2d::from_f64(6.0, 0.0),
            ))));
            let b = doc.add(EntityKind::Curve(Curve::Line(LineSeg::from_endpoints(
                Point2d::from_f64(0.0, 3.0),
                Point2d::from_f64(6.0, 3.0),
            ))));
            constrain_lines(&mut doc, &[a, b], kind)
                .unwrap_or_else(|e| panic!("{kind:?} should record cleanly: {e:?}"));
            let length_of = |d: &Document, id| match d.get(id).unwrap().as_curve().unwrap() {
                Curve::Line(l) => l.p0.dist_f64(&l.p1),
                _ => panic!("expected a line"),
            };
            let before = length_of(&doc, b);

            // Rotate the reference to vertical — the worst case, where the
            // projection of the follower onto it is a single point.
            let xf = oxidraft_geometry::Transform2d::rotation_about(
                &Point2d::from_f64(0.0, 0.0),
                std::f64::consts::FRAC_PI_2,
            );
            if let Some(e) = doc.get_mut(a) {
                e.transform(&xf);
            }
            resolve_after_transform(&mut doc, &[a]);
            let after = length_of(&doc, b);
            assert!(
                (after - before).abs() < 1e-6,
                "{kind:?}: follower must rotate, not shrink: {before} -> {after}"
            );
        }
    }

    #[test]
    fn rigid_groups_follow_a_transform_instead_of_deforming() {
        // `lower_block` freezes its targets from the sketch it is handed —
        // which, on the transform path, is already the post-edit geometry. So
        // every residual arrives at zero, the solve converges in no iterations
        // and the un-moved members simply stay behind while the moved one
        // walks off. `resolve_after_transform` reports success either way.
        let mut doc = Document::new();
        let a = doc.add(EntityKind::Curve(Curve::Line(LineSeg::from_endpoints(
            Point2d::from_f64(0.0, 0.0),
            Point2d::from_f64(4.0, 0.0),
        ))));
        let b = doc.add(EntityKind::Curve(Curve::Line(LineSeg::from_endpoints(
            Point2d::from_f64(0.0, 3.0),
            Point2d::from_f64(4.0, 3.0),
        ))));
        constrain_lines(&mut doc, &[a, b], ConstraintKind::Block).expect("record");
        let ends_of = |d: &Document, id| match d.get(id).unwrap().as_curve().unwrap() {
            Curve::Line(l) => (l.p0.to_f64(), l.p1.to_f64()),
            _ => panic!("expected a line"),
        };
        let (b0, b1) = ends_of(&doc, b);

        let xf = oxidraft_geometry::Transform2d::translation(10.0, 0.0);
        if let Some(e) = doc.get_mut(a) {
            e.transform(&xf);
        }
        assert!(resolve_after_transform_rigid(&mut doc, &[a], &xf));

        let (n0, n1) = ends_of(&doc, b);
        assert!(
            (n0.0 - (b0.0 + 10.0)).abs() < 1e-6
                && (n1.0 - (b1.0 + 10.0)).abs() < 1e-6
                && (n0.1 - b0.1).abs() < 1e-6
                && (n1.1 - b1.1).abs() < 1e-6,
            "the rest of the group must travel with the transform, not stay \
             behind: {b0:?}..{b1:?} -> {n0:?}..{n1:?}"
        );
    }

    #[test]
    fn a_whole_star_group_travels_when_driven_from_the_middle() {
        // `constrain_lines` records an N-entity Block as pair constraints all
        // hubbed on the FIRST entity. Driven from any other member, the rest
        // of the group is two hops away, so a single-hop follower search
        // moves the hub and leaves the far members behind — the group opens
        // up exactly as if it had never been blocked.
        let mut doc = Document::new();
        let mut ids = Vec::new();
        for i in 0..3 {
            ids.push(
                doc.add(EntityKind::Curve(Curve::Line(LineSeg::from_endpoints(
                    Point2d::from_f64(0.0, 3.0 * f64::from(i)),
                    Point2d::from_f64(4.0, 3.0 * f64::from(i)),
                )))),
            );
        }
        constrain_lines(&mut doc, &ids, ConstraintKind::Block).expect("record");
        let x_of = |d: &Document, id| match d.get(id).unwrap().as_curve().unwrap() {
            Curve::Line(l) => l.p0.to_f64().0,
            _ => panic!("expected a line"),
        };

        // Drive from the middle — the member that is NOT the hub.
        let xf = oxidraft_geometry::Transform2d::translation(10.0, 0.0);
        if let Some(e) = doc.get_mut(ids[1]) {
            e.transform(&xf);
        }
        resolve_after_transform_rigid(&mut doc, &[ids[1]], &xf);

        for &id in &ids {
            let x = x_of(&doc, id);
            assert!(
                (x - 10.0).abs() < 1e-6,
                "every member of the group must travel, including the ones \
                 reachable only through the hub; {id:?} sits at x={x}"
            );
        }
    }

    #[test]
    fn a_pinned_member_is_not_dragged_by_its_group() {
        // `Fixed` is a single-entity pin, and `pin_shape` freezes the entity
        // wherever it sits at solve time. So dragging a pinned member along
        // would not be caught and undone by the solver — it would re-pin the
        // entity at the new spot and quietly discard the user's anchor.
        let mut doc = Document::new();
        let a = doc.add(EntityKind::Curve(Curve::Line(LineSeg::from_endpoints(
            Point2d::from_f64(0.0, 0.0),
            Point2d::from_f64(4.0, 0.0),
        ))));
        let b = doc.add(EntityKind::Curve(Curve::Line(LineSeg::from_endpoints(
            Point2d::from_f64(0.0, 3.0),
            Point2d::from_f64(4.0, 3.0),
        ))));
        constrain_lines(&mut doc, &[a, b], ConstraintKind::Block).expect("record");
        doc.constraints
            .push(oxidraft_document::SketchConstraint::fixed(b));

        let xf = oxidraft_geometry::Transform2d::translation(10.0, 0.0);
        if let Some(e) = doc.get_mut(a) {
            e.transform(&xf);
        }
        resolve_after_transform_rigid(&mut doc, &[a], &xf);

        let Curve::Line(l) = doc.get(b).unwrap().as_curve().unwrap() else {
            panic!("expected a line");
        };
        assert!(
            l.p0.to_f64().0.abs() < 1e-6,
            "a pinned entity must hold its ground; it moved to x={}",
            l.p0.to_f64().0
        );
    }

    #[test]
    fn a_non_rigid_neighbour_is_not_dragged_along() {
        // The follower search keys on Block/Fixed only. A Parallel pair shares
        // the same shape as the rigid case, so it is the check that the fix
        // moves geometry for the right reason rather than moving whatever it
        // finds attached.
        let mut doc = Document::new();
        let a = doc.add(EntityKind::Curve(Curve::Line(LineSeg::from_endpoints(
            Point2d::from_f64(0.0, 0.0),
            Point2d::from_f64(4.0, 0.0),
        ))));
        let b = doc.add(EntityKind::Curve(Curve::Line(LineSeg::from_endpoints(
            Point2d::from_f64(0.0, 3.0),
            Point2d::from_f64(4.0, 3.0),
        ))));
        constrain_lines(&mut doc, &[a, b], ConstraintKind::Parallel).expect("record");

        let xf = oxidraft_geometry::Transform2d::translation(10.0, 0.0);
        if let Some(e) = doc.get_mut(a) {
            e.transform(&xf);
        }
        resolve_after_transform_rigid(&mut doc, &[a], &xf);

        let Curve::Line(l) = doc.get(b).unwrap().as_curve().unwrap() else {
            panic!("expected a line");
        };
        assert!(
            l.p0.to_f64().0.abs() < 1e-6,
            "Parallel is satisfied where it stands; sliding it {} in x is a \
             translation nobody asked for",
            l.p0.to_f64().0
        );
    }

    #[test]
    fn resolve_after_transform_survives_the_axis_aligned_saddle() {
        // Pure-angle residuals have an exact saddle 90° from the target;
        // axis-aligned drawings land on it exactly. a is pinned vertical,
        // b sits exactly horizontal — plain solve() stalls on the saddle
        // and the recorded Parallel is silently left violated.
        let mut doc = Document::new();
        let a = add_line(&mut doc, 0.0, 0.0, 0.0, 5.0);
        let b = add_line(&mut doc, 2.0, 0.0, 7.0, 0.0);
        doc.add_constraint(SketchConstraint::fixed(a));
        doc.add_constraint(SketchConstraint::pair(ConstraintKind::Parallel, a, b));
        assert!(
            resolve_after_transform(&mut doc, &[a]),
            "the saddle start must not be reported as unsolvable"
        );
        let lb = line_of(&doc, b).unwrap();
        assert!(
            (lb.p1.x - lb.p0.x).abs() < 1e-6,
            "b must actually be vertical after the resolve: {lb:?}"
        );
    }

    #[test]
    fn line_distance_does_not_settle_with_the_mover_crossing_the_reference() {
        // |signed distance| − v is also zero with b's endpoints on OPPOSITE
        // sides of a. Without an explicit Parallel row the solver accepts
        // that crossing state as fully satisfied.
        let mut doc = Document::new();
        let a = add_line(&mut doc, 0.0, 0.0, 10.0, 0.0);
        let b = add_line(&mut doc, 0.0, 5.0, 10.0, 5.0);
        doc.add_constraint(SketchConstraint::fixed(a));
        constrain_line_distance(&mut doc, &[a, b], Some(5.0)).expect("must solve");
        // Push b into the crossing state: both endpoints at distance 5 but
        // on opposite sides of a. The two |distance| rows are exactly
        // satisfied there, so the old lowering CONVERGED and left b crossing
        // the reference while the record claimed a parallel width. With the
        // Parallel row the state is correctly refused (the residual barrier
        // at the crossing means it cannot be solved from here), so the
        // caller keeps/rolls back its own edit instead of accepting junk.
        set_line(&mut doc, b, 0.0, 5.0, 10.0, -5.0);
        assert!(
            !resolve_after_edit(&mut doc, a, None),
            "the crossing state must be refused, not accepted as satisfied"
        );
        // A full flip to the far side is a legitimate second state of an
        // unsigned width and must still resolve fine.
        set_line(&mut doc, b, 0.0, -5.0, 10.0, -5.0);
        assert!(resolve_after_edit(&mut doc, a, None));
        let lb = line_of(&doc, b).unwrap();
        assert!(
            lb.p0.y.signum() == lb.p1.y.signum()
                && (lb.p0.y.abs() - 5.0).abs() < 1e-6
                && (lb.p1.y.abs() - 5.0).abs() < 1e-6,
            "the flipped-but-parallel state stays valid: {lb:?}"
        );
    }

    #[test]
    fn dof_report_flags_a_transitively_redundant_parallel() {
        // Three lines: a‖b and b‖c already force a‖c — recording it too
        // (a real thing a user might do, e.g. via three separate PAR
        // commands) is redundant, not a new relation.
        let mut doc = Document::new();
        let a = add_line(&mut doc, 0.0, 0.0, 4.0, 0.0);
        let b = add_line(&mut doc, 0.0, 1.0, 4.0, 1.3);
        let c = add_line(&mut doc, 0.0, 2.0, 4.0, 2.2);
        constrain_lines(&mut doc, &[a, b], ConstraintKind::Parallel).unwrap();
        constrain_lines(&mut doc, &[b, c], ConstraintKind::Parallel).unwrap();
        constrain_lines(&mut doc, &[a, c], ConstraintKind::Parallel).unwrap();
        let report = dof_report(&doc, &[a]);
        assert_eq!(
            report.redundant.len(),
            1,
            "exactly one of the three Parallel records is redundant: {:?}",
            report.redundant
        );
        assert_eq!(
            doc.constraints[report.redundant[0]].kind,
            ConstraintKind::Parallel
        );
    }

    #[test]
    fn distance_conflict_is_named_in_the_error_and_left_unrecorded() {
        // A closed triangle with side lengths 1, 1, 10 violates the
        // triangle inequality — no real triangle has those sides, so the
        // third length lock can never solve against the other two.
        let mut doc = Document::new();
        let a = add_line(&mut doc, 0.0, 0.0, 1.0, 0.0);
        let b = add_line(&mut doc, 1.0, 0.0, 1.0, 1.0);
        let c = add_line(&mut doc, 1.0, 1.0, 0.0, 0.0);
        doc.add_constraint(SketchConstraint::coincident(a, 1, b, 0));
        doc.add_constraint(SketchConstraint::coincident(b, 1, c, 0));
        doc.add_constraint(SketchConstraint::coincident(c, 1, a, 0));
        constrain_distance(&mut doc, &[a], Some(1.0)).unwrap();
        constrain_distance(&mut doc, &[b], Some(1.0)).unwrap();
        let before = doc.constraints.len();
        let err = constrain_distance(&mut doc, &[c], Some(10.0)).unwrap_err();
        // Both the other length locks AND either weld are legitimate
        // leave-one-out culprits (breaking the loop "solves" it too), so
        // the message names both kinds.
        assert!(
            err.message.contains("conflicts with its existing") && err.message.contains("length"),
            "names the conflicting kind(s): {err}"
        );
        assert_eq!(
            doc.constraints.len(),
            before,
            "the impossible length is not left recorded"
        );
    }

    #[test]
    fn point_line_distance_holds_the_point_off_the_line() {
        // The point starts 4 above a horizontal line. Recording the relation
        // at its current separation must be a no-op; dragging the LINE down
        // must then carry the point with it, keeping the gap at 4.
        let mut doc = Document::new();
        let l = add_line(&mut doc, 0.0, 0.0, 10.0, 0.0);
        let p = doc.add(EntityKind::Point(Point2d::from_f64(5.0, 4.0)));
        constrain_point_line_distance(&mut doc, (p, 0), l, None, None)
            .expect("recording the current separation must hold");

        set_line(&mut doc, l, 0.0, -3.0, 10.0, -3.0);
        assert!(resolve_after_edit(&mut doc, l, None));

        let moved = point_of(&doc, p).expect("the point survives");
        assert!(
            ((moved.y - (-3.0)).abs() - 4.0).abs() < 1e-6,
            "the point stays 4 from the line, got y = {}",
            moved.y
        );
    }

    #[test]
    fn point_line_distance_refuses_a_zero_gap() {
        // Zero separation is PointOnLine, not a driving distance -- the same
        // rule `constrain_point_distance` already applies to its own kinds.
        let mut doc = Document::new();
        let l = add_line(&mut doc, 0.0, 0.0, 10.0, 0.0);
        let p = doc.add(EntityKind::Point(Point2d::from_f64(5.0, 0.0)));
        assert!(constrain_point_line_distance(&mut doc, (p, 0), l, None, None).is_err());
    }
}
