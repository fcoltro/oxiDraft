//! The sketch constraint model: [`ConstraintKind`] (what relationship is being
//! held) and [`SketchConstraint`] (which entities/anchors it ties together and
//! any driving value). The numeric solver in `oxidraft_constraint` enforces
//! these; this crate only records them.

use crate::entity::EntityId;

/// The kind of geometric relationship a constraint enforces. Some are pure
/// relations (parallel, coincident); "driving" kinds also carry a numeric
/// value (a radius, length, or angle) that fixes a dimension.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConstraintKind {
    /// A line entity held horizontal.
    Horizontal,
    /// A line entity held vertical.
    Vertical,
    /// Two line entities held parallel.
    Parallel,
    /// Two line entities held at a right angle.
    Perpendicular,
    /// Two line entities held to equal length.
    EqualLength,
    /// Two point anchors (indices in `pts`) welded together.
    Coincident,
    /// A line entity tangent to a circular-arc entity (either order).
    Tangent,
    /// A circular-arc entity holding a driving radius, stored in `val`.
    Radius,
    /// A line entity holding a driving length, stored in `val`.
    Distance,
    /// Two parallel line entities held at a driving perpendicular distance
    /// (a width), stored in `val`. `a` is the reference line; `b` slides.
    LineDistance,
    /// Two line entities held at a driving angle. `val` is in degrees,
    /// normalized to (0, 180] — lines are undirected, so θ and θ+180° are
    /// the same relation, and 0° is recorded as 180° to survive the
    /// loader's positive-value check.
    Angle,
    /// A point entity permanently pinned wherever it currently sits — set
    /// automatically on structural anchors (the origin), never by a user
    /// command, so there's no bar/menu path that creates one.
    Fixed,
    /// Two circular-arc entities sharing one center.
    Concentric,
    /// Two line entities lying on one infinite line.
    Collinear,
    /// A point anchor on `a` welded to the midpoint of line `b`. Anchor
    /// indices in `pts`; `eb` is [`ANCHOR_DERIVED`] by construction.
    Midpoint,
    /// Two circular-arc entities holding equal radii.
    EqualRadius,
    /// A point anchor on `a` (index in `pts.0`) held on the infinite line
    /// through line entity `b`.
    PointOnLine,
    /// A point anchor on `a` (index in `pts.0`) held on the rim of
    /// circular-arc entity `b`.
    PointOnCircle,
    /// Two point anchors (one per entity, indices in `pts`) held at a
    /// driving straight-line distance, stored in `val`.
    PointDistance,
    /// Like [`ConstraintKind::PointDistance`] but driving only the
    /// horizontal separation of the two anchors.
    HDistance,
    /// Like [`ConstraintKind::PointDistance`] but driving only the
    /// vertical separation of the two anchors.
    VDistance,
    /// Two point anchors (one per entity, indices in `pts`) mirrored about
    /// the line entity in `c`.
    Symmetric,
    /// Entity `a` locked rigid relative to reference entity `b` — one
    /// record per member of a blocked group, all sharing the same `b`. The
    /// group keeps only its translate/rotate freedom.
    Block,
    /// A point anchor on `a` (index in `pts.0`) held at a driving
    /// perpendicular distance from the infinite line through line entity
    /// `b`, stored in `val`. `pts.1` is unused by construction, the same
    /// way [`ConstraintKind::PointOnLine`] leaves it unused — this is that
    /// relation with a non-zero distance.
    PointLineDistance,
}

impl ConstraintKind {
    /// A short human-readable name for the constraint (for the status bar and
    /// menus).
    pub fn label(&self) -> &'static str {
        match self {
            ConstraintKind::Horizontal => "horizontal",
            ConstraintKind::Vertical => "vertical",
            ConstraintKind::Parallel => "parallel",
            ConstraintKind::Perpendicular => "perpendicular",
            ConstraintKind::EqualLength => "equal length",
            ConstraintKind::Coincident => "coincident",
            ConstraintKind::Tangent => "tangent",
            ConstraintKind::Radius => "radius",
            ConstraintKind::Distance => "length",
            ConstraintKind::LineDistance => "distance",
            ConstraintKind::Angle => "angle",
            ConstraintKind::Fixed => "fixed",
            ConstraintKind::Concentric => "concentric",
            ConstraintKind::Collinear => "collinear",
            ConstraintKind::Midpoint => "midpoint",
            ConstraintKind::EqualRadius => "equal radius",
            ConstraintKind::PointOnLine => "point on line",
            ConstraintKind::PointOnCircle => "point on circle",
            ConstraintKind::PointDistance => "distance",
            ConstraintKind::HDistance => "horizontal distance",
            ConstraintKind::VDistance => "vertical distance",
            ConstraintKind::Symmetric => "symmetric",
            ConstraintKind::Block => "block",
            ConstraintKind::PointLineDistance => "point-line distance",
        }
    }

    /// Pair kinds relate two entities; the rest apply to one entity each.
    pub fn is_pair(&self) -> bool {
        matches!(
            self,
            ConstraintKind::Parallel
                | ConstraintKind::Perpendicular
                | ConstraintKind::EqualLength
                | ConstraintKind::Coincident
                | ConstraintKind::Tangent
                | ConstraintKind::Angle
                | ConstraintKind::LineDistance
                | ConstraintKind::Concentric
                | ConstraintKind::Collinear
                | ConstraintKind::Midpoint
                | ConstraintKind::EqualRadius
                | ConstraintKind::PointOnLine
                | ConstraintKind::PointOnCircle
                | ConstraintKind::PointDistance
                | ConstraintKind::HDistance
                | ConstraintKind::VDistance
                | ConstraintKind::Symmetric
                | ConstraintKind::Block
                | ConstraintKind::PointLineDistance
        )
    }

    /// Valued kinds carry a driving number in `val` (a radius, a length,
    /// a width, an angle); their record is corrupt without a positive value.
    pub fn is_valued(&self) -> bool {
        matches!(
            self,
            ConstraintKind::Radius
                | ConstraintKind::Distance
                | ConstraintKind::LineDistance
                | ConstraintKind::Angle
                | ConstraintKind::PointDistance
                | ConstraintKind::HDistance
                | ConstraintKind::VDistance
                | ConstraintKind::PointLineDistance
        )
    }

    /// Kinds whose record carries per-entity anchor indices in
    /// [`SketchConstraint::pts`] (0/1 = endpoints, [`ANCHOR_DERIVED`] =
    /// midpoint/center). The saver and loader serialize `pts` exactly for
    /// these kinds.
    pub fn has_anchors(&self) -> bool {
        matches!(
            self,
            ConstraintKind::Coincident
                | ConstraintKind::Midpoint
                | ConstraintKind::PointOnLine
                | ConstraintKind::PointOnCircle
                | ConstraintKind::PointDistance
                | ConstraintKind::HDistance
                | ConstraintKind::VDistance
                | ConstraintKind::Symmetric
                | ConstraintKind::PointLineDistance
        )
    }

    /// The short mnemonic code used on the command line and in the native
    /// file format (e.g. `PAR`, `COI`, `RAD`).
    pub fn code(&self) -> &'static str {
        match self {
            ConstraintKind::Horizontal => "H",
            ConstraintKind::Vertical => "V",
            ConstraintKind::Parallel => "PAR",
            ConstraintKind::Perpendicular => "PERP",
            ConstraintKind::EqualLength => "EQL",
            ConstraintKind::Coincident => "COI",
            ConstraintKind::Tangent => "TAN",
            ConstraintKind::Radius => "RAD",
            ConstraintKind::Distance => "LEN",
            ConstraintKind::LineDistance => "LDIST",
            ConstraintKind::Angle => "ANG",
            ConstraintKind::Fixed => "FIX",
            ConstraintKind::Concentric => "CONC",
            ConstraintKind::Collinear => "COLL",
            ConstraintKind::Midpoint => "MID",
            ConstraintKind::EqualRadius => "EQR",
            ConstraintKind::PointOnLine => "POL",
            ConstraintKind::PointOnCircle => "POC",
            ConstraintKind::PointDistance => "PDIST",
            ConstraintKind::HDistance => "HDIST",
            ConstraintKind::VDistance => "VDIST",
            ConstraintKind::Symmetric => "SYM",
            ConstraintKind::Block => "BLOCK",
            ConstraintKind::PointLineDistance => "PLDIST",
        }
    }

    /// Parses a [`ConstraintKind::code`] mnemonic back into a kind, or `None`
    /// if unrecognised.
    pub fn from_code(s: &str) -> Option<ConstraintKind> {
        Some(match s {
            "FIX" => ConstraintKind::Fixed,
            "H" => ConstraintKind::Horizontal,
            "V" => ConstraintKind::Vertical,
            "PAR" => ConstraintKind::Parallel,
            "PERP" => ConstraintKind::Perpendicular,
            "EQL" => ConstraintKind::EqualLength,
            "COI" => ConstraintKind::Coincident,
            "TAN" => ConstraintKind::Tangent,
            "RAD" => ConstraintKind::Radius,
            "LEN" => ConstraintKind::Distance,
            "LDIST" => ConstraintKind::LineDistance,
            "ANG" => ConstraintKind::Angle,
            "CONC" => ConstraintKind::Concentric,
            "COLL" => ConstraintKind::Collinear,
            "MID" => ConstraintKind::Midpoint,
            "EQR" => ConstraintKind::EqualRadius,
            "POL" => ConstraintKind::PointOnLine,
            "POC" => ConstraintKind::PointOnCircle,
            "PDIST" => ConstraintKind::PointDistance,
            "HDIST" => ConstraintKind::HDistance,
            "VDIST" => ConstraintKind::VDistance,
            "SYM" => ConstraintKind::Symmetric,
            "BLOCK" => ConstraintKind::Block,
            "PLDIST" => ConstraintKind::PointLineDistance,
            _ => return None,
        })
    }

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
}

/// Folds an angle in degrees into the canonical (0, 180]: lines are
/// undirected, so θ and θ+180° name the same relation, and 0° stores as
/// 180° so the record keeps a positive driving value (the loader rejects
/// non-positive ones). Non-finite input is left for the caller to reject.
pub fn normalize_angle_deg(deg: f64) -> f64 {
    if !deg.is_finite() {
        return deg;
    }
    let a = deg.rem_euclid(180.0);
    if a == 0.0 { 180.0 } else { a }
}

/// Anchor index in [`SketchConstraint::pts`] naming an entity's derived
/// point rather than an endpoint: a line's midpoint, an arc's center.
pub const ANCHOR_DERIVED: u8 = 2;

/// A persistent geometric relation between line entities. Pair kinds keep
/// `a` as the reference entity picked first; single kinds leave `b` empty.
/// Point-level kinds (Coincident) carry an anchor index for each entity in
/// `pts`: 0/1 name the entity's endpoints, and the special index
/// [`ANCHOR_DERIVED`] (2) names its derived point — a line's midpoint or an
/// arc/circle's center. Point entities use 0. Valued kinds (Radius,
/// Distance) carry their driving value in `val`, which identifies the
/// constraint's target, not its relation: two Radius constraints on one arc
/// are the same relation with different values.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SketchConstraint {
    /// Which relationship this constraint enforces.
    pub kind: ConstraintKind,
    /// The primary entity (the reference, picked first for pair kinds).
    pub a: EntityId,
    /// The second entity, for pair kinds; `None` for single-entity kinds.
    pub b: Option<EntityId>,
    /// A third entity for kinds that need one — Symmetric's mirror line.
    /// `None` for every other kind.
    pub c: Option<EntityId>,
    /// Per-entity anchor indices for point-level kinds (see [`ANCHOR_DERIVED`]).
    pub pts: Option<(u8, u8)>,
    /// The driving value (radius/length/width/angle) for valued kinds.
    pub val: Option<f64>,
    /// Where the user placed the dimension annotation (world coordinates),
    /// for valued kinds dimensioned interactively. `None` falls back to the
    /// automatic layout. Not part of the relation's identity.
    pub place: Option<(f64, f64)>,
}

impl SketchConstraint {
    /// A single-entity constraint (Horizontal, Vertical, Fixed, …) on `a`.
    pub fn single(kind: ConstraintKind, a: EntityId) -> Self {
        SketchConstraint {
            kind,
            a,
            b: None,
            c: None,
            pts: None,
            val: None,
            place: None,
        }
    }

    /// A two-entity relation of `kind` between `a` (reference) and `b`.
    pub fn pair(kind: ConstraintKind, a: EntityId, b: EntityId) -> Self {
        SketchConstraint {
            kind,
            a,
            b: Some(b),
            c: None,
            pts: None,
            val: None,
            place: None,
        }
    }

    /// Welds anchor `ea` of entity `a` to anchor `eb` of entity `b`.
    pub fn coincident(a: EntityId, ea: u8, b: EntityId, eb: u8) -> Self {
        SketchConstraint {
            kind: ConstraintKind::Coincident,
            a,
            b: Some(b),
            c: None,
            pts: Some((ea, eb)),
            val: None,
            place: None,
        }
    }

    /// A pair relation carrying per-entity anchor indices (Midpoint,
    /// PointOnLine, PointOnCircle, and the anchored distance kinds before
    /// their value is set).
    pub fn anchored(kind: ConstraintKind, a: EntityId, ea: u8, b: EntityId, eb: u8) -> Self {
        SketchConstraint {
            kind,
            a,
            b: Some(b),
            c: None,
            pts: Some((ea, eb)),
            val: None,
            place: None,
        }
    }

    /// A driving distance between two point anchors: straight-line
    /// (PointDistance) or axis-projected (HDistance / VDistance).
    pub fn point_distance(
        kind: ConstraintKind,
        a: EntityId,
        ea: u8,
        b: EntityId,
        eb: u8,
        value: f64,
    ) -> Self {
        SketchConstraint {
            kind,
            a,
            b: Some(b),
            c: None,
            pts: Some((ea, eb)),
            val: Some(value),
            place: None,
        }
    }

    /// A driving radius `value` on a circular-arc entity.
    pub fn radius(a: EntityId, value: f64) -> Self {
        SketchConstraint {
            kind: ConstraintKind::Radius,
            a,
            b: None,
            c: None,
            pts: None,
            val: Some(value),
            place: None,
        }
    }

    /// A driving angle between two line entities, in degrees normalized
    /// to (0, 180] (see [`ConstraintKind::Angle`]).
    pub fn angle(a: EntityId, b: EntityId, degrees: f64) -> Self {
        SketchConstraint {
            kind: ConstraintKind::Angle,
            a,
            b: Some(b),
            c: None,
            pts: None,
            val: Some(normalize_angle_deg(degrees)),
            place: None,
        }
    }

    /// A driving length on a single line entity, stored in `val`.
    pub fn distance(a: EntityId, value: f64) -> Self {
        SketchConstraint {
            kind: ConstraintKind::Distance,
            a,
            b: None,
            c: None,
            pts: None,
            val: Some(value),
            place: None,
        }
    }

    /// A driving perpendicular distance (width) between two parallel line
    /// entities; `a` is the reference, `b` slides to meet the value.
    pub fn line_distance(a: EntityId, b: EntityId, value: f64) -> Self {
        SketchConstraint {
            kind: ConstraintKind::LineDistance,
            a,
            b: Some(b),
            c: None,
            pts: None,
            val: Some(value),
            place: None,
        }
    }

    /// Permanently pins a point entity wherever it currently sits.
    pub fn fixed(a: EntityId) -> Self {
        SketchConstraint::single(ConstraintKind::Fixed, a)
    }

    /// Two point anchors mirrored about a line: `a`/`b` carry the anchors
    /// (indices in `pts`), `mirror` is the line reflected about.
    pub fn symmetric(a: EntityId, ea: u8, b: EntityId, eb: u8, mirror: EntityId) -> Self {
        SketchConstraint {
            kind: ConstraintKind::Symmetric,
            a,
            b: Some(b),
            c: Some(mirror),
            pts: Some((ea, eb)),
            val: None,
            place: None,
        }
    }

    /// One member of a blocked rigid group, held rigid relative to the
    /// group's reference entity.
    pub fn block(member: EntityId, reference: EntityId) -> Self {
        SketchConstraint::pair(ConstraintKind::Block, member, reference)
    }

    /// True when this constraint mentions entity `id` in any of its slots.
    pub fn references(&self, id: EntityId) -> bool {
        self.a == id || self.b == Some(id) || self.c == Some(id)
    }

    /// All pair kinds are symmetric relations, so a duplicate with the
    /// entities (and endpoint indices) swapped is still the same constraint.
    /// The third entity (`c`, a mirror line) plays a distinct role and is
    /// never swapped. `val` is deliberately not compared: a valued
    /// constraint on the same geometry is the same relation with a
    /// different target.
    pub fn same_relation(&self, other: &SketchConstraint) -> bool {
        if self.kind != other.kind || self.c != other.c {
            return false;
        }
        let straight = self.a == other.a && self.b == other.b && self.pts == other.pts;
        let swapped = self.b.is_some()
            && Some(self.a) == other.b
            && self.b == Some(other.a)
            && self.pts.map(|(x, y)| (y, x)) == other.pts;
        straight || swapped
    }
}

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
