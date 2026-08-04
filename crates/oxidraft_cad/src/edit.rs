//! The modify commands: transforming and reshaping existing entities — erase,
//! move/copy/rotate/scale/mirror, offset, rectangular/polar arrays, join,
//! explode, trim, extend, break, fillet, chamfer, blend, and stretch. Each
//! operates in place on a [`Document`], re-satisfying any constraints touched.

use oxidraft_document::{
    ANCHOR_DERIVED, ConstraintKind, Document, EntityId, EntityKind, SketchConstraint,
};
use oxidraft_geometry::{
    CircularArc, Continuity, Curve, CurveSegment, EllipticalArc, LineSeg, MinTracker, Point2d,
    PolyCurve, Transform2d, blend_curves, intersect, offset_curve, point_to_curve_distance,
    reverse_curve, split_curve,
};

/// Endpoint positions of a constrainable entity (line or partial arc);
/// index 0 is the line start / arc start angle.
fn end_positions(doc: &Document, id: EntityId) -> Option<[(f64, f64); 2]> {
    match doc.get(id)?.as_curve()? {
        Curve::Line(l) => Some([l.p0.to_f64(), l.p1.to_f64()]),
        Curve::Arc(a) if (a.end_angle - a.start_angle).abs() < std::f64::consts::TAU - 1e-9 => {
            let (cx, cy) = a.center.to_f64();
            let at = |th: f64| (cx + a.radius * th.cos(), cy + a.radius * th.sin());
            Some([at(a.start_angle), at(a.end_angle)])
        }
        _ => None,
    }
}

/// World position of one coincident anchor: 0/1 an endpoint, ANCHOR_DERIVED
/// a line's midpoint or an arc's center. A point entity is its own anchor
/// at any index.
fn anchor_position(doc: &Document, id: EntityId, idx: u8) -> Option<(f64, f64)> {
    match &doc.get(id)?.kind {
        EntityKind::Point(p) => Some(p.to_f64()),
        EntityKind::Curve(Curve::Line(l)) if idx == ANCHOR_DERIVED => {
            let (x0, y0) = l.p0.to_f64();
            let (x1, y1) = l.p1.to_f64();
            Some(((x0 + x1) * 0.5, (y0 + y1) * 0.5))
        }
        EntityKind::Curve(Curve::Arc(a)) if idx == ANCHOR_DERIVED => Some(a.center.to_f64()),
        _ if idx <= 1 => end_positions(doc, id).map(|e| e[idx as usize]),
        _ => None,
    }
}

/// Drops coincident constraints touching any of `ids` whose welded
/// endpoints no longer sit on top of each other — an edit (fillet trim,
/// extend, …) moved an endpoint away, and keeping the weld would make the
/// next constraint solve yank the geometry back together.
fn prune_broken_welds(doc: &mut Document, ids: &[EntityId]) {
    let keep: Vec<bool> = doc
        .constraints
        .iter()
        .map(|c| {
            if c.kind != ConstraintKind::Coincident {
                return true;
            }
            let touches = ids.contains(&c.a) || c.b.map(|b| ids.contains(&b)).unwrap_or(false);
            if !touches {
                return true;
            }
            let (Some(b), Some((ea, eb))) = (c.b, c.pts) else {
                return true;
            };
            let (Some(p), Some(q)) = (anchor_position(doc, c.a, ea), anchor_position(doc, b, eb))
            else {
                return false;
            };
            (p.0 - q.0).hypot(p.1 - q.1) <= 1e-6
        })
        .collect();
    let mut it = keep.into_iter();
    doc.constraints.retain(|_| it.next().unwrap_or(true));
}

/// Carries the constraints of a removed entity over to the pieces that
/// replaced it (trim/break). The carrier geometry is unchanged, so
/// direction-based relations transfer to every piece; welds re-attach to
/// the piece that kept the shared endpoint; length-based relations drop.
fn remap_constraints_to_pieces(
    doc: &mut Document,
    old: EntityId,
    old_cons: &[SketchConstraint],
    old_ends: Option<[(f64, f64); 2]>,
    pieces: &[EntityId],
) {
    for c in old_cons {
        match c.kind {
            // Length-based relations no longer describe the shorter pieces;
            // Fixed only ever applies to point entities, which are never
            // trimmed into pieces. Anchor-based relations (midpoint,
            // point-on-curve, point distances) name picked points the
            // pieces may no longer carry — dropped the same way.
            ConstraintKind::EqualLength
            | ConstraintKind::Distance
            | ConstraintKind::Fixed
            | ConstraintKind::Midpoint
            | ConstraintKind::PointOnLine
            | ConstraintKind::PointOnCircle
            | ConstraintKind::PointDistance
            | ConstraintKind::HDistance
            | ConstraintKind::VDistance
            | ConstraintKind::PointLineDistance
            | ConstraintKind::Symmetric
            | ConstraintKind::Block => {}
            ConstraintKind::Coincident => {
                let (other, other_end, my_end) = if c.a == old {
                    let (Some(b), Some((ea, eb))) = (c.b, c.pts) else {
                        continue;
                    };
                    (b, eb, ea)
                } else {
                    let Some((ea, eb)) = c.pts else { continue };
                    (c.a, ea, eb)
                };
                // A weld on the removed entity's derived point (midpoint)
                // names a spot no piece still has — drop it like the
                // length-based kinds.
                if my_end > 1 {
                    continue;
                }
                let Some(p) = old_ends.map(|e| e[my_end as usize]) else {
                    continue;
                };
                for &piece in pieces {
                    if piece == other {
                        continue;
                    }
                    let Some(ends) = end_positions(doc, piece) else {
                        continue;
                    };
                    for (pi, pp) in ends.iter().enumerate() {
                        if (pp.0 - p.0).hypot(pp.1 - p.1) <= 1e-6 {
                            doc.add_constraint(SketchConstraint::coincident(
                                piece, pi as u8, other, other_end,
                            ));
                        }
                    }
                }
            }
            ConstraintKind::Horizontal | ConstraintKind::Vertical => {
                for &piece in pieces {
                    if matches!(
                        doc.get(piece).and_then(|e| e.as_curve()),
                        Some(Curve::Line(_))
                    ) {
                        doc.add_constraint(SketchConstraint::single(c.kind, piece));
                    }
                }
            }
            // LineDistance rides along here: trimming doesn't move the
            // carrier's infinite line, so the driving width still describes
            // every piece (and, like Angle, it keeps its value). Collinear
            // (same carrier), Concentric and EqualRadius (same center and
            // radius on every arc piece) survive trimming the same way.
            ConstraintKind::Parallel
            | ConstraintKind::Perpendicular
            | ConstraintKind::Tangent
            | ConstraintKind::LineDistance
            | ConstraintKind::Collinear
            | ConstraintKind::Concentric
            | ConstraintKind::EqualRadius
            | ConstraintKind::Angle => {
                let Some(other) = (if c.a == old { c.b } else { Some(c.a) }) else {
                    continue;
                };
                for &piece in pieces {
                    if piece == other {
                        continue;
                    }
                    let mut rec = if c.a == old {
                        SketchConstraint::pair(c.kind, piece, other)
                    } else {
                        SketchConstraint::pair(c.kind, other, piece)
                    };
                    // Angle keeps its driving value; the others carry none.
                    rec.val = c.val;
                    doc.add_constraint(rec);
                }
            }
            ConstraintKind::Radius => {
                // Trimming an arc keeps its radius: every arc piece
                // inherits the driving value.
                let Some(v) = c.val else { continue };
                for &piece in pieces {
                    if matches!(
                        doc.get(piece).and_then(|e| e.as_curve()),
                        Some(Curve::Arc(_))
                    ) {
                        doc.add_constraint(SketchConstraint::radius(piece, v));
                    }
                }
            }
        }
    }
}

/// Deletes the given entities (and any constraints on them) from the document.
pub fn erase(doc: &mut Document, ids: &[EntityId]) {
    for &id in ids {
        doc.remove(id);
    }
}

/// Breaks polycurves into their individual segment entities; returns the new
/// segment ids. Non-polycurve entities are left untouched.
pub fn explode(doc: &mut Document, ids: &[EntityId]) -> Vec<EntityId> {
    let mut new_ids = Vec::new();
    for &id in ids {
        let Some(e) = doc.get(id) else { continue };
        let Some(Curve::Poly(pc)) = e.as_curve() else {
            continue;
        };
        let (segments, layer) = (pc.segments.clone(), e.layer);
        for seg in segments {
            new_ids.push(doc.add_on_layer(EntityKind::Curve(seg), layer));
        }
        doc.remove(id);
    }
    new_ids
}

/// Translates the given entities by `(dx, dy)` in place.
pub fn move_by(doc: &mut Document, ids: &[EntityId], dx: f64, dy: f64) {
    let t = Transform2d::translation(dx, dy);
    apply_to(doc, ids, &t);
}

/// Duplicates the given entities translated by `(dx, dy)`; returns the copies.
pub fn copy_by(doc: &mut Document, ids: &[EntityId], dx: f64, dy: f64) -> Vec<EntityId> {
    let t = Transform2d::translation(dx, dy);
    duplicate_with(doc, ids, &t)
}

/// Rotates the given entities about `center` by `angle` radians.
pub fn rotate(doc: &mut Document, ids: &[EntityId], center: &Point2d, angle: f64) {
    let t = Transform2d::rotation_about(center, angle);
    apply_to(doc, ids, &t);
}

/// Uniformly scales the given entities about `base` by factor `s`.
pub fn scale(doc: &mut Document, ids: &[EntityId], base: &Point2d, s: f64) {
    let t = Transform2d::scale_about(base, s, s);
    apply_to(doc, ids, &t);
}

/// Mirrors the given entities across a line; optionally keeps the originals.
pub fn mirror(
    doc: &mut Document,
    ids: &[EntityId],
    p0: &Point2d,
    p1: &Point2d,
    keep_original: bool,
) -> Vec<EntityId> {
    let t = Transform2d::mirror_line(p0, p1);
    if keep_original {
        duplicate_with(doc, ids, &t)
    } else {
        apply_to(doc, ids, &t);
        ids.to_vec()
    }
}

/// Adds offset copies of the given curves at signed distance `dist`; returns
/// the new entities.
pub fn offset(doc: &mut Document, ids: &[EntityId], dist: f64) -> Vec<EntityId> {
    // offset_curve survives a non-finite distance but its output is
    // unspecified; there is no meaningful offset to add anyway.
    if !dist.is_finite() {
        return Vec::new();
    }
    let mut new_ids = Vec::new();
    for &id in ids {
        if let Some(e) = doc.get(id)
            && let Some(c) = e.as_curve()
        {
            let off = offset_curve(c, dist);
            let layer = e.layer;
            new_ids.push(doc.add_on_layer(EntityKind::Curve(off), layer));
        }
    }
    new_ids
}

/// Upper bound on the elements a single array command may create.
const MAX_ARRAY_ELEMENTS: u64 = 100_000;

/// Creates a rectangular grid of copies of the given entities; returns the new
/// copies (the originals stay put).
pub fn array_rect(
    doc: &mut Document,
    ids: &[EntityId],
    rows: u32,
    cols: u32,
    dx: f64,
    dy: f64,
) -> Vec<EntityId> {
    // Mistyped counts must not freeze the app duplicating entities
    // forever (matches AutoCAD's array element limit).
    if rows as u64 * cols as u64 > MAX_ARRAY_ELEMENTS {
        return Vec::new();
    }
    let mut new_ids = Vec::new();
    for r in 0..rows {
        for c in 0..cols {
            if r == 0 && c == 0 {
                continue;
            }
            let tx = dx * c as f64;
            let ty = dy * r as f64;
            let t = Transform2d::translation(tx, ty);
            new_ids.extend(duplicate_with(doc, ids, &t));
        }
    }
    new_ids
}

/// Creates copies of the given entities arrayed around `center` over
/// `total_angle`; returns the new copies.
pub fn array_polar(
    doc: &mut Document,
    ids: &[EntityId],
    center: &Point2d,
    count: u32,
    total_angle: f64,
) -> Vec<EntityId> {
    let mut new_ids = Vec::new();
    if count < 2 || count as u64 > MAX_ARRAY_ELEMENTS {
        return new_ids;
    }
    let step = total_angle / count as f64;
    for k in 1..count {
        let t = Transform2d::rotation_about(center, step * k as f64);
        new_ids.extend(duplicate_with(doc, ids, &t));
    }
    new_ids
}

enum TrimOutcome {
    NoOp,
    RemoveWhole,
    RemoveSpan { lo: f64, hi: f64 },
}

fn trim_outcome(
    doc: &Document,
    curve: &Curve,
    target: EntityId,
    cutters: &[EntityId],
    px: f64,
    py: f64,
) -> TrimOutcome {
    let (t0, t1) = curve.domain();
    let span = t1 - t0;
    let mut params: Vec<f64> = vec![0.0, 1.0];
    let target_bb = curve.bounding_box();
    for &cid in cutters {
        if cid == target {
            continue;
        }
        let Some(cc) = doc.get(cid).and_then(|e| e.as_curve()) else {
            continue;
        };
        if !target_bb.intersects(&cc.bounding_box()) {
            continue;
        }
        for hit in intersect(curve, cc) {
            let tn = (hit.t1 - t0) / span;
            if tn > 1e-6 && tn < 1.0 - 1e-6 {
                params.push(tn);
            }
        }
    }
    params.sort_by(f64::total_cmp);
    params.dedup_by(|a, b| (*a - *b).abs() < 1e-6);
    if params.len() <= 2 {
        let eps = 1e-6;
        let touches = |x: f64, y: f64| {
            cutters.iter().any(|&cid| {
                cid != target
                    && doc
                        .get(cid)
                        .and_then(|e| e.as_curve())
                        .map(|c| point_to_curve_distance(c, x, y) < eps)
                        .unwrap_or(false)
            })
        };
        let (sx, sy) = curve.evaluate_f64(t0);
        let (ex, ey) = curve.evaluate_f64(t1);
        if touches(sx, sy) || touches(ex, ey) {
            return TrimOutcome::RemoveWhole;
        }
        return TrimOutcome::NoOp;
    }

    let pick_t = normalized_pick_param(curve, px, py);
    let (mut lo, mut hi) = (0.0, 1.0);
    for w in params.windows(2) {
        if pick_t >= w[0] && pick_t <= w[1] {
            lo = w[0];
            hi = w[1];
            break;
        }
    }
    TrimOutcome::RemoveSpan { lo, hi }
}

/// Joins connected curves end-to-end into polycurves; returns the ids of the
/// resulting joined entities (empty when nothing could be joined).
pub fn join(doc: &mut Document, ids: &[EntityId]) -> Vec<EntityId> {
    const EPS: f64 = 1e-6;
    let mut segs: Vec<(Curve, EntityId, usize)> = Vec::new();
    for &id in ids {
        if let Some(e) = doc.get(id) {
            match e.as_curve() {
                Some(Curve::Poly(pc)) => {
                    segs.extend(pc.segments.iter().cloned().map(|c| (c, id, e.layer)))
                }
                Some(c) => segs.push((c.clone(), id, e.layer)),
                None => {}
            }
        }
    }
    if segs.len() < 2 {
        return Vec::new();
    }

    let ep = |c: &Curve| {
        let (t0, t1) = c.domain();
        (c.evaluate_f64(t0), c.evaluate_f64(t1))
    };
    let near = |a: (f64, f64), b: (f64, f64)| sq_dist(a, b) < EPS * EPS;

    let mut used = vec![false; segs.len()];
    let mut new_ids = Vec::new();
    let mut consumed_sources: Vec<EntityId> = Vec::new();

    for start in 0..segs.len() {
        if used[start] {
            continue;
        }
        used[start] = true;
        let (curve0, src0, layer) = segs[start].clone();
        let (mut head, mut tail) = ep(&curve0);
        let mut chain: std::collections::VecDeque<Curve> = std::collections::VecDeque::new();
        chain.push_back(curve0);
        let mut sources = vec![src0];

        let mut grew = true;
        while grew {
            grew = false;
            for k in 0..segs.len() {
                if used[k] {
                    continue;
                }
                let (s, en) = ep(&segs[k].0);
                if near(s, tail) {
                    let c = segs[k].0.clone();
                    tail = en;
                    chain.push_back(c);
                } else if near(en, tail) {
                    let c = reverse_curve(&segs[k].0);
                    tail = s;
                    chain.push_back(c);
                } else if near(en, head) {
                    let c = segs[k].0.clone();
                    head = s;
                    chain.push_front(c);
                } else if near(s, head) {
                    let c = reverse_curve(&segs[k].0);
                    head = en;
                    chain.push_front(c);
                } else {
                    continue;
                }
                used[k] = true;
                sources.push(segs[k].1);
                grew = true;
            }
        }

        if chain.len() >= 2 {
            let poly = PolyCurve::new(chain.into_iter().collect());
            new_ids.push(doc.add_on_layer(EntityKind::Curve(Curve::Poly(Box::new(poly))), layer));
            consumed_sources.extend(sources);
        }
    }

    consumed_sources.sort();
    consumed_sources.dedup();
    for id in consumed_sources {
        doc.remove(id);
    }
    new_ids
}

fn closed_conic_trim(
    doc: &Document,
    curve: &Curve,
    target: EntityId,
    cutters: &[EntityId],
    px: f64,
    py: f64,
) -> Option<(Curve, Curve)> {
    if !is_full_conic(curve) {
        return None;
    }
    let cuts = full_conic_cut_fracs(doc, curve, target, cutters);
    if cuts.len() < 2 {
        return None;
    }
    let pick = normalized_pick_param(curve, px, py).rem_euclid(1.0);
    let n = cuts.len();
    for i in 0..n - 1 {
        if pick >= cuts[i] && pick <= cuts[i + 1] {
            return Some((
                conic_arc(curve, cuts[i], cuts[i + 1]),
                conic_arc(curve, cuts[i + 1], cuts[i] + 1.0),
            ));
        }
    }
    Some((
        conic_arc(curve, cuts[n - 1], cuts[0] + 1.0),
        conic_arc(curve, cuts[0], cuts[n - 1]),
    ))
}

fn is_full_conic(curve: &Curve) -> bool {
    let span = match curve {
        Curve::Arc(a) => (a.end_angle - a.start_angle).abs(),
        Curve::Ellipse(e) => (e.end_angle - e.start_angle).abs(),
        _ => return false,
    };
    (span - std::f64::consts::TAU).abs() < 1e-9
}

fn full_conic_cut_fracs(
    doc: &Document,
    curve: &Curve,
    target: EntityId,
    cutters: &[EntityId],
) -> Vec<f64> {
    let (t0, t1) = curve.domain();
    let span = t1 - t0;
    let bb = curve.bounding_box();
    let mut fr: Vec<f64> = Vec::new();
    for &cid in cutters {
        if cid == target {
            continue;
        }
        let Some(cc) = doc.get(cid).and_then(|e| e.as_curve()) else {
            continue;
        };
        if !bb.intersects(&cc.bounding_box()) {
            continue;
        }
        for hit in intersect(curve, cc) {
            fr.push(((hit.t1 - t0) / span).rem_euclid(1.0));
        }
    }
    fr.sort_by(f64::total_cmp);
    fr.dedup_by(|a, b| (*a - *b).abs() < 1e-6);
    fr
}

fn conic_arc(curve: &Curve, fa: f64, fb: f64) -> Curve {
    match curve {
        Curve::Arc(a) => {
            let s = a.start_angle;
            let span = a.end_angle - a.start_angle;
            Curve::Arc(CircularArc::new(
                a.center,
                a.radius,
                s + fa * span,
                s + fb * span,
            ))
        }
        Curve::Ellipse(e) => {
            let s = e.start_angle;
            let span = e.end_angle - e.start_angle;
            Curve::Ellipse(EllipticalArc::new(
                e.center,
                e.semi_major,
                e.semi_minor,
                e.rotation,
                s + fa * span,
                s + fb * span,
            ))
        }
        _ => curve.clone(),
    }
}

/// Trims `target` at the point `(px, py)` against the cutting `cutters`,
/// removing the picked span; returns the surviving piece ids (unchanged when
/// there's nothing to cut).
pub fn trim(
    doc: &mut Document,
    target: EntityId,
    cutters: &[EntityId],
    px: f64,
    py: f64,
) -> Vec<EntityId> {
    let (curve, layer) = match doc.get(target) {
        Some(e) => match e.as_curve() {
            Some(c) => (c.clone(), e.layer),
            None => return vec![target],
        },
        None => return vec![],
    };

    let old_cons: Vec<SketchConstraint> = doc.constraints_on(target).copied().collect();
    let old_ends = end_positions(doc, target);

    if let Some((_removed, survivor)) = closed_conic_trim(doc, &curve, target, cutters, px, py) {
        doc.remove(target);
        let id = doc.add_on_layer(EntityKind::Curve(survivor), layer);
        remap_constraints_to_pieces(doc, target, &old_cons, old_ends, &[id]);
        return vec![id];
    }

    match trim_outcome(doc, &curve, target, cutters, px, py) {
        TrimOutcome::NoOp => vec![target],
        TrimOutcome::RemoveWhole => {
            doc.remove(target);
            vec![]
        }
        TrimOutcome::RemoveSpan { lo, hi } => {
            let mut survivors = Vec::new();
            doc.remove(target);
            if lo > 1e-6 {
                let piece = extract_piece(&curve, 0.0, lo);
                survivors.push(doc.add_on_layer(EntityKind::Curve(piece), layer));
            }
            if hi < 1.0 - 1e-6 {
                let piece = extract_piece(&curve, hi, 1.0);
                survivors.push(doc.add_on_layer(EntityKind::Curve(piece), layer));
            }
            remap_constraints_to_pieces(doc, target, &old_cons, old_ends, &survivors);
            survivors
        }
    }
}

/// The span [`trim`] would remove for this pick, without modifying the
/// document — for live highlight. `None` when trimming would do nothing.
pub fn trim_preview(
    doc: &Document,
    target: EntityId,
    cutters: &[EntityId],
    px: f64,
    py: f64,
) -> Option<Curve> {
    let curve = doc.get(target)?.as_curve()?.clone();
    if let Some((removed, _survivor)) = closed_conic_trim(doc, &curve, target, cutters, px, py) {
        return Some(removed);
    }
    match trim_outcome(doc, &curve, target, cutters, px, py) {
        TrimOutcome::NoOp => None,
        TrimOutcome::RemoveWhole => Some(curve),
        TrimOutcome::RemoveSpan { lo, hi } => Some(extract_piece(&curve, lo, hi)),
    }
}

/// Splits `target` into two entities at parameter `t`; returns the piece ids.
pub fn break_at(doc: &mut Document, target: EntityId, t: f64) -> Vec<EntityId> {
    // Breaking at an undefined parameter is a no-op, not two junk pieces.
    if !t.is_finite() {
        return vec![target];
    }
    let (curve, layer) = match doc
        .get(target)
        .and_then(|e| e.as_curve().map(|c| (c.clone(), e.layer)))
    {
        Some(v) => v,
        None => return vec![target],
    };
    let old_cons: Vec<SketchConstraint> = doc.constraints_on(target).copied().collect();
    let old_ends = end_positions(doc, target);
    let (left, right) = split_curve(&curve, t);
    doc.remove(target);
    let pieces = vec![
        doc.add_on_layer(EntityKind::Curve(left), layer),
        doc.add_on_layer(EntityKind::Curve(right), layer),
    ];
    remap_constraints_to_pieces(doc, target, &old_cons, old_ends, &pieces);
    pieces
}

enum ExtendSolution {
    Line {
        which_p1: bool,
        hit: Point2d,
    },
    Arc {
        set_end: bool,
        angle: f64,
        added: Curve,
    },
}

fn extend_solution(
    doc: &Document,
    target: EntityId,
    boundaries: &[EntityId],
    px: f64,
    py: f64,
) -> Option<ExtendSolution> {
    let curve = doc.get(target)?.as_curve()?.clone();
    match curve {
        Curve::Line(l) => extend_line_solution(doc, &l, target, boundaries, px, py),
        Curve::Arc(a) => extend_arc_solution(doc, &a, target, boundaries, px, py),
        _ => None,
    }
}

fn extend_line_solution(
    doc: &Document,
    l: &LineSeg,
    target: EntityId,
    boundaries: &[EntityId],
    px: f64,
    py: f64,
) -> Option<ExtendSolution> {
    let (p0, p1) = (l.p0.to_f64(), l.p1.to_f64());
    let (m, far, which_p1) = if sq_dist(p1, (px, py)) < sq_dist(p0, (px, py)) {
        (p1, p0, true)
    } else {
        (p0, p1, false)
    };
    let (dx, dy) = (m.0 - far.0, m.1 - far.1);
    let len = (dx * dx + dy * dy).sqrt();
    if len < 1e-12 {
        return None;
    }
    let (ux, uy) = (dx / len, dy / len);

    let mut best = MinTracker::new();
    for &bid in boundaries {
        if bid == target {
            continue;
        }
        let Some(bc) = doc.get(bid).and_then(|e| e.as_curve()) else {
            continue;
        };
        let big = ray_len_for(m, bc);
        let ray = Curve::Line(LineSeg::from_endpoints(
            Point2d::from_f64(m.0, m.1),
            Point2d::from_f64(m.0 + ux * big, m.1 + uy * big),
        ));
        for hit in intersect(&ray, bc) {
            let dist = hit.t1 * big;
            if dist > 1e-7 {
                best.offer(dist, Point2d::from_f64(hit.point.0, hit.point.1));
            }
        }
    }
    best.value()
        .map(|hit| ExtendSolution::Line { which_p1, hit })
}

fn extend_arc_solution(
    doc: &Document,
    a: &CircularArc,
    target: EntityId,
    boundaries: &[EntityId],
    px: f64,
    py: f64,
) -> Option<ExtendSolution> {
    let (cx, cy) = a.center.to_f64();
    let r = a.radius;
    let sp = (cx + r * a.start_angle.cos(), cy + r * a.start_angle.sin());
    let ep = (cx + r * a.end_angle.cos(), cy + r * a.end_angle.sin());
    let set_end = sq_dist(ep, (px, py)) <= sq_dist(sp, (px, py));

    let circle = Curve::Arc(CircularArc::new(a.center, r, 0.0, std::f64::consts::TAU));
    let mut best = MinTracker::new();
    for &bid in boundaries {
        if bid == target {
            continue;
        }
        let Some(bc) = doc.get(bid).and_then(|e| e.as_curve()) else {
            continue;
        };
        for hit in intersect(&circle, bc) {
            let (hx, hy) = hit.point;
            let ang = (hy - cy).atan2(hx - cx);
            let delta = if set_end {
                norm_pos(ang - a.end_angle)
            } else {
                norm_pos(a.start_angle - ang)
            };
            if delta > 1e-6 {
                best.offer(delta, delta);
            }
        }
    }
    let delta = best.value()?;
    let (angle, added) = if set_end {
        (
            a.end_angle + delta,
            Curve::Arc(CircularArc::new(
                a.center,
                r,
                a.end_angle,
                a.end_angle + delta,
            )),
        )
    } else {
        (
            a.start_angle - delta,
            Curve::Arc(CircularArc::new(
                a.center,
                r,
                a.start_angle - delta,
                a.start_angle,
            )),
        )
    };
    Some(ExtendSolution::Arc {
        set_end,
        angle,
        added,
    })
}

fn ray_len_for(m: (f64, f64), bc: &Curve) -> f64 {
    let bb = bc.bounding_box();
    [
        (bb.min.x, bb.min.y),
        (bb.max.x, bb.min.y),
        (bb.max.x, bb.max.y),
        (bb.min.x, bb.max.y),
    ]
    .iter()
    .map(|&(x, y)| ((x - m.0).powi(2) + (y - m.1).powi(2)).sqrt())
    .fold(0.0_f64, f64::max)
        * 1.5
        + 1.0
}

fn norm_pos(x: f64) -> f64 {
    let t = std::f64::consts::TAU;
    let mut v = x % t;
    if v < 0.0 {
        v += t;
    }
    v
}

/// Extends `target` from the end nearest the pick `(px, py)` until it meets one
/// of the `boundaries`; returns whether it changed.
pub fn extend(
    doc: &mut Document,
    target: EntityId,
    boundaries: &[EntityId],
    px: f64,
    py: f64,
) -> bool {
    let done = match extend_solution(doc, target, boundaries, px, py) {
        Some(ExtendSolution::Line { which_p1, hit }) => {
            let (hx, hy) = hit.to_f64();
            set_line_endpoint(doc, target, which_p1, hx, hy)
        }
        Some(ExtendSolution::Arc { set_end, angle, .. }) => {
            set_arc_endpoint(doc, target, set_end, angle)
        }
        None => false,
    };
    if done {
        // The moved endpoint deliberately left any weld it had; keeping the
        // constraint would snap the extension back on the next solve.
        prune_broken_welds(doc, &[target]);
    }
    done
}

/// The curve [`extend`] would produce for this pick, without modifying the
/// document — for live preview. `None` when it wouldn't reach a boundary.
pub fn extend_preview(
    doc: &Document,
    target: EntityId,
    boundaries: &[EntityId],
    px: f64,
    py: f64,
) -> Option<Curve> {
    match extend_solution(doc, target, boundaries, px, py)? {
        ExtendSolution::Line { which_p1, hit } => {
            let ((x0, y0), (x1, y1)) = line_endpoints(doc, target)?;
            let m = if which_p1 {
                Point2d::from_f64(x1, y1)
            } else {
                Point2d::from_f64(x0, y0)
            };
            Some(Curve::Line(LineSeg::from_endpoints(m, hit)))
        }
        ExtendSolution::Arc { added, .. } => Some(added),
    }
}

/// Rounds the corner between entities `a` and `b` with an arc of the given
/// `radius`, trimming both to meet it; returns the new arc's id, or `None` when
/// no valid fillet exists. `(px, py)` picks the corner side.
pub fn fillet(
    doc: &mut Document,
    a: EntityId,
    b: EntityId,
    radius: f64,
    px: f64,
    py: f64,
) -> Option<EntityId> {
    // NaN slips past a plain `<= 0.0` check and would reach the solver.
    if !radius.is_finite() || radius <= 0.0 || a == b {
        return None;
    }
    let layer = doc.get(a)?.layer;
    let (ea, eb) = match (
        CornerEdge::from_curve(doc.get(a)?.as_curve()?),
        CornerEdge::from_curve(doc.get(b)?.as_curve()?),
    ) {
        (Some(ea), Some(eb)) => (ea, eb),
        // Splines, ellipses, and mixed pairs have no closed form — solve
        // the tangency numerically instead of silently declining.
        _ => return fillet_freeform(doc, a, b, radius, (px, py)),
    };
    let sol = solve_fillet(ea, eb, radius, (px, py))?;
    let vtx = corner_vertex(ea, eb);
    trim_entity_for_corner(doc, a, ea, sol.ta, sol.a_angle, vtx);
    trim_entity_for_corner(doc, b, eb, sol.tb, sol.b_angle, vtx);
    // The trims moved the shared corner apart; a recorded weld between the
    // legs would now contradict the fillet and wedge the solver.
    prune_broken_welds(doc, &[a, b]);
    let arc = arc_between(sol.center, sol.ta, sol.tb, radius);
    Some(doc.add_on_layer(EntityKind::Curve(Curve::Arc(arc)), layer))
}

/// Adds a blend spline joining two entities (line, arc, or spline) with the
/// requested order of geometric continuity (G0–G3).
///
/// The nearest pair of endpoints is chosen automatically, and the source
/// entities are left untouched — the blend is a new connecting curve. `tension`
/// scales the blend's handle length on both sides (1.0 is a good default; larger
/// values bow the blend out further, smaller values pull it tighter to the chord).
pub fn blend(
    doc: &mut Document,
    a: EntityId,
    b: EntityId,
    continuity: Continuity,
    tension: f64,
) -> Option<EntityId> {
    let layer = doc.get(a)?.layer;
    let curve = blend_preview(doc, a, b, continuity, tension)?;
    Some(doc.add_on_layer(EntityKind::Curve(curve), layer))
}

/// Computes the blend curve between two entities without modifying the
/// document — used for both [`blend`]'s commit path and live tool previews.
pub fn blend_preview(
    doc: &Document,
    a: EntityId,
    b: EntityId,
    continuity: Continuity,
    tension: f64,
) -> Option<Curve> {
    // blend_curves survives a non-finite tension but may hand back a
    // non-finite curve; decline before it can reach the document.
    if !tension.is_finite() || a == b {
        return None;
    }
    let ca = doc.get(a)?.as_curve()?.clone();
    let cb = doc.get(b)?.as_curve()?.clone();
    let (a_at_end, b_at_end) = nearest_ends(&ca, &cb);
    blend_curves(&ca, a_at_end, &cb, b_at_end, continuity, tension, tension)
}

/// Endpoint of `c` at its start (`false`) or end (`true`).
fn curve_endpoint(c: &Curve, at_end: bool) -> (f64, f64) {
    let (t0, t1) = c.domain();
    c.evaluate_f64(if at_end { t1 } else { t0 })
}

/// Of the four start/end pairings of two curves, the one whose endpoints are
/// closest — the natural pair to bridge with a blend.
fn nearest_ends(a: &Curve, b: &Curve) -> (bool, bool) {
    let mut best = f64::INFINITY;
    let mut choice = (true, false);
    for &ae in &[false, true] {
        for &be in &[false, true] {
            let pa = curve_endpoint(a, ae);
            let pb = curve_endpoint(b, be);
            let d = (pa.0 - pb.0).hypot(pa.1 - pb.1);
            if d < best {
                best = d;
                choice = (ae, be);
            }
        }
    }
    choice
}

/// Bevels the corner between entities `a` and `b` with a straight segment,
/// cutting `dist_a`/`dist_b` back along each; returns the new segment's id, or
/// `None` for a non-positive distance or invalid corner.
pub fn chamfer(
    doc: &mut Document,
    a: EntityId,
    b: EntityId,
    dist_a: f64,
    dist_b: f64,
) -> Option<EntityId> {
    if !(dist_a.is_finite() && dist_b.is_finite()) || a == b {
        return None;
    }
    let layer = doc.get(a)?.layer;
    let (ea, eb) = match (
        CornerEdge::from_curve(doc.get(a)?.as_curve()?),
        CornerEdge::from_curve(doc.get(b)?.as_curve()?),
    ) {
        (Some(ea), Some(eb)) => (ea, eb),
        // Splines, ellipses, and mixed pairs: measure the distances along
        // the curves from their crossing instead of silently declining.
        _ => return chamfer_freeform(doc, a, b, dist_a, dist_b),
    };
    let sol = solve_chamfer(ea, eb, dist_a, dist_b)?;
    let vtx = corner_vertex(ea, eb);
    trim_entity_for_corner(doc, a, ea, sol.pa, None, vtx);
    trim_entity_for_corner(doc, b, eb, sol.pb, None, vtx);
    prune_broken_welds(doc, &[a, b]);
    let conn = LineSeg::from_endpoints(
        Point2d::from_f64(sol.pa.0, sol.pa.1),
        Point2d::from_f64(sol.pb.0, sol.pb.1),
    );
    Some(doc.add_on_layer(EntityKind::Curve(Curve::Line(conn)), layer))
}

/// Numeric fillet for curve pairs the exact solver doesn't cover. The
/// fillet arc's center sits at distance `radius` from both curves — i.e.
/// on an intersection of their offset curves; the candidate nearest the
/// pick wins, the tangent points come from projecting the center back
/// onto the curves, and each leg keeps its piece on the far side of the
/// tangency from the pick (the pick marks the corner being rounded away).
fn fillet_freeform(
    doc: &mut Document,
    a: EntityId,
    b: EntityId,
    radius: f64,
    pick: (f64, f64),
) -> Option<EntityId> {
    let ca = doc.get(a)?.as_curve()?.clone();
    let cb = doc.get(b)?.as_curve()?.clone();
    if !(ca.is_finite() && cb.is_finite()) {
        return None;
    }
    let mut best = MinTracker::new();
    for sa in [radius, -radius] {
        let oa = offset_curve(&ca, sa);
        for sb in [radius, -radius] {
            for hit in intersect(&oa, &offset_curve(&cb, sb)) {
                let d = (hit.point.0 - pick.0).hypot(hit.point.1 - pick.1);
                if d.is_finite() {
                    best.offer(d, hit.point);
                }
            }
        }
    }
    let center = best.value()?;
    // Spline offsets are approximations, so tolerate a small tangency
    // error — but reject a center that isn't genuinely at ~radius from
    // both curves (offsets can also cross far from any fillet).
    let pa = oxidraft_geometry::project_point_onto_curve(&ca, center.0, center.1);
    let pb = oxidraft_geometry::project_point_onto_curve(&cb, center.0, center.1);
    let tol = (radius * 0.02).max(1e-6);
    if (pa.distance - radius).abs() > tol || (pb.distance - radius).abs() > tol {
        return None;
    }
    // Work out both retained legs before touching the document, so a
    // decline on the second leg can't leave the first one half-trimmed.
    let keep_a = leg_away_from(&ca, pa.t, pb.point)?;
    let keep_b = leg_away_from(&cb, pb.t, pa.point)?;
    doc.get_mut(a)?.kind = EntityKind::Curve(keep_a);
    doc.get_mut(b)?.kind = EntityKind::Curve(keep_b);
    prune_broken_welds(doc, &[a, b]);
    let layer = doc.get(a)?.layer;
    let r = 0.5 * (pa.distance + pb.distance);
    let arc = arc_between(center, pa.point, pb.point, r);
    Some(doc.add_on_layer(EntityKind::Curve(Curve::Arc(arc)), layer))
}

/// The piece of `c` cut at its tangency parameter `t` that leaves the cut
/// heading *away* from `other` (the opposite leg's tangent point) — that
/// is the leg the fillet arc connects to; the piece heading toward the
/// corner is the one being rounded off. Local and exact: moving along the
/// cut tangent `d` increases the distance to `other` iff d·(cut − other)
/// is positive, so leg length never skews the decision the way a
/// midpoint-distance heuristic would. Returns the whole curve when the
/// tangency lands on an end.
fn leg_away_from(c: &Curve, t: f64, other: (f64, f64)) -> Option<Curve> {
    let tn = c.normalized_param(t)?;
    if !(1e-6..=1.0 - 1e-6).contains(&tn) {
        return Some(c.clone());
    }
    let (left, right) = split_curve(c, tn);
    let (cx, cy) = c.evaluate_f64(t);
    let (dx, dy) = c.tangent_f64(t);
    let keep_right = dx * (cx - other.0) + dy * (cy - other.1) >= 0.0;
    let keep = if keep_right { right } else { left };
    keep.is_finite().then_some(keep)
}

/// Numeric chamfer for curve pairs the exact solver doesn't cover: the
/// corner is the curves' (single, unambiguous) crossing, each distance is
/// measured as arc length along its curve from that corner into the
/// longer side, and the shorter side is cut away with the stub.
fn chamfer_freeform(
    doc: &mut Document,
    a: EntityId,
    b: EntityId,
    dist_a: f64,
    dist_b: f64,
) -> Option<EntityId> {
    if !(dist_a > 0.0 && dist_b > 0.0) {
        return None;
    }
    let ca = doc.get(a)?.as_curve()?.clone();
    let cb = doc.get(b)?.as_curve()?.clone();
    if !(ca.is_finite() && cb.is_finite()) {
        return None;
    }
    let hits = intersect(&ca, &cb);
    // Without a pick there is nothing to disambiguate multiple crossings.
    let [hit] = hits.as_slice() else { return None };

    // Both legs solve before the document is touched.
    let (keep_a, end_a) = chamfer_cut(&ca, hit.t1, dist_a)?;
    let (keep_b, end_b) = chamfer_cut(&cb, hit.t2, dist_b)?;
    doc.get_mut(a)?.kind = EntityKind::Curve(keep_a);
    doc.get_mut(b)?.kind = EntityKind::Curve(keep_b);
    prune_broken_welds(doc, &[a, b]);
    let layer = doc.get(a)?.layer;
    let conn = LineSeg::from_endpoints(
        Point2d::from_f64(end_a.0, end_a.1),
        Point2d::from_f64(end_b.0, end_b.1),
    );
    Some(doc.add_on_layer(EntityKind::Curve(Curve::Line(conn)), layer))
}

/// One chamfer leg: from the corner at domain parameter `t`, keep the
/// longer side of the curve and cut `dist` of arc length into it,
/// returning the retained piece and the cut point.
fn chamfer_cut(c: &Curve, t: f64, dist: f64) -> Option<(Curve, (f64, f64))> {
    let tn = c.normalized_param(t)?;
    let total = c.arc_length();
    let s_corner = if tn <= 0.0 {
        0.0
    } else if tn >= 1.0 {
        total
    } else {
        extract_piece(c, 0.0, tn).arc_length()
    };
    let (s_cut, keep_low) = if total - s_corner >= s_corner {
        (s_corner + dist, false)
    } else {
        (s_corner - dist, true)
    };
    if !(s_cut > 1e-9 && s_cut < total - 1e-9) {
        return None; // the distance doesn't fit on the retained side
    }
    let tc = c.param_at_length(s_cut);
    let tcn = c.normalized_param(tc)?;
    let (left, right) = split_curve(c, tcn);
    let keep = if keep_low { left } else { right };
    let end = c.evaluate_f64(tc);
    (keep.is_finite() && end.0.is_finite() && end.1.is_finite()).then_some((keep, end))
}

/// One of the two edges meeting at a corner, reduced to the minimal geometry
/// the fillet/chamfer solvers need (a line or an arc).
#[derive(Clone, Copy, Debug)]
pub enum CornerEdge {
    /// A straight edge between two points.
    Line { p0: (f64, f64), p1: (f64, f64) },
    /// A circular edge.
    Arc {
        cx: f64,
        cy: f64,
        r: f64,
        start: f64,
        end: f64,
    },
}

impl CornerEdge {
    /// Builds a corner edge from a line or arc curve, or `None` for other kinds.
    pub fn from_curve(c: &Curve) -> Option<CornerEdge> {
        match c {
            Curve::Line(l) => Some(CornerEdge::Line {
                p0: l.p0.to_f64(),
                p1: l.p1.to_f64(),
            }),
            Curve::Arc(a) => {
                let (cx, cy) = a.center.to_f64();
                Some(CornerEdge::Arc {
                    cx,
                    cy,
                    r: a.radius,
                    start: a.start_angle,
                    end: a.end_angle,
                })
            }
            _ => None,
        }
    }

    /// True when this edge is a straight line.
    pub fn is_line(&self) -> bool {
        matches!(self, CornerEdge::Line { .. })
    }
}

/// The geometry of a solved fillet: the arc's centre and radius, the two
/// tangent points on the edges, and the trim angle on each arc edge (if any).
#[derive(Clone, Copy, Debug)]
pub struct FilletSolution {
    /// Centre of the fillet arc.
    pub center: (f64, f64),
    /// Radius of the fillet arc.
    pub radius: f64,
    /// Tangent point on the first edge.
    pub ta: (f64, f64),
    /// Tangent point on the second edge.
    pub tb: (f64, f64),
    /// Trim angle on the first edge, when it is an arc.
    pub a_angle: Option<f64>,
    /// Trim angle on the second edge, when it is an arc.
    pub b_angle: Option<f64>,
}

/// The geometry of a solved chamfer: the two cut points on the edges the bevel
/// segment connects.
#[derive(Clone, Copy, Debug)]
pub struct ChamferSolution {
    /// Cut point on the first edge.
    pub pa: (f64, f64),
    /// Cut point on the second edge.
    pub pb: (f64, f64),
}

/// Solves the fillet arc of the given `radius` between two edges, near `pick`;
/// returns its geometry or `None` when no valid fillet fits.
pub fn solve_fillet(
    a: CornerEdge,
    b: CornerEdge,
    radius: f64,
    pick: (f64, f64),
) -> Option<FilletSolution> {
    if radius <= 0.0 {
        return None;
    }
    match (a, b) {
        (CornerEdge::Line { p0: a0, p1: a1 }, CornerEdge::Line { p0: b0, p1: b1 }) => {
            solve_fillet_ll((a0, a1), (b0, b1), radius, pick)
        }
        (
            CornerEdge::Line { p0, p1 },
            CornerEdge::Arc {
                cx,
                cy,
                r,
                start,
                end,
            },
        ) => solve_fillet_la(
            (p0, p1),
            ArcSnap {
                cx,
                cy,
                r,
                start,
                end,
            },
            radius,
            pick,
            false,
        ),
        (
            CornerEdge::Arc {
                cx,
                cy,
                r,
                start,
                end,
            },
            CornerEdge::Line { p0, p1 },
        ) => solve_fillet_la(
            (p0, p1),
            ArcSnap {
                cx,
                cy,
                r,
                start,
                end,
            },
            radius,
            pick,
            true,
        ),
        (
            CornerEdge::Arc {
                cx: ax,
                cy: ay,
                r: ar,
                start: as_,
                end: ae,
            },
            CornerEdge::Arc {
                cx: bx,
                cy: by,
                r: br,
                start: bs,
                end: be,
            },
        ) => solve_fillet_aa(
            ArcSnap {
                cx: ax,
                cy: ay,
                r: ar,
                start: as_,
                end: ae,
            },
            ArcSnap {
                cx: bx,
                cy: by,
                r: br,
                start: bs,
                end: be,
            },
            radius,
            pick,
        ),
    }
}

/// Solves the chamfer bevel between two edges cut back by `dist_a`/`dist_b`;
/// returns its two cut points, or `None` for a non-positive distance.
pub fn solve_chamfer(
    a: CornerEdge,
    b: CornerEdge,
    dist_a: f64,
    dist_b: f64,
) -> Option<ChamferSolution> {
    // pa/pb are placed at `corner + dir * dist`, walking *away* from the
    // corner along each edge; a non-positive distance either does nothing
    // (0.0) or walks backward through the corner, and trim_entity_for_corner
    // then extends the edge the wrong way instead of cutting it. solve_fillet
    // already rejects a non-positive radius the same way.
    if !(dist_a > 0.0 && dist_b > 0.0) {
        return None;
    }
    let (la, lb) = match (a, b) {
        (CornerEdge::Line { p0: a0, p1: a1 }, CornerEdge::Line { p0: b0, p1: b1 }) => {
            ((a0, a1), (b0, b1))
        }
        _ => return None,
    };
    let (cx, cy) = infinite_line_intersection(la, lb)?;
    let dir_a = dir_from_corner(cx, cy, la);
    let dir_b = dir_from_corner(cx, cy, lb);
    Some(ChamferSolution {
        pa: (cx + dir_a.0 * dist_a, cy + dir_a.1 * dist_a),
        pb: (cx + dir_b.0 * dist_b, cy + dir_b.1 * dist_b),
    })
}

fn solve_fillet_ll(
    la: LineData,
    lb: LineData,
    radius: f64,
    pick: (f64, f64),
) -> Option<FilletSolution> {
    let (cx, cy) = infinite_line_intersection(la, lb)?;
    let dir_a = dir_from_corner_toward(cx, cy, la, pick);
    let dir_b = dir_from_corner_toward(cx, cy, lb, pick);
    let cos_t = (dir_a.0 * dir_b.0 + dir_a.1 * dir_b.1).clamp(-1.0, 1.0);
    let theta = cos_t.acos();
    if theta < 1e-6 || (std::f64::consts::PI - theta) < 1e-6 {
        return None;
    }
    let tan_dist = radius / (theta / 2.0).tan();
    let center_dist = radius / (theta / 2.0).sin();
    let ta = (cx + dir_a.0 * tan_dist, cy + dir_a.1 * tan_dist);
    let tb = (cx + dir_b.0 * tan_dist, cy + dir_b.1 * tan_dist);
    let (mut bx, mut by) = (dir_a.0 + dir_b.0, dir_a.1 + dir_b.1);
    let bl = (bx * bx + by * by).sqrt();
    if bl < 1e-12 {
        return None;
    }
    bx /= bl;
    by /= bl;
    let center = (cx + bx * center_dist, cy + by * center_dist);
    Some(FilletSolution {
        center,
        radius,
        ta,
        tb,
        a_angle: None,
        b_angle: None,
    })
}

fn solve_fillet_la(
    la: LineData,
    arc: ArcSnap,
    radius: f64,
    pick: (f64, f64),
    arc_is_a: bool,
) -> Option<FilletSolution> {
    let (px, py) = pick;
    let mut best_dist = f64::MAX;
    let mut best: Option<LaCandidate> = None;
    for &side in &[radius, -radius] {
        for &cr in &[arc.r + radius, arc.r - radius] {
            if cr < 1e-9 {
                continue;
            }
            for fc in line_offset_circle_intersects(la.0, la.1, side, arc.cx, arc.cy, cr) {
                let ta_angle = (fc.1 - arc.cy).atan2(fc.0 - arc.cx);
                if !angle_on_arc(ta_angle, arc.start, arc.end) {
                    continue;
                }
                let tl = foot_on_line(la.0, la.1, fc);
                let d = sq_dist(fc, (px, py));
                if d < best_dist {
                    best_dist = d;
                    best = Some((fc, tl, ta_angle));
                }
            }
        }
    }
    let (fc, tl, ta_angle) = best?;
    let arc_pt = (
        arc.cx + arc.r * ta_angle.cos(),
        arc.cy + arc.r * ta_angle.sin(),
    );
    if arc_is_a {
        Some(FilletSolution {
            center: fc,
            radius,
            ta: arc_pt,
            tb: tl,
            a_angle: Some(ta_angle),
            b_angle: None,
        })
    } else {
        Some(FilletSolution {
            center: fc,
            radius,
            ta: tl,
            tb: arc_pt,
            a_angle: None,
            b_angle: Some(ta_angle),
        })
    }
}

fn solve_fillet_aa(
    a: ArcSnap,
    b: ArcSnap,
    radius: f64,
    pick: (f64, f64),
) -> Option<FilletSolution> {
    let (px, py) = pick;
    let mut best_dist = f64::MAX;
    let mut best: Option<AaCandidate> = None;
    for &ra in &[a.r + radius, a.r - radius] {
        if ra < 1e-9 {
            continue;
        }
        for &rb in &[b.r + radius, b.r - radius] {
            if rb < 1e-9 {
                continue;
            }
            for fc in circle_circle_intersects(a.cx, a.cy, ra, b.cx, b.cy, rb) {
                let ta = (fc.1 - a.cy).atan2(fc.0 - a.cx);
                let tb = (fc.1 - b.cy).atan2(fc.0 - b.cx);
                if !angle_on_arc(ta, a.start, a.end) {
                    continue;
                }
                if !angle_on_arc(tb, b.start, b.end) {
                    continue;
                }
                let d = sq_dist(fc, (px, py));
                if d < best_dist {
                    best_dist = d;
                    best = Some((fc, ta, tb));
                }
            }
        }
    }
    let (fc, ta_angle, tb_angle) = best?;
    let ta = (a.cx + a.r * ta_angle.cos(), a.cy + a.r * ta_angle.sin());
    let tb = (b.cx + b.r * tb_angle.cos(), b.cy + b.r * tb_angle.sin());
    Some(FilletSolution {
        center: fc,
        radius,
        ta,
        tb,
        a_angle: Some(ta_angle),
        b_angle: Some(tb_angle),
    })
}

fn trim_entity_for_corner(
    doc: &mut Document,
    id: EntityId,
    edge: CornerEdge,
    pt: (f64, f64),
    angle: Option<f64>,
    vtx: (f64, f64),
) {
    match edge {
        CornerEdge::Line { .. } => {
            if let Some(la) = line_endpoints(doc, id) {
                set_line_endpoint(doc, id, endpoint_nearer_is_p1(la, vtx.0, vtx.1), pt.0, pt.1);
            }
        }
        CornerEdge::Arc {
            cx,
            cy,
            r,
            start,
            end,
        } => {
            if let Some(ang) = angle {
                let snap = ArcSnap {
                    cx,
                    cy,
                    r,
                    start,
                    end,
                };
                set_arc_endpoint(doc, id, arc_endpoint_nearer(&snap, vtx.0, vtx.1), ang);
            }
        }
    }
}

fn corner_vertex(a: CornerEdge, b: CornerEdge) -> (f64, f64) {
    if let (CornerEdge::Line { p0: a0, p1: a1 }, CornerEdge::Line { p0: b0, p1: b1 }) = (a, b)
        && let Some(v) = infinite_line_intersection((a0, a1), (b0, b1))
    {
        return v;
    }
    let (ea, eb) = (edge_endpoints(a), edge_endpoints(b));
    let mut best = (f64::MAX, (0.0, 0.0));
    for &pa in &ea {
        for &pb in &eb {
            let d = sq_dist(pa, pb);
            if d < best.0 {
                best = (d, ((pa.0 + pb.0) * 0.5, (pa.1 + pb.1) * 0.5));
            }
        }
    }
    best.1
}

fn edge_endpoints(e: CornerEdge) -> [(f64, f64); 2] {
    match e {
        CornerEdge::Line { p0, p1 } => [p0, p1],
        CornerEdge::Arc {
            cx,
            cy,
            r,
            start,
            end,
        } => [
            (cx + r * start.cos(), cy + r * start.sin()),
            (cx + r * end.cos(), cy + r * end.sin()),
        ],
    }
}

/// Fillets the corner between segments `seg_i` and `seg_i + 1` of a polycurve
/// in place, splicing in the arc; returns whether it succeeded.
pub fn fillet_poly_corner(doc: &mut Document, id: EntityId, seg_i: usize, radius: f64) -> bool {
    apply_poly_corner(doc, id, seg_i, PolyCorner::Fillet(radius))
}

/// Chamfers the corner between segments `seg_i` and `seg_i + 1` of a polycurve
/// in place, splicing in the bevel; returns whether it succeeded.
pub fn chamfer_poly_corner(doc: &mut Document, id: EntityId, seg_i: usize, dist: f64) -> bool {
    apply_poly_corner(doc, id, seg_i, PolyCorner::Chamfer(dist))
}

enum PolyCorner {
    Fillet(f64),
    Chamfer(f64),
}

fn apply_poly_corner(doc: &mut Document, id: EntityId, seg_i: usize, op: PolyCorner) -> bool {
    let mut segs = match doc.get(id).and_then(|e| e.as_curve()) {
        Some(Curve::Poly(pc)) => pc.segments.clone(),
        _ => return false,
    };
    let n = segs.len();
    if n < 2 || seg_i >= n {
        return false;
    }
    let j = (seg_i + 1) % n;
    let wrap = j < seg_i;
    let ea = match CornerEdge::from_curve(&segs[seg_i]) {
        Some(e) => e,
        None => return false,
    };
    let eb = match CornerEdge::from_curve(&segs[j]) {
        Some(e) => e,
        None => return false,
    };
    let vertex = shared_vertex(&segs[seg_i], &segs[j]);

    let inserted: Curve = match op {
        PolyCorner::Fillet(r) => {
            let sol = match solve_fillet(ea, eb, r, vertex) {
                Some(s) => s,
                None => return false,
            };
            trim_seg_endpoint(&mut segs[seg_i], vertex, sol.ta, sol.a_angle);
            trim_seg_endpoint(&mut segs[j], vertex, sol.tb, sol.b_angle);
            let mut arc = Curve::Arc(arc_between(sol.center, sol.ta, sol.tb, r));
            let (t0, _) = arc.domain();
            let start = arc.evaluate_f64(t0);
            if sq_dist(start, sol.ta) > sq_dist(start, sol.tb) {
                arc = reverse_curve(&arc);
            }
            arc
        }
        PolyCorner::Chamfer(d) => {
            let sol = match solve_chamfer(ea, eb, d, d) {
                Some(s) => s,
                None => return false,
            };
            trim_seg_endpoint(&mut segs[seg_i], vertex, sol.pa, None);
            trim_seg_endpoint(&mut segs[j], vertex, sol.pb, None);
            Curve::Line(LineSeg::from_endpoints(
                Point2d::from_f64(sol.pa.0, sol.pa.1),
                Point2d::from_f64(sol.pb.0, sol.pb.1),
            ))
        }
    };

    if wrap {
        segs.push(inserted);
    } else {
        segs.insert(j, inserted);
    }
    segs.retain(|s| s.arc_length() > 1e-6);
    if let Some(e) = doc.get_mut(id) {
        e.kind = EntityKind::Curve(Curve::Poly(Box::new(PolyCurve::new(segs))));
        true
    } else {
        false
    }
}

fn shared_vertex(a: &Curve, b: &Curve) -> (f64, f64) {
    let (_, ta1) = a.domain();
    let ae = a.evaluate_f64(ta1);
    let (tb0, _) = b.domain();
    let bs = b.evaluate_f64(tb0);
    ((ae.0 + bs.0) * 0.5, (ae.1 + bs.1) * 0.5)
}

fn trim_seg_endpoint(seg: &mut Curve, vertex: (f64, f64), pt: (f64, f64), angle: Option<f64>) {
    match seg {
        Curve::Line(l) => {
            let (p0, p1) = (l.p0.to_f64(), l.p1.to_f64());
            if sq_dist(p0, vertex) <= sq_dist(p1, vertex) {
                l.p0 = Point2d::from_f64(pt.0, pt.1);
            } else {
                l.p1 = Point2d::from_f64(pt.0, pt.1);
            }
        }
        Curve::Arc(a) => {
            if let Some(ang) = angle {
                let set_end = sq_dist(a.end_point(), vertex) < sq_dist(a.start_point(), vertex);
                set_arc_angle(a, set_end, ang);
            }
        }
        _ => {}
    }
}

fn set_arc_angle(arc: &mut CircularArc, set_end: bool, new_angle: f64) {
    let tau = std::f64::consts::TAU;
    if set_end {
        let mut a = new_angle;
        while a <= arc.start_angle {
            a += tau;
        }
        while a > arc.start_angle + tau {
            a -= tau;
        }
        arc.end_angle = a;
    } else {
        let mut a = new_angle;
        while a >= arc.end_angle {
            a -= tau;
        }
        while a < arc.end_angle - tau {
            a += tau;
        }
        arc.start_angle = a;
    }
}

fn arc_point(arc: &CircularArc, angle: f64) -> Point2d {
    Point2d::from_f64(
        arc.center.x + arc.radius * angle.cos(),
        arc.center.y + arc.radius * angle.sin(),
    )
}

/// Nudges every control point of `ids` that falls inside `window` by
/// `(dx, dy)`. Returns whether anything was actually inside the window (and
/// so the document changed) — a caller doing its own undo bookkeeping needs
/// this to avoid recording a snapshot for a no-op drag.
pub fn stretch(
    doc: &mut Document,
    ids: &[EntityId],
    window: (f64, f64, f64, f64),
    dx: f64,
    dy: f64,
) -> bool {
    if !(dx.is_finite() && dy.is_finite()) || (dx == 0.0 && dy == 0.0) {
        return false;
    }
    let (xmin, ymin, xmax, ymax) = window;
    let inside = |x: f64, y: f64| x >= xmin && x <= xmax && y >= ymin && y <= ymax;
    let changed = std::cell::Cell::new(false);
    let nudge = |p: &Point2d| -> Point2d {
        let (x, y) = p.to_f64();
        if inside(x, y) {
            changed.set(true);
            Point2d::from_f64(x + dx, y + dy)
        } else {
            *p
        }
    };
    for &id in ids {
        if let Some(e) = doc.get_mut(id) {
            match &mut e.kind {
                EntityKind::Curve(Curve::Line(l)) => {
                    l.p0 = nudge(&l.p0);
                    l.p1 = nudge(&l.p1);
                }
                EntityKind::Curve(Curve::Bezier(bz)) => {
                    bz.p0 = nudge(&bz.p0);
                    bz.p1 = nudge(&bz.p1);
                    bz.p2 = nudge(&bz.p2);
                    bz.p3 = nudge(&bz.p3);
                }
                EntityKind::Curve(Curve::Rational(rb)) => {
                    for p in &mut rb.points {
                        *p = nudge(p);
                    }
                }
                EntityKind::Curve(Curve::Nurbs(nc)) => {
                    for p in nc.control_mut() {
                        *p = nudge(p);
                    }
                }
                EntityKind::Curve(Curve::Poly(pc)) => {
                    let edit_pts = crate::grips::poly_edit_points(pc);
                    let mut segs = pc.segments.clone();
                    for ep in &edit_pts {
                        if inside(ep.pos.x, ep.pos.y) {
                            let moved = nudge(&ep.pos);
                            for &(s, ci) in &ep.writes {
                                if let Some(seg) = segs.get_mut(s) {
                                    crate::grips::set_poly_ctrl(seg, ci, moved);
                                }
                            }
                        }
                    }
                    **pc = PolyCurve::new(segs);
                }
                EntityKind::Curve(Curve::Arc(arc)) => {
                    // Endpoints must be read from the arc's original (pre-nudge)
                    // center: computing them after moving the center would already
                    // include the center's translation, and nudging them again on
                    // top of that double-applies dx/dy whenever the whole arc (center
                    // and both endpoints) sits inside the window.
                    let start_pt = arc_point(arc, arc.start_angle);
                    let end_pt = arc_point(arc, arc.end_angle);
                    if inside(arc.center.x, arc.center.y) {
                        arc.center = nudge(&arc.center);
                    }
                    if inside(start_pt.x, start_pt.y) {
                        let moved = nudge(&start_pt);
                        let a = (moved.y - arc.center.y).atan2(moved.x - arc.center.x);
                        *arc = crate::grips::with_angles(arc, a, arc.end_angle);
                    }
                    if inside(end_pt.x, end_pt.y) {
                        let moved = nudge(&end_pt);
                        let a = (moved.y - arc.center.y).atan2(moved.x - arc.center.x);
                        *arc = crate::grips::with_angles(arc, arc.start_angle, a);
                    }
                }
                EntityKind::Curve(Curve::Ellipse(el)) => {
                    if inside(el.center.x, el.center.y) {
                        el.center = nudge(&el.center);
                    }
                }
                EntityKind::Point(p) => {
                    *p = nudge(p);
                }
                _ => {}
            }
        }
    }
    changed.get()
}

type LineData = ((f64, f64), (f64, f64));

type LaCandidate = ((f64, f64), (f64, f64), f64);
type AaCandidate = ((f64, f64), f64, f64);

#[derive(Clone, Copy)]
struct ArcSnap {
    cx: f64,
    cy: f64,
    r: f64,
    start: f64,
    end: f64,
}

fn arc_endpoint_nearer(a: &ArcSnap, px: f64, py: f64) -> bool {
    let sp = (a.cx + a.r * a.start.cos(), a.cy + a.r * a.start.sin());
    let ep = (a.cx + a.r * a.end.cos(), a.cy + a.r * a.end.sin());
    sq_dist(ep, (px, py)) < sq_dist(sp, (px, py))
}

fn set_arc_endpoint(doc: &mut Document, id: EntityId, set_end: bool, new_angle: f64) -> bool {
    let tau = std::f64::consts::TAU;
    if let Some(e) = doc.get_mut(id)
        && let EntityKind::Curve(Curve::Arc(arc)) = &mut e.kind
    {
        if set_end {
            let mut a = new_angle;
            while a <= arc.start_angle {
                a += tau;
            }
            while a > arc.start_angle + tau {
                a -= tau;
            }
            arc.end_angle = a;
        } else {
            let mut a = new_angle;
            while a >= arc.end_angle {
                a -= tau;
            }
            while a < arc.end_angle - tau {
                a += tau;
            }
            arc.start_angle = a;
        }
        return true;
    }
    false
}

fn angle_on_arc(angle: f64, start: f64, end: f64) -> bool {
    let tau = std::f64::consts::TAU;
    let mut a = angle;
    while a < start - 1e-9 {
        a += tau;
    }
    while a > start + tau + 1e-9 {
        a -= tau;
    }
    a <= end + 1e-9
}

fn line_offset_circle_intersects(
    p0: (f64, f64),
    p1: (f64, f64),
    side: f64,
    cx: f64,
    cy: f64,
    cr: f64,
) -> Vec<(f64, f64)> {
    let (dx, dy) = (p1.0 - p0.0, p1.1 - p0.1);
    let len = (dx * dx + dy * dy).sqrt();
    if len < 1e-12 {
        return vec![];
    }
    let (ux, uy) = (dx / len, dy / len);
    let (nx, ny) = (-uy, ux);
    let (ox, oy) = (p0.0 + side * nx, p0.1 + side * ny);
    let (fx, fy) = (ox - cx, oy - cy);
    let b = 2.0 * (fx * ux + fy * uy);
    let c = fx * fx + fy * fy - cr * cr;
    let disc = b * b - 4.0 * c;
    if disc < 0.0 {
        return vec![];
    }
    let sq = disc.sqrt();
    vec![
        (ox + ((-b - sq) / 2.0) * ux, oy + ((-b - sq) / 2.0) * uy),
        (ox + ((-b + sq) / 2.0) * ux, oy + ((-b + sq) / 2.0) * uy),
    ]
}

fn circle_circle_intersects(
    cx1: f64,
    cy1: f64,
    r1: f64,
    cx2: f64,
    cy2: f64,
    r2: f64,
) -> Vec<(f64, f64)> {
    let dx = cx2 - cx1;
    let dy = cy2 - cy1;
    let d2 = dx * dx + dy * dy;
    let d = d2.sqrt();
    if d < 1e-12 || d > r1 + r2 + 1e-9 || d < (r1 - r2).abs() - 1e-9 {
        return vec![];
    }
    let a = (r1 * r1 - r2 * r2 + d2) / (2.0 * d);
    let h2 = r1 * r1 - a * a;
    if h2 < 0.0 {
        return vec![];
    }
    let h = h2.sqrt();
    let mx = cx1 + a * dx / d;
    let my = cy1 + a * dy / d;
    if h < 1e-9 {
        return vec![(mx, my)];
    }
    let px = h * dy / d;
    let py = h * dx / d;
    vec![(mx + px, my - py), (mx - px, my + py)]
}

fn foot_on_line(p0: (f64, f64), p1: (f64, f64), pt: (f64, f64)) -> (f64, f64) {
    let (dx, dy) = (p1.0 - p0.0, p1.1 - p0.1);
    let len2 = dx * dx + dy * dy;
    if len2 < 1e-24 {
        return p0;
    }
    let t = ((pt.0 - p0.0) * dx + (pt.1 - p0.1) * dy) / len2;
    (p0.0 + t * dx, p0.1 + t * dy)
}

fn sq_dist(a: (f64, f64), b: (f64, f64)) -> f64 {
    (a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)
}

fn line_endpoints(doc: &Document, id: EntityId) -> Option<LineData> {
    match doc.get(id).and_then(|e| e.as_curve()) {
        Some(Curve::Line(l)) => Some((l.p0.to_f64(), l.p1.to_f64())),
        _ => None,
    }
}

fn set_line_endpoint(doc: &mut Document, id: EntityId, which_p1: bool, x: f64, y: f64) -> bool {
    if let Some(e) = doc.get_mut(id)
        && let EntityKind::Curve(Curve::Line(l)) = &mut e.kind
    {
        if which_p1 {
            l.p1 = Point2d::from_f64(x, y);
        } else {
            l.p0 = Point2d::from_f64(x, y);
        }
        return true;
    }
    false
}

fn endpoint_nearer_is_p1(l: LineData, cx: f64, cy: f64) -> bool {
    let d0 = (l.0.0 - cx).powi(2) + (l.0.1 - cy).powi(2);
    let d1 = (l.1.0 - cx).powi(2) + (l.1.1 - cy).powi(2);
    d1 < d0
}

/// The direction to run along `l` from the corner, choosing the side the user
/// picked when the line genuinely has two.
///
/// [`dir_from_corner`] takes the end farther from the corner, which is right
/// for two segments meeting in an L — only one side exists. But two lines that
/// *cross* have equal halves either side, so "farther" is arbitrary: filleting
/// an X rounded a fixed quadrant no matter where the user clicked, and trimmed
/// away the two legs they were pointing at. `fillet_freeform` already treats
/// the pick as marking the corner to round; this makes the exact line/line
/// path agree.
fn dir_from_corner_toward(cx: f64, cy: f64, l: LineData, pick: (f64, f64)) -> (f64, f64) {
    let want = (pick.0 - cx, pick.1 - cy);
    let mut best: Option<(f64, (f64, f64))> = None;
    for end in [l.0, l.1] {
        let (dx, dy) = (end.0 - cx, end.1 - cy);
        let n = (dx * dx + dy * dy).sqrt();
        // An end sitting on the corner offers no direction — which is how an
        // L-corner ends up with a single candidate and keeps its old answer.
        if n < 1e-12 {
            continue;
        }
        let d = (dx / n, dy / n);
        let score = d.0 * want.0 + d.1 * want.1;
        if best.is_none_or(|(s, _)| score > s) {
            best = Some((score, d));
        }
    }
    best.map_or_else(|| dir_from_corner(cx, cy, l), |(_, d)| d)
}

fn dir_from_corner(cx: f64, cy: f64, l: LineData) -> (f64, f64) {
    let far = if endpoint_nearer_is_p1(l, cx, cy) {
        l.0
    } else {
        l.1
    };
    let (dx, dy) = (far.0 - cx, far.1 - cy);
    let n = (dx * dx + dy * dy).sqrt().max(1e-12);
    (dx / n, dy / n)
}

fn infinite_line_intersection(la: LineData, lb: LineData) -> Option<(f64, f64)> {
    let (a1, b1, c1) = implicit(la);
    let (a2, b2, c2) = implicit(lb);
    let det = a1 * b2 - a2 * b1;
    if det.abs() < 1e-12 {
        return None;
    }
    Some(((b1 * c2 - b2 * c1) / det, (a2 * c1 - a1 * c2) / det))
}

fn implicit(l: LineData) -> (f64, f64, f64) {
    let ((x0, y0), (x1, y1)) = l;
    (y0 - y1, x1 - x0, x0 * y1 - x1 * y0)
}

fn arc_between(center: (f64, f64), ta: (f64, f64), tb: (f64, f64), radius: f64) -> CircularArc {
    let a0 = (ta.1 - center.1).atan2(ta.0 - center.0);
    let mut a1 = (tb.1 - center.1).atan2(tb.0 - center.0);
    let mut sweep = a1 - a0;
    while sweep <= -std::f64::consts::PI {
        sweep += std::f64::consts::TAU;
        a1 += std::f64::consts::TAU;
    }
    while sweep > std::f64::consts::PI {
        sweep -= std::f64::consts::TAU;
        a1 -= std::f64::consts::TAU;
    }
    let (start, end) = if a1 >= a0 { (a0, a1) } else { (a1, a0) };
    CircularArc::new(Point2d::from_f64(center.0, center.1), radius, start, end)
}

fn apply_to(doc: &mut Document, ids: &[EntityId], t: &Transform2d) {
    // A non-finite transform is floored inside `Entity::transform`, so
    // this loop simply no-ops on one — no guard needed here.
    for &id in ids {
        if let Some(e) = doc.get_mut(id) {
            e.transform(t);
        }
    }
}

fn duplicate_with(doc: &mut Document, ids: &[EntityId], t: &Transform2d) -> Vec<EntityId> {
    // A non-finite transform makes `Entity::transform` a no-op, so the
    // copies would land exactly on their sources — decline outright
    // rather than stack invisible duplicates.
    if !t.is_finite() {
        return Vec::new();
    }
    let mut new_ids = Vec::new();
    for &id in ids {
        if let Some(e) = doc.get(id) {
            new_ids.push(doc.add_entity(e.transformed(t)));
        }
    }
    new_ids
}

fn normalized_pick_param(curve: &Curve, px: f64, py: f64) -> f64 {
    let pr = oxidraft_geometry::project_point_onto_curve(curve, px, py);
    curve.normalized_param(pr.t).unwrap_or(0.0)
}

fn extract_piece(curve: &Curve, a: f64, b: f64) -> Curve {
    match curve {
        Curve::Poly(pc) => requantize(extract_poly_span(pc, a, b)),
        Curve::Nurbs(nc) => Curve::Nurbs(oxidraft_geometry::refit_nurbs_subcurve(nc, a, b)),
        _ => {
            let left = if b < 1.0 - 1e-9 {
                split_curve(curve, b).0
            } else {
                curve.clone()
            };
            let piece = if a < 1e-9 {
                left
            } else {
                let a_scaled = (a / b).min(1.0);
                split_curve(&left, a_scaled).1
            };
            requantize(piece)
        }
    }
}

fn extract_poly_span(pc: &oxidraft_geometry::PolyCurve, a: f64, b: f64) -> Curve {
    use oxidraft_geometry::PolyCurve;
    let n = pc.segments.len();
    if n == 0 {
        return Curve::Poly(Box::new(PolyCurve::new(vec![])));
    }
    let locate = |t: f64| -> (usize, f64) {
        let tn = (t * n as f64).clamp(0.0, n as f64);
        let seg = (tn.floor() as usize).min(n - 1);
        (seg, (tn - seg as f64).clamp(0.0, 1.0))
    };
    let (sa, la) = locate(a);
    let (sb, lb) = locate(b);

    let mut out: Vec<Curve> = Vec::new();
    if sa == sb {
        let after_a = split_curve(&pc.segments[sa], la).1;
        let denom = (1.0 - la).max(1e-12);
        let local = ((lb - la) / denom).clamp(0.0, 1.0);
        out.push(split_curve(&after_a, local).0);
    } else {
        out.push(split_curve(&pc.segments[sa], la).1);
        for s in (sa + 1)..sb {
            out.push(pc.segments[s].clone());
        }
        if lb > 1e-9 {
            out.push(split_curve(&pc.segments[sb], lb).0);
        }
    }
    Curve::Poly(Box::new(PolyCurve::new(out)))
}

fn requantize(c: Curve) -> Curve {
    let q = |p: &Point2d| {
        let (x, y) = p.to_f64();
        Point2d::from_f64(x, y)
    };
    match c {
        Curve::Line(l) => Curve::Line(LineSeg::from_endpoints(q(&l.p0), q(&l.p1))),
        Curve::Bezier(b) => Curve::Bezier(oxidraft_geometry::CubicBezier::new(
            q(&b.p0),
            q(&b.p1),
            q(&b.p2),
            q(&b.p3),
        )),
        Curve::Poly(pc) => Curve::Poly(Box::new(oxidraft_geometry::PolyCurve::new(
            pc.segments.into_iter().map(requantize).collect(),
        ))),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::draw;
    use oxidraft_geometry::CircularArc;

    fn pt(x: i64, y: i64) -> Point2d {
        Point2d::from_i64(x, y)
    }
    fn r(n: i64) -> f64 {
        n as f64
    }

    fn weld(doc: &mut Document, a: EntityId, ea: u8, b: EntityId, eb: u8) {
        doc.add_constraint(SketchConstraint::coincident(a, ea, b, eb));
    }

    fn line_seg(doc: &Document, id: EntityId) -> LineSeg {
        match doc.get(id).unwrap().as_curve().unwrap() {
            Curve::Line(l) => l.clone(),
            other => panic!("expected line, got {other:?}"),
        }
    }

    #[test]
    fn trim_remaps_welds_and_axis_constraints_to_the_surviving_piece() {
        let mut doc = Document::new();
        let a = draw::line(&mut doc, pt(0, 0), pt(10, 0));
        let b = draw::line(&mut doc, pt(10, 0), pt(10, 5));
        let cutter = draw::line(&mut doc, pt(4, -1), pt(4, 1));
        weld(&mut doc, a, 1, b, 0);
        doc.add_constraint(SketchConstraint::single(ConstraintKind::Horizontal, a));

        // Cut away the left span; the survivor keeps the welded (10,0) end.
        let survivors = trim(&mut doc, a, &[cutter], 2.0, 0.0);
        assert_eq!(survivors.len(), 1);
        let s = survivors[0];
        assert!((line_seg(&doc, s).p0.x - 4.0).abs() < 1e-9);
        let welds: Vec<_> = doc
            .constraints
            .iter()
            .filter(|c| c.kind == ConstraintKind::Coincident)
            .collect();
        assert_eq!(welds.len(), 1, "weld carried over to the piece");
        assert!(welds[0].references(s) && welds[0].references(b));
        assert!(
            doc.constraints
                .iter()
                .any(|c| c.kind == ConstraintKind::Horizontal && c.a == s),
            "horizontal carried over"
        );
    }

    #[test]
    fn trim_drops_the_weld_when_the_welded_span_is_cut_away() {
        let mut doc = Document::new();
        let a = draw::line(&mut doc, pt(0, 0), pt(10, 0));
        let b = draw::line(&mut doc, pt(10, 0), pt(10, 5));
        let cutter = draw::line(&mut doc, pt(4, -1), pt(4, 1));
        weld(&mut doc, a, 1, b, 0);

        // Cut away the right span, which contained the welded corner.
        let survivors = trim(&mut doc, a, &[cutter], 7.0, 0.0);
        assert_eq!(survivors.len(), 1);
        assert!((line_seg(&doc, survivors[0]).p1.x - 4.0).abs() < 1e-9);
        assert!(
            !doc.constraints
                .iter()
                .any(|c| c.kind == ConstraintKind::Coincident),
            "weld to the removed corner dropped"
        );
    }

    #[test]
    fn extend_drops_the_weld_at_the_moved_end_only() {
        let mut doc = Document::new();
        let a = draw::line(&mut doc, pt(0, 0), pt(5, 0));
        let b = draw::line(&mut doc, pt(5, 0), pt(5, 3));
        let c = draw::line(&mut doc, pt(0, 0), pt(0, -3));
        let boundary = draw::line(&mut doc, pt(8, -1), pt(8, 1));
        weld(&mut doc, a, 1, b, 0);
        weld(&mut doc, a, 0, c, 0);

        assert!(extend(&mut doc, a, &[boundary], 4.5, 0.0));
        assert!((line_seg(&doc, a).p1.x - 8.0).abs() < 1e-9, "extended to 8");
        let welds: Vec<_> = doc
            .constraints
            .iter()
            .filter(|c| c.kind == ConstraintKind::Coincident)
            .collect();
        assert_eq!(welds.len(), 1, "only the moved end's weld dropped");
        assert!(welds[0].references(c), "the untouched-end weld survives");
    }

    #[test]
    fn filleting_a_crossing_rounds_the_corner_that_was_picked() {
        // Two lines that cross have four corners, and the pick says which.
        // `dir_from_corner` took the end farther from the crossing, which for
        // equal halves is arbitrary: every pick rounded the same quadrant and
        // trimmed away the two legs the user was pointing at — the operation
        // reported success and destroyed the geometry it was aimed at.
        for (px, py) in [(3.0, 3.0), (-3.0, 3.0), (-3.0, -3.0), (3.0, -3.0)] {
            let mut doc = Document::new();
            let a = draw::line(&mut doc, pt(-10, 0), pt(10, 0));
            let b = draw::line(&mut doc, pt(0, -10), pt(0, 10));
            let id = fillet(&mut doc, a, b, 2.0, px, py).expect("fillet applies");
            let Some(Curve::Arc(arc)) = doc.get(id).and_then(|e| e.as_curve()) else {
                panic!("expected a fillet arc");
            };
            let (cx, cy) = arc.center.to_f64();
            assert!(
                cx.signum() == px.signum() && cy.signum() == py.signum(),
                "picking ({px},{py}) should round that corner, but the arc \
                 centre came out at ({cx:.3},{cy:.3})"
            );
        }
    }

    #[test]
    fn filleting_an_l_corner_is_unchanged_by_the_pick() {
        // Two segments meeting at an end have only one corner, so the pick has
        // nothing to choose between and must not be able to move the result.
        for (px, py) in [(1.0, 1.0), (-5.0, -5.0), (20.0, 20.0)] {
            let mut doc = Document::new();
            let a = draw::line(&mut doc, pt(10, 0), pt(0, 0));
            let b = draw::line(&mut doc, pt(0, 0), pt(0, 10));
            let id = fillet(&mut doc, a, b, 2.0, px, py).expect("fillet applies");
            let Some(Curve::Arc(arc)) = doc.get(id).and_then(|e| e.as_curve()) else {
                panic!("expected a fillet arc");
            };
            let (cx, cy) = arc.center.to_f64();
            assert!(
                (cx - 2.0).abs() < 1e-6 && (cy - 2.0).abs() < 1e-6,
                "the only corner there is sits at (2,2); pick ({px},{py}) gave \
                 ({cx:.3},{cy:.3})"
            );
        }
    }

    #[test]
    fn fillet_prunes_the_now_broken_corner_weld() {
        let mut doc = Document::new();
        let a = draw::line(&mut doc, pt(10, 0), pt(0, 0));
        let b = draw::line(&mut doc, pt(0, 0), pt(0, 10));
        weld(&mut doc, a, 1, b, 0);
        let arc = fillet(&mut doc, a, b, 2.0, 1.0, 1.0).expect("fillet applies");
        assert!(
            !doc.constraints
                .iter()
                .any(|c| c.kind == ConstraintKind::Coincident
                    && c.references(a)
                    && c.references(b)),
            "direct leg weld dropped after the corner opened"
        );
        let _ = arc;
    }

    #[test]
    fn trim_with_bezier_cutters_is_fast_and_correct() {
        let mut doc = Document::new();
        let target = draw::line(&mut doc, pt(0, 0), pt(10, 0));
        let c1 = draw::bezier(&mut doc, pt(2, -3), pt(2, -1), pt(3, 1), pt(3, 3));
        let c2 = draw::bezier(&mut doc, pt(7, -3), pt(7, -1), pt(8, 1), pt(8, 3));
        let start = std::time::Instant::now();
        let survivors = trim(&mut doc, target, &[c1, c2], 5.0, 0.0);
        assert!(
            start.elapsed().as_millis() < 500,
            "trim took {:?} — exact kernel is back in the interactive path?",
            start.elapsed()
        );
        assert_eq!(survivors.len(), 2);
    }

    #[test]
    fn trim_against_polyline_zigzag_cuts_at_first_crossing() {
        use oxidraft_geometry::Point2d;
        let mut doc = Document::new();
        let target = draw::line(&mut doc, pt(0, 0), pt(10, 0));
        let mut segs = Vec::new();
        for i in 0..40 {
            let x0 = 0.25 * i as f64;
            let x1 = 0.25 * (i + 1) as f64;
            let y0 = if i % 2 == 0 { -2.0 } else { 2.0 };
            segs.push(Curve::Line(oxidraft_geometry::LineSeg::from_endpoints(
                Point2d::from_f64(x0, y0),
                Point2d::from_f64(x1, -y0),
            )));
        }
        let zig = draw::polycurve(&mut doc, segs);
        let survivors = trim(&mut doc, target, &[zig], 0.05, 0.0);
        assert_eq!(survivors.len(), 1);
        if let Some(Curve::Line(l)) = doc.get(survivors[0]).and_then(|e| e.as_curve()) {
            let x0 = l.p0.x.min(l.p1.x);
            let x1 = l.p0.x.max(l.p1.x);
            assert!(
                (x0 - 0.125).abs() < 1e-6,
                "survivor must start at the FIRST zigzag crossing, got {x0}"
            );
            assert!((x1 - 10.0).abs() < 1e-6);
        } else {
            panic!("survivor is not a line");
        }
    }

    #[test]
    fn trim_cuts_only_adjacent_boundaries() {
        let mut doc = Document::new();
        let target = draw::line(&mut doc, pt(0, 0), pt(10, 0));
        let v: Vec<_> = [2, 5, 8]
            .iter()
            .map(|&x| draw::line(&mut doc, pt(x, -2), pt(x, 2)))
            .collect();
        let survivors = trim(&mut doc, target, &v, 3.5, 0.0);
        assert_eq!(
            survivors.len(),
            2,
            "exactly two contiguous sides, not fragments"
        );
        let mut spans: Vec<(f64, f64)> = survivors
            .iter()
            .map(|&id| match doc.get(id).and_then(|e| e.as_curve()) {
                Some(Curve::Line(l)) => {
                    let (a, b) = (l.p0.x, l.p1.x);
                    (a.min(b), a.max(b))
                }
                _ => panic!("survivor is not a line"),
            })
            .collect();
        spans.sort_by(|a, b| a.0.total_cmp(&b.0));
        assert!((spans[0].0 - 0.0).abs() < 1e-6 && (spans[0].1 - 2.0).abs() < 1e-6);
        assert!(
            (spans[1].0 - 5.0).abs() < 1e-6 && (spans[1].1 - 10.0).abs() < 1e-6,
            "right side must stay contiguous across the x=8 cutter, got {:?}",
            spans[1]
        );
    }

    #[test]
    fn trim_removes_bounded_leftover_pieces() {
        let mut doc = Document::new();
        let target = draw::line(&mut doc, pt(0, 0), pt(10, 0));
        let v: Vec<_> = [2, 5]
            .iter()
            .map(|&x| draw::line(&mut doc, pt(x, -2), pt(x, 2)))
            .collect();
        let first = trim(&mut doc, target, &v, 3.5, 0.0);
        assert_eq!(first.len(), 2);
        let left = *first
            .iter()
            .find(|&&id| {
                matches!(doc.get(id).and_then(|e| e.as_curve()),
                Some(Curve::Line(l)) if l.p0.x.min(l.p1.x) < 1.0)
            })
            .expect("left piece exists");
        let cutters: Vec<_> = doc.iter().map(|e| e.id).filter(|&i| i != left).collect();
        let second = trim(&mut doc, left, &cutters, 1.0, 0.0);
        assert!(second.is_empty(), "bounded leftover must be deleted");
        assert!(
            doc.get(left).is_none(),
            "the piece must be gone from the document"
        );
    }

    #[test]
    fn trim_removes_bounded_leftover_arc_piece() {
        let mut doc = Document::new();
        let target = draw::arc(&mut doc, pt(0, 0), r(5), 0.0, std::f64::consts::PI);
        let l1 = draw::line(&mut doc, pt(3, 0), pt(3, 6));
        let l2 = draw::line(&mut doc, pt(-3, 0), pt(-3, 6));
        let first = trim(&mut doc, target, &[l1, l2], 0.0, 5.0);
        assert_eq!(first.len(), 2);
        let right = *first
            .iter()
            .find(|&&id| {
                matches!(doc.get(id).and_then(|e| e.as_curve()),
                Some(Curve::Arc(a)) if a.start_angle < 0.1)
            })
            .expect("right piece exists");
        let cutters: Vec<_> = doc.iter().map(|e| e.id).filter(|&i| i != right).collect();
        let second = trim(&mut doc, right, &cutters, 4.8, 1.0);
        assert!(second.is_empty(), "bounded arc piece must be deleted");
        assert!(doc.get(right).is_none());
    }

    #[test]
    fn trim_leaves_untouched_entities_alone() {
        let mut doc = Document::new();
        let lonely = draw::line(&mut doc, pt(20, 20), pt(30, 20));
        let far = draw::line(&mut doc, pt(0, 0), pt(0, 5));
        let result = trim(&mut doc, lonely, &[far], 25.0, 20.0);
        assert_eq!(
            result,
            vec![lonely],
            "no intersection, no endpoint contact → no-op"
        );
        assert!(doc.get(lonely).is_some());
    }

    #[test]
    fn trim_same_line_twice() {
        let mut doc = Document::new();
        let target = draw::line(&mut doc, pt(0, 0), pt(10, 0));
        let v: Vec<_> = [2, 5]
            .iter()
            .map(|&x| draw::line(&mut doc, pt(x, -2), pt(x, 2)))
            .collect();
        let first = trim(&mut doc, target, &v, 3.5, 0.0);
        assert_eq!(first.len(), 2);
        let right = *first
            .iter()
            .find(|&&id| {
                matches!(doc.get(id).and_then(|e| e.as_curve()),
                Some(Curve::Line(l)) if l.p0.x.max(l.p1.x) > 9.0)
            })
            .expect("right piece exists");
        draw::line(&mut doc, pt(8, -2), pt(8, 2));
        let cutters: Vec<_> = doc.iter().map(|e| e.id).filter(|&i| i != right).collect();
        let second = trim(&mut doc, right, &cutters, 6.5, 0.0);
        assert_eq!(
            second.len(),
            1,
            "second trim on the same line must still cut"
        );
        if let Some(Curve::Line(l)) = doc.get(second[0]).and_then(|e| e.as_curve()) {
            let (x0, x1) = (l.p0.x.min(l.p1.x), l.p0.x.max(l.p1.x));
            assert!(
                (x0 - 8.0).abs() < 1e-6 && (x1 - 10.0).abs() < 1e-6,
                "expected the [8,10] piece, got [{x0},{x1}]"
            );
        } else {
            panic!("survivor is not a line");
        }
    }

    #[test]
    fn trim_arc_between_two_lines() {
        let mut doc = Document::new();
        let target = draw::arc(&mut doc, pt(0, 0), r(5), 0.0, std::f64::consts::PI);
        let l1 = draw::line(&mut doc, pt(3, 0), pt(3, 6));
        let l2 = draw::line(&mut doc, pt(-3, 0), pt(-3, 6));
        let survivors = trim(&mut doc, target, &[l1, l2], 0.0, 5.0);
        assert_eq!(survivors.len(), 2, "both line cutters must register");
        for id in &survivors {
            if let Some(Curve::Arc(a)) = doc.get(*id).and_then(|e| e.as_curve()) {
                let hits_cut = [a.start_point(), a.end_point()]
                    .iter()
                    .any(|(x, y)| (x.abs() - 3.0).abs() < 1e-6 && (y - 4.0).abs() < 1e-6);
                assert!(hits_cut, "piece does not end at a cut point");
            } else {
                panic!("survivor is not an arc");
            }
        }
    }

    #[test]
    fn trim_arc_with_wrapped_angle_cut() {
        let mut doc = Document::new();
        let target = draw::arc(&mut doc, pt(0, 0), r(5), 0.0, 1.5 * std::f64::consts::PI);
        let x = -5.0 / 2f64.sqrt();
        let l = draw::line(
            &mut doc,
            Point2d::from_f64(x, -6.0),
            Point2d::from_f64(x, 0.0),
        );
        let (px, py) = (5.0 * 4.3f64.cos(), 5.0 * 4.3f64.sin());
        let survivors = trim(&mut doc, target, &[l], px, py);
        assert_eq!(survivors.len(), 1, "the wrapped-angle cut must register");
        assert_ne!(survivors[0], target, "trim must actually split the arc");
        if let Some(Curve::Arc(a)) = doc.get(survivors[0]).and_then(|e| e.as_curve()) {
            let expected = 1.25 * std::f64::consts::PI;
            assert!(
                (a.end_angle - expected).abs() < 1e-3,
                "survivor must end at 5π/4, got {}",
                a.end_angle
            );
        } else {
            panic!("survivor is not an arc");
        }
    }

    #[test]
    fn explode_polycurve_into_segments() {
        let mut doc = Document::new();
        let segs = vec![
            Curve::Line(LineSeg::from_endpoints(pt(0, 0), pt(4, 0))),
            Curve::Line(LineSeg::from_endpoints(pt(4, 0), pt(4, 4))),
            Curve::Line(LineSeg::from_endpoints(pt(4, 4), pt(0, 4))),
            Curve::Line(LineSeg::from_endpoints(pt(0, 4), pt(0, 0))),
        ];
        let poly = draw::polycurve(&mut doc, segs);
        let pieces = explode(&mut doc, &[poly]);
        assert_eq!(pieces.len(), 4, "square explodes into 4 lines");
        assert!(doc.get(poly).is_none(), "original poly is removed");
        for id in &pieces {
            assert!(matches!(
                doc.get(*id).and_then(|e| e.as_curve()),
                Some(Curve::Line(_))
            ));
        }
    }

    #[test]
    fn join_chains_connected_lines_into_one_poly() {
        let mut doc = Document::new();
        let a = draw::line(&mut doc, pt(0, 0), pt(4, 0));
        let b = draw::line(&mut doc, pt(4, 4), pt(4, 0));
        let c = draw::line(&mut doc, pt(4, 4), pt(0, 4));
        let joined = join(&mut doc, &[a, b, c]);
        assert_eq!(joined.len(), 1, "one connected chain → one poly");
        assert!(
            doc.get(a).is_none() && doc.get(b).is_none() && doc.get(c).is_none(),
            "the joined originals are removed"
        );
        if let Some(Curve::Poly(pc)) = doc.get(joined[0]).and_then(|e| e.as_curve()) {
            assert_eq!(pc.segments.len(), 3);
            for w in pc.segments.windows(2) {
                let (_, e0) = {
                    let (t0, t1) = w[0].domain();
                    (w[0].evaluate_f64(t0), w[0].evaluate_f64(t1))
                };
                let (s1, _) = {
                    let (t0, t1) = w[1].domain();
                    (w[1].evaluate_f64(t0), w[1].evaluate_f64(t1))
                };
                assert!(sq_dist(e0, s1) < 1e-9, "segments must connect end→start");
            }
        } else {
            panic!("expected a PolyCurve");
        }
    }

    #[test]
    fn join_leaves_disconnected_curves_alone() {
        let mut doc = Document::new();
        let a = draw::line(&mut doc, pt(0, 0), pt(1, 0));
        let b = draw::line(&mut doc, pt(5, 5), pt(6, 5));
        let joined = join(&mut doc, &[a, b]);
        assert!(joined.is_empty(), "no shared endpoints → nothing joins");
        assert!(doc.get(a).is_some() && doc.get(b).is_some());
    }

    #[test]
    fn explode_leaves_non_poly_untouched() {
        let mut doc = Document::new();
        let line = draw::line(&mut doc, pt(0, 0), pt(1, 0));
        let pieces = explode(&mut doc, &[line]);
        assert!(pieces.is_empty(), "a plain line has nothing to explode");
        assert!(doc.get(line).is_some(), "the line is left alone");
    }

    #[test]
    fn trim_preview_matches_removed_span() {
        let mut doc = Document::new();
        let target = draw::line(&mut doc, pt(0, 0), pt(10, 0));
        let c1 = draw::line(&mut doc, pt(3, -1), pt(3, 1));
        let c2 = draw::line(&mut doc, pt(7, -1), pt(7, 1));
        let preview =
            trim_preview(&doc, target, &[c1, c2], 5.0, 0.0).expect("a crossed span has a preview");
        if let Curve::Line(l) = preview {
            let (a, b) = (l.p0.x.min(l.p1.x), l.p0.x.max(l.p1.x));
            assert!(
                (a - 3.0).abs() < 1e-6 && (b - 7.0).abs() < 1e-6,
                "removed span [{a},{b}]"
            );
        } else {
            panic!("expected a line piece");
        }
        assert!(doc.get(target).is_some(), "preview is non-destructive");
        assert_eq!(trim(&mut doc, target, &[c1, c2], 5.0, 0.0).len(), 2);
    }

    #[test]
    fn trim_preview_none_when_untouched() {
        let mut doc = Document::new();
        let lonely = draw::line(&mut doc, pt(20, 20), pt(30, 20));
        let far = draw::line(&mut doc, pt(0, 0), pt(0, 5));
        assert!(trim_preview(&doc, lonely, &[far], 25.0, 20.0).is_none());
    }

    #[test]
    fn trim_preview_polyline_span_is_complete() {
        let mut doc = Document::new();
        let target = draw::polycurve(
            &mut doc,
            vec![
                Curve::Line(LineSeg::from_endpoints(pt(0, 0), pt(3, 0))),
                Curve::Line(LineSeg::from_endpoints(pt(3, 0), pt(6, 0))),
                Curve::Line(LineSeg::from_endpoints(pt(6, 0), pt(9, 0))),
            ],
        );
        let c1 = draw::line(&mut doc, pt(4, -1), pt(4, 1));
        let c2 = draw::line(&mut doc, pt(5, -1), pt(5, 1));
        let removed =
            trim_preview(&doc, target, &[c1, c2], 4.5, 0.0).expect("a crossed span previews");
        let bb = removed.bounding_box();
        assert!(
            (bb.min.x - 4.0).abs() < 1e-6,
            "removed span must start at x=4, got {}",
            bb.min.x
        );
        assert!(
            (bb.max.x - 5.0).abs() < 1e-6,
            "removed span must end at x=5, got {}",
            bb.max.x
        );
    }

    #[test]
    fn trim_spline_stays_a_spline() {
        let mut doc = Document::new();
        let cvs = vec![
            pt(0, 0),
            pt(2, 4),
            pt(4, -2),
            pt(6, 3),
            pt(8, -1),
            pt(10, 2),
        ];
        let nc = oxidraft_geometry::NurbsCurve::uniform(cvs.clone());
        let (mx, my) = nc.evaluate_f64(0.5);
        let spline = doc.add(EntityKind::Curve(Curve::Nurbs(nc)));
        let c1 = draw::line(&mut doc, pt(3, -6), pt(3, 6));
        let c2 = draw::line(&mut doc, pt(7, -6), pt(7, 6));
        let survivors = trim(&mut doc, spline, &[c1, c2], mx, my);
        assert_eq!(
            survivors.len(),
            2,
            "middle span removed → two spline pieces"
        );
        for id in &survivors {
            match doc.get(*id).and_then(|e| e.as_curve()) {
                Some(Curve::Nurbs(s)) => assert!(s.control().len() >= 2, "keeps control vertices"),
                other => panic!("survivor must stay a NURBS, got {other:?}"),
            }
        }
    }

    #[test]
    fn trim_full_circle_keeps_single_wrapping_arc() {
        let mut doc = Document::new();
        let target = draw::arc(&mut doc, pt(0, 0), r(5), 0.0, std::f64::consts::TAU);
        let cut = draw::line(&mut doc, pt(3, -6), pt(3, 6));
        let survivors = trim(&mut doc, target, &[cut], 5.0, 0.0);
        assert_eq!(
            survivors.len(),
            1,
            "a trimmed circle yields ONE wrapping arc, not two"
        );
        if let Some(Curve::Arc(a)) = doc.get(survivors[0]).and_then(|e| e.as_curve()) {
            let inc = a.included_angle();
            assert!(
                inc < std::f64::consts::TAU - 1e-3,
                "survivor must not be the full circle"
            );
            assert!(
                (inc - 4.428).abs() < 0.05,
                "survivor span ≈ 4.428 rad, got {inc}"
            );
        } else {
            panic!("survivor must be an arc");
        }
    }

    #[test]
    fn extend_preview_matches_applied_hit() {
        let mut doc = Document::new();
        let target = draw::line(&mut doc, pt(0, 0), pt(4, 0));
        let boundary = draw::line(&mut doc, pt(10, -5), pt(10, 5));
        let added = extend_preview(&doc, target, &[boundary], 4.0, 0.0)
            .expect("the line reaches the boundary");
        if let Curve::Line(l) = added {
            assert!(
                (l.p0.x - 4.0).abs() < 1e-6 && l.p0.y.abs() < 1e-6,
                "added starts at (4,0)"
            );
            assert!(
                (l.p1.x - 10.0).abs() < 1e-6 && l.p1.y.abs() < 1e-6,
                "added ends at (10,0)"
            );
        } else {
            panic!("expected a line preview");
        }
        assert!(doc.get(target).is_some(), "preview is non-destructive");
        assert!(extend(&mut doc, target, &[boundary], 4.0, 0.0));
        if let Curve::Line(l) = doc.get(target).unwrap().as_curve().unwrap() {
            let (x, y) = l.p1.to_f64();
            assert!((x - 10.0).abs() < 1e-6 && y.abs() < 1e-6);
        } else {
            panic!()
        }
    }

    #[test]
    fn extend_line_to_circle_stops_at_near_rim() {
        let mut doc = Document::new();
        let target = draw::line(&mut doc, pt(0, 0), pt(2, 0));
        let circle = draw::arc(&mut doc, pt(10, 0), r(3), 0.0, std::f64::consts::TAU);
        assert!(extend(&mut doc, target, &[circle], 2.0, 0.0));
        if let Curve::Line(l) = doc.get(target).unwrap().as_curve().unwrap() {
            let (x, y) = l.p1.to_f64();
            assert!(
                (x - 7.0).abs() < 1e-6 && y.abs() < 1e-6,
                "must stop at near rim (7,0), got ({x},{y})"
            );
        } else {
            panic!()
        }
    }

    #[test]
    fn extend_arc_grows_sweep_to_boundary() {
        use std::f64::consts::FRAC_PI_2;
        let mut doc = Document::new();
        let target = draw::arc(&mut doc, pt(0, 0), r(5), 0.0, FRAC_PI_2);
        let bound = draw::line(&mut doc, pt(-9, -5), pt(9, -5));
        assert!(extend(&mut doc, target, &[bound], 5.0, 0.1));
        if let Curve::Arc(a) = doc.get(target).unwrap().as_curve().unwrap() {
            assert!(
                (a.start_angle - (-FRAC_PI_2)).abs() < 1e-6,
                "start should extend to −90°, got {}",
                a.start_angle
            );
        } else {
            panic!("target must remain an arc");
        }
    }

    #[test]
    fn move_translates() {
        let mut doc = Document::new();
        let id = draw::line(&mut doc, pt(0, 0), pt(2, 0));
        move_by(&mut doc, &[id], r(5), r(3));
        if let Curve::Line(l) = doc.get(id).unwrap().as_curve().unwrap() {
            assert_eq!(l.p0, pt(5, 3));
            assert_eq!(l.p1, pt(7, 3));
        } else {
            panic!()
        }
    }

    #[test]
    fn copy_keeps_original_and_adds_new() {
        let mut doc = Document::new();
        let id = draw::line(&mut doc, pt(0, 0), pt(1, 0));
        let new = copy_by(&mut doc, &[id], r(10), r(0));
        assert_eq!(doc.len(), 2);
        assert_ne!(new[0], id);
        if let Curve::Line(l) = doc.get(new[0]).unwrap().as_curve().unwrap() {
            assert_eq!(l.p0, pt(10, 0));
        } else {
            panic!()
        }
    }

    #[test]
    fn rotate_90_about_origin() {
        let mut doc = Document::new();
        let id = draw::line(&mut doc, pt(1, 0), pt(2, 0));
        rotate(&mut doc, &[id], &pt(0, 0), std::f64::consts::FRAC_PI_2);
        if let Curve::Line(l) = doc.get(id).unwrap().as_curve().unwrap() {
            assert!((l.p0.x).abs() < 1e-9 && (l.p0.y - 1.0).abs() < 1e-6);
            assert!((l.p1.x).abs() < 1e-9 && (l.p1.y - 2.0).abs() < 1e-6);
        } else {
            panic!()
        }
    }

    #[test]
    fn scale_doubles_size() {
        let mut doc = Document::new();
        let id = draw::line(&mut doc, pt(1, 1), pt(3, 1));
        scale(&mut doc, &[id], &pt(1, 1), r(2));
        if let Curve::Line(l) = doc.get(id).unwrap().as_curve().unwrap() {
            assert_eq!(l.p0, pt(1, 1));
            assert_eq!(l.p1, pt(5, 1));
        } else {
            panic!()
        }
    }

    #[test]
    fn mirror_keep_original_adds_copy() {
        let mut doc = Document::new();
        let id = draw::line(&mut doc, pt(1, 2), pt(3, 4));
        let new = mirror(&mut doc, &[id], &pt(0, 0), &pt(1, 0), true);
        assert_eq!(doc.len(), 2);
        if let Curve::Line(l) = doc.get(new[0]).unwrap().as_curve().unwrap() {
            assert_eq!(l.p0, pt(1, -2));
            assert_eq!(l.p1, pt(3, -4));
        } else {
            panic!()
        }
    }

    #[test]
    fn offset_circle_changes_radius_by_the_signed_distance() {
        let mut doc = Document::new();
        let id = doc.add(EntityKind::Curve(Curve::Arc(CircularArc::new(
            pt(0, 0),
            r(5),
            0.0,
            2.0 * std::f64::consts::PI,
        ))));
        // Offset distance is signed "left of travel"; this circle is CCW, so
        // left points inward and a negative distance is the outward one. (The
        // OFFSET tool picks the sign from which side the user clicks, so both
        // directions stay reachable interactively.)
        let grown = offset(&mut doc, &[id], -2.0);
        if let Curve::Arc(a) = doc.get(grown[0]).unwrap().as_curve().unwrap() {
            assert!((a.radius - 7.0).abs() < 1e-6, "radius {}", a.radius);
        } else {
            panic!()
        }
        let shrunk = offset(&mut doc, &[id], 2.0);
        if let Curve::Arc(a) = doc.get(shrunk[0]).unwrap().as_curve().unwrap() {
            assert!((a.radius - 3.0).abs() < 1e-6, "radius {}", a.radius);
        } else {
            panic!()
        }
    }

    #[test]
    fn rect_array_count() {
        let mut doc = Document::new();
        let id = draw::line(&mut doc, pt(0, 0), pt(1, 0));
        let new = array_rect(&mut doc, &[id], 2, 3, r(5), r(5));
        assert_eq!(new.len(), 5);
        assert_eq!(doc.len(), 6);
    }

    #[test]
    fn polar_array_count() {
        let mut doc = Document::new();
        let id = draw::line(&mut doc, pt(1, 0), pt(2, 0));
        let new = array_polar(&mut doc, &[id], &pt(0, 0), 4, 2.0 * std::f64::consts::PI);
        assert_eq!(new.len(), 3);
    }

    #[test]
    fn break_splits_in_two() {
        let mut doc = Document::new();
        let id = draw::line(&mut doc, pt(0, 0), pt(10, 0));
        let pieces = break_at(&mut doc, id, 0.5);
        assert_eq!(pieces.len(), 2);
        assert!(doc.get(id).is_none());
    }

    #[test]
    fn extend_line_to_line_boundary() {
        let mut doc = Document::new();
        let target = draw::line(&mut doc, pt(0, 0), pt(4, 0));
        let boundary = draw::line(&mut doc, pt(10, -5), pt(10, 5));
        assert!(extend(&mut doc, target, &[boundary], 4.0, 0.0));
        if let Curve::Line(l) = doc.get(target).unwrap().as_curve().unwrap() {
            let (x, y) = l.p1.to_f64();
            assert!((x - 10.0).abs() < 1e-6 && y.abs() < 1e-6, "got ({x},{y})");
        } else {
            panic!()
        }
    }

    #[test]
    fn blend_g0_connects_nearest_ends_with_a_line() {
        let mut doc = Document::new();
        // Two horizontal lines with a gap; nearest ends are a.p1=(2,0) and b.p0=(5,0).
        let a = draw::line(&mut doc, pt(0, 0), pt(2, 0));
        let b = draw::line(&mut doc, pt(5, 0), pt(7, 0));
        let id = blend(&mut doc, a, b, Continuity::G0, 1.0).expect("blend should succeed");
        let c = doc.get(id).unwrap().as_curve().unwrap();
        assert!(matches!(c, Curve::Line(_)), "G0 blend is a line");
        let s = c.evaluate_f64(0.0);
        let e = c.evaluate_f64(1.0);
        assert!((s.0 - 2.0).abs() < 1e-9 && s.1.abs() < 1e-9, "start {s:?}");
        assert!((e.0 - 5.0).abs() < 1e-9 && e.1.abs() < 1e-9, "end {e:?}");
    }

    #[test]
    fn blend_g1_is_tangent_continuous_with_both_sources() {
        let mut doc = Document::new();
        let a = draw::line(&mut doc, pt(0, 0), pt(2, 0)); // ends horizontal at (2,0)
        let b = draw::line(&mut doc, pt(4, 2), pt(4, 5)); // starts vertical at (4,2)
        let id = blend(&mut doc, a, b, Continuity::G1, 1.0).expect("blend should succeed");
        let c = doc.get(id).unwrap().as_curve().unwrap();
        assert!(matches!(c, Curve::Bezier(_)), "G1 blend is a cubic");

        let unit = |v: (f64, f64)| {
            let m = v.0.hypot(v.1);
            (v.0 / m, v.1 / m)
        };
        let t0 = unit(c.tangent_f64(0.0));
        let t1 = unit(c.tangent_f64(1.0));
        assert!(
            (t0.0 - 1.0).abs() < 1e-6 && t0.1.abs() < 1e-6,
            "leaves +x: {t0:?}"
        );
        assert!(
            t1.0.abs() < 1e-6 && (t1.1 - 1.0).abs() < 1e-6,
            "arrives +y: {t1:?}"
        );
    }

    #[test]
    fn blend_rejects_same_entity() {
        let mut doc = Document::new();
        let a = draw::line(&mut doc, pt(0, 0), pt(2, 0));
        assert!(blend(&mut doc, a, a, Continuity::G1, 1.0).is_none());
    }

    #[test]
    fn blend_preview_matches_committed_blend_without_mutating_doc() {
        // blend_preview backs the live-preview popup: it must compute exactly
        // the curve that `blend` would commit, and must not touch the document
        // (no entities added) so it's safe to call every frame while the user
        // is still tuning continuity/tension.
        let mut doc = Document::new();
        let a = draw::line(&mut doc, pt(0, 0), pt(2, 0));
        let b = draw::line(&mut doc, pt(4, 2), pt(4, 5));
        let before_len = doc.len();

        let preview = blend_preview(&doc, a, b, Continuity::G2, 1.0).expect("preview");
        assert_eq!(
            doc.len(),
            before_len,
            "blend_preview must not mutate the document"
        );

        let id = blend(&mut doc, a, b, Continuity::G2, 1.0).expect("blend should succeed");
        let committed = doc.get(id).unwrap().as_curve().unwrap();

        for t in [0.0, 0.25, 0.5, 0.75, 1.0] {
            let p = preview.evaluate_f64(t);
            let c = committed.evaluate_f64(t);
            assert!(
                (p.0 - c.0).abs() < 1e-9 && (p.1 - c.1).abs() < 1e-9,
                "preview and committed diverge at t={t}: preview={p:?} committed={c:?}"
            );
        }
    }

    #[test]
    fn fillet_rounds_a_line_bezier_corner() {
        use oxidraft_geometry::point_to_curve_distance;
        let mut doc = Document::new();
        let a = draw::line(&mut doc, pt(0, 0), pt(8, 0));
        // Near-vertical bezier crossing the line around x = 4.
        let b = draw::bezier(
            &mut doc,
            Point2d::from_f64(4.0, 4.0),
            Point2d::from_f64(4.1, 1.5),
            Point2d::from_f64(3.9, -1.5),
            Point2d::from_f64(4.0, -4.0),
        );
        let arc_id = fillet(&mut doc, a, b, 0.5, 5.0, 1.0).expect("freeform fillet should succeed");
        let Some(Curve::Arc(arc)) = doc.get(arc_id).unwrap().as_curve() else {
            panic!("expected a fillet arc");
        };
        assert!(
            (arc.radius - 0.5).abs() < 0.02,
            "radius {} off target",
            arc.radius
        );
        // The arc's endpoints must land on the retained legs.
        let (t0, t1) = (arc.start_angle, arc.end_angle);
        for th in [t0, t1] {
            let p = (
                arc.center.x + arc.radius * th.cos(),
                arc.center.y + arc.radius * th.sin(),
            );
            let da = point_to_curve_distance(doc.get(a).unwrap().as_curve().unwrap(), p.0, p.1);
            let db = point_to_curve_distance(doc.get(b).unwrap().as_curve().unwrap(), p.0, p.1);
            assert!(
                da.min(db) < 0.02,
                "arc end ({},{}) floats off both legs: {da} / {db}",
                p.0,
                p.1
            );
        }
        // The picked (+x, +y) corner is kept: the line keeps its right
        // side, the bezier its top side.
        let l = line_seg(&doc, a);
        assert!(
            l.p0.x.min(l.p1.x) > 3.0 && l.p0.x.max(l.p1.x) > 7.9,
            "line kept its +x leg: {l:?}"
        );
        let bb = doc.get(b).unwrap().as_curve().unwrap().bounding_box();
        assert!(
            bb.min.y > -0.1 && bb.max.y > 3.5,
            "bezier kept its top leg: {bb:?}"
        );
    }

    #[test]
    fn chamfer_cuts_across_a_line_bezier_crossing() {
        let mut doc = Document::new();
        let a = draw::line(&mut doc, pt(0, 0), pt(10, 0));
        let b = draw::bezier(
            &mut doc,
            Point2d::from_f64(4.0, 6.0),
            Point2d::from_f64(4.1, 2.0),
            Point2d::from_f64(3.9, -2.0),
            Point2d::from_f64(4.0, -4.0),
        );
        let conn = chamfer(&mut doc, a, b, 0.8, 0.6).expect("freeform chamfer should succeed");
        let Some(Curve::Line(cl)) = doc.get(conn).unwrap().as_curve() else {
            panic!("expected a chamfer line");
        };
        // Longer sides kept: the line's right side (cut ~0.8 past x=4),
        // the bezier's top side (cut ~0.6 above y=0).
        let (e0, e1) = (cl.p0.to_f64(), cl.p1.to_f64());
        let on_line = if e0.1.abs() < e1.1.abs() { e0 } else { e1 };
        let on_bez = if e0.1.abs() < e1.1.abs() { e1 } else { e0 };
        assert!(
            (on_line.0 - 4.8).abs() < 0.1 && on_line.1.abs() < 0.05,
            "line cut at {on_line:?}"
        );
        assert!(
            (on_bez.1 - 0.6).abs() < 0.1 && (on_bez.0 - 4.0).abs() < 0.2,
            "bezier cut at {on_bez:?}"
        );
        let l = line_seg(&doc, a);
        assert!(
            l.p0.x.min(l.p1.x) > 4.5 && l.p0.x.max(l.p1.x) > 9.9,
            "line kept its longer right side: {l:?}"
        );
    }

    #[test]
    fn fillet_two_perpendicular_lines() {
        let mut doc = Document::new();
        let a = draw::line(&mut doc, pt(10, 0), pt(0, 0));
        let b = draw::line(&mut doc, pt(0, 0), pt(0, 10));
        let arc_id = fillet(&mut doc, a, b, 2.0, 0.0, 0.0).expect("fillet should succeed");
        if let Curve::Arc(arc) = doc.get(arc_id).unwrap().as_curve().unwrap() {
            let (ccx, ccy) = arc.center.to_f64();
            assert!(
                (ccx - 2.0).abs() < 1e-6 && (ccy - 2.0).abs() < 1e-6,
                "center ({ccx},{ccy})"
            );
            assert!((arc.radius - 2.0).abs() < 1e-6);
        } else {
            panic!()
        }
        if let Curve::Line(l) = doc.get(a).unwrap().as_curve().unwrap() {
            let (x, y) = l.p1.to_f64();
            assert!(
                (x - 2.0).abs() < 1e-6 && y.abs() < 1e-6,
                "a tangent ({x},{y})"
            );
        } else {
            panic!()
        }
    }

    #[test]
    fn fillet_line_arc_at_shared_point() {
        let mut doc = Document::new();
        let line_id = draw::line(&mut doc, pt(10, 0), pt(5, 0));
        let arc_id = doc.add(EntityKind::Curve(Curve::Arc(CircularArc::new(
            pt(0, 0),
            r(5),
            0.0,
            std::f64::consts::FRAC_PI_2,
        ))));
        let fid = fillet(&mut doc, line_id, arc_id, 1.0, 4.0, 0.5)
            .expect("line-arc fillet should succeed");

        if let Curve::Arc(fa) = doc.get(fid).unwrap().as_curve().unwrap() {
            let (cx, cy) = fa.center.to_f64();
            assert!(
                (cx - 15f64.sqrt()).abs() < 1e-3,
                "fillet cx ≈ √15, got {cx:.5}"
            );
            assert!((cy - 1.0).abs() < 1e-3, "fillet cy ≈ 1,   got {cy:.5}");
            assert!((fa.radius - 1.0).abs() < 1e-4);
        } else {
            panic!("expected Arc")
        }

        if let Curve::Line(l) = doc.get(line_id).unwrap().as_curve().unwrap() {
            let (x, y) = l.p1.to_f64();
            assert!(
                (x - 15f64.sqrt()).abs() < 1e-3,
                "line tangent x ≈ √15, got {x:.5}"
            );
            assert!(y.abs() < 1e-6, "line tangent y = 0, got {y:.9}");
        } else {
            panic!("expected Line")
        }
    }

    #[test]
    fn fillet_arc_arc_at_shared_point() {
        use std::f64::consts::FRAC_PI_2;
        let mut doc = Document::new();
        let id_a = doc.add(EntityKind::Curve(Curve::Arc(CircularArc::new(
            pt(0, 5),
            r(5),
            -FRAC_PI_2,
            0.0,
        ))));
        let id_b = doc.add(EntityKind::Curve(Curve::Arc(CircularArc::new(
            Point2d::from_i64(-5, 0),
            r(5),
            0.0,
            FRAC_PI_2,
        ))));
        let fid =
            fillet(&mut doc, id_a, id_b, 1.0, 0.5, 0.5).expect("arc-arc fillet should succeed");

        if let Curve::Arc(fa) = doc.get(fid).unwrap().as_curve().unwrap() {
            assert!(
                (fa.radius - 1.0).abs() < 1e-4,
                "fillet arc radius should be 1"
            );
            let (fx, fy) = fa.center.to_f64();
            let d_a = (fx.powi(2) + (fy - 5.0).powi(2)).sqrt();
            let d_b = ((fx + 5.0).powi(2) + fy.powi(2)).sqrt();
            let (dlo, dhi) = if d_a < d_b { (d_a, d_b) } else { (d_b, d_a) };
            assert!(
                (dlo - 4.0).abs() < 0.01,
                "near dist should be 4 (r−1), got {dlo:.4}"
            );
            assert!(
                (dhi - 6.0).abs() < 0.01,
                "far  dist should be 6 (r+1), got {dhi:.4}"
            );
        } else {
            panic!("expected Arc")
        }
    }

    fn square_poly(doc: &mut Document) -> EntityId {
        let segs = vec![
            Curve::Line(LineSeg::from_endpoints(pt(0, 0), pt(4, 0))),
            Curve::Line(LineSeg::from_endpoints(pt(4, 0), pt(4, 4))),
            Curve::Line(LineSeg::from_endpoints(pt(4, 4), pt(0, 4))),
            Curve::Line(LineSeg::from_endpoints(pt(0, 4), pt(0, 0))),
        ];
        doc.add(EntityKind::Curve(Curve::Poly(Box::new(PolyCurve::new(
            segs,
        )))))
    }

    fn poly_segments(doc: &Document, id: EntityId) -> Vec<Curve> {
        match doc.get(id).and_then(|e| e.as_curve()) {
            Some(Curve::Poly(pc)) => pc.segments.clone(),
            _ => panic!("expected a polycurve"),
        }
    }

    #[test]
    fn fillet_poly_corner_inserts_arc_without_explode() {
        let mut doc = Document::new();
        let id = square_poly(&mut doc);
        assert!(
            fillet_poly_corner(&mut doc, id, 0, 1.0),
            "fillet should succeed"
        );
        let segs = poly_segments(&doc, id);
        assert_eq!(
            segs.len(),
            5,
            "one arc spliced in → 5 segments, still ONE entity"
        );
        match &segs[1] {
            Curve::Arc(a) => assert!(
                (a.radius - 1.0).abs() < 1e-6,
                "fillet radius 1, got {}",
                a.radius
            ),
            other => panic!("expected an arc at the corner, got {other:?}"),
        }
        if let Curve::Line(l) = &segs[0] {
            let (x, y) = l.p1.to_f64();
            assert!(
                (x - 3.0).abs() < 1e-6 && y.abs() < 1e-6,
                "trimmed bottom edge end ({x},{y})"
            );
        } else {
            panic!("segment 0 should still be a line")
        }
        if let Some(Curve::Poly(pc)) = doc.get(id).and_then(|e| e.as_curve()) {
            assert!(pc.check_g0(1e-6), "polycurve must stay connected");
        }
    }

    #[test]
    fn fillet_poly_corner_clockwise_stays_continuous() {
        let mut doc = Document::new();
        let segs = vec![
            Curve::Line(LineSeg::from_endpoints(pt(0, 0), pt(0, 4))),
            Curve::Line(LineSeg::from_endpoints(pt(0, 4), pt(4, 4))),
            Curve::Line(LineSeg::from_endpoints(pt(4, 4), pt(4, 0))),
            Curve::Line(LineSeg::from_endpoints(pt(4, 0), pt(0, 0))),
        ];
        let id = doc.add(EntityKind::Curve(Curve::Poly(Box::new(PolyCurve::new(
            segs,
        )))));
        assert!(
            fillet_poly_corner(&mut doc, id, 1, 1.0),
            "fillet should succeed"
        );
        if let Some(Curve::Poly(pc)) = doc.get(id).and_then(|e| e.as_curve()) {
            assert!(pc.check_g0(1e-6), "CW poly fillet must stay G0-continuous");
            assert!(
                matches!(&pc.segments[2], Curve::Arc(_)),
                "fillet arc spliced in"
            );
        } else {
            panic!("expected a polycurve");
        }
    }

    #[test]
    fn chamfer_poly_corner_inserts_line_without_explode() {
        let mut doc = Document::new();
        let id = square_poly(&mut doc);
        assert!(
            chamfer_poly_corner(&mut doc, id, 0, 1.0),
            "chamfer should succeed"
        );
        let segs = poly_segments(&doc, id);
        assert_eq!(segs.len(), 5, "one bevel line spliced in → 5 segments");
        assert!(matches!(&segs[1], Curve::Line(_)), "the bevel is a line");
        if let Some(Curve::Poly(pc)) = doc.get(id).and_then(|e| e.as_curve()) {
            assert!(pc.check_g0(1e-6), "polycurve must stay connected");
        }
    }

    #[test]
    fn chamfer_two_perpendicular_lines() {
        let mut doc = Document::new();
        let a = draw::line(&mut doc, pt(10, 0), pt(0, 0));
        let b = draw::line(&mut doc, pt(0, 0), pt(0, 10));
        let conn = chamfer(&mut doc, a, b, 3.0, 3.0).expect("chamfer should succeed");
        if let Curve::Line(l) = doc.get(conn).unwrap().as_curve().unwrap() {
            let (x0, y0) = l.p0.to_f64();
            let (x1, y1) = l.p1.to_f64();
            let ok = ((x0 - 3.0).abs() < 1e-6
                && y0.abs() < 1e-6
                && x1.abs() < 1e-6
                && (y1 - 3.0).abs() < 1e-6)
                || ((x1 - 3.0).abs() < 1e-6
                    && y1.abs() < 1e-6
                    && x0.abs() < 1e-6
                    && (y0 - 3.0).abs() < 1e-6);
            assert!(ok, "chamfer endpoints ({x0},{y0})-({x1},{y1})");
        } else {
            panic!()
        }
    }

    #[test]
    fn chamfer_rejects_non_positive_distances() {
        let mut doc = Document::new();
        let a = draw::line(&mut doc, pt(10, 0), pt(0, 0));
        let b = draw::line(&mut doc, pt(0, 0), pt(0, 10));
        assert!(
            chamfer(&mut doc, a, b, 0.0, 3.0).is_none(),
            "a zero distance must not move either line"
        );
        assert!(
            chamfer(&mut doc, a, b, -3.0, 3.0).is_none(),
            "a negative distance must not extend the line backward through the corner"
        );
        // Neither rejected call should have mutated the source lines.
        if let Curve::Line(l) = doc.get(a).unwrap().as_curve().unwrap() {
            assert_eq!(l.p0, pt(10, 0));
            assert_eq!(l.p1, pt(0, 0));
        } else {
            panic!()
        }
    }

    #[test]
    fn chamfer_poly_corner_rejects_non_positive_distance() {
        let mut doc = Document::new();
        let id = square_poly(&mut doc);
        assert!(
            !chamfer_poly_corner(&mut doc, id, 0, 0.0),
            "a zero distance must be rejected"
        );
        assert!(
            !chamfer_poly_corner(&mut doc, id, 0, -1.0),
            "a negative distance must be rejected"
        );
        let segs = poly_segments(&doc, id);
        assert_eq!(
            segs.len(),
            4,
            "a rejected chamfer must not splice in a bevel"
        );
        if let Curve::Line(l) = &segs[0] {
            assert_eq!(l.p0, pt(0, 0));
            assert_eq!(l.p1, pt(4, 0));
        } else {
            panic!()
        }
    }

    #[test]
    fn stretch_moves_only_windowed_endpoints() {
        let mut doc = Document::new();
        let id = draw::line(&mut doc, pt(0, 0), pt(10, 0));
        stretch(&mut doc, &[id], (9.0, -1.0, 11.0, 1.0), 0.0, 5.0);
        if let Curve::Line(l) = doc.get(id).unwrap().as_curve().unwrap() {
            assert_eq!(l.p0, pt(0, 0));
            let (x, y) = l.p1.to_f64();
            assert!(
                (x - 10.0).abs() < 1e-6 && (y - 5.0).abs() < 1e-6,
                "stretched end ({x},{y})"
            );
        } else {
            panic!()
        }
    }

    #[test]
    fn stretch_arc_endpoint_moves_only_that_endpoint() {
        let mut doc = Document::new();
        let id = draw::arc(&mut doc, pt(0, 0), 5.0, 0.0, std::f64::consts::FRAC_PI_2);
        let start = arc_point(
            &CircularArc::new(pt(0, 0), 5.0, 0.0, std::f64::consts::FRAC_PI_2),
            0.0,
        );
        stretch(
            &mut doc,
            &[id],
            (start.x - 1.0, start.y - 1.0, start.x + 1.0, start.y + 1.0),
            0.0,
            5.0,
        );
        if let Curve::Arc(a) = doc.get(id).unwrap().as_curve().unwrap() {
            assert!((a.center.x - 0.0).abs() < 1e-6 && (a.center.y - 0.0).abs() < 1e-6);
            assert!((a.radius - 5.0).abs() < 1e-6);
            let new_start = arc_point(a, a.start_angle);
            let expected_angle = (start.y + 5.0 - a.center.y).atan2(start.x - a.center.x);
            let expected = arc_point(a, expected_angle);
            assert!(
                (new_start.x - expected.x).abs() < 1e-3 && (new_start.y - expected.y).abs() < 1e-3,
                "start endpoint moved onto the same circle in the nudged direction: {new_start:?}"
            );
            let end = arc_point(a, a.end_angle);
            assert!(
                (end.x - 0.0).abs() < 1e-3 && (end.y - 5.0).abs() < 1e-3,
                "end endpoint untouched: {end:?}"
            );
        } else {
            panic!()
        }
    }

    #[test]
    fn stretch_arc_center_translates_whole_arc() {
        let mut doc = Document::new();
        let id = draw::arc(&mut doc, pt(0, 0), 5.0, 0.0, std::f64::consts::FRAC_PI_2);
        stretch(&mut doc, &[id], (-1.0, -1.0, 1.0, 1.0), 3.0, 4.0);
        if let Curve::Arc(a) = doc.get(id).unwrap().as_curve().unwrap() {
            assert!((a.center.x - 3.0).abs() < 1e-6 && (a.center.y - 4.0).abs() < 1e-6);
            assert!((a.radius - 5.0).abs() < 1e-6);
        } else {
            panic!()
        }
    }

    #[test]
    fn stretch_arc_whole_arc_in_window_translates_rigidly() {
        let mut doc = Document::new();
        let id = draw::arc(&mut doc, pt(0, 0), 1.0, 0.0, std::f64::consts::FRAC_PI_2);
        // Window encloses the center and both endpoints, as when a user
        // rubber-band-selects the whole arc and drags it.
        stretch(&mut doc, &[id], (-2.0, -2.0, 2.0, 2.0), 0.5, 0.3);
        if let Curve::Arc(a) = doc.get(id).unwrap().as_curve().unwrap() {
            assert!((a.center.x - 0.5).abs() < 1e-6 && (a.center.y - 0.3).abs() < 1e-6);
            assert!((a.radius - 1.0).abs() < 1e-6);
            assert!(
                a.start_angle.abs() < 1e-6,
                "start angle must be unchanged by a rigid translation: {}",
                a.start_angle
            );
            assert!(
                (a.end_angle - std::f64::consts::FRAC_PI_2).abs() < 1e-6,
                "end angle must be unchanged by a rigid translation: {}",
                a.end_angle
            );
        } else {
            panic!()
        }
    }

    #[test]
    fn stretch_polyline_joint_moves_shared_vertex_in_both_segments() {
        let mut doc = Document::new();
        let id = draw::polycurve(
            &mut doc,
            vec![
                Curve::Line(LineSeg::from_endpoints(pt(0, 0), pt(5, 0))),
                Curve::Line(LineSeg::from_endpoints(pt(5, 0), pt(5, 5))),
            ],
        );
        stretch(&mut doc, &[id], (4.0, -1.0, 6.0, 1.0), 0.0, 2.0);
        if let Curve::Poly(pc) = doc.get(id).unwrap().as_curve().unwrap() {
            if let (Curve::Line(l0), Curve::Line(l1)) = (&pc.segments[0], &pc.segments[1]) {
                assert!((l0.p1.x - 5.0).abs() < 1e-6 && (l0.p1.y - 2.0).abs() < 1e-6);
                assert!((l1.p0.x - 5.0).abs() < 1e-6 && (l1.p0.y - 2.0).abs() < 1e-6);
                assert!((l0.p0.x - 0.0).abs() < 1e-6 && (l0.p0.y - 0.0).abs() < 1e-6);
                assert!((l1.p1.x - 5.0).abs() < 1e-6 && (l1.p1.y - 5.0).abs() < 1e-6);
            } else {
                panic!()
            }
        } else {
            panic!()
        }
    }

    #[test]
    fn stretch_nurbs_control_point_moves_only_that_point() {
        use oxidraft_geometry::NurbsCurve;
        let mut doc = Document::new();
        let control = vec![pt(0, 0), pt(5, 5), pt(10, 5), pt(15, 0)];
        let id = doc.add(EntityKind::Curve(Curve::Nurbs(NurbsCurve::new(
            control.clone(),
            vec![1.0; 4],
        ))));
        stretch(&mut doc, &[id], (4.0, 4.0, 6.0, 6.0), 1.0, 1.0);
        if let Curve::Nurbs(nc) = doc.get(id).unwrap().as_curve().unwrap() {
            assert!(
                (nc.control()[0].x - 0.0).abs() < 1e-6 && (nc.control()[0].y - 0.0).abs() < 1e-6
            );
            assert!(
                (nc.control()[1].x - 6.0).abs() < 1e-6 && (nc.control()[1].y - 6.0).abs() < 1e-6
            );
            assert!(
                (nc.control()[2].x - 10.0).abs() < 1e-6 && (nc.control()[2].y - 5.0).abs() < 1e-6
            );
            assert!(
                (nc.control()[3].x - 15.0).abs() < 1e-6 && (nc.control()[3].y - 0.0).abs() < 1e-6
            );
        } else {
            panic!()
        }
    }

    #[test]
    fn trim_removes_middle_piece() {
        let mut doc = Document::new();
        let target = draw::line(&mut doc, pt(0, 0), pt(10, 0));
        let c1 = draw::line(&mut doc, pt(3, -1), pt(3, 1));
        let c2 = draw::line(&mut doc, pt(7, -1), pt(7, 1));
        let survivors = trim(&mut doc, target, &[c1, c2], 5.0, 0.0);
        assert_eq!(survivors.len(), 2, "middle trimmed → 2 outer pieces");
        assert!(doc.get(target).is_none());
    }
}
