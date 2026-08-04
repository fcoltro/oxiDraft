# Anchor-aware Smart Dimension Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let Smart Dimension pick point anchors (endpoints, midpoints, circle centres) so it can dimension between them, which makes three already-built-but-unreachable constraint kinds usable and adds one new point-to-line kind.

**Architecture:** `Tool::DimConstraint` currently stores `Option<EntityId>`, which is what forces entity-level picking. It gains a `DimTarget` enum that is either a whole entity or a point anchor on one. Classification reuses the existing `weld_anchor_at` helper — the same one `Tool::Weld` and `Tool::ConPick` already use — so a click that snapped onto an anchor becomes `Anchor`, and anything else becomes `Entity`. Which constraint results is a table over the pair of picks; for two anchors the placement click picks among aligned/horizontal/vertical via the existing `linear_orientation`.

**Tech Stack:** Rust 2024, egui 0.35, workspace crates `oxidraft_document` (model), `oxidraft_constraint` (numeric solver), `oxidraft_cad` (constraint operations), `oxidraft_io` (native format), `oxidraft_ui` (tools + view).

## Global Constraints

- Spec: `docs/superpowers/specs/2026-08-04-anchor-aware-dimensioning-design.md`.
- No native file-format shape change. A new kind must serialize on the existing anchored+valued line (`C <CODE> ia ea ib eb v [px py]`), which happens automatically once `has_anchors()` and `is_valued()` include it.
- Driving distances must stay strictly positive — `constrain_point_distance` already rejects `<= 0.0`; the new builder must match that rule.
- The four existing entity-level dimension behaviours (line length, arc radius, parallel-line width, line-line angle) must not change.
- Point-to-rim (anchor + circle/arc) distance is out of scope and must be refused with a message, never guessed.
- Run `cargo fmt --all` before every commit; the repo is fmt-clean and CI checks it.
- Comments explain *why*, not *what* — match the density and voice of surrounding code.

**Adding the `PointLineDistance` variant will break seven exhaustive matches.** This is intentional (the compiler finds them all). They are:

| File | Line | What it needs |
| ---- | ---- | ------------- |
| `oxidraft_document/src/constraint.rs` | ~103 | `label()` arm |
| `oxidraft_document/src/constraint.rs` | ~191 | `code()` arm |
| `oxidraft_cad/src/constrain.rs` | ~179 | join the pick-based error group |
| `oxidraft_cad/src/constrain.rs` | ~1377 | join the pick-based `selection_validity` group |
| `oxidraft_cad/src/constrain.rs` | ~1985 | the real lowering arm (Task 2) |
| `oxidraft_cad/src/edit.rs` | ~104 | join the no-op remap group |
| `oxidraft_ui/src/view/overlays.rs` | ~320 | join the `dim_badges` group (Task 3) |

---

## File Structure

| File | Responsibility | Tasks |
| ---- | -------------- | ----- |
| `crates/oxidraft_document/src/constraint.rs` | `PointLineDistance` variant + its classifiers | 1 |
| `crates/oxidraft_io/src/native.rs` | round-trip proof only (no production change) | 1 |
| `crates/oxidraft_cad/src/constrain.rs` | lowering arm, builder, `anchor_pos` visibility | 2, 3 |
| `crates/oxidraft_cad/src/edit.rs` | exhaustive-match arm | 1 |
| `crates/oxidraft_cad/src/lib.rs` | re-exports | 2, 3 |
| `crates/oxidraft_ui/src/view/overlays.rs` | badge model + dimension badge layout | 3 |
| `crates/oxidraft_ui/src/tools.rs` | `DimTarget`, `Tool::DimConstraint`, Tab cycling | 4 |
| `crates/oxidraft_ui/src/state/modify.rs` | click dispatch / pick classification | 4, 5, 6 |
| `crates/oxidraft_ui/src/state.rs` | `smart_dimension` resolution table | 5, 6 |
| `crates/oxidraft_ui/src/command.rs` | `PDIST`/`HDIST`/`VDIST`/`PLDIST` verbs | 7 |
| `crates/oxidraft_ui/src/view/render.rs` | `tool_prompt` arms | 7 |

---

### Task 1: `PointLineDistance` kind — model and persistence

Adds the variant and its classifiers. Persistence needs no production code at all — the serializer is driven entirely off `has_anchors()`/`is_valued()`/`code()` — so the round-trip test is what proves that claim.

**Files:**
- Modify: `crates/oxidraft_document/src/constraint.rs`
- Modify: `crates/oxidraft_cad/src/constrain.rs` (two exhaustive matches)
- Modify: `crates/oxidraft_cad/src/edit.rs` (one exhaustive match)
- Modify: `crates/oxidraft_ui/src/view/overlays.rs` (one exhaustive match)
- Test: `crates/oxidraft_document/src/constraint.rs` (new `#[cfg(test)]` module — the file currently has none)
- Test: `crates/oxidraft_io/src/native.rs` (existing `mod tests`)

**Interfaces:**
- Consumes: nothing (first task).
- Produces: `ConstraintKind::PointLineDistance`, with `code() == "PLDIST"`, `is_pair() == true`, `is_valued() == true`, `has_anchors() == true`. Every later task depends on these exact values.

- [ ] **Step 1: Write the failing classifier test**

Append to `crates/oxidraft_document/src/constraint.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn point_line_distance_is_an_anchored_valued_pair() {
        // These three predicates are what drive serialization in
        // `oxidraft_io::native` — an anchored+valued pair kind lands on the
        // existing `C CODE ia ea ib eb v` line with no format change. Getting
        // any of them wrong silently writes the wrong line shape.
        let k = ConstraintKind::PointLineDistance;
        assert!(k.is_pair(), "it relates a point anchor to a line entity");
        assert!(k.is_valued(), "the driving distance lives in `val`");
        assert!(k.has_anchors(), "the point anchor lives in `pts.0`");
        assert_eq!(k.code(), "PLDIST");
        assert_eq!(ConstraintKind::from_code("PLDIST"), Some(k));
    }

    #[test]
    fn every_kind_round_trips_through_its_code() {
        // `code()`/`from_code()` are a bijection or the loader drops records
        // it just wrote. Adding a kind without a `from_code` entry is the
        // exact mistake this catches.
        for k in ConstraintKind::ALL {
            assert_eq!(
                ConstraintKind::from_code(k.code()),
                Some(*k),
                "{} does not round-trip through its code",
                k.label()
            );
        }
    }
}
```

- [ ] **Step 2: Run it to make sure it fails**

Run: `cargo test -p oxidraft_document --lib constraint::tests`
Expected: FAIL — `no variant named PointLineDistance`, and `no associated item named ALL`.

- [ ] **Step 3: Add the variant, its classifiers, and `ALL`**

In `crates/oxidraft_document/src/constraint.rs`, add the variant after `Block`:

```rust
    /// A point anchor on `a` (index in `pts.0`) held at a driving
    /// perpendicular distance from the infinite line through line entity
    /// `b`, stored in `val`. `pts.1` is unused by construction, the same
    /// way [`ConstraintKind::PointOnLine`] leaves it unused — this is that
    /// relation with a non-zero distance.
    PointLineDistance,
```

Add to `label()`:

```rust
            ConstraintKind::PointLineDistance => "point-line distance",
```

Add to `code()`:

```rust
            ConstraintKind::PointLineDistance => "PLDIST",
```

Add to `from_code()` (before the `_ => return None` arm):

```rust
            "PLDIST" => ConstraintKind::PointLineDistance,
```

Add `ConstraintKind::PointLineDistance` to the `matches!` lists in `is_pair()`, `is_valued()`, and `has_anchors()`.

Add the `ALL` constant inside `impl ConstraintKind`, listing every variant in declaration order:

```rust
    /// Every kind, so tests can walk the whole set. A new variant that is
    /// left out of this list is a variant no exhaustive test covers.
    pub const ALL: &'static [ConstraintKind] = &[
        ConstraintKind::Horizontal,
        ConstraintKind::Vertical,
        ConstraintKind::Parallel,
        ConstraintKind::Perpendicular,
        ConstraintKind::EqualLength,
        ConstraintKind::Coincident,
        ConstraintKind::Tangent,
        ConstraintKind::Radius,
        ConstraintKind::Distance,
        ConstraintKind::LineDistance,
        ConstraintKind::Angle,
        ConstraintKind::Fixed,
        ConstraintKind::Concentric,
        ConstraintKind::Collinear,
        ConstraintKind::Midpoint,
        ConstraintKind::EqualRadius,
        ConstraintKind::PointOnLine,
        ConstraintKind::PointOnCircle,
        ConstraintKind::PointDistance,
        ConstraintKind::HDistance,
        ConstraintKind::VDistance,
        ConstraintKind::Symmetric,
        ConstraintKind::Block,
        ConstraintKind::PointLineDistance,
    ];
```

- [ ] **Step 4: Run it to verify the model tests pass**

Run: `cargo test -p oxidraft_document --lib constraint::tests`
Expected: PASS (2 tests).

- [ ] **Step 5: Fix the four downstream exhaustive matches**

`cargo build --workspace` now fails in four places. Fix each:

`crates/oxidraft_cad/src/constrain.rs` ~179 — add `PointLineDistance` to the pick-based group that returns the "pick its points on canvas" error:

```rust
        | ConstraintKind::VDistance
        | ConstraintKind::PointLineDistance
        | ConstraintKind::Symmetric => {
```

`crates/oxidraft_cad/src/constrain.rs` ~1377 — add it to the pick-based group in `selection_validity` (the arm commented "Pick-based kinds open a pick tool; any selection state is fine").

`crates/oxidraft_cad/src/edit.rs` ~104 — add it to the no-op group:

```rust
            | ConstraintKind::VDistance
            | ConstraintKind::PointLineDistance
```

`crates/oxidraft_ui/src/view/overlays.rs` ~301 — add it to the `dim_badges` group beside its siblings:

```rust
            | ConstraintKind::VDistance
            | ConstraintKind::PointLineDistance => {
                if c.val.is_some() {
                    dim_badges.push(*c);
                }
                continue;
            }
```

- [ ] **Step 6: Write the failing persistence round-trip test**

Append inside `mod tests` in `crates/oxidraft_io/src/native.rs`:

```rust
    #[test]
    fn roundtrip_point_line_distance_keeps_its_anchor_value_and_placement() {
        // PLDIST is anchored AND valued, so it must land on the existing
        // `C CODE ia ea ib eb v px py` line with no new format. This test is
        // the whole proof that adding the kind needed no serializer change.
        let mut doc = Document::new();
        let l = doc.add(EntityKind::Curve(Curve::Line(LineSeg::from_endpoints(
            pt_i(0, 0),
            pt_i(10, 0),
        ))));
        let p = doc.add(EntityKind::Point(pt_i(3, 4)));
        let mut c = SketchConstraint::point_distance(
            ConstraintKind::PointLineDistance,
            p,
            0,
            l,
            0,
            4.0,
        );
        c.place = Some((3.0, 2.0));
        doc.add_constraint(c);

        let doc2 = from_string(&to_string(&doc)).unwrap();
        assert_eq!(doc2.constraints.len(), 1);
        let got = doc2.constraints[0];
        let ids: Vec<_> = doc2.iter().map(|e| e.id).collect();
        assert_eq!(
            (got.kind, got.a, got.b, got.pts, got.val, got.place),
            (
                ConstraintKind::PointLineDistance,
                ids[1],
                Some(ids[0]),
                Some((0, 0)),
                Some(4.0),
                Some((3.0, 2.0))
            )
        );
    }
```

- [ ] **Step 7: Run it to verify it passes**

Run: `cargo test -p oxidraft_io --lib roundtrip_point_line_distance`
Expected: PASS with no production change to `native.rs` — if it fails, the classifiers in Step 3 are wrong, not the serializer.

- [ ] **Step 8: Verify nothing else broke, then commit**

```bash
cargo fmt --all
cargo test --workspace
git add -A
git commit -m "Add the PointLineDistance constraint kind

Anchored and valued, so it serializes on the existing anchored+valued
line with no format change -- the round-trip test is the proof. Solver
lowering and a builder follow; this commit is the model only, plus the
four exhaustive matches the new variant breaks."
```

---

### Task 2: `PointLineDistance` — solver lowering and builder

Makes the kind actually hold geometry. The solver primitive already exists and is already used to implement `LineDistance`, so this is a lowering arm plus a builder shaped like its point-distance sibling.

**Files:**
- Modify: `crates/oxidraft_cad/src/constrain.rs`
- Modify: `crates/oxidraft_cad/src/lib.rs` (re-export)
- Test: `crates/oxidraft_cad/src/constrain.rs` (existing `mod tests`)

**Interfaces:**
- Consumes: `ConstraintKind::PointLineDistance` from Task 1.
- Produces: `pub fn constrain_point_line_distance(doc: &mut Document, anchor: (EntityId, u8), line: EntityId, value: Option<f64>, place: Option<(f64, f64)>) -> Result<String, ConstrainError>`, re-exported from `oxidraft_cad`. Task 6 calls it.

- [ ] **Step 1: Write the failing test**

Append inside `mod tests` in `crates/oxidraft_cad/src/constrain.rs`:

```rust
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
```

- [ ] **Step 2: Run it to make sure it fails**

Run: `cargo test -p oxidraft_cad --lib point_line_distance`
Expected: FAIL — `cannot find function constrain_point_line_distance`.

- [ ] **Step 3: Add the lowering arm**

In `component_sketch` in `crates/oxidraft_cad/src/constrain.rs`, add beside the `PointOnLine` arm:

```rust
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
```

- [ ] **Step 4: Add the builder**

Add after `constrain_point_distance` in the same file:

```rust
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
    let mut candidate =
        SketchConstraint::point_distance(ConstraintKind::PointLineDistance, a_id, ea, line, 0, target);
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
```

- [ ] **Step 5: Re-export it**

In `crates/oxidraft_cad/src/lib.rs`, add `constrain_point_line_distance` to the `pub use constrain::{…}` list, in alphabetical position after `constrain_point_distance`.

- [ ] **Step 6: Run the tests**

Run: `cargo test -p oxidraft_cad --lib point_line_distance`
Expected: PASS (2 tests).

- [ ] **Step 7: Commit**

```bash
cargo fmt --all
cargo test --workspace
git add -A
git commit -m "Hold a point off a line at a driving distance

The solver primitive already existed -- LineDistance is two of them on a
segment's endpoints -- so this is one lowering arm plus a builder shaped
like its point-distance sibling. Zero separation is refused: that is
PointOnLine, not a driving distance."
```

---

### Task 3: Dimension badges for the anchored valued kinds

`dim_badge_layout` has arms only for `Distance`, `LineDistance`, `Angle`, and `Radius`, and its `match` opens with `app.document.get(c.a)?.as_curve()?` — so a constraint whose `a` is a *point entity* bails before reaching any arm. All four anchored valued kinds are therefore pushed into `dim_badges` and then silently never drawn, which also means their values can never be clicked to edit. Fix that before the tool can create them.

**Files:**
- Modify: `crates/oxidraft_cad/src/constrain.rs` (make `anchor_pos` public)
- Modify: `crates/oxidraft_cad/src/lib.rs` (re-export)
- Modify: `crates/oxidraft_ui/src/view/overlays.rs`
- Test: `crates/oxidraft_ui/src/view/overlays.rs` (existing `mod tests`)

**Interfaces:**
- Consumes: `ConstraintKind::PointLineDistance` (Task 1).
- Produces: `oxidraft_cad::anchor_pos(doc: &Document, id: EntityId, idx: u8) -> Option<(f64, f64)>`. `dim_badge_layout` returns `Some` for all four anchored valued kinds.

- [ ] **Step 1: Write the failing test**

Append inside `mod tests` in `crates/oxidraft_ui/src/view/overlays.rs`:

```rust
    #[test]
    fn anchored_distance_badges_lay_out_instead_of_vanishing() {
        // `dim_badge_layout` used to open with `get(c.a)?.as_curve()?`, so a
        // constraint anchored on a POINT entity returned None before reaching
        // any arm -- the badge was collected and then silently never drawn,
        // taking its click-to-edit target with it.
        let mut app = AppState::new(800.0, 600.0);
        let l = app.add_entity(EntityKind::Curve(Curve::Line(LineSeg::from_endpoints(
            Point2d::from_f64(0.0, 0.0),
            Point2d::from_f64(10.0, 0.0),
        ))));
        let p = app.add_entity(EntityKind::Point(Point2d::from_f64(3.0, 4.0)));

        for kind in [
            ConstraintKind::PointDistance,
            ConstraintKind::HDistance,
            ConstraintKind::VDistance,
            ConstraintKind::PointLineDistance,
        ] {
            let mut doc = app.document.clone();
            doc.constraints.clear();
            doc.add_constraint(SketchConstraint::point_distance(kind, p, 0, l, 0, 4.0));
            app.document = doc;
            assert!(
                dim_badge_layout(&app, &app.document.constraints[0]).is_some(),
                "{} must produce a drawable badge",
                kind.label()
            );
        }
    }
```

- [ ] **Step 2: Run it to make sure it fails**

Run: `cargo test -p oxidraft_ui --lib anchored_distance_badges`
Expected: FAIL — the assert trips on `PointDistance`, the first kind in the list.

- [ ] **Step 3: Make `anchor_pos` public and re-export it**

In `crates/oxidraft_cad/src/constrain.rs`, change the signature and give it a doc comment:

```rust
/// World position of anchor `idx` on entity `id`: 0/1 an endpoint,
/// [`ANCHOR_DERIVED`] a line's midpoint or an arc's centre; a point entity
/// is its own anchor at any index. `None` when the entity is neither a
/// point, a line, nor an arc.
pub fn anchor_pos(doc: &Document, id: EntityId, idx: u8) -> Option<(f64, f64)> {
```

Add `anchor_pos` to the `pub use constrain::{…}` list in `crates/oxidraft_cad/src/lib.rs`.

- [ ] **Step 4: Add the layout arm**

In `dim_badge_layout` in `crates/oxidraft_ui/src/view/overlays.rs`, insert **before** the existing `match (c.kind, …as_curve()?)` — it must run first, because that match's `as_curve()?` bails on point-anchored records:

```rust
    // Anchored valued kinds resolve to two world points, not to a curve, so
    // they are handled before the curve match below -- its `as_curve()?`
    // returns None for a constraint anchored on a point entity, which is why
    // these badges were collected and then never drawn.
    //
    // The measured span runs anchor -> anchor, except for PointLineDistance,
    // where `b` is a whole line and the far end is the anchor's foot of
    // perpendicular on it. H/VDistance measure only one axis, so their span
    // is the projection, not the straight run between the anchors.
    if matches!(
        c.kind,
        ConstraintKind::PointDistance
            | ConstraintKind::HDistance
            | ConstraintKind::VDistance
            | ConstraintKind::PointLineDistance
    ) {
        let (ea, eb) = c.pts?;
        let (ax, ay) = oxidraft_cad::anchor_pos(&app.document, c.a, ea)?;
        let bid = c.b?;
        let (bx, by) = if c.kind == ConstraintKind::PointLineDistance {
            let Curve::Line(l) = app.document.get(bid)?.as_curve()? else {
                return None;
            };
            let (ux, uy) = (l.p1.x - l.p0.x, l.p1.y - l.p0.y);
            let n2 = ux * ux + uy * uy;
            if n2 <= 1e-18 {
                return None;
            }
            let t = ((ax - l.p0.x) * ux + (ay - l.p0.y) * uy) / n2;
            (l.p0.x + ux * t, l.p0.y + uy * t)
        } else {
            let (rx, ry) = oxidraft_cad::anchor_pos(&app.document, bid, eb)?;
            match c.kind {
                ConstraintKind::HDistance => (rx, ay),
                ConstraintKind::VDistance => (ax, ry),
                _ => (rx, ry),
            }
        };
        let a = px(ax, ay);
        let b = px(bx, by);
        if (b - a).length() < 12.0 {
            return None;
        }
        let d = (b - a).normalized();
        // Same offset convention as the Distance arm: the user's placement
        // picks the side and the distance, otherwise sit above the span.
        let mut n = vec2(d.y, -d.x);
        let o = match c.place {
            Some((wx, wy)) => {
                let q = px(wx, wy);
                if n.dot(q - a) < 0.0 {
                    n = -n;
                }
                n.dot(q - a).max(12.0)
            }
            None => {
                if n.y < 0.0 {
                    n = -n;
                }
                22.0
            }
        };
        let (ta, tb) = (a + n * o, b + n * o);
        let label = units.format_measure(val, style.precision);
        return Some(DimBadge {
            lines: vec![
                [a + n * 4.0, a + n * (o + 5.0)],
                [b + n * 4.0, b + n * (o + 5.0)],
                [ta, tb],
            ],
            arrows: vec![dim_arrow(ta, d), dim_arrow(tb, -d)],
            text_rect: dim_label_rect(ta + (tb - ta) * 0.5 + n * 13.0, &label),
            label,
        });
    }
```

`DimBadge` is `{ lines: Vec<[Pos2; 2]>, arrows: Vec<[Pos2; 3]>, label: String, text_rect: Rect }`; `dim_arrow`, `dim_label_rect`, and `units.format_measure(val, style.precision)` are the same helpers the `Distance` arm above uses. Do not invent fields or helpers.

- [ ] **Step 5: Run the test**

Run: `cargo test -p oxidraft_ui --lib anchored_distance_badges`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
cargo fmt --all
cargo test --workspace
git add -A
git commit -m "Draw the badges for point-anchored driving distances

dim_badge_layout opened with `get(c.a)?.as_curve()?`, so any constraint
anchored on a point entity returned None before reaching an arm: the four
anchored valued kinds were collected into dim_badges and then silently
never drawn, taking their click-to-edit target with them. They now resolve
through anchor_pos, which becomes public for it."
```

---

### Task 4: `DimTarget` — the pick model

Replaces `Tool::DimConstraint`'s entity-only fields. Classification only — no new constraints yet, so existing behaviour must be unchanged at the end of this task.

**Files:**
- Modify: `crates/oxidraft_ui/src/tools.rs`
- Modify: `crates/oxidraft_ui/src/state/modify.rs`
- Modify: `crates/oxidraft_ui/src/view/overlays.rs` (preview reads the tool's fields)
- Modify: `crates/oxidraft_ui/src/view/chrome.rs`, `crates/oxidraft_ui/src/view/render.rs` (construction sites)
- Test: `crates/oxidraft_ui/src/state.rs` (existing `mod tests`)

**Interfaces:**
- Consumes: nothing new.
- Produces:

```rust
pub enum DimTarget {
    Entity(EntityId),
    Anchor(EntityId, u8, Point2d),
}

pub enum Tool {
    DimConstraint {
        first: Option<DimTarget>,
        pending: Option<(DimTarget, Option<DimTarget>)>,
    },
    // …
}
```

Tasks 5 and 6 match on these.

- [ ] **Step 1: Write the failing classification test**

Append inside `mod tests` in `crates/oxidraft_ui/src/state.rs`:

```rust
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
```

- [ ] **Step 2: Run to make sure it fails**

Run: `cargo test -p oxidraft_ui --lib smart_dimension_reads`
Expected: FAIL — `no variant named DimTarget`.

- [ ] **Step 3: Add `DimTarget` and rewire the tool**

In `crates/oxidraft_ui/src/tools.rs`, add above `enum Tool`:

```rust
/// One thing Smart Dimension has picked: a whole entity, or a point on one.
///
/// The distinction is what makes dimensioning to a circle's centre possible
/// at all — before this, the tool was typed over `EntityId` and had no way
/// to name a point *on* an entity. `Anchor` carries its resolved world
/// position inline for the same reason [`TanAnchor`] does: the preview and
/// the commit then need no document to look it up from.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DimTarget {
    /// The entity itself — a line (its length) or a circle/arc (its radius).
    Entity(EntityId),
    /// A point anchor: entity, anchor index, resolved world position.
    Anchor(EntityId, u8, Point2d),
}

impl DimTarget {
    /// The entity this target names, whichever kind it is.
    pub fn entity(self) -> EntityId {
        match self {
            DimTarget::Entity(id) | DimTarget::Anchor(id, _, _) => id,
        }
    }
}
```

Change the `Tool::DimConstraint` variant to:

```rust
    DimConstraint {
        first: Option<DimTarget>,
        pending: Option<(DimTarget, Option<DimTarget>)>,
    },
```

- [ ] **Step 4: Classify picks in the click dispatch**

In `crates/oxidraft_ui/src/state/modify.rs`, in the `Tool::DimConstraint` arm, replace the raw `pick(self)` result with a classified target. Add just above the `match`:

```rust
                // An anchor within tolerance means the click was aimed at a
                // point on the entity, not the entity itself — the same rule
                // Weld and ConPick already use. With snapping on the click
                // lands exactly on the anchor, so this is exact rather than
                // a guess.
                let classify = |s: &Self, id: EntityId| match weld_anchor_at(s, id, px, py, tol) {
                    Some((idx, pos)) => DimTarget::Anchor(id, idx, pos),
                    None => DimTarget::Entity(id),
                };
```

Rewrite the arm's `match` so the existing four behaviours are preserved when both picks are `Entity`, banking `DimTarget` values instead of bare ids. Every `Tool::DimConstraint { first: Some(id), … }` becomes `first: Some(target)`, and the calls into `smart_dimension` unwrap with `.entity()` for now — Tasks 5 and 6 replace those call sites with the real resolution.

- [ ] **Step 5: Fix the other construction and read sites**

`cargo build -p oxidraft_ui` lists them. Expect: `view/chrome.rs` (the Smart Dimension button constructs the tool), `view/render.rs` (`tool_prompt`), `view/overlays.rs` (the preview reads `first`/`pending`), `command.rs` (the `DIMCON` verb), and `tools.rs` itself (`reset`, `has_partial`, `preview`, `last_point`). Preview code that had an `EntityId` now calls `.entity()`.

- [ ] **Step 6: Run the tests**

Run: `cargo test -p oxidraft_ui --lib smart_dimension`
Expected: PASS (2 tests). Existing dimension tests must also still pass — run `cargo test -p oxidraft_ui` and confirm zero failures.

- [ ] **Step 7: Commit**

```bash
cargo fmt --all
cargo test --workspace
git add -A
git commit -m "Give Smart Dimension a pick that can name a point

Tool::DimConstraint was typed over EntityId, which is precisely why it
could never dimension to a circle's centre. It now banks DimTarget --
either the whole entity or an anchor on it -- classified by whether the
click landed on a weldable anchor, the same rule Weld and ConPick use.
Behaviour is unchanged in this commit: every existing path still reads
Entity and produces the same four dimensions as before."
```

---

### Task 5: Anchor + anchor → `PointDistance` / `HDistance` / `VDistance`

The payoff task: two anchor picks, and the placement click chooses among three kinds. `constrain_point_distance` already does all the work; this wires it up.

**Files:**
- Modify: `crates/oxidraft_ui/src/state.rs` (`smart_dimension`)
- Modify: `crates/oxidraft_ui/src/state/modify.rs` (dispatch)
- Test: `crates/oxidraft_ui/src/state.rs`

**Interfaces:**
- Consumes: `DimTarget` (Task 4), `constrain_point_distance` (already exists).
- Produces: `AppState::smart_dimension_targets(&mut self, a: DimTarget, b: Option<DimTarget>, place: Option<(f64, f64)>) -> bool`. Task 6 extends its match.

- [ ] **Step 1: Write the failing test**

Append inside `mod tests` in `crates/oxidraft_ui/src/state.rs`:

```rust
    #[test]
    fn dimensioning_between_two_circle_centres_picks_the_kind_from_the_placement() {
        // The originally reported case. The placement click chooses among
        // the three kinds exactly the way drafting dimensions already do:
        // diagonal is the aligned distance, above/below is horizontal-only,
        // off to the side is vertical-only.
        use crate::tools::DimTarget;
        for (place, want) in [
            ((6.0, 6.0), ConstraintKind::PointDistance),
            ((5.0, 20.0), ConstraintKind::HDistance),
            ((20.0, 4.0), ConstraintKind::VDistance),
        ] {
            let mut a = app();
            let c1 = a.add_entity(EntityKind::Curve(Curve::Arc(
                oxidraft_geometry::CircularArc::new(
                    Point2d::from_f64(0.0, 0.0),
                    2.0,
                    0.0,
                    std::f64::consts::TAU,
                ),
            )));
            let c2 = a.add_entity(EntityKind::Curve(Curve::Arc(
                oxidraft_geometry::CircularArc::new(
                    Point2d::from_f64(10.0, 8.0),
                    2.0,
                    0.0,
                    std::f64::consts::TAU,
                ),
            )));
            let t1 = DimTarget::Anchor(c1, oxidraft_document::ANCHOR_DERIVED, Point2d::from_f64(0.0, 0.0));
            let t2 = DimTarget::Anchor(c2, oxidraft_document::ANCHOR_DERIVED, Point2d::from_f64(10.0, 8.0));

            assert!(
                a.smart_dimension_targets(t1, Some(t2), Some(place)),
                "centre-to-centre dimension must be created"
            );
            let got = a
                .document
                .constraints
                .iter()
                .find(|c| c.val.is_some())
                .expect("a driving constraint was recorded");
            assert_eq!(got.kind, want, "placement {place:?} chose the wrong kind");
        }
    }
```

- [ ] **Step 2: Run to make sure it fails**

Run: `cargo test -p oxidraft_ui --lib dimensioning_between_two_circle_centres`
Expected: FAIL — `no method named smart_dimension_targets`.

- [ ] **Step 3: Add the resolution method**

In `crates/oxidraft_ui/src/state.rs`, add beside `smart_dimension`:

```rust
    /// Smart Dimension's commit over [`DimTarget`]s: the resolution table
    /// from the design. Two entities behave exactly as `smart_dimension`
    /// always did; a pair of anchors becomes a driving point distance, whose
    /// kind the placement click chooses the same way drafting dimensions
    /// already choose aligned/horizontal/vertical.
    pub fn smart_dimension_targets(
        &mut self,
        a: DimTarget,
        b: Option<DimTarget>,
        place: Option<(f64, f64)>,
    ) -> bool {
        match (a, b) {
            (DimTarget::Anchor(ia, ea, pa), Some(DimTarget::Anchor(ib, eb, pb))) => {
                let kind = match place {
                    Some((lx, ly)) => match oxidraft_document::linear_orientation(
                        pa,
                        pb,
                        Point2d::from_f64(lx, ly),
                    ) {
                        Some(true) => ConstraintKind::VDistance,
                        Some(false) => ConstraintKind::HDistance,
                        None => ConstraintKind::PointDistance,
                    },
                    None => ConstraintKind::PointDistance,
                };
                let mut doc = self.document.clone();
                let res = oxidraft_cad::constrain_point_distance(
                    &mut doc,
                    kind,
                    (ia, ea),
                    (ib, eb),
                    None,
                    place,
                );
                self.finish_smart_dimension(doc, res, kind, ia, Some(ib))
            }
            // Entity-level picks keep the behaviour they always had.
            (a, b) => self.smart_dimension(a.entity(), b.map(|t| t.entity()), place),
        }
    }
```

Extract the tail of the existing `smart_dimension` (the `commit_constraint` / `pending_dim_edit` block) into a shared helper so both paths open the value editor identically:

```rust
    /// Commits a freshly built dimension and, on success, stashes it in
    /// `pending_dim_edit` so the UI opens its value editor immediately.
    fn finish_smart_dimension(
        &mut self,
        doc: oxidraft_document::Document,
        res: Result<String, oxidraft_cad::ConstrainError>,
        kind: ConstraintKind,
        a: EntityId,
        b: Option<EntityId>,
    ) -> bool {
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
```

Rewrite `smart_dimension`'s tail to call `finish_smart_dimension`, keeping its existing `place`-stamping step ahead of the call.

- [ ] **Step 4: Route the dispatch through it**

In `crates/oxidraft_ui/src/state/modify.rs`, change the `Tool::DimConstraint` arm's two commit sites from `self.smart_dimension(a.entity(), …)` to `self.smart_dimension_targets(a, b, Some((px, py)))`, and let a first `Anchor` pick fall into `first` (so it can pair with a second pick) rather than into `pending`.

- [ ] **Step 5: Run the tests**

Run: `cargo test -p oxidraft_ui --lib dimensioning_between_two_circle_centres`
Expected: PASS (all three placements).

- [ ] **Step 6: Commit**

```bash
cargo fmt --all
cargo test --workspace
git add -A
git commit -m "Dimension between two point anchors

PointDistance, HDistance and VDistance were fully built -- declared,
lowered, constructed by a finished constrain_point_distance, drawn, saved
and loaded -- with no caller anywhere in the UI. Two anchor picks now
reach them, and the placement click chooses among the three through
linear_orientation, the same call drafting dimensions already use.
Dimensioning two circle centres works."
```

---

### Task 6: Anchor + line → `PointLineDistance`

**Files:**
- Modify: `crates/oxidraft_ui/src/state.rs`
- Test: `crates/oxidraft_ui/src/state.rs`

**Interfaces:**
- Consumes: `constrain_point_line_distance` (Task 2), `smart_dimension_targets` (Task 5).
- Produces: no new public surface.

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn dimensioning_a_point_to_a_line_records_a_point_line_distance() {
        use crate::tools::DimTarget;
        let mut a = app();
        let l = a.add_entity(EntityKind::Curve(Curve::Line(LineSeg::from_endpoints(
            Point2d::from_f64(0.0, 0.0),
            Point2d::from_f64(10.0, 0.0),
        ))));
        let p = a.add_entity(EntityKind::Point(Point2d::from_f64(5.0, 4.0)));

        assert!(a.smart_dimension_targets(
            DimTarget::Anchor(p, 0, Point2d::from_f64(5.0, 4.0)),
            Some(DimTarget::Entity(l)),
            Some((5.0, 2.0)),
        ));
        assert!(
            a.document
                .constraints
                .iter()
                .any(|c| c.kind == ConstraintKind::PointLineDistance && c.val.is_some()),
            "a PLDIST must be recorded"
        );
    }

    #[test]
    fn dimensioning_a_point_to_a_circle_is_refused() {
        // Point-to-rim would be a fifth kind and is deliberately out of
        // scope -- refuse it rather than silently producing something else.
        use crate::tools::DimTarget;
        let mut a = app();
        let c = a.add_entity(EntityKind::Curve(Curve::Arc(
            oxidraft_geometry::CircularArc::new(
                Point2d::from_f64(0.0, 0.0),
                5.0,
                0.0,
                std::f64::consts::TAU,
            ),
        )));
        let p = a.add_entity(EntityKind::Point(Point2d::from_f64(20.0, 0.0)));
        assert!(!a.smart_dimension_targets(
            DimTarget::Anchor(p, 0, Point2d::from_f64(20.0, 0.0)),
            Some(DimTarget::Entity(c)),
            Some((10.0, 3.0)),
        ));
    }
```

- [ ] **Step 2: Run to make sure it fails**

Run: `cargo test -p oxidraft_ui --lib dimensioning_a_point_to_a`
Expected: FAIL — the first test records no PLDIST (the mixed pair currently falls through to the entity path).

- [ ] **Step 3: Add the mixed-pair arms**

In `smart_dimension_targets`, insert **before** the catch-all `(a, b)` arm:

```rust
            // A point and a line, in either pick order — the relation is the
            // same, so normalise it to anchor-first.
            (DimTarget::Anchor(ia, ea, _), Some(DimTarget::Entity(lb)))
            | (DimTarget::Entity(lb), Some(DimTarget::Anchor(ia, ea, _))) => {
                if !line_endpoints_of_doc(&self.document, lb) {
                    self.problem(
                        "Dimension a point to a line. Point-to-circle distance isn't \
                         supported yet — dimension the circle's centre instead."
                            .into(),
                    );
                    return false;
                }
                let mut doc = self.document.clone();
                let res =
                    oxidraft_cad::constrain_point_line_distance(&mut doc, (ia, ea), lb, None, place);
                self.finish_smart_dimension(
                    doc,
                    res,
                    ConstraintKind::PointLineDistance,
                    ia,
                    Some(lb),
                )
            }
```

Add the small predicate beside the other free functions in `state.rs`:

```rust
/// Whether `id` is a plain line segment — the only thing a point-line
/// distance can measure to.
fn line_endpoints_of_doc(doc: &oxidraft_document::Document, id: EntityId) -> bool {
    matches!(
        doc.get(id).and_then(|e| e.as_curve()),
        Some(Curve::Line(_))
    )
}
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p oxidraft_ui --lib dimensioning_a_point_to_a`
Expected: PASS (2 tests).

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
cargo test --workspace
git add -A
git commit -m "Dimension a point to a line

Either pick order means the same relation, so it normalises to
anchor-first. Point-to-circle is refused with a message that points at
the workaround (dimension the centre) rather than guessing at a fifth
constraint kind."
```

---

### Task 7: Command verbs, Tab cycling, and prompts

The four kinds are the only ones in the model with no command verb. Tab lets a mis-read pick be corrected in place.

**Files:**
- Modify: `crates/oxidraft_ui/src/command.rs`
- Modify: `crates/oxidraft_ui/src/tools.rs` (`cycle_reading`)
- Modify: `crates/oxidraft_ui/src/view/render.rs` (`tool_prompt`)
- Test: `crates/oxidraft_ui/src/command.rs`, `crates/oxidraft_ui/src/tools.rs`

**Interfaces:**
- Consumes: everything above.
- Produces: no new public surface.

- [ ] **Step 1: Write the failing tests**

In `crates/oxidraft_ui/src/command.rs`'s `mod tests`:

```rust
    #[test]
    fn the_anchored_distance_verbs_activate_smart_dimension() {
        // These four kinds were the only ones in the model with no verb at
        // all. They all open Smart Dimension: which of them you get is
        // decided by the picks and the placement, not chosen up front.
        for verb in ["PDIST", "HDIST", "VDIST", "PLDIST"] {
            assert!(
                matches!(
                    parse_command(verb),
                    Command::Activate(Tool::DimConstraint { .. })
                ),
                "{verb} must open Smart Dimension"
            );
        }
    }
```

In `crates/oxidraft_ui/src/tools.rs`'s `mod tests`:

```rust
    #[test]
    fn tab_flips_a_dimension_pick_between_anchor_and_entity() {
        // A centre snap that was meant as "this circle's radius" is corrected
        // in place rather than by cancelling the pick.
        let mut t = Tool::DimConstraint {
            first: Some(DimTarget::Anchor(EntityId(1), 2, pt(0, 0))),
            pending: None,
        };
        t.cycle_reading();
        assert!(matches!(
            t,
            Tool::DimConstraint {
                first: Some(DimTarget::Entity(EntityId(1))),
                ..
            }
        ));
        t.cycle_reading();
        assert!(matches!(
            t,
            Tool::DimConstraint {
                first: Some(DimTarget::Anchor(EntityId(1), 2, _)),
                ..
            }
        ));
    }
```

- [ ] **Step 2: Run to make sure they fail**

Run: `cargo test -p oxidraft_ui --lib the_anchored_distance_verbs; cargo test -p oxidraft_ui --lib tab_flips_a_dimension_pick`
Expected: both FAIL.

- [ ] **Step 3: Add the verbs**

In `crates/oxidraft_ui/src/command.rs`, extend the existing `DIMCON | SMARTDIM | GCDIM | SD` arm to also match `"PDIST" | "HDIST" | "VDIST" | "PLDIST"`, with a comment noting that the kind comes from the picks and placement, not from which verb was typed — the same reason `DIMDIAMETER` is an alias for `DIMENSION`.

- [ ] **Step 4: Add Tab cycling**

In `Tool::cycle_reading` in `crates/oxidraft_ui/src/tools.rs`:

```rust
            // A pick has two readings — the anchor, or the entity it sits on.
            // Tab corrects a snap that guessed wrong without losing the pick.
            // The anchor's stored position is kept so flipping back is exact.
            Tool::DimConstraint {
                first: Some(target),
                ..
            } => {
                *target = match *target {
                    DimTarget::Anchor(id, idx, pos) => {
                        *last_anchor = Some((id, idx, pos));
                        DimTarget::Entity(id)
                    }
                    DimTarget::Entity(id) => match *last_anchor {
                        Some((aid, idx, pos)) if aid == id => DimTarget::Anchor(id, idx, pos),
                        _ => DimTarget::Entity(id),
                    },
                };
            }
```

**Note for the implementer:** `Tool` has no field to stash `last_anchor` in. Add one to the `DimConstraint` variant — `last_anchor: Option<(EntityId, u8, Point2d)>` — defaulting to `None` at every construction site, and use it as above. This is what makes the flip reversible; without it, flipping to `Entity` would discard the anchor index and Tab could not flip back.

- [ ] **Step 5: Update the prompts**

In `tool_prompt` in `crates/oxidraft_ui/src/view/render.rs`, replace the `Tool::DimConstraint` arms so they describe the anchor cases:

```rust
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
```

- [ ] **Step 6: Run the tests**

Run: `cargo test -p oxidraft_ui`
Expected: PASS, zero failures.

- [ ] **Step 7: Update the README command table**

`README.md`'s command table lists constraint verbs. Add one row covering `PDIST` · `HDIST` · `VDIST` · `PLDIST` → "Dimension between points / to a line", matching the table's existing two-column style.

- [ ] **Step 8: Commit**

```bash
cargo fmt --all
cargo test --workspace
git add -A
git commit -m "Give the anchored distance kinds verbs, Tab, and prompts

PDIST/HDIST/VDIST/PLDIST were the only kinds in the model with no command
verb at all. All four open Smart Dimension -- which one you get is decided
by the picks and the placement, not chosen up front, the same way
DIMDIAMETER is an alias for DIMENSION. Tab re-reads the pick just made so
a snap that guessed wrong is corrected in place."
```

---

## Verification

After Task 7, confirm end to end in the running app (`cargo run --release`):

1. Draw two circles. Run `SMARTDIM`. Click one centre, then the other. Move the cursor and watch the preview switch between aligned, horizontal, and vertical as you move it diagonally, above, and to the side. Click to place. The badge appears and its value is editable.
2. Draw a line and a point. `SMARTDIM`, click the point, click the line, place. Drag the line — the point follows, holding its gap.
3. Save, reopen, and confirm all dimensions survive.
4. Confirm the four original behaviours are untouched: line length, circle radius, two parallel lines, two crossing lines.

## Self-Review Notes

- **Spec coverage:** pick model → Task 4; resolution table rows 5-7 → Tasks 5-6; rows 1-4 preserved and re-asserted in Task 4 Step 6; `PointLineDistance` model/persistence → Task 1; solver + builder → Task 2; badges → Task 3; commands/prompts/preview → Task 7; every spec test → the task owning that behaviour.
- **Gap found and added while reviewing:** the spec assumed badge rendering already worked for the three orphaned kinds. It does not — `dim_badge_layout` bails on point-anchored records before reaching any arm, so the badges were collected and never drawn. Task 3 exists to fix that and must land before Task 5, or centre-to-centre dimensions would be created invisibly.
- **Deviation from the spec, deliberate:** the spec says classification reads the pick's `SnapKind`. `handle_modify_click` receives an already-snapped point without the snap kind, so classification uses `weld_anchor_at` instead — same outcome (the snap is what puts the click exactly on the anchor), via the helper `Weld` and `ConPick` already use, and with no new plumbing.
- **Type consistency:** `DimTarget` is defined once (Task 4) and used identically in Tasks 5-7; `smart_dimension_targets` is introduced in Task 5 and extended in Task 6 with the same signature; `finish_smart_dimension` is introduced in Task 5 and used by both.
