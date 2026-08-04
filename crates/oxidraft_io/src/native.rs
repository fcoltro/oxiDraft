//! The native `.o2d` file format: a full-fidelity, line-oriented text
//! serialization of a [`Document`] (entities, layers, line types, dimension
//! styles, sketch constraints) — the only format that round-trips everything
//! oxiDRAFT can represent.

use oxidraft_document::{
    ANCHOR_DERIVED, Color, ConstraintKind, Document, Entity, EntityId, EntityKind, HatchPattern,
    Layer, LineTypeDef, LineTypeRef, LineWeight, SketchConstraint, Units,
};
use oxidraft_geometry::{
    CircularArc, CubicBezier, Curve, EllipticalArc, LineSeg, NurbsCurve, Point2d, PolyCurve,
    RationalBezier,
};
use std::fmt::Write as _;

const TAU: f64 = std::f64::consts::TAU;
/// The magic string every current `.o2d` file starts with.
pub const MAGIC: &str = "O2D";
/// The format's old name, from when the app was eiderFLAT — files written
/// before the rename to oxiDRAFT still start with this instead of [`MAGIC`].
/// Accepted on read so those files keep opening; never written.
const LEGACY_MAGIC: &str = "E2D";
/// The format revision written to new files; read to gate any future
/// backward-compatible parsing changes.
pub const VERSION: u32 = 1;

/// Serializes `doc` to the native `.o2d` text format.
pub fn to_string(doc: &Document) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "{MAGIC} {VERSION}");
    let _ = writeln!(s, "UNITS {}", units_name(doc.settings.units));
    let _ = writeln!(s, "GRID {}", doc.settings.grid_spacing);
    let _ = writeln!(s, "SNAP {}", doc.settings.snap_spacing);
    let ds = &doc.settings.dim_style;
    let _ = writeln!(
        s,
        "DIMSTYLE {} {} {} {}",
        ds.text_height,
        ds.arrow_size,
        ds.font.as_deref().map(esc).unwrap_or_else(|| "-".into()),
        ds.precision
    );

    for lt in &doc.line_types {
        let pat: Vec<String> = lt.pattern.iter().map(|p| p.to_string()).collect();
        let _ = writeln!(s, "LT {} {}", esc(&lt.name), pat.join(","));
    }
    for l in &doc.layers.layers {
        let _ = writeln!(
            s,
            "LAYER {} {},{},{} {} {} {} {}",
            esc(&l.name),
            l.color.0,
            l.color.1,
            l.color.2,
            l.on as u8,
            l.frozen as u8,
            l.locked as u8,
            esc(&linetype_name(&l.line_type))
        );
    }
    // Constraints reference entities by ordinal in the write order, because
    // entity ids are reassigned on load. Only entities that actually produce
    // a record get an ordinal — a skipped kind must not shift the ones after.
    let mut index = std::collections::HashMap::new();
    for e in doc.iter() {
        if write_entity(&mut s, e) {
            index.insert(e.id, index.len());
        }
    }
    for c in &doc.constraints {
        let Some(&ia) = index.get(&c.a) else { continue };
        match (c.b, c.pts) {
            (Some(b), Some((ea, eb))) => {
                let Some(&ib) = index.get(&b) else { continue };
                // Symmetric appends its mirror line's ordinal after the
                // anchors; the record is dropped when the mirror wasn't
                // serialized, same as any dangling reference.
                if c.kind == ConstraintKind::Symmetric {
                    let Some(&im) = c.c.and_then(|m| index.get(&m)) else {
                        continue;
                    };
                    let _ = writeln!(s, "C {} {} {} {} {} {}", c.kind.code(), ia, ea, ib, eb, im);
                    continue;
                }
                // Anchored valued kinds (PointDistance, HDistance,
                // VDistance) append their value and optional placement
                // after the anchor tokens.
                match (c.val, c.place) {
                    (Some(v), Some((px, py))) => {
                        let _ = writeln!(
                            s,
                            "C {} {} {} {} {} {} {} {}",
                            c.kind.code(),
                            ia,
                            ea,
                            ib,
                            eb,
                            v,
                            px,
                            py
                        );
                    }
                    (Some(v), None) => {
                        let _ =
                            writeln!(s, "C {} {} {} {} {} {}", c.kind.code(), ia, ea, ib, eb, v);
                    }
                    (None, _) => {
                        let _ = writeln!(s, "C {} {} {} {} {}", c.kind.code(), ia, ea, ib, eb);
                    }
                }
            }
            (Some(b), None) => {
                let Some(&ib) = index.get(&b) else { continue };
                // Valued pair kinds (Angle, LineDistance) append their value
                // and, when the user placed the annotation, its world point;
                // the loader reads the pair token first, then the optionals.
                match (c.val, c.place) {
                    (Some(v), Some((px, py))) => {
                        let _ =
                            writeln!(s, "C {} {} {} {} {} {}", c.kind.code(), ia, ib, v, px, py);
                    }
                    (Some(v), None) => {
                        let _ = writeln!(s, "C {} {} {} {}", c.kind.code(), ia, ib, v);
                    }
                    (None, _) => {
                        let _ = writeln!(s, "C {} {} {}", c.kind.code(), ia, ib);
                    }
                }
            }
            (None, _) => {
                // Valued single kinds (Radius, Distance) append their value
                // and optional placement.
                match (c.val, c.place) {
                    (Some(v), Some((px, py))) => {
                        let _ = writeln!(s, "C {} {} {} {} {}", c.kind.code(), ia, v, px, py);
                    }
                    (Some(v), None) => {
                        let _ = writeln!(s, "C {} {} {}", c.kind.code(), ia, v);
                    }
                    (None, _) => {
                        let _ = writeln!(s, "C {} {}", c.kind.code(), ia);
                    }
                }
            }
        }
    }
    s
}

/// Serializes `doc` and writes it to `path` via [`crate::write_atomic`].
pub fn save(doc: &Document, path: &std::path::Path) -> std::io::Result<()> {
    crate::write_atomic(path, to_string(doc).as_bytes())
}

/// Serializes one entity record; returns false for kinds the format skips.
fn write_entity(s: &mut String, e: &Entity) -> bool {
    let layer = e.layer;
    let color = color_str(&e.color);
    let extra = format!(
        "{} {}",
        esc(&linetype_name(&e.line_type)),
        line_weight_str(&e.line_weight)
    );
    match &e.kind {
        EntityKind::Curve(Curve::Line(l)) => {
            let _ = writeln!(
                s,
                "E LINE {layer} {color} {} {} {extra}",
                pt(&l.p0),
                pt(&l.p1)
            );
        }
        EntityKind::Curve(Curve::Arc(a)) => {
            let _ = writeln!(
                s,
                "E ARC {layer} {color} {};{} {} {} {} {extra}",
                rat(a.center.x),
                rat(a.center.y),
                rat(a.radius),
                a.start_angle,
                a.end_angle
            );
        }
        EntityKind::Curve(Curve::Ellipse(el)) => {
            let _ = writeln!(
                s,
                "E ELLIPSE {layer} {color} {};{} {} {} {} {} {} {extra}",
                rat(el.center.x),
                rat(el.center.y),
                rat(el.semi_major),
                rat(el.semi_minor),
                el.rotation,
                el.start_angle,
                el.end_angle
            );
        }
        EntityKind::Curve(Curve::Bezier(b)) => {
            let _ = writeln!(
                s,
                "E BEZIER {layer} {color} {} {} {} {} {extra}",
                pt(&b.p0),
                pt(&b.p1),
                pt(&b.p2),
                pt(&b.p3)
            );
        }
        EntityKind::Curve(Curve::Rational(rb)) => {
            let _ = writeln!(
                s,
                "E RATIONAL {layer} {color} {} {extra}",
                control_fields(&rb.points, &rb.weights)
            );
        }
        EntityKind::Curve(Curve::Nurbs(nc)) => {
            let _ = writeln!(
                s,
                "E NURBS {layer} {color} {} {extra}",
                control_fields(nc.control(), nc.weights())
            );
        }
        EntityKind::Curve(Curve::Poly(pc)) => {
            let _ = writeln!(s, "E POLY {layer} {color} {} {extra}", pc.segments.len());
            for seg in &pc.segments {
                write_segment(s, seg);
            }
        }
        EntityKind::Hatch {
            boundary,
            holes,
            fill,
            pattern,
        } => {
            let _ = writeln!(
                s,
                "E HATCH {layer} {color} {},{},{} {} {} {} {extra}",
                fill.0,
                fill.1,
                fill.2,
                boundary.len(),
                holes.len(),
                pattern_str(pattern)
            );
            for seg in boundary {
                write_segment(s, seg);
            }
            for hole in holes {
                let _ = writeln!(s, "HOLE {}", hole.len());
                for seg in hole {
                    write_segment(s, seg);
                }
            }
        }
        EntityKind::Point(p) => {
            let _ = writeln!(s, "E POINT {layer} {color} {} {extra}", pt(p));
        }
        EntityKind::Text {
            anchor,
            content,
            height,
            rotation,
            font,
        } => {
            let _ = writeln!(
                s,
                "E TEXT {layer} {color} {} {} {} {} {} {extra}",
                pt(anchor),
                height,
                rotation,
                esc(content),
                font.as_deref().map(esc).unwrap_or_else(|| "-".into())
            );
        }
        EntityKind::Dimension {
            p1,
            p2,
            line,
            height,
            override_text,
        } => {
            let _ = writeln!(
                s,
                "E DIM {layer} {color} {} {} {} {} {} {extra}",
                pt(p1),
                pt(p2),
                pt(line),
                height,
                dim_override(override_text)
            );
        }
        EntityKind::OrthoDim {
            p1,
            p2,
            line,
            vertical,
            height,
            override_text,
        } => {
            let _ = writeln!(
                s,
                "E DIMORTHO {layer} {color} {} {} {} {} {} {} {extra}",
                pt(p1),
                pt(p2),
                pt(line),
                *vertical as u8,
                height,
                dim_override(override_text)
            );
        }
        EntityKind::AngularDim {
            center,
            p1,
            p2,
            line,
            height,
            override_text,
        } => {
            let _ = writeln!(
                s,
                "E DIMANG {layer} {color} {} {} {} {} {} {} {extra}",
                pt(center),
                pt(p1),
                pt(p2),
                pt(line),
                height,
                dim_override(override_text)
            );
        }
        EntityKind::RadialDim {
            center,
            edge,
            diameter,
            height,
            override_text,
        } => {
            let _ = writeln!(
                s,
                "E DIMRAD {layer} {color} {} {} {} {} {} {extra}",
                pt(center),
                pt(edge),
                *diameter as u8,
                height,
                dim_override(override_text)
            );
        }
        _ => return false,
    }
    true
}

fn write_segment(s: &mut String, seg: &Curve) {
    match seg {
        Curve::Line(l) => {
            let _ = writeln!(s, "SEG LINE {} {}", pt(&l.p0), pt(&l.p1));
        }
        Curve::Arc(a) => {
            let _ = writeln!(
                s,
                "SEG ARC {};{} {} {} {}",
                rat(a.center.x),
                rat(a.center.y),
                rat(a.radius),
                a.start_angle,
                a.end_angle
            );
        }
        Curve::Bezier(b) => {
            let _ = writeln!(
                s,
                "SEG BEZIER {} {} {} {}",
                pt(&b.p0),
                pt(&b.p1),
                pt(&b.p2),
                pt(&b.p3)
            );
        }
        Curve::Rational(rb) => {
            let _ = writeln!(
                s,
                "SEG RATIONAL {}",
                control_fields(&rb.points, &rb.weights)
            );
        }
        Curve::Ellipse(e) => {
            let _ = writeln!(
                s,
                "SEG ELLIPSE {};{} {} {} {} {} {}",
                rat(e.center.x),
                rat(e.center.y),
                rat(e.semi_major),
                rat(e.semi_minor),
                e.rotation,
                e.start_angle,
                e.end_angle
            );
        }
        _ => s.push_str("SEG LINE 0;0 0;0\n"),
    }
}

fn control_fields(points: &[Point2d], weights: &[f64]) -> String {
    let mut out = points.len().to_string();
    for (p, w) in points.iter().zip(weights) {
        let _ = write!(out, " {} {}", pt(p), rat(*w));
    }
    out
}

/// Parses `.o2d` text into a [`Document`], or an error describing the first
/// malformed line.
pub fn from_string(text: &str) -> Result<Document, String> {
    let mut lines = text.lines().peekable();
    let header = lines.next().ok_or("empty file")?;
    let mut hp = header.split_whitespace();
    match hp.next() {
        Some(MAGIC) | Some(LEGACY_MAGIC) => {}
        _ => return Err("not an O2D file".into()),
    }
    let ver: u32 = hp
        .next()
        .and_then(|v| v.parse().ok())
        .ok_or("missing version")?;
    if ver > VERSION {
        return Err(format!("unsupported version {}", ver));
    }

    let mut doc = Document::new();
    doc.layers.layers.clear();
    doc.line_types.clear();
    type PendingConstraint = (
        ConstraintKind,
        usize,
        Option<usize>,
        Option<usize>,
        Option<(u8, u8)>,
        Option<f64>,
        Option<(f64, f64)>,
    );
    let mut pending_constraints: Vec<PendingConstraint> = Vec::new();
    // Constraints reference entities by ordinal in the file's write order.
    // Dropped records (corrupt values, unknown types) must still occupy their
    // slot here, or every later ordinal would shift onto the wrong entity.
    let mut entity_ids: Vec<Option<EntityId>> = Vec::new();

    while let Some(line) = lines.next() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut tok = line.split_whitespace();
        match tok.next() {
            Some("UNITS") => doc.settings.units = parse_units(tok.next().unwrap_or("")),
            Some("GRID") => doc.settings.grid_spacing = next_parse(&mut tok, 10.0),
            Some("SNAP") => doc.settings.snap_spacing = next_parse(&mut tok, 1.0),
            Some("DIMSTYLE") => {
                let ds = &mut doc.settings.dim_style;
                ds.text_height = next_parse(&mut tok, 2.5);
                ds.arrow_size = next_parse(&mut tok, 2.5);
                ds.font = parse_opt_text(&mut tok);
                // Clamped here too, not just where it is formatted: a stored
                // value of 20_000_000 is nonsense whatever reads it next, and
                // leaving it in the document keeps the panic one unguarded
                // `format!` away.
                ds.precision =
                    oxidraft_document::clamp_precision(next_parse(&mut tok, ds.precision));
            }
            Some("LT") => {
                if let Some(lt) = parse_lt(&mut tok) {
                    doc.line_types.push(lt);
                }
            }
            Some("LAYER") => {
                if let Some(l) = parse_layer(&mut tok) {
                    doc.layers.layers.push(l);
                }
            }
            Some("E") => {
                entity_ids.push(parse_entity(&mut tok, &mut lines, &mut doc));
            }
            Some("C") => {
                if let Some(kind) = tok.next().and_then(ConstraintKind::from_code)
                    && let Some(ia) = tok.next().and_then(|v| v.parse().ok())
                {
                    if kind.has_anchors() {
                        let ea: Option<u8> = tok.next().and_then(|v| v.parse().ok());
                        let ib: Option<usize> = tok.next().and_then(|v| v.parse().ok());
                        let eb: Option<u8> = tok.next().and_then(|v| v.parse().ok());
                        // Symmetric carries its mirror line's ordinal after
                        // the anchors; the anchored valued kinds
                        // (PointDistance, HDistance, VDistance) carry
                        // `val [px py]` there instead.
                        let ic: Option<usize> = if kind == ConstraintKind::Symmetric {
                            tok.next().and_then(|v| v.parse().ok())
                        } else {
                            None
                        };
                        let val: Option<f64> = tok.next().and_then(|v| v.parse().ok());
                        let px: Option<f64> = tok.next().and_then(|v| v.parse().ok());
                        let py: Option<f64> = tok.next().and_then(|v| v.parse().ok());
                        let place = match (px, py) {
                            (Some(x), Some(y)) if x.is_finite() && y.is_finite() => Some((x, y)),
                            _ => None,
                        };
                        let val_ok =
                            !kind.is_valued() || val.is_some_and(|v| v.is_finite() && v > 0.0);
                        let mirror_ok = kind != ConstraintKind::Symmetric || ic.is_some();
                        // 0/1 are endpoints; ANCHOR_DERIVED (2) is a line's
                        // midpoint or an arc's center.
                        if let (Some(ea), Some(ib), Some(eb)) = (ea, ib, eb)
                            && ea <= ANCHOR_DERIVED
                            && eb <= ANCHOR_DERIVED
                            && val_ok
                            && mirror_ok
                        {
                            pending_constraints.push((
                                kind,
                                ia,
                                Some(ib),
                                ic,
                                Some((ea, eb)),
                                if kind.is_valued() { val } else { None },
                                if kind.is_valued() { place } else { None },
                            ));
                        }
                    } else {
                        let ib = if kind.is_pair() {
                            tok.next().and_then(|v| v.parse().ok())
                        } else {
                            None
                        };
                        let val: Option<f64> = tok.next().and_then(|v| v.parse().ok());
                        // Optional annotation placement follows the value;
                        // both coordinates must parse finite or it's dropped
                        // (the record itself stays valid without one).
                        let px: Option<f64> = tok.next().and_then(|v| v.parse().ok());
                        let py: Option<f64> = tok.next().and_then(|v| v.parse().ok());
                        let place = match (px, py) {
                            (Some(x), Some(y)) if x.is_finite() && y.is_finite() => Some((x, y)),
                            _ => None,
                        };
                        // A valued kind without a sane value is corrupt.
                        let val_ok =
                            !kind.is_valued() || val.is_some_and(|v| v.is_finite() && v > 0.0);
                        if (!kind.is_pair() || ib.is_some()) && val_ok {
                            pending_constraints.push((kind, ia, ib, None, None, val, place));
                        }
                    }
                }
            }
            _ => {}
        }
    }

    // Out-of-range references (truncated or hand-edited files) and references
    // to dropped records are discarded rather than mis-attached.
    for (kind, ia, ib, ic, pts, val, place) in pending_constraints {
        let Some(&Some(a)) = entity_ids.get(ia) else {
            continue;
        };
        let b = match ib {
            Some(i) => match entity_ids.get(i) {
                Some(&Some(b)) => Some(b),
                _ => continue,
            },
            None => None,
        };
        let c = match ic {
            Some(i) => match entity_ids.get(i) {
                Some(&Some(m)) => Some(m),
                _ => continue,
            },
            None => None,
        };
        doc.add_constraint(SketchConstraint {
            kind,
            a,
            b,
            c,
            pts,
            val,
            place,
        });
    }

    if doc.layers.layers.is_empty() {
        doc.layers.layers.push(Layer::new("0"));
    }
    Ok(doc)
}

/// Reads and parses a `.o2d` file from `path`.
pub fn load(path: &std::path::Path) -> std::io::Result<Document> {
    let text = std::fs::read_to_string(path)?;
    from_string(&text).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// Parses one entity record; returns the id it was added under, or `None`
/// when the record is unknown or too corrupt to keep.
fn parse_entity<'a>(
    tok: &mut impl Iterator<Item = &'a str>,
    lines: &mut std::iter::Peekable<std::str::Lines>,
    doc: &mut Document,
) -> Option<EntityId> {
    let etype = tok.next()?;
    let layer: usize = next_parse(tok, 0);
    let color = parse_color(tok.next().unwrap_or("bylayer"));

    let kind = match etype {
        "LINE" => {
            let p0 = parse_pt(tok.next());
            let p1 = parse_pt(tok.next());
            Some(EntityKind::Curve(Curve::Line(LineSeg::from_endpoints(
                p0, p1,
            ))))
        }
        "ARC" => {
            let c = parse_pt(tok.next());
            let r = parse_num(tok.next().unwrap_or("1"));
            let start = next_parse(tok, 0.0);
            let end = next_parse(tok, TAU);
            // A corrupt radius must drop the record, not panic the app.
            CircularArc::try_new(c, r, start, end)
                .ok()
                .map(|a| EntityKind::Curve(Curve::Arc(a)))
        }
        "ELLIPSE" => {
            let c = parse_pt(tok.next());
            let major = parse_num(tok.next().unwrap_or("1"));
            let minor = parse_num(tok.next().unwrap_or("1"));
            let rot = next_parse(tok, 0.0);
            let start = next_parse(tok, 0.0);
            let end = next_parse(tok, TAU);
            // try_new also rejects non-finite rotation/angles ("inf" parses
            // as a valid f64), which the old positivity check let through.
            EllipticalArc::try_new(c, major, minor, rot, start, end)
                .ok()
                .map(|e| EntityKind::Curve(Curve::Ellipse(e)))
        }
        "BEZIER" => {
            let p0 = parse_pt(tok.next());
            let p1 = parse_pt(tok.next());
            let p2 = parse_pt(tok.next());
            let p3 = parse_pt(tok.next());
            Some(EntityKind::Curve(Curve::Bezier(CubicBezier::new(
                p0, p1, p2, p3,
            ))))
        }
        // Fallible constructors: `new` panics, and this is untrusted file
        // data. The validation above should already have caught anything the
        // kernel rejects, but the importers must not be the thing that has to
        // be right — a mismatch between the two rules is a crash on open.
        "RATIONAL" => {
            parse_control_data(tok, crate::MAX_RATIONAL_CONTROL_POINTS).and_then(|(p, w)| {
                RationalBezier::try_new(p, w)
                    .ok()
                    .map(|rb| EntityKind::Curve(Curve::Rational(rb)))
            })
        }
        "NURBS" => parse_control_data(tok, crate::MAX_NURBS_CONTROL_POINTS).and_then(|(c, w)| {
            NurbsCurve::try_new(c, w)
                .ok()
                .map(|nc| EntityKind::Curve(Curve::Nurbs(nc)))
        }),
        "POINT" => Some(EntityKind::Point(parse_pt(tok.next()))),
        "TEXT" => {
            let anchor = parse_pt(tok.next());
            let height = next_parse(tok, 1.0);
            let rotation = next_parse(tok, 0.0);
            let content = unesc(tok.next().unwrap_or(""));
            let font = parse_opt_text(tok);
            Some(EntityKind::Text {
                anchor,
                content,
                height,
                rotation,
                font,
            })
        }
        "DIM" => {
            let p1 = parse_pt(tok.next());
            let p2 = parse_pt(tok.next());
            let line = parse_pt(tok.next());
            let height = next_parse(tok, 2.5);
            let override_text = parse_opt_text(tok);
            Some(EntityKind::Dimension {
                p1,
                p2,
                line,
                height,
                override_text,
            })
        }
        "DIMORTHO" => {
            let p1 = parse_pt(tok.next());
            let p2 = parse_pt(tok.next());
            let line = parse_pt(tok.next());
            let vertical = next_flag(tok);
            let height = next_parse(tok, 2.5);
            let override_text = parse_opt_text(tok);
            Some(EntityKind::OrthoDim {
                p1,
                p2,
                line,
                vertical,
                height,
                override_text,
            })
        }
        "DIMANG" => {
            let center = parse_pt(tok.next());
            let p1 = parse_pt(tok.next());
            let p2 = parse_pt(tok.next());
            let line = parse_pt(tok.next());
            let height = next_parse(tok, 2.5);
            let override_text = parse_opt_text(tok);
            Some(EntityKind::AngularDim {
                center,
                p1,
                p2,
                line,
                height,
                override_text,
            })
        }
        "DIMRAD" => {
            let center = parse_pt(tok.next());
            let edge = parse_pt(tok.next());
            let diameter = next_flag(tok);
            let height = next_parse(tok, 2.5);
            let override_text = parse_opt_text(tok);
            Some(EntityKind::RadialDim {
                center,
                edge,
                diameter,
                height,
                override_text,
            })
        }
        "POLY" => {
            let count: usize = next_parse(tok, 0);
            let mut segs = Vec::new();
            for _ in 0..count {
                let Some(segline) = lines.next() else { break };
                if let Some(seg) = parse_segment(segline.trim()) {
                    segs.push(seg);
                }
            }
            Some(EntityKind::Curve(Curve::Poly(Box::new(PolyCurve::new(
                segs,
            )))))
        }
        "HATCH" => {
            let fill = parse_rgb_triple(tok.next().unwrap_or(""));
            let nb: usize = next_parse(tok, 0);
            let nh: usize = next_parse(tok, 0);
            let read_segs = |lines: &mut std::iter::Peekable<std::str::Lines>, n: usize| {
                let mut v = Vec::new();
                for _ in 0..n {
                    let Some(line) = lines.next() else { break };
                    if let Some(seg) = parse_segment(line.trim()) {
                        v.push(seg);
                    }
                }
                v
            };
            let pattern = parse_pattern(tok.next());
            let boundary = read_segs(lines, nb);
            let mut holes = Vec::new();
            for _ in 0..nh {
                // Stop at EOF: the declared count is untrusted, and a huge
                // value with no lines behind it must not spin the loop dry.
                let Some(line) = lines.next() else { break };
                let ns = line
                    .trim()
                    .strip_prefix("HOLE ")
                    .and_then(|c| c.trim().parse().ok())
                    .unwrap_or(0);
                holes.push(read_segs(lines, ns));
            }
            Some(EntityKind::Hatch {
                boundary,
                holes,
                fill,
                pattern,
            })
        }
        _ => None,
    };

    let line_type = parse_line_type_ref(tok.next().unwrap_or("ByLayer"));
    let line_weight = parse_line_weight(tok.next().unwrap_or("bylayer"));

    // A single NaN/inf coordinate poisons zoom-to-fit and the spatial index
    // for the whole session, so drop any entity carrying non-finite numbers —
    // the same salvage policy as out-of-range constraint references.
    let k = kind.filter(|k| k.is_finite())?;
    let id = doc.add_on_layer(k, layer.min(doc.layers.layers.len().saturating_sub(1)));
    if let Some(e) = doc.get_mut(id) {
        e.color = color;
        e.line_type = line_type;
        e.line_weight = line_weight;
    }
    Some(id)
}

fn parse_segment(line: &str) -> Option<Curve> {
    let mut tok = line.split_whitespace();
    if tok.next() != Some("SEG") {
        return None;
    }
    match tok.next()? {
        "LINE" => Some(Curve::Line(LineSeg::from_endpoints(
            parse_pt(tok.next()),
            parse_pt(tok.next()),
        ))),
        "ARC" => {
            let c = parse_pt(tok.next());
            let r = parse_num(tok.next().unwrap_or("1"));
            let start = next_parse(&mut tok, 0.0);
            let end = next_parse(&mut tok, TAU);
            CircularArc::try_new(c, r, start, end).ok().map(Curve::Arc)
        }
        "BEZIER" => Some(Curve::Bezier(CubicBezier::new(
            parse_pt(tok.next()),
            parse_pt(tok.next()),
            parse_pt(tok.next()),
            parse_pt(tok.next()),
        ))),
        "RATIONAL" => parse_control_data(&mut tok, crate::MAX_RATIONAL_CONTROL_POINTS)
            .and_then(|(p, w)| RationalBezier::try_new(p, w).ok().map(Curve::Rational)),
        "ELLIPSE" => {
            let c = parse_pt(tok.next());
            let major = parse_num(tok.next().unwrap_or("1"));
            let minor = parse_num(tok.next().unwrap_or("1"));
            let rot = next_parse(&mut tok, 0.0);
            let start = next_parse(&mut tok, 0.0);
            let end = next_parse(&mut tok, TAU);
            EllipticalArc::try_new(c, major, minor, rot, start, end)
                .ok()
                .map(Curve::Ellipse)
        }
        _ => None,
    }
}

/// Reads a `count point weight …` control block. `max` bounds how many points
/// are actually taken — it differs by curve kind, since a rational Bézier
/// costs O(n²) per evaluation while a NURBS decomposes into fixed-degree
/// segments.
fn parse_control_data<'a, I: Iterator<Item = &'a str>>(
    tok: &mut I,
    max: usize,
) -> Option<(Vec<Point2d>, Vec<f64>)> {
    let n: usize = tok.next().and_then(|v| v.parse().ok())?;
    let mut points = Vec::with_capacity(n.min(1024));
    let mut weights = Vec::with_capacity(n.min(1024));
    // Bound the *actual* points read, not the declared count: a huge declared
    // count with few real tokens still loads (the loop breaks on token
    // exhaustion), but a crafted line that really carries a pathological
    // number of points is truncated so the curve can't freeze rendering.
    for _ in 0..n.min(max) {
        let Some(p) = tok.next() else { break };
        points.push(parse_pt(Some(p)));
        weights.push(parse_num(tok.next().unwrap_or("1")));
    }
    // `w > 0.0` alone lets +inf through, and the geometry constructors reject
    // non-finite weights — so an infinite weight in a crafted file reached the
    // *panicking* constructor and crashed the app on open. Match the kernel's
    // rule exactly here.
    (points.len() >= 2
        && points.len() == weights.len()
        && weights.iter().all(|&w| w.is_finite() && w > 0.0))
    .then_some((points, weights))
}

fn rat(v: f64) -> String {
    format!("{}", v)
}
fn pt(p: &Point2d) -> String {
    format!("{};{}", rat(p.x), rat(p.y))
}

/// Pops the next token and parses it, falling back to `default` when absent or invalid.
fn next_parse<'a, T: std::str::FromStr>(tok: &mut impl Iterator<Item = &'a str>, default: T) -> T {
    tok.next().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// Pops the next token and reads it as a `1`/`0` boolean flag (defaults to false).
fn next_flag<'a>(tok: &mut impl Iterator<Item = &'a str>) -> bool {
    tok.next() == Some("1")
}

fn parse_num(s: &str) -> f64 {
    if let Some((n, d)) = s.split_once('/') {
        let n: f64 = n.parse().unwrap_or(0.0);
        let d: f64 = d.parse().unwrap_or(1.0);
        if d != 0.0 { n / d } else { 0.0 }
    } else {
        s.parse().unwrap_or(0.0)
    }
}

fn parse_rgb_triple(s: &str) -> (u8, u8, u8) {
    let p: Vec<u8> = s.split(',').filter_map(|v| v.parse().ok()).collect();
    if p.len() == 3 {
        (p[0], p[1], p[2])
    } else {
        (128, 128, 128)
    }
}

fn parse_pt(s: Option<&str>) -> Point2d {
    let s = s.unwrap_or("0;0");
    let (x, y) = s.split_once(';').unwrap_or(("0", "0"));
    Point2d::new(parse_num(x), parse_num(y))
}

fn color_str(c: &Color) -> String {
    match c {
        Color::ByLayer => "bylayer".into(),
        Color::ByBlock => "byblock".into(),
        Color::Rgb(r, g, b) => format!("rgb:{}:{}:{}", r, g, b),
    }
}
fn parse_color(s: &str) -> Color {
    match s {
        "bylayer" => Color::ByLayer,
        "byblock" => Color::ByBlock,
        other => {
            if let Some(rest) = other.strip_prefix("rgb:") {
                let p: Vec<u8> = rest.split(':').filter_map(|v| v.parse().ok()).collect();
                if p.len() == 3 {
                    return Color::Rgb(p[0], p[1], p[2]);
                }
            }
            Color::ByLayer
        }
    }
}

fn parse_layer<'a>(tok: &mut impl Iterator<Item = &'a str>) -> Option<Layer> {
    let name = unesc(tok.next()?);
    let rgb: Vec<u8> = tok
        .next()?
        .split(',')
        .filter_map(|v| v.parse().ok())
        .collect();
    let on = tok.next()? == "1";
    let frozen = tok.next()? == "1";
    let locked = tok.next()? == "1";
    let lt = unesc(tok.next().unwrap_or("Continuous"));
    let mut l = Layer::new(name);
    if rgb.len() == 3 {
        l.color = (rgb[0], rgb[1], rgb[2]);
    }
    l.on = on;
    l.frozen = frozen;
    l.locked = locked;
    l.line_type = LineTypeRef::Named(lt);
    Some(l)
}

fn parse_lt<'a>(tok: &mut impl Iterator<Item = &'a str>) -> Option<LineTypeDef> {
    let name = unesc(tok.next()?);
    let pat: Vec<f64> = tok
        .next()
        .map(|s| s.split(',').filter_map(|v| v.parse().ok()).collect())
        .unwrap_or_default();
    Some(LineTypeDef {
        name,
        description: String::new(),
        pattern: pat,
    })
}

fn linetype_name(lt: &LineTypeRef) -> String {
    match lt {
        LineTypeRef::Named(n) => n.clone(),
        LineTypeRef::ByLayer => "ByLayer".into(),
        LineTypeRef::ByBlock => "ByBlock".into(),
    }
}

fn parse_line_type_ref(s: &str) -> LineTypeRef {
    match s {
        "ByLayer" => LineTypeRef::ByLayer,
        "ByBlock" => LineTypeRef::ByBlock,
        other => LineTypeRef::Named(unesc(other)),
    }
}

fn line_weight_str(w: &LineWeight) -> String {
    match w {
        LineWeight::ByLayer => "bylayer".into(),
        LineWeight::ByBlock => "byblock".into(),
        LineWeight::Hundredths(h) => format!("h:{h}"),
    }
}

fn parse_line_weight(s: &str) -> LineWeight {
    match s {
        "byblock" => LineWeight::ByBlock,
        other => other
            .strip_prefix("h:")
            .and_then(|v| v.parse().ok())
            .map(LineWeight::Hundredths)
            .unwrap_or(LineWeight::ByLayer),
    }
}

fn units_name(u: Units) -> &'static str {
    match u {
        Units::Unitless => "Unitless",
        Units::Millimeters => "Millimeters",
        Units::Centimeters => "Centimeters",
        Units::Meters => "Meters",
        Units::Kilometers => "Kilometers",
        Units::Inches => "Inches",
        Units::Feet => "Feet",
    }
}
fn parse_units(s: &str) -> Units {
    match s {
        "Centimeters" => Units::Centimeters,
        "Meters" => Units::Meters,
        "Kilometers" => Units::Kilometers,
        "Inches" => Units::Inches,
        "Feet" => Units::Feet,
        "Unitless" => Units::Unitless,
        _ => Units::Millimeters,
    }
}

fn pattern_str(p: &HatchPattern) -> String {
    match p {
        HatchPattern::Solid => "solid".into(),
        HatchPattern::Lines { angle_deg, spacing } => format!("lines:{angle_deg}:{spacing}"),
        HatchPattern::Cross { angle_deg, spacing } => format!("cross:{angle_deg}:{spacing}"),
        HatchPattern::Dots { spacing } => format!("dots:{spacing}"),
    }
}

fn parse_pattern(tok: Option<&str>) -> HatchPattern {
    let Some(s) = tok else {
        return HatchPattern::Solid;
    };
    let mut it = s.split(':');
    match it.next() {
        Some("lines") => HatchPattern::Lines {
            angle_deg: next_parse(&mut it, 45.0),
            spacing: next_parse(&mut it, 1.0),
        },
        Some("cross") => HatchPattern::Cross {
            angle_deg: next_parse(&mut it, 45.0),
            spacing: next_parse(&mut it, 1.0),
        },
        Some("dots") => HatchPattern::Dots {
            spacing: next_parse(&mut it, 1.0),
        },
        _ => HatchPattern::Solid,
    }
}

fn esc(s: &str) -> String {
    if s.is_empty() {
        return "_".into();
    }
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            ' ' => out.push_str("\\s"),
            '\n' => out.push_str("\\n"),
            _ => out.push(c),
        }
    }
    out
}

fn dim_override(o: &Option<String>) -> String {
    match o {
        None => "-".into(),
        Some(t) => esc(t),
    }
}

/// Reads an optional escaped string written as `-` when absent (dim overrides, fonts).
fn parse_opt_text<'a>(tok: &mut impl Iterator<Item = &'a str>) -> Option<String> {
    match tok.next() {
        None | Some("-") => None,
        Some(t) => Some(unesc(t)),
    }
}
fn unesc(s: &str) -> String {
    if s == "_" {
        return String::new();
    }
    // A single left-to-right scan, consuming each backslash together with
    // exactly the character after it. Doing this as a sequence of whole-string
    // replace() passes instead (as this used to) is unsound: encoding a lone
    // backslash immediately followed by a literal 's' or 'n' doubles the
    // backslash, and a later pass matching "\s"/"\n" as a substring can then
    // span the boundary between the doubled backslash and that literal
    // letter — e.g. `esc("C:\notes.txt")` decodes back to `"C:\<NEWLINE>otes.txt"`
    // instead of the original text. Scanning once and treating "backslash +
    // next char" as an atomic unit removes the ambiguity entirely.
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('\\') => out.push('\\'),
            Some('s') => out.push(' '),
            Some('n') => out.push('\n'),
            // Not one of ours: preserve both characters rather than
            // silently dropping the backslash.
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxidraft_document::{EntityKind, HatchPattern};

    fn pt_i(x: i64, y: i64) -> Point2d {
        Point2d::from_i64(x, y)
    }

    #[test]
    fn roundtrip_f64_is_lossless() {
        let mut doc = Document::new();
        let third = 1.0 / 3.0;
        doc.add(EntityKind::Curve(Curve::Line(LineSeg::from_endpoints(
            Point2d::new(third, 0.0),
            Point2d::new(2.0, third),
        ))));

        let text = to_string(&doc);
        let doc2 = from_string(&text).unwrap();
        let es: Vec<_> = doc2.iter().collect();
        if let Some(Curve::Line(l)) = es[0].as_curve() {
            assert_eq!(l.p0.x, third);
            assert_eq!(l.p1.y, third);
        } else {
            panic!()
        }
    }

    #[test]
    fn roundtrip_constraints_remap_entity_ids() {
        let mut doc = Document::new();
        let a = doc.add(EntityKind::Curve(Curve::Line(LineSeg::from_endpoints(
            pt_i(0, 0),
            pt_i(4, 0),
        ))));
        let b = doc.add(EntityKind::Curve(Curve::Line(LineSeg::from_endpoints(
            pt_i(0, 1),
            pt_i(4, 2),
        ))));
        doc.add_constraint(SketchConstraint::single(ConstraintKind::Horizontal, a));
        doc.add_constraint(SketchConstraint::pair(ConstraintKind::Parallel, a, b));
        // Force reloaded ids to differ from the originals.
        let gap = doc.add(EntityKind::Point(pt_i(9, 9)));
        doc.remove(gap);

        let doc2 = from_string(&to_string(&doc)).unwrap();
        assert_eq!(doc2.constraints.len(), 2);
        let ids: Vec<_> = doc2.iter().map(|e| e.id).collect();
        assert_eq!(doc2.constraints[0].kind, ConstraintKind::Horizontal);
        assert_eq!(doc2.constraints[0].a, ids[0]);
        assert_eq!(doc2.constraints[0].b, None);
        assert_eq!(doc2.constraints[1].kind, ConstraintKind::Parallel);
        assert_eq!(doc2.constraints[1].a, ids[0]);
        assert_eq!(doc2.constraints[1].b, Some(ids[1]));
    }

    #[test]
    fn roundtrip_tangent_between_line_and_arc() {
        let mut doc = Document::new();
        let l = doc.add(EntityKind::Curve(Curve::Line(LineSeg::from_endpoints(
            pt_i(-4, 2),
            pt_i(4, 2),
        ))));
        let a = doc.add(EntityKind::Curve(Curve::Arc(
            oxidraft_geometry::CircularArc::new(pt_i(0, 0), 2.0, 0.0, std::f64::consts::TAU),
        )));
        doc.add_constraint(SketchConstraint::pair(ConstraintKind::Tangent, l, a));

        let doc2 = from_string(&to_string(&doc)).unwrap();
        assert_eq!(doc2.constraints.len(), 1);
        let c = doc2.constraints[0];
        assert_eq!(c.kind, ConstraintKind::Tangent);
        let ids: Vec<_> = doc2.iter().map(|e| e.id).collect();
        assert_eq!((c.a, c.b, c.pts), (ids[0], Some(ids[1]), None));
    }

    #[test]
    fn roundtrip_radius_keeps_the_driving_value() {
        let mut doc = Document::new();
        let a = doc.add(EntityKind::Curve(Curve::Arc(
            oxidraft_geometry::CircularArc::new(pt_i(0, 0), 2.5, 0.0, std::f64::consts::TAU),
        )));
        doc.add_constraint(SketchConstraint::radius(a, 2.5));

        let doc2 = from_string(&to_string(&doc)).unwrap();
        assert_eq!(doc2.constraints.len(), 1);
        let c = doc2.constraints[0];
        assert_eq!(c.kind, ConstraintKind::Radius);
        assert_eq!(c.val, Some(2.5));
        let ids: Vec<_> = doc2.iter().map(|e| e.id).collect();
        assert_eq!((c.a, c.b), (ids[0], None));
    }

    #[test]
    fn roundtrip_distance_keeps_the_driving_value() {
        let mut doc = Document::new();
        let a = doc.add(EntityKind::Curve(Curve::Line(LineSeg::from_endpoints(
            pt_i(0, 0),
            pt_i(4, 0),
        ))));
        doc.add_constraint(SketchConstraint::distance(a, 4.0));

        let doc2 = from_string(&to_string(&doc)).unwrap();
        assert_eq!(doc2.constraints.len(), 1);
        let c = doc2.constraints[0];
        assert_eq!(c.kind, ConstraintKind::Distance);
        assert_eq!(c.val, Some(4.0));
        let ids: Vec<_> = doc2.iter().map(|e| e.id).collect();
        assert_eq!((c.a, c.b), (ids[0], None));
    }

    #[test]
    fn roundtrip_new_pair_kinds_survive() {
        let mut doc = Document::new();
        let c1 = doc.add(EntityKind::Curve(Curve::Arc(
            oxidraft_geometry::CircularArc::new(pt_i(0, 0), 2.0, 0.0, std::f64::consts::TAU),
        )));
        let c2 = doc.add(EntityKind::Curve(Curve::Arc(
            oxidraft_geometry::CircularArc::new(pt_i(5, 0), 1.0, 0.0, std::f64::consts::TAU),
        )));
        let l1 = doc.add(EntityKind::Curve(Curve::Line(LineSeg::from_endpoints(
            pt_i(0, 3),
            pt_i(4, 3),
        ))));
        let l2 = doc.add(EntityKind::Curve(Curve::Line(LineSeg::from_endpoints(
            pt_i(5, 3),
            pt_i(9, 3),
        ))));
        doc.add_constraint(SketchConstraint::pair(ConstraintKind::Concentric, c1, c2));
        doc.add_constraint(SketchConstraint::pair(ConstraintKind::EqualRadius, c1, c2));
        doc.add_constraint(SketchConstraint::pair(ConstraintKind::Collinear, l1, l2));

        let doc2 = from_string(&to_string(&doc)).unwrap();
        let kinds: Vec<_> = doc2.constraints.iter().map(|c| c.kind).collect();
        assert_eq!(
            kinds,
            vec![
                ConstraintKind::Concentric,
                ConstraintKind::EqualRadius,
                ConstraintKind::Collinear
            ]
        );
    }

    #[test]
    fn roundtrip_anchored_kinds_keep_anchors_and_values() {
        let mut doc = Document::new();
        let l = doc.add(EntityKind::Curve(Curve::Line(LineSeg::from_endpoints(
            pt_i(0, 0),
            pt_i(4, 0),
        ))));
        let p = doc.add(EntityKind::Point(pt_i(2, 3)));
        let c = doc.add(EntityKind::Curve(Curve::Arc(
            oxidraft_geometry::CircularArc::new(pt_i(8, 0), 2.0, 0.0, std::f64::consts::TAU),
        )));
        doc.add_constraint(SketchConstraint::anchored(
            ConstraintKind::Midpoint,
            p,
            0,
            l,
            ANCHOR_DERIVED,
        ));
        doc.add_constraint(SketchConstraint::anchored(
            ConstraintKind::PointOnCircle,
            p,
            0,
            c,
            0,
        ));
        let mut pd =
            SketchConstraint::point_distance(ConstraintKind::PointDistance, l, 0, p, 0, 5.5);
        pd.place = Some((1.5, 2.5));
        doc.add_constraint(pd);
        doc.add_constraint(SketchConstraint::point_distance(
            ConstraintKind::HDistance,
            l,
            1,
            p,
            0,
            2.25,
        ));

        let doc2 = from_string(&to_string(&doc)).unwrap();
        assert_eq!(doc2.constraints.len(), 4);
        let ids: Vec<_> = doc2.iter().map(|e| e.id).collect();
        let mid = doc2.constraints[0];
        assert_eq!(
            (mid.kind, mid.a, mid.b, mid.pts),
            (
                ConstraintKind::Midpoint,
                ids[1],
                Some(ids[0]),
                Some((0, ANCHOR_DERIVED))
            )
        );
        let poc = doc2.constraints[1];
        assert_eq!(
            (poc.kind, poc.a, poc.b, poc.pts),
            (
                ConstraintKind::PointOnCircle,
                ids[1],
                Some(ids[2]),
                Some((0, 0))
            )
        );
        let pd = doc2.constraints[2];
        assert_eq!(
            (pd.kind, pd.val, pd.place, pd.pts),
            (
                ConstraintKind::PointDistance,
                Some(5.5),
                Some((1.5, 2.5)),
                Some((0, 0))
            )
        );
        let hd = doc2.constraints[3];
        assert_eq!(
            (hd.kind, hd.val, hd.pts),
            (ConstraintKind::HDistance, Some(2.25), Some((1, 0)))
        );
    }

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
        let mut c =
            SketchConstraint::point_distance(ConstraintKind::PointLineDistance, p, 0, l, 0, 4.0);
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

    #[test]
    fn roundtrip_symmetric_keeps_its_mirror_line() {
        let mut doc = Document::new();
        let mirror = doc.add(EntityKind::Curve(Curve::Line(LineSeg::from_endpoints(
            pt_i(0, 0),
            pt_i(10, 0),
        ))));
        let p1 = doc.add(EntityKind::Point(pt_i(3, 2)));
        let p2 = doc.add(EntityKind::Point(pt_i(3, -2)));
        doc.add_constraint(SketchConstraint::symmetric(p1, 0, p2, 0, mirror));

        let doc2 = from_string(&to_string(&doc)).unwrap();
        assert_eq!(doc2.constraints.len(), 1);
        let c = doc2.constraints[0];
        let ids: Vec<_> = doc2.iter().map(|e| e.id).collect();
        assert_eq!(
            (c.kind, c.a, c.b, c.c, c.pts),
            (
                ConstraintKind::Symmetric,
                ids[1],
                Some(ids[2]),
                Some(ids[0]),
                Some((0, 0))
            )
        );
    }

    #[test]
    fn roundtrip_block_keeps_its_member_records() {
        let mut doc = Document::new();
        let a = doc.add(EntityKind::Curve(Curve::Line(LineSeg::from_endpoints(
            pt_i(0, 0),
            pt_i(4, 0),
        ))));
        let b = doc.add(EntityKind::Curve(Curve::Line(LineSeg::from_endpoints(
            pt_i(4, 0),
            pt_i(4, 3),
        ))));
        doc.add_constraint(SketchConstraint::block(b, a));

        let doc2 = from_string(&to_string(&doc)).unwrap();
        assert_eq!(doc2.constraints.len(), 1);
        let c = doc2.constraints[0];
        let ids: Vec<_> = doc2.iter().map(|e| e.id).collect();
        assert_eq!(
            (c.kind, c.a, c.b),
            (ConstraintKind::Block, ids[1], Some(ids[0]))
        );
    }

    #[test]
    fn roundtrip_angle_keeps_the_driving_value_and_both_lines() {
        let mut doc = Document::new();
        let a = doc.add(EntityKind::Curve(Curve::Line(LineSeg::from_endpoints(
            pt_i(0, 0),
            pt_i(4, 0),
        ))));
        let b = doc.add(EntityKind::Curve(Curve::Line(LineSeg::from_endpoints(
            pt_i(0, 1),
            pt_i(3, 4),
        ))));
        doc.add_constraint(SketchConstraint::angle(a, b, 72.5));

        let doc2 = from_string(&to_string(&doc)).unwrap();
        assert_eq!(doc2.constraints.len(), 1);
        let c = doc2.constraints[0];
        let ids: Vec<_> = doc2.iter().map(|e| e.id).collect();
        assert_eq!(
            (c.kind, c.a, c.b, c.val),
            (ConstraintKind::Angle, ids[0], Some(ids[1]), Some(72.5))
        );
    }

    #[test]
    fn roundtrip_line_distance_keeps_value_placement_and_both_lines() {
        let mut doc = Document::new();
        let a = doc.add(EntityKind::Curve(Curve::Line(LineSeg::from_endpoints(
            pt_i(0, 0),
            pt_i(4, 0),
        ))));
        let b = doc.add(EntityKind::Curve(Curve::Line(LineSeg::from_endpoints(
            pt_i(0, 3),
            pt_i(4, 3),
        ))));
        let mut c = SketchConstraint::line_distance(a, b, 3.0);
        c.place = Some((2.0, 1.5));
        doc.add_constraint(c);

        let doc2 = from_string(&to_string(&doc)).unwrap();
        assert_eq!(doc2.constraints.len(), 1);
        let c = doc2.constraints[0];
        let ids: Vec<_> = doc2.iter().map(|e| e.id).collect();
        assert_eq!(
            (c.kind, c.a, c.b, c.val),
            (
                ConstraintKind::LineDistance,
                ids[0],
                Some(ids[1]),
                Some(3.0)
            )
        );
        assert_eq!(c.place, Some((2.0, 1.5)));
    }

    #[test]
    fn roundtrip_single_kind_placement_survives_and_absence_stays_none() {
        let mut doc = Document::new();
        let a = doc.add(EntityKind::Curve(Curve::Line(LineSeg::from_endpoints(
            pt_i(0, 0),
            pt_i(4, 0),
        ))));
        let mut placed = SketchConstraint::distance(a, 4.0);
        placed.place = Some((1.0, -2.5));
        doc.add_constraint(placed);

        let doc2 = from_string(&to_string(&doc)).unwrap();
        assert_eq!(doc2.constraints.len(), 1);
        assert_eq!(doc2.constraints[0].place, Some((1.0, -2.5)));

        // Un-placed records must round-trip with `place` still None so the
        // automatic layout keeps handling them.
        let mut doc = Document::new();
        let a = doc.add(EntityKind::Curve(Curve::Line(LineSeg::from_endpoints(
            pt_i(0, 0),
            pt_i(4, 0),
        ))));
        doc.add_constraint(SketchConstraint::distance(a, 4.0));
        let doc2 = from_string(&to_string(&doc)).unwrap();
        assert_eq!(doc2.constraints[0].place, None);
    }

    #[test]
    fn bad_placement_tokens_are_dropped_but_the_record_survives() {
        let mut doc = Document::new();
        doc.add(EntityKind::Curve(Curve::Line(LineSeg::from_endpoints(
            pt_i(0, 0),
            pt_i(4, 0),
        ))));
        let base = to_string(&doc);
        // A lone x, a non-numeric y, and a non-finite coordinate all fail to
        // form a placement; the constraint itself must still load.
        for bad in ["C LEN 0 4 1.0", "C LEN 0 4 1.0 nope", "C LEN 0 4 inf 2.0"] {
            let doc2 = from_string(&format!("{base}{bad}\n")).unwrap();
            assert_eq!(
                doc2.constraints.len(),
                1,
                "record {bad:?} must survive without a placement"
            );
            assert_eq!(doc2.constraints[0].val, Some(4.0));
            assert_eq!(doc2.constraints[0].place, None, "for {bad:?}");
        }
    }

    #[test]
    fn line_distance_records_without_a_sane_value_are_dropped() {
        let mut doc = Document::new();
        doc.add(EntityKind::Curve(Curve::Line(LineSeg::from_endpoints(
            pt_i(0, 0),
            pt_i(4, 0),
        ))));
        doc.add(EntityKind::Curve(Curve::Line(LineSeg::from_endpoints(
            pt_i(0, 3),
            pt_i(4, 3),
        ))));
        let base = to_string(&doc);
        for bad in ["C LDIST 0 1", "C LDIST 0 1 -3", "C LDIST 0 1 nope"] {
            let doc2 = from_string(&format!("{base}{bad}\n")).unwrap();
            assert_eq!(doc2.len(), 2, "both lines still load");
            assert!(
                doc2.constraints.is_empty(),
                "corrupt line-distance record {bad:?} must be dropped"
            );
        }
    }

    #[test]
    fn radius_records_without_a_sane_value_are_dropped() {
        let mut doc = Document::new();
        doc.add(EntityKind::Curve(Curve::Arc(
            oxidraft_geometry::CircularArc::new(pt_i(0, 0), 2.0, 0.0, std::f64::consts::TAU),
        )));
        let base = to_string(&doc);
        for bad in ["C RAD 0", "C RAD 0 -1", "C RAD 0 nope", "C RAD 0 inf"] {
            let doc2 = from_string(&format!("{base}{bad}\n")).unwrap();
            assert_eq!(doc2.len(), 1, "the arc itself still loads");
            assert!(
                doc2.constraints.is_empty(),
                "corrupt radius record {bad:?} must be dropped"
            );
        }
    }

    #[test]
    fn roundtrip_coincident_keeps_endpoint_indices() {
        let mut doc = Document::new();
        let a = doc.add(EntityKind::Curve(Curve::Line(LineSeg::from_endpoints(
            pt_i(0, 0),
            pt_i(4, 0),
        ))));
        let b = doc.add(EntityKind::Curve(Curve::Line(LineSeg::from_endpoints(
            pt_i(4, 0),
            pt_i(4, 3),
        ))));
        doc.add_constraint(SketchConstraint::coincident(a, 1, b, 0));

        let doc2 = from_string(&to_string(&doc)).unwrap();
        assert_eq!(doc2.constraints.len(), 1);
        let c = doc2.constraints[0];
        assert_eq!(c.kind, ConstraintKind::Coincident);
        assert_eq!(c.pts, Some((1, 0)));
        let ids: Vec<_> = doc2.iter().map(|e| e.id).collect();
        assert_eq!(c.a, ids[0]);
        assert_eq!(c.b, Some(ids[1]));
    }

    #[test]
    fn roundtrip_coincident_keeps_a_derived_anchor() {
        // A midpoint weld: the origin point welded to the middle of a line
        // (anchor index 2). The index must survive the trip; 3+ is corrupt.
        let mut doc = Document::new();
        let p = doc.add(EntityKind::Point(pt_i(0, 0)));
        let l = doc.add(EntityKind::Curve(Curve::Line(LineSeg::from_endpoints(
            pt_i(-2, 3),
            pt_i(2, 3),
        ))));
        doc.add_constraint(SketchConstraint::coincident(p, 0, l, ANCHOR_DERIVED));

        let doc2 = from_string(&to_string(&doc)).unwrap();
        assert_eq!(doc2.constraints.len(), 1);
        assert_eq!(doc2.constraints[0].pts, Some((0, ANCHOR_DERIVED)));

        let text = to_string(&doc);
        let bad = text.replace(&format!("C COI 0 0 1 {ANCHOR_DERIVED}"), "C COI 0 0 1 3");
        assert_ne!(text, bad, "the record was found and rewritten");
        let doc3 = from_string(&bad).unwrap();
        assert!(
            doc3.constraints.is_empty(),
            "anchor index 3 does not exist and must be dropped"
        );
    }

    #[test]
    fn constraints_with_bad_ordinals_are_dropped() {
        let text =
            "O2D 1\nE LINE 0 bylayer 0;0 4;0 ByLayer bylayer\nC H 0\nC PAR 0 7\nC EQL 1\nC WAT 0\n";
        let doc = from_string(text).unwrap();
        assert_eq!(doc.constraints.len(), 1, "only the valid H survives");
        assert_eq!(doc.constraints[0].kind, ConstraintKind::Horizontal);
    }

    #[test]
    fn roundtrip_layers_and_settings() {
        let mut doc = Document::new();
        doc.settings.units = Units::Inches;
        doc.layers.add(Layer::new("walls").with_color(255, 0, 0));
        let mut frozen = Layer::new("hidden");
        frozen.frozen = true;
        doc.layers.add(frozen);

        let doc2 = from_string(&to_string(&doc)).unwrap();
        assert_eq!(doc2.settings.units, Units::Inches);
        let w = doc2
            .layers
            .get(doc2.layers.index_of("walls").unwrap())
            .unwrap();
        assert_eq!(w.color, (255, 0, 0));
        assert!(
            doc2.layers
                .get(doc2.layers.index_of("hidden").unwrap())
                .unwrap()
                .frozen
        );
    }

    #[test]
    fn roundtrip_all_entity_types() {
        let mut doc = Document::new();
        doc.add(EntityKind::Curve(Curve::Line(LineSeg::from_endpoints(
            pt_i(0, 0),
            pt_i(5, 5),
        ))));
        doc.add(EntityKind::Curve(Curve::Arc(CircularArc::new(
            pt_i(3, 4),
            5.0,
            0.0,
            TAU,
        ))));
        doc.add(EntityKind::Curve(Curve::Bezier(CubicBezier::new(
            pt_i(0, 0),
            pt_i(1, 2),
            pt_i(3, 2),
            pt_i(4, 0),
        ))));
        doc.add(EntityKind::Point(pt_i(7, 8)));
        doc.add(EntityKind::Text {
            anchor: pt_i(1, 1),
            content: "hello world".into(),
            height: 2.5,
            rotation: 0.0,
            font: Some("Arial".into()),
        });

        let doc2 = from_string(&to_string(&doc)).unwrap();
        assert_eq!(doc2.len(), 5);
        let has_text = doc2.iter().any(
            |e| matches!(&e.kind, EntityKind::Text { content, .. } if content == "hello world"),
        );
        assert!(has_text);
    }

    #[test]
    fn roundtrip_multiline_text_is_not_truncated() {
        let mut doc = Document::new();
        doc.add(EntityKind::Curve(Curve::Line(LineSeg::from_endpoints(
            pt_i(0, 0),
            pt_i(1, 1),
        ))));
        let id = doc.add(EntityKind::Text {
            anchor: pt_i(1, 1),
            content: "line one\nline two\nline three".into(),
            height: 2.5,
            rotation: 0.0,
            font: None,
        });

        let doc2 = from_string(&to_string(&doc)).unwrap();
        assert_eq!(
            doc2.len(),
            2,
            "the second physical line must not be dropped as junk"
        );
        let text = doc2.get(id).unwrap();
        assert!(
            matches!(&text.kind, EntityKind::Text { content, .. } if content == "line one\nline two\nline three")
        );
    }

    #[test]
    fn ellipse_with_non_finite_rotation_is_dropped_not_stored() {
        // "inf" parses as a valid f64, so the old positivity-only guard let a
        // non-finite rotation through; the stored ellipse then evaluated to
        // NaN and poisoned every bounding box union it touched.
        let text = "O2D 1\nLAYER 0 0,0,0 1 0 0 Continuous\n\
                    E ELLIPSE 0 bylayer 0;0 5 3 inf 0 6.28 - -\n\
                    E LINE 0 bylayer 0;0 5;5 - -\n";
        let doc = from_string(text).unwrap();
        assert_eq!(
            doc.len(),
            1,
            "the junk ellipse is dropped, the line survives"
        );
        assert!(
            doc.iter()
                .all(|e| matches!(&e.kind, EntityKind::Curve(Curve::Line(_))))
        );
    }

    #[test]
    fn esc_unesc_roundtrips_a_literal_backslash_next_to_an_escape_letter() {
        // A whole-string-replace-chain decoder can misfire on content like
        // this: encoding the lone backslash doubles it, and a later pass
        // matching "\s"/"\n" as a substring can span the boundary between
        // that doubled backslash and the literal letter that follows.
        for text in [
            "C:\\notes.txt",
            "a\\sb",
            "use \\n in regex",
            "\\\\already escaped",
        ] {
            assert_eq!(unesc(&esc(text)), text, "roundtrip of {text:?}");
        }
    }

    #[test]
    fn roundtrip_entity_line_type_and_line_weight() {
        let mut doc = Document::new();
        let id = doc.add(EntityKind::Curve(Curve::Line(LineSeg::from_endpoints(
            pt_i(0, 0),
            pt_i(5, 5),
        ))));
        {
            let e = doc.get_mut(id).unwrap();
            e.line_type = LineTypeRef::Named("Dashed".into());
            e.line_weight = LineWeight::Hundredths(50);
        }
        let doc2 = from_string(&to_string(&doc)).unwrap();
        let e2 = doc2.get(id).unwrap();
        assert_eq!(e2.line_type, LineTypeRef::Named("Dashed".into()));
        assert_eq!(e2.line_weight, LineWeight::Hundredths(50));
    }

    #[test]
    fn old_format_line_without_trailing_fields_defaults_to_bylayer() {
        let dxf_free = "O2D 1\nLAYER 0 0,0,0 1 0 0 Continuous\nE LINE 0 bylayer 0;0 5;5\n";
        let doc = from_string(dxf_free).unwrap();
        let e = doc.iter().next().unwrap();
        assert_eq!(e.line_type, LineTypeRef::ByLayer);
        assert_eq!(e.line_weight, LineWeight::ByLayer);
    }

    #[test]
    fn roundtrip_dim_style() {
        let mut doc = Document::new();
        doc.settings.dim_style.text_height = 3.5;
        doc.settings.dim_style.arrow_size = 1.25;
        doc.settings.dim_style.font = Some("Arial".into());
        let doc2 = from_string(&to_string(&doc)).unwrap();
        assert!((doc2.settings.dim_style.text_height - 3.5).abs() < 1e-9);
        assert!((doc2.settings.dim_style.arrow_size - 1.25).abs() < 1e-9);
        assert_eq!(doc2.settings.dim_style.font.as_deref(), Some("Arial"));
    }

    #[test]
    fn roundtrip_dimension() {
        let mut doc = Document::new();
        doc.add(EntityKind::Dimension {
            p1: pt_i(0, 0),
            p2: pt_i(10, 0),
            line: pt_i(0, 3),
            height: 2.5,
            override_text: None,
        });
        let doc2 = from_string(&to_string(&doc)).unwrap();
        let e = doc2.iter().next().expect("one entity");
        match &e.kind {
            EntityKind::Dimension {
                p1,
                p2,
                line,
                height,
                ..
            } => {
                assert_eq!(*p1, pt_i(0, 0));
                assert_eq!(*p2, pt_i(10, 0));
                assert_eq!(*line, pt_i(0, 3));
                assert!((*height - 2.5).abs() < 1e-9);
            }
            other => panic!("expected a Dimension, got {other:?}"),
        }
    }

    #[test]
    fn roundtrip_angular_and_radial_dimensions() {
        let mut doc = Document::new();
        doc.add(EntityKind::AngularDim {
            center: pt_i(0, 0),
            p1: pt_i(10, 0),
            p2: pt_i(0, 10),
            line: pt_i(5, 5),
            height: 2.5,
            override_text: None,
        });
        doc.add(EntityKind::RadialDim {
            center: pt_i(3, 4),
            edge: pt_i(8, 4),
            diameter: true,
            height: 2.5,
            override_text: None,
        });
        let doc2 = from_string(&to_string(&doc)).unwrap();
        assert_eq!(doc2.len(), 2);
        let ang = doc2.iter().any(
            |e| matches!(&e.kind, EntityKind::AngularDim { center, .. } if *center == pt_i(0, 0)),
        );
        let rad = doc2.iter().any(
            |e| matches!(&e.kind, EntityKind::RadialDim { diameter, edge, .. } if *diameter && *edge == pt_i(8, 4)),
        );
        assert!(ang, "angular dimension survived the round-trip");
        assert!(rad, "radial (diameter) dimension survived the round-trip");
    }

    #[test]
    fn roundtrip_dimension_text_override() {
        let mut doc = Document::new();
        doc.add(EntityKind::Dimension {
            p1: pt_i(0, 0),
            p2: pt_i(10, 0),
            line: pt_i(0, 3),
            height: 2.5,
            override_text: Some("≈ 10 cm".into()),
        });
        let doc2 = from_string(&to_string(&doc)).unwrap();
        let ovr = doc2.iter().find_map(|e| match &e.kind {
            EntityKind::Dimension { override_text, .. } => override_text.clone(),
            _ => None,
        });
        assert_eq!(
            ovr.as_deref(),
            Some("≈ 10 cm"),
            "override text survives with spaces"
        );
    }

    #[test]
    fn roundtrip_dimstyle_precision() {
        let mut doc = Document::new();
        doc.settings.dim_style.precision = 4;
        let doc2 = from_string(&to_string(&doc)).unwrap();
        assert_eq!(doc2.settings.dim_style.precision, 4);
    }

    #[test]
    fn roundtrip_rational_is_lossless() {
        let mut doc = Document::new();
        let rb = RationalBezier::new(
            vec![pt_i(0, 0), pt_i(2, 4), pt_i(6, 4), pt_i(8, 0)],
            vec![1.0, 2.0, 0.5, 1.0],
        );
        doc.add(EntityKind::Curve(Curve::Rational(rb.clone())));

        let doc2 = from_string(&to_string(&doc)).unwrap();
        let e = doc2.iter().next().expect("one entity");
        if let EntityKind::Curve(Curve::Rational(r2)) = &e.kind {
            assert_eq!(r2.points, rb.points, "control points must survive exactly");
            assert_eq!(r2.weights, rb.weights, "weights must survive exactly");
        } else {
            panic!("expected a Rational curve after round-trip");
        }
    }

    #[test]
    fn control_point_count_is_capped() {
        // Rationals get a far tighter cap than NURBS (asserted at compile time
        // beside the definitions): a rational Bézier is evaluated as one piece
        // and is O(n²) per evaluate, while a NURBS decomposes into
        // fixed-degree cubic segments.
        for cap in [
            crate::MAX_RATIONAL_CONTROL_POINTS,
            crate::MAX_NURBS_CONTROL_POINTS,
        ] {
            // A crafted line really carrying far more points than the cap is
            // truncated, so a pathological curve can't be built.
            let mut toks: Vec<String> = vec![(cap + 500).to_string()];
            for _ in 0..(cap + 500) {
                toks.push("1;1".into());
                toks.push("1".into());
            }
            let refs: Vec<&str> = toks.iter().map(String::as_str).collect();
            let (points, _) =
                parse_control_data(&mut refs.into_iter(), cap).expect("still loads, bounded");
            assert!(
                points.len() <= cap,
                "control points must be capped at {cap}: got {}",
                points.len()
            );
        }

        // A normal control set (count, then point/weight pairs) still parses.
        let tokens = ["4", "0;0", "1", "2;4", "1", "6;4", "1", "8;0", "1"];
        let mut ok = tokens.into_iter();
        assert!(parse_control_data(&mut ok, crate::MAX_RATIONAL_CONTROL_POINTS).is_some());
    }

    #[test]
    fn roundtrip_polycurve_of_rational_segments() {
        let seg = || {
            RationalBezier::new(
                vec![pt_i(0, 0), pt_i(1, 2), pt_i(3, 2), pt_i(4, 0)],
                vec![1.0, 1.0, 1.0, 1.0],
            )
        };
        let mut doc = Document::new();
        doc.add(EntityKind::Curve(Curve::Poly(Box::new(PolyCurve::new(
            vec![Curve::Rational(seg()), Curve::Rational(seg())],
        )))));

        let doc2 = from_string(&to_string(&doc)).unwrap();
        let e = doc2.iter().next().expect("one entity");
        if let EntityKind::Curve(Curve::Poly(pc)) = &e.kind {
            assert_eq!(pc.segments.len(), 2);
            assert!(pc.segments.iter().all(|s| matches!(s, Curve::Rational(_))));
        } else {
            panic!("expected a PolyCurve of rational segments");
        }
    }

    #[test]
    fn roundtrip_nurbs_is_lossless() {
        let mut doc = Document::new();
        let nc = NurbsCurve::new(
            vec![pt_i(0, 0), pt_i(2, 5), pt_i(6, 5), pt_i(9, 0), pt_i(12, 4)],
            vec![1.0, 2.0, 0.5, 1.0, 3.0],
        );
        doc.add(EntityKind::Curve(Curve::Nurbs(nc.clone())));

        let doc2 = from_string(&to_string(&doc)).unwrap();
        let e = doc2.iter().next().expect("one entity");
        if let EntityKind::Curve(Curve::Nurbs(n2)) = &e.kind {
            assert_eq!(
                n2.control(),
                nc.control(),
                "control vertices must survive exactly"
            );
            assert_eq!(n2.weights(), nc.weights(), "weights must survive exactly");
        } else {
            panic!("expected a NURBS curve after round-trip");
        }
    }

    #[test]
    fn roundtrip_polycurve() {
        let mut doc = Document::new();
        let segs = vec![
            Curve::Line(LineSeg::from_endpoints(pt_i(0, 0), pt_i(4, 0))),
            Curve::Arc(CircularArc::new(
                pt_i(4, 2),
                2.0,
                -std::f64::consts::FRAC_PI_2,
                std::f64::consts::FRAC_PI_2,
            )),
        ];
        doc.add(EntityKind::Curve(Curve::Poly(Box::new(PolyCurve::new(
            segs,
        )))));
        let doc2 = from_string(&to_string(&doc)).unwrap();
        let es: Vec<_> = doc2.iter().collect();
        if let Some(Curve::Poly(pc)) = es[0].as_curve() {
            assert_eq!(pc.segments.len(), 2);
        } else {
            panic!()
        }
    }

    #[test]
    fn roundtrip_hatch() {
        let mut doc = Document::new();
        doc.add(EntityKind::Hatch {
            boundary: vec![
                Curve::Line(LineSeg::from_endpoints(pt_i(0, 0), pt_i(6, 0))),
                Curve::Line(LineSeg::from_endpoints(pt_i(6, 0), pt_i(6, 6))),
                Curve::Line(LineSeg::from_endpoints(pt_i(6, 6), pt_i(0, 6))),
                Curve::Line(LineSeg::from_endpoints(pt_i(0, 6), pt_i(0, 0))),
            ],
            holes: vec![vec![
                Curve::Line(LineSeg::from_endpoints(pt_i(2, 2), pt_i(4, 2))),
                Curve::Line(LineSeg::from_endpoints(pt_i(4, 2), pt_i(4, 4))),
                Curve::Line(LineSeg::from_endpoints(pt_i(4, 4), pt_i(2, 4))),
                Curve::Line(LineSeg::from_endpoints(pt_i(2, 4), pt_i(2, 2))),
            ]],
            fill: (10, 200, 150),
            pattern: HatchPattern::Lines {
                angle_deg: 30.0,
                spacing: 1.5,
            },
        });
        let doc2 = from_string(&to_string(&doc)).unwrap();
        let es: Vec<_> = doc2.iter().collect();
        match &es[0].kind {
            EntityKind::Hatch {
                boundary,
                holes,
                fill,
                pattern,
            } => {
                assert_eq!(boundary.len(), 4);
                assert_eq!(holes.len(), 1);
                assert_eq!(holes[0].len(), 4);
                assert_eq!(*fill, (10, 200, 150));
                assert_eq!(
                    *pattern,
                    HatchPattern::Lines {
                        angle_deg: 30.0,
                        spacing: 1.5
                    }
                );
            }
            other => panic!("expected Hatch, got {other:?}"),
        }
    }

    #[test]
    fn a_poisoned_ellipse_cannot_blank_the_whole_export() {
        // The ellipse's angles are individually finite but span infinity, so
        // no per-field check caught it. Its bounding box then came back as the
        // whole plane, document extents went infinite, and the SVG/PDF export
        // of the *valid* line alongside it came out blank — export_pdf even
        // returned Ok, so nothing warned the user.
        let doc = from_string(&format!(
            "{MAGIC} {VERSION}\n\
             E LINE 0 bylayer 0;0 10;10 ByLayer bylayer\n\
             E ELLIPSE 0 bylayer 0;0 2 1 0 -1e308 1e308\n"
        ))
        .expect("the good entity must still load");
        assert_eq!(doc.len(), 1, "the poisoned ellipse must be dropped");

        let bb = doc.extents().expect("the line gives real extents");
        assert!(
            bb.min.x.is_finite() && bb.max.x.is_finite(),
            "extents must stay finite: {bb:?}"
        );
        let svg = crate::export_svg(&doc);
        assert!(
            !svg.contains("inf") && !svg.contains("NaN"),
            "export must not carry inf/NaN"
        );
    }

    #[test]
    fn crafted_arc_spans_are_dropped_on_import() {
        // Both angles are individually finite, so no per-field check catches
        // these. The first describes an infinite sweep; the second a finite
        // but unrepresentable one, which used to saturate the flattening
        // vertex cap at every tolerance — a 300 KB file of these expanded to
        // ~10 GB of cached geometry on open, and tessellated to 65k NaN
        // vertices that the SVG exporter wrote out verbatim.
        for rec in [
            "E ARC 0 bylayer 0;0 2 -1e308 1e308",
            "E ARC 0 bylayer 0;0 2 1e308 -1e308",
            "E ARC 0 bylayer 0;0 2 0 1e300",
        ] {
            let doc = from_string(&format!("{MAGIC} {VERSION}\n{rec}\n"))
                .expect("a bad arc must not fail the whole load");
            assert_eq!(doc.len(), 0, "{rec} must be dropped");
        }
        // Ordinary arcs, full circles and multi-turn spans still load.
        for rec in [
            "E ARC 0 bylayer 0;0 2 0 1.5708",
            "E ARC 0 bylayer 0;0 2 0 6.28318530717958",
            "E ARC 0 bylayer 0;0 2 0 25.1327412287183",
        ] {
            let doc = from_string(&format!("{MAGIC} {VERSION}\n{rec}\n")).unwrap();
            assert_eq!(doc.len(), 1, "{rec} must still load");
        }
    }

    #[test]
    fn crafted_weights_are_rejected_not_panicked() {
        // `w > 0.0` admits +inf, and the geometry constructors reject
        // non-finite weights — so an infinite weight in a one-line crafted
        // file reached the *panicking* constructor and crashed the app on
        // open. Every weight the kernel refuses must be dropped here instead.
        for bad in ["inf", "-inf", "NaN", "0", "-1"] {
            let src = format!("{MAGIC} {VERSION}\nE RATIONAL 0 bylayer 3 0;0 1 1;1 {bad} 2;0 1\n");
            let doc = from_string(&src).expect("a bad weight must not fail the whole load");
            assert_eq!(doc.len(), 0, "weight {bad} must be dropped, not accepted");
        }
        // A well-formed rational still loads.
        let ok = format!("{MAGIC} {VERSION}\nE RATIONAL 0 bylayer 3 0;0 1 1;1 2 2;0 1\n");
        assert_eq!(from_string(&ok).unwrap().len(), 1);
    }

    #[test]
    fn corrupt_counts_do_not_hang_or_oom() {
        let nurbs = format!(
            "{} {}\nE NURBS 0 bylayer 100000000 0;0 1 5;5 1\n",
            MAGIC, VERSION
        );
        let doc = from_string(&nurbs).expect("loads without hanging");
        assert_eq!(doc.len(), 1);

        let poly = format!(
            "{} {}\nE POLY 0 bylayer 100000000\nSEG LINE 0;0 4;0\n",
            MAGIC, VERSION
        );
        let doc = from_string(&poly).expect("loads without hanging");
        assert_eq!(doc.len(), 1);
    }

    #[test]
    fn rejects_bad_header() {
        assert!(from_string("NOPE 1\n").is_err());
        assert!(from_string("").is_err());
    }

    #[test]
    fn accepts_legacy_e2d_magic() {
        // Files saved before the rename to oxiDRAFT (extension .e2d, magic
        // "E2D") must keep opening under the new "O2D" magic/.o2d extension.
        let doc = from_string("E2D 1\nE LINE 0 bylayer 0;0 4;0 ByLayer bylayer\n")
            .expect("legacy E2D header should still load");
        assert_eq!(doc.len(), 1);
    }

    #[test]
    fn save_and_load_file_atomic() {
        let mut doc = Document::new();
        doc.add(EntityKind::Curve(Curve::Line(LineSeg::from_endpoints(
            pt_i(1, 2),
            pt_i(3, 4),
        ))));
        let dir = std::env::temp_dir();
        let path = dir.join("oxidraft_native_test.o2d");
        save(&doc, &path).unwrap();
        assert!(path.exists());
        let doc2 = load(&path).unwrap();
        assert_eq!(doc2.len(), 1);
        let _ = std::fs::remove_file(&path);
    }
}
