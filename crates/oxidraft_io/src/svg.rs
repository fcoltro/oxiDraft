//! SVG import/export: renders a [`Document`] to SVG markup (curves,
//! dimensions, hatch fills, colors) and reads a constrained subset of SVG
//! (paths, lines, circles, ellipses) back into one.

use oxidraft_document::{Color, Document, EntityKind};
use oxidraft_geometry::{
    CircularArc, CubicBezier, Curve, CurveSegment, EllipticalArc, LineSeg, Point2d, PolyCurve,
    Transform2d,
};
use std::fmt::Write as _;

const TAU: f64 = std::f64::consts::TAU;

/// An exported drawing SVG together with the world→SVG mapping the
/// exporter used. Callers that must place things in the SVG's coordinate
/// frame (the PDF window plot) read the typed fields instead of scraping
/// the `data-*` attributes back out of the string. `x_svg = x_world −
/// x_shift`, `y_svg = h_flip − y_world`.
pub(crate) struct SvgFrame {
    pub svg: String,
    pub view_w: f64,
    pub view_h: f64,
    pub x_shift: f64,
    pub h_flip: f64,
}

/// Renders `doc` to a standalone SVG document, sized and viewBox-fit to its
/// own extents.
pub fn export_svg(doc: &Document) -> String {
    export_svg_framed(doc).svg
}

pub(crate) fn export_svg_framed(doc: &Document) -> SvgFrame {
    let (view_w, view_h, h_flip, x_shift) = match doc.extents() {
        Some(bb) => {
            let (x0, y0) = bb.min.to_f64();
            let (x1, y1) = bb.max.to_f64();
            let margin = 0.05 * ((x1 - x0).max(y1 - y0)).max(1.0);
            (
                (x1 - x0) + 2.0 * margin,
                (y1 - y0) + 2.0 * margin,
                y1 + margin,
                x0 - margin,
            )
        }
        None => (100.0, 100.0, 100.0, 0.0),
    };

    let fy = |y: f64| h_flip - y;
    let fx = |x: f64| x - x_shift;

    let mut s = String::new();
    let _ = writeln!(
        s,
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {view_w:.6} {view_h:.6}\" data-h-flip=\"{h_flip:.9}\" data-x-shift=\"{x_shift:.9}\">"
    );

    for e in doc.iter() {
        let stroke = stroke_for(doc, e);
        if let Some(path) = entity_to_svg(&e.kind, &fx, &fy, &stroke) {
            s.push_str(&path);
            s.push('\n');
        } else if let Some(prims) =
            crate::dim::dimension_primitives(&e.kind, &doc.settings.dim_style, doc.settings.units)
        {
            s.push_str(&dimension_to_svg(&prims, &fx, &fy, &stroke));
        }
    }
    s.push_str("</svg>\n");
    SvgFrame {
        svg: s,
        view_w,
        view_h,
        x_shift,
        h_flip,
    }
}

fn dimension_to_svg(
    prims: &crate::dim::DimPrimitives,
    fx: &impl Fn(f64) -> f64,
    fy: &impl Fn(f64) -> f64,
    stroke: &str,
) -> String {
    let mut out = String::new();
    let style = format!("fill=\"none\" stroke=\"{}\" stroke-width=\"0.25\"", stroke);
    for (a, b) in &prims.segs {
        let _ = writeln!(
            out,
            "  <line x1=\"{:.6}\" y1=\"{:.6}\" x2=\"{:.6}\" y2=\"{:.6}\" {style}/>",
            fx(a.x),
            fy(a.y),
            fx(b.x),
            fy(b.y),
        );
    }
    if let Some(t) = &prims.text {
        let (tx, ty) = (fx(t.anchor.x), fy(t.anchor.y));
        let transform = if t.rotation_deg.abs() > 1e-6 {
            format!(
                " transform=\"rotate({:.4} {:.6} {:.6})\"",
                -t.rotation_deg, tx, ty
            )
        } else {
            String::new()
        };
        let _ = writeln!(
            out,
            "  <text x=\"{tx:.6}\" y=\"{ty:.6}\" font-size=\"{:.6}\" fill=\"{stroke}\" text-anchor=\"middle\"{transform}>{}</text>",
            t.height,
            text_content_markup(&t.content, tx, t.height * 1.2)
        );
    }
    out
}

fn stroke_for(doc: &Document, e: &oxidraft_document::Entity) -> String {
    let (r, g, b) = match &e.color {
        oxidraft_document::Color::Rgb(r, g, b) => (*r, *g, *b),
        _ => doc
            .layers
            .get(e.layer)
            .map(|l| l.color)
            .unwrap_or((0, 0, 0)),
    };
    format!("#{:02x}{:02x}{:02x}", r, g, b)
}

fn entity_to_svg(
    kind: &EntityKind,
    fx: &impl Fn(f64) -> f64,
    fy: &impl Fn(f64) -> f64,
    stroke: &str,
) -> Option<String> {
    let style = format!("fill=\"none\" stroke=\"{}\" stroke-width=\"0.25\"", stroke);
    match kind {
        EntityKind::Curve(Curve::Line(l)) => {
            let (x1, y1) = l.p0.to_f64();
            let (x2, y2) = l.p1.to_f64();
            Some(format!(
                "  <line x1=\"{:.6}\" y1=\"{:.6}\" x2=\"{:.6}\" y2=\"{:.6}\" {}/>",
                fx(x1),
                fy(y1),
                fx(x2),
                fy(y2),
                style
            ))
        }
        EntityKind::Curve(Curve::Arc(a)) => {
            let (cx, cy) = a.center.to_f64();
            let r = a.radius;
            let span = (a.end_angle - a.start_angle).abs();
            if (span - TAU).abs() < 1e-9 {
                Some(format!(
                    "  <circle cx=\"{:.6}\" cy=\"{:.6}\" r=\"{:.6}\" {}/>",
                    fx(cx),
                    fy(cy),
                    r,
                    style
                ))
            } else {
                Some(format!("  <path d=\"{}\" {}/>", arc_path(a, fx, fy), style))
            }
        }
        EntityKind::Curve(Curve::Ellipse(e)) => {
            let (cx, cy) = e.center.to_f64();
            if e.rotation.abs() < 1e-9 {
                Some(format!(
                    "  <ellipse cx=\"{:.6}\" cy=\"{:.6}\" rx=\"{:.6}\" ry=\"{:.6}\" {}/>",
                    fx(cx),
                    fy(cy),
                    e.semi_major,
                    e.semi_minor,
                    style
                ))
            } else {
                Some(format!(
                    "  <path d=\"{}\" {}/>",
                    sampled_path(&Curve::Ellipse(*e), fx, fy),
                    style
                ))
            }
        }
        EntityKind::Curve(Curve::Bezier(b)) => {
            let (x0, y0) = b.p0.to_f64();
            let (x1, y1) = b.p1.to_f64();
            let (x2, y2) = b.p2.to_f64();
            let (x3, y3) = b.p3.to_f64();
            Some(format!(
                "  <path d=\"M {:.6} {:.6} C {:.6} {:.6} {:.6} {:.6} {:.6} {:.6}\" {}/>",
                fx(x0),
                fy(y0),
                fx(x1),
                fy(y1),
                fx(x2),
                fy(y2),
                fx(x3),
                fy(y3),
                style
            ))
        }
        EntityKind::Curve(Curve::Poly(pc)) => Some(format!(
            "  <path d=\"{}\" {}/>",
            polycurve_path(pc, fx, fy),
            style
        )),
        EntityKind::Curve(Curve::Rational(rb)) => Some(format!(
            "  <path d=\"{}\" {}/>",
            sampled_path(&Curve::Rational(rb.clone()), fx, fy),
            style
        )),
        EntityKind::Curve(Curve::Nurbs(nc)) => Some(format!(
            "  <path d=\"{}\" {}/>",
            sampled_path(&Curve::Nurbs(nc.clone()), fx, fy),
            style
        )),
        EntityKind::Point(p) => {
            let (x, y) = p.to_f64();
            Some(format!(
                "  <circle cx=\"{:.6}\" cy=\"{:.6}\" r=\"0.5\" fill=\"{}\"/>",
                fx(x),
                fy(y),
                stroke
            ))
        }
        EntityKind::Text {
            anchor,
            content,
            height,
            ..
        } => {
            let (x, y) = anchor.to_f64();
            let (sx, sy) = (fx(x), fy(y));
            Some(format!(
                "  <text x=\"{sx:.6}\" y=\"{sy:.6}\" font-size=\"{:.6}\" fill=\"{}\">{}</text>",
                height,
                stroke,
                text_content_markup(content, sx, *height * 1.2)
            ))
        }
        EntityKind::Hatch {
            boundary,
            holes,
            fill,
            ..
        } => {
            let fill_hex = format!("#{:02x}{:02x}{:02x}", fill.0, fill.1, fill.2);
            let mut d = String::new();
            for loop_segs in std::iter::once(boundary).chain(holes.iter()) {
                let mut first = true;
                for seg in loop_segs {
                    for p in crate::flatten_for_export(seg) {
                        let cmd = if first {
                            first = false;
                            "M"
                        } else {
                            "L"
                        };
                        let _ = write!(d, "{cmd} {:.6} {:.6} ", fx(p.x), fy(p.y));
                    }
                }
                d.push_str("Z ");
            }
            Some(format!(
                "  <path d=\"{}\" fill=\"{}\" fill-rule=\"evenodd\" stroke=\"none\"/>",
                d, fill_hex
            ))
        }
        _ => None,
    }
}

fn arc_path(a: &CircularArc, fx: &impl Fn(f64) -> f64, fy: &impl Fn(f64) -> f64) -> String {
    let (sx, sy) = a.start_point();
    let (ex, ey) = a.end_point();
    let r = a.radius;
    let large = if a.included_angle() > std::f64::consts::PI {
        1
    } else {
        0
    };
    // World-CCW arcs render clockwise in SVG's y-down frame (sweep 0); a
    // reversed arc (end < start, from reverse_curve) runs the other way.
    let sweep = if a.end_angle >= a.start_angle { 0 } else { 1 };
    format!(
        "M {:.6} {:.6} A {:.6} {:.6} 0 {} {} {:.6} {:.6}",
        fx(sx),
        fy(sy),
        r,
        r,
        large,
        sweep,
        fx(ex),
        fy(ey)
    )
}

fn sampled_path(c: &Curve, fx: &impl Fn(f64) -> f64, fy: &impl Fn(f64) -> f64) -> String {
    let mut d = String::new();
    for (i, p) in crate::flatten_for_export(c).iter().enumerate() {
        let _ = write!(
            d,
            "{} {:.6} {:.6} ",
            if i == 0 { "M" } else { "L" },
            fx(p.x),
            fy(p.y)
        );
    }
    d.trim_end().to_string()
}

fn polycurve_path(pc: &PolyCurve, fx: &impl Fn(f64) -> f64, fy: &impl Fn(f64) -> f64) -> String {
    let mut d = String::new();
    let mut first = true;
    for seg in &pc.segments {
        match seg {
            Curve::Line(l) => {
                let (x0, y0) = l.p0.to_f64();
                let (x1, y1) = l.p1.to_f64();
                if first {
                    let _ = write!(d, "M {:.6} {:.6} ", fx(x0), fy(y0));
                    first = false;
                }
                let _ = write!(d, "L {:.6} {:.6} ", fx(x1), fy(y1));
            }
            Curve::Bezier(b) => {
                let (x0, y0) = b.p0.to_f64();
                if first {
                    let _ = write!(d, "M {:.6} {:.6} ", fx(x0), fy(y0));
                    first = false;
                }
                let (x1, y1) = b.p1.to_f64();
                let (x2, y2) = b.p2.to_f64();
                let (x3, y3) = b.p3.to_f64();
                let _ = write!(
                    d,
                    "C {:.6} {:.6} {:.6} {:.6} {:.6} {:.6} ",
                    fx(x1),
                    fy(y1),
                    fx(x2),
                    fy(y2),
                    fx(x3),
                    fy(y3)
                );
            }
            Curve::Arc(a) => {
                let (sx, sy) = a.start_point();
                let (ex, ey) = a.end_point();
                if first {
                    let _ = write!(d, "M {:.6} {:.6} ", fx(sx), fy(sy));
                    first = false;
                }
                let r = a.radius;
                let large = if a.included_angle() > std::f64::consts::PI {
                    1
                } else {
                    0
                };
                // Reversed segments (end < start) occur routinely inside
                // joined polycurves; their sweep runs the other way.
                let sweep = if a.end_angle >= a.start_angle { 0 } else { 1 };
                let _ = write!(
                    d,
                    "A {r:.6} {r:.6} 0 {large} {sweep} {:.6} {:.6} ",
                    fx(ex),
                    fy(ey)
                );
            }
            Curve::Rational(_) => {
                let poly = crate::flatten_for_export(seg);
                if let Some(p0) = poly.first()
                    && first
                {
                    let _ = write!(d, "M {:.6} {:.6} ", fx(p0.x), fy(p0.y));
                    first = false;
                }
                for p in poly.iter().skip(1) {
                    let _ = write!(d, "L {:.6} {:.6} ", fx(p.x), fy(p.y));
                }
            }
            other => {
                let (x, y) = other.evaluate_f64(other.domain().1);
                let _ = write!(d, "L {:.6} {:.6} ", fx(x), fy(y));
            }
        }
    }
    d.trim_end().to_string()
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Renders text content as SVG markup, splitting embedded newlines into one
/// `<tspan>` per line so multi-line content doesn't collapse onto one line
/// (SVG has no notion of a literal line break inside plain character data).
fn text_content_markup(content: &str, x: f64, line_height: f64) -> String {
    if !content.contains('\n') {
        return xml_escape(content);
    }
    let mut out = String::new();
    for (i, line) in content.split('\n').enumerate() {
        let dy = if i == 0 { 0.0 } else { line_height };
        let _ = write!(
            out,
            "<tspan x=\"{x:.6}\" dy=\"{dy:.6}\">{}</tspan>",
            xml_escape(line)
        );
    }
    out
}

/// Parses `svg` markup into a [`Document`], reading the paths/shapes it
/// understands and ignoring the rest.
pub fn import_svg(svg: &str) -> Document {
    let mut doc = Document::new();
    let h_flip = attr(svg, "data-h-flip")
        .and_then(|v| v.parse().ok())
        .or_else(|| viewbox_height(svg))
        .unwrap_or(0.0);
    let x_shift: f64 = attr(svg, "data-x-shift")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.0);
    // Maps SVG user space (y-down) into document space (y-up):
    //   x_doc = x_svg + x_shift,  y_doc = h_flip - y_svg
    let flip = Transform2d {
        m00: 1.0,
        m01: 0.0,
        tx: x_shift,
        m10: 0.0,
        m11: -1.0,
        ty: h_flip,
    };

    // Current transform matrix stack, one frame per open container element.
    let mut stack: Vec<Transform2d> = vec![Transform2d::identity()];
    for tag in tags(svg) {
        match tag {
            Tag::Open {
                name,
                attrs,
                self_closing,
            } => {
                let parent = *stack.last().unwrap();
                let ctm = parent.compose(&transform_attr(&attrs));
                if is_container(&name) {
                    if !self_closing {
                        stack.push(ctm);
                    }
                    continue;
                }
                let to_doc = flip.compose(&ctm);
                // A hostile transform attribute (scale(nan)) or NaN shape
                // coordinates must not reach the document.
                if !to_doc.is_finite() {
                    continue;
                }
                let color = stroke_color(&attrs);
                for curve in shape_curves(&name, &attrs) {
                    let kind = EntityKind::Curve(to_doc.apply_curve(&curve));
                    if !kind.is_finite() {
                        continue;
                    }
                    let id = doc.add(kind);
                    if let (Some(c), Some(e)) = (&color, doc.get_mut(id)) {
                        e.color = c.clone();
                    }
                }
            }
            Tag::Close { name } => {
                if is_container(&name) && stack.len() > 1 {
                    stack.pop();
                }
            }
        }
    }
    doc
}

#[derive(Debug)]
enum Tag {
    Open {
        name: String,
        attrs: Vec<(String, String)>,
        self_closing: bool,
    },
    Close {
        name: String,
    },
}

fn is_container(name: &str) -> bool {
    matches!(name, "g" | "svg" | "a" | "switch")
}

/// Splits SVG markup into open/close tag events, skipping comments, CDATA,
/// declarations and processing instructions, and honouring quoted attributes.
fn tags(svg: &str) -> Vec<Tag> {
    let bytes = svg.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        let rest = &svg[i..];
        if rest.starts_with("<!--") {
            i += rest.find("-->").map_or(rest.len(), |e| e + 3);
            continue;
        }
        if rest.starts_with("<!") || rest.starts_with("<?") {
            i += rest.find('>').map_or(rest.len(), |e| e + 1);
            continue;
        }
        if rest.starts_with("</") {
            let end = rest.find('>').unwrap_or(rest.len());
            out.push(Tag::Close {
                name: rest[2..end].trim().to_string(),
            });
            i += end + 1;
            continue;
        }
        // opening tag: scan to the closing '>' while skipping quoted regions
        let mut j = i + 1;
        let mut quote = 0u8;
        while j < bytes.len() {
            let c = bytes[j];
            if quote != 0 {
                if c == quote {
                    quote = 0;
                }
            } else if c == b'"' || c == b'\'' {
                quote = c;
            } else if c == b'>' {
                break;
            }
            j += 1;
        }
        let mut inner = &svg[i + 1..j.min(svg.len())];
        let self_closing = inner.trim_end().ends_with('/');
        if self_closing {
            inner = inner.trim_end();
            inner = &inner[..inner.len() - 1];
        }
        let inner = inner.trim_start();
        let name_end = inner
            .find(|c: char| c.is_whitespace())
            .unwrap_or(inner.len());
        out.push(Tag::Open {
            name: inner[..name_end].to_string(),
            attrs: parse_attrs(&inner[name_end..]),
            self_closing,
        });
        i = j + 1;
    }
    out
}

fn attrs_get<'a>(attrs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    attrs
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

/// Reads a coordinate/length attribute, tolerating unit suffixes (px, mm, …).
fn attrf(attrs: &[(String, String)], key: &str) -> f64 {
    attrs_get(attrs, key).and_then(parse_len).unwrap_or(0.0)
}

fn parse_len(s: &str) -> Option<f64> {
    let s = s.trim();
    let end = s
        .find(|c: char| !(c.is_ascii_digit() || matches!(c, '.' | '-' | '+')))
        .unwrap_or(s.len());
    s[..end].parse().ok()
}

fn transform_attr(attrs: &[(String, String)]) -> Transform2d {
    attrs_get(attrs, "transform").map_or_else(Transform2d::identity, parse_transform_list)
}

/// Parses a `transform` attribute (a sequence of `name(args)` functions),
/// composing them left to right as SVG specifies.
fn parse_transform_list(s: &str) -> Transform2d {
    let mut t = Transform2d::identity();
    let mut rest = s;
    while let Some(open) = rest.find('(') {
        let name: String = rest[..open]
            .chars()
            .rev()
            .take_while(|c| c.is_ascii_alphabetic())
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        let Some(close_rel) = rest[open..].find(')') else {
            break;
        };
        let close = open + close_rel;
        let args: Vec<f64> = rest[open + 1..close]
            .split(|c: char| c == ',' || c.is_whitespace())
            .filter_map(|p| p.trim().parse().ok())
            .collect();
        t = t.compose(&transform_from(&name, &args));
        rest = &rest[close + 1..];
    }
    t
}

fn transform_from(name: &str, a: &[f64]) -> Transform2d {
    let g = |i: usize| a.get(i).copied();
    match name {
        "translate" => Transform2d::translation(g(0).unwrap_or(0.0), g(1).unwrap_or(0.0)),
        "scale" => {
            let sx = g(0).unwrap_or(1.0);
            Transform2d::scale(sx, g(1).unwrap_or(sx))
        }
        "rotate" => {
            let r = Transform2d::rotation(g(0).unwrap_or(0.0).to_radians());
            match (g(1), g(2)) {
                (Some(cx), Some(cy)) => Transform2d::translation(cx, cy)
                    .compose(&r)
                    .compose(&Transform2d::translation(-cx, -cy)),
                _ => r,
            }
        }
        "matrix" => Transform2d {
            m00: g(0).unwrap_or(1.0),
            m10: g(1).unwrap_or(0.0),
            m01: g(2).unwrap_or(0.0),
            m11: g(3).unwrap_or(1.0),
            tx: g(4).unwrap_or(0.0),
            ty: g(5).unwrap_or(0.0),
        },
        "skewX" => {
            let mut t = Transform2d::identity();
            t.m01 = g(0).unwrap_or(0.0).to_radians().tan();
            t
        }
        "skewY" => {
            let mut t = Transform2d::identity();
            t.m10 = g(0).unwrap_or(0.0).to_radians().tan();
            t
        }
        _ => Transform2d::identity(),
    }
}

fn stroke_color(attrs: &[(String, String)]) -> Option<Color> {
    let raw = attrs_get(attrs, "stroke").or_else(|| {
        attrs_get(attrs, "style").and_then(|style| {
            style.split(';').find_map(|decl| {
                decl.split_once(':')
                    .filter(|(k, _)| k.trim() == "stroke")
                    .map(|(_, v)| v.trim())
            })
        })
    })?;
    parse_color(raw)
}

fn parse_color(s: &str) -> Option<Color> {
    let s = s.trim();
    if s.is_empty() || s.eq_ignore_ascii_case("none") || s.eq_ignore_ascii_case("currentColor") {
        return None;
    }
    if let Some(hex) = s.strip_prefix('#') {
        let p = |x: &str| u8::from_str_radix(x, 16).ok();
        return match hex.len() {
            3 => Some(Color::Rgb(
                p(&hex[0..1].repeat(2))?,
                p(&hex[1..2].repeat(2))?,
                p(&hex[2..3].repeat(2))?,
            )),
            6 => Some(Color::Rgb(p(&hex[0..2])?, p(&hex[2..4])?, p(&hex[4..6])?)),
            _ => None,
        };
    }
    if let Some(inner) = s.strip_prefix("rgb(").and_then(|x| x.strip_suffix(')')) {
        let v: Vec<u8> = inner
            .split(',')
            .filter_map(|p| p.trim().parse().ok())
            .collect();
        return (v.len() == 3).then(|| Color::Rgb(v[0], v[1], v[2]));
    }
    named_color(s)
}

fn named_color(s: &str) -> Option<Color> {
    let (r, g, b) = match s.to_ascii_lowercase().as_str() {
        "black" => (0, 0, 0),
        "white" => (255, 255, 255),
        "red" => (255, 0, 0),
        "green" => (0, 128, 0),
        "lime" => (0, 255, 0),
        "blue" => (0, 0, 255),
        "yellow" => (255, 255, 0),
        "cyan" | "aqua" => (0, 255, 255),
        "magenta" | "fuchsia" => (255, 0, 255),
        "gray" | "grey" => (128, 128, 128),
        "orange" => (255, 165, 0),
        _ => return None,
    };
    Some(Color::Rgb(r, g, b))
}

/// Builds the geometry for a single shape element in SVG user space.
fn shape_curves(name: &str, attrs: &[(String, String)]) -> Vec<Curve> {
    let p = |x: f64, y: f64| Point2d::from_f64(x, y);
    match name {
        "line" => vec![Curve::Line(LineSeg::from_endpoints(
            p(attrf(attrs, "x1"), attrf(attrs, "y1")),
            p(attrf(attrs, "x2"), attrf(attrs, "y2")),
        ))],
        "polyline" | "polygon" => {
            let pts = points_list(attrs_get(attrs, "points").unwrap_or(""));
            let mut curves = open_chain(&pts);
            if name == "polygon" && pts.len() >= 2 {
                curves.push(Curve::Line(LineSeg::from_endpoints(
                    pts[pts.len() - 1],
                    pts[0],
                )));
            }
            curves
        }
        "rect" => rect_curves(attrs),
        "circle" => {
            let r = attrf(attrs, "r");
            (r > 1e-9)
                .then(|| {
                    CircularArc::try_new(p(attrf(attrs, "cx"), attrf(attrs, "cy")), r, 0.0, TAU)
                        .ok()
                        .map(Curve::Arc)
                })
                .flatten()
                .into_iter()
                .collect()
        }
        "ellipse" => {
            let (rx, ry) = (attrf(attrs, "rx"), attrf(attrs, "ry"));
            (rx > 1e-9 && ry > 1e-9)
                .then(|| {
                    EllipticalArc::try_new(
                        p(attrf(attrs, "cx"), attrf(attrs, "cy")),
                        rx,
                        ry,
                        0.0,
                        0.0,
                        TAU,
                    )
                    .ok()
                    .map(Curve::Ellipse)
                })
                .flatten()
                .into_iter()
                .collect()
        }
        "path" => attrs_get(attrs, "d").map(path_curves).unwrap_or_default(),
        _ => vec![],
    }
}

fn open_chain(pts: &[Point2d]) -> Vec<Curve> {
    pts.windows(2)
        .map(|w| Curve::Line(LineSeg::from_endpoints(w[0], w[1])))
        .collect()
}

fn points_list(s: &str) -> Vec<Point2d> {
    let nums: Vec<f64> = s
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter_map(|t| t.trim().parse().ok())
        .collect();
    nums.as_chunks::<2>()
        .0
        .iter()
        .map(|&[x, y]| Point2d::from_f64(x, y))
        .collect()
}

fn rect_curves(attrs: &[(String, String)]) -> Vec<Curve> {
    let (x, y) = (attrf(attrs, "x"), attrf(attrs, "y"));
    let (w, h) = (attrf(attrs, "width"), attrf(attrs, "height"));
    if w <= 0.0 || h <= 0.0 {
        return vec![];
    }
    let p = |px: f64, py: f64| Point2d::from_f64(px, py);
    let corners = [p(x, y), p(x + w, y), p(x + w, y + h), p(x, y + h)];
    let mut curves = open_chain(&corners);
    curves.push(Curve::Line(LineSeg::from_endpoints(corners[3], corners[0])));
    curves
}

fn line_seg(a: (f64, f64), b: (f64, f64)) -> Curve {
    Curve::Line(LineSeg::from_endpoints(
        Point2d::from_f64(a.0, a.1),
        Point2d::from_f64(b.0, b.1),
    ))
}

fn cubic_seg(p0: (f64, f64), p1: (f64, f64), p2: (f64, f64), p3: (f64, f64)) -> Curve {
    Curve::Bezier(CubicBezier::new(
        Point2d::from_f64(p0.0, p0.1),
        Point2d::from_f64(p1.0, p1.1),
        Point2d::from_f64(p2.0, p2.1),
        Point2d::from_f64(p3.0, p3.1),
    ))
}

fn quad_seg(p0: (f64, f64), q: (f64, f64), p2: (f64, f64)) -> Curve {
    let c1 = (
        p0.0 + 2.0 / 3.0 * (q.0 - p0.0),
        p0.1 + 2.0 / 3.0 * (q.1 - p0.1),
    );
    let c2 = (
        p2.0 + 2.0 / 3.0 * (q.0 - p2.0),
        p2.1 + 2.0 / 3.0 * (q.1 - p2.1),
    );
    cubic_seg(p0, c1, c2, p2)
}

fn dist2(a: (f64, f64), b: (f64, f64)) -> f64 {
    (a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)
}

fn resolve(rel: bool, base: (f64, f64), p: (f64, f64)) -> (f64, f64) {
    if rel { (base.0 + p.0, base.1 + p.1) } else { p }
}

fn take_num(toks: &[Tok], i: &mut usize) -> Option<f64> {
    if let Some(Tok::Num(v)) = toks.get(*i) {
        *i += 1;
        Some(*v)
    } else {
        None
    }
}

fn take_pt(toks: &[Tok], i: &mut usize) -> Option<(f64, f64)> {
    Some((take_num(toks, i)?, take_num(toks, i)?))
}

/// Parses SVG path data into curves, supporting the full command set
/// (M/L/H/V/C/S/Q/T/A/Z) in both absolute and relative forms.
fn path_curves(d: &str) -> Vec<Curve> {
    let toks = tokenize_path(d);
    let mut curves = Vec::new();
    let mut i = 0;
    let mut cur = (0.0f64, 0.0f64);
    let mut start = (0.0f64, 0.0f64);
    let mut cmd = ' ';
    let mut prev_cubic: Option<(f64, f64)> = None;
    let mut prev_quad: Option<(f64, f64)> = None;

    while i < toks.len() {
        let c = match toks[i] {
            Tok::Cmd(ch) => {
                i += 1;
                cmd = ch;
                ch
            }
            Tok::Num(_) => cmd,
        };
        if c == ' ' {
            i += 1;
            continue;
        }
        let rel = c.is_ascii_lowercase();
        let mut is_cubic = false;
        let mut is_quad = false;
        match c.to_ascii_uppercase() {
            'M' => {
                let Some(p) = take_pt(&toks, &mut i) else {
                    break;
                };
                cur = resolve(rel, cur, p);
                start = cur;
                cmd = if rel { 'l' } else { 'L' };
            }
            'L' => {
                let Some(p) = take_pt(&toks, &mut i) else {
                    break;
                };
                let next = resolve(rel, cur, p);
                curves.push(line_seg(cur, next));
                cur = next;
            }
            'H' => {
                let Some(x) = take_num(&toks, &mut i) else {
                    break;
                };
                let next = if rel { (cur.0 + x, cur.1) } else { (x, cur.1) };
                curves.push(line_seg(cur, next));
                cur = next;
            }
            'V' => {
                let Some(y) = take_num(&toks, &mut i) else {
                    break;
                };
                let next = if rel { (cur.0, cur.1 + y) } else { (cur.0, y) };
                curves.push(line_seg(cur, next));
                cur = next;
            }
            'C' => {
                let (Some(c1), Some(c2), Some(p)) = (
                    take_pt(&toks, &mut i),
                    take_pt(&toks, &mut i),
                    take_pt(&toks, &mut i),
                ) else {
                    break;
                };
                let (c1, c2, e) = (
                    resolve(rel, cur, c1),
                    resolve(rel, cur, c2),
                    resolve(rel, cur, p),
                );
                curves.push(cubic_seg(cur, c1, c2, e));
                prev_cubic = Some(c2);
                cur = e;
                is_cubic = true;
            }
            'S' => {
                let (Some(c2), Some(p)) = (take_pt(&toks, &mut i), take_pt(&toks, &mut i)) else {
                    break;
                };
                let c1 = match prev_cubic {
                    Some(pc) => (2.0 * cur.0 - pc.0, 2.0 * cur.1 - pc.1),
                    None => cur,
                };
                let (c2, e) = (resolve(rel, cur, c2), resolve(rel, cur, p));
                curves.push(cubic_seg(cur, c1, c2, e));
                prev_cubic = Some(c2);
                cur = e;
                is_cubic = true;
            }
            'Q' => {
                let (Some(q), Some(p)) = (take_pt(&toks, &mut i), take_pt(&toks, &mut i)) else {
                    break;
                };
                let (q, e) = (resolve(rel, cur, q), resolve(rel, cur, p));
                curves.push(quad_seg(cur, q, e));
                prev_quad = Some(q);
                cur = e;
                is_quad = true;
            }
            'T' => {
                let Some(p) = take_pt(&toks, &mut i) else {
                    break;
                };
                let q = match prev_quad {
                    Some(pq) => (2.0 * cur.0 - pq.0, 2.0 * cur.1 - pq.1),
                    None => cur,
                };
                let e = resolve(rel, cur, p);
                curves.push(quad_seg(cur, q, e));
                prev_quad = Some(q);
                cur = e;
                is_quad = true;
            }
            'A' => {
                let (Some(rx), Some(ry), Some(rot), Some(large), Some(sweep), Some(p)) = (
                    take_num(&toks, &mut i),
                    take_num(&toks, &mut i),
                    take_num(&toks, &mut i),
                    take_num(&toks, &mut i),
                    take_num(&toks, &mut i),
                    take_pt(&toks, &mut i),
                ) else {
                    break;
                };
                let e = resolve(rel, cur, p);
                let arc = svg_arc_to_curve(cur, e, rx, ry, rot, large != 0.0, sweep != 0.0);
                curves.push(arc.unwrap_or_else(|| line_seg(cur, e)));
                cur = e;
            }
            'Z' => {
                if dist2(cur, start) > 1e-18 {
                    curves.push(line_seg(cur, start));
                }
                cur = start;
                cmd = ' ';
            }
            _ => {
                i += 1;
            }
        }
        if !is_cubic {
            prev_cubic = None;
        }
        if !is_quad {
            prev_quad = None;
        }
    }
    curves
}

/// Converts an SVG elliptical-arc command (endpoint parameterization) into a
/// circular or elliptical arc using the W3C centre-parameterization formulas.
fn svg_arc_to_curve(
    p0: (f64, f64),
    p1: (f64, f64),
    rx: f64,
    ry: f64,
    x_rot_deg: f64,
    large: bool,
    sweep: bool,
) -> Option<Curve> {
    let (mut rx, mut ry) = (rx.abs(), ry.abs());
    // Positive-polarity guard: NaN radii or endpoints fail every `<` test,
    // so the degenerate check must require validity rather than reject it.
    if !(rx >= 1e-12 && ry >= 1e-12 && dist2(p0, p1) >= 1e-24) {
        return None;
    }
    let phi = x_rot_deg.to_radians();
    let (cosp, sinp) = (phi.cos(), phi.sin());
    let dx = (p0.0 - p1.0) / 2.0;
    let dy = (p0.1 - p1.1) / 2.0;
    let x1 = cosp * dx + sinp * dy;
    let y1 = -sinp * dx + cosp * dy;
    let lambda = x1 * x1 / (rx * rx) + y1 * y1 / (ry * ry);
    if lambda > 1.0 {
        let s = lambda.sqrt();
        rx *= s;
        ry *= s;
    }
    let denom = rx * rx * y1 * y1 + ry * ry * x1 * x1;
    let numer = (rx * rx * ry * ry - denom).max(0.0);
    let sign = if large != sweep { 1.0 } else { -1.0 };
    let co = sign * (numer / denom).sqrt();
    let cxp = co * rx * y1 / ry;
    let cyp = -co * ry * x1 / rx;
    let cx = cosp * cxp - sinp * cyp + (p0.0 + p1.0) / 2.0;
    let cy = sinp * cxp + cosp * cyp + (p0.1 + p1.1) / 2.0;
    let angle = |ux: f64, uy: f64, vx: f64, vy: f64| {
        let dot = ux * vx + uy * vy;
        let len = ((ux * ux + uy * uy) * (vx * vx + vy * vy)).sqrt();
        let mut a = (dot / len).clamp(-1.0, 1.0).acos();
        if ux * vy - uy * vx < 0.0 {
            a = -a;
        }
        a
    };
    let (ux, uy) = ((x1 - cxp) / rx, (y1 - cyp) / ry);
    let theta1 = angle(1.0, 0.0, ux, uy);
    let mut dtheta = angle(ux, uy, (-x1 - cxp) / rx, (-y1 - cyp) / ry);
    if !sweep && dtheta > 0.0 {
        dtheta -= TAU;
    } else if sweep && dtheta < 0.0 {
        dtheta += TAU;
    }
    let (start, end) = (theta1, theta1 + dtheta);
    // Fallible: every value here is derived from the `d` attribute, and the
    // arc constructors reject spans they cannot represent. `new` would panic
    // on a crafted path — the importer must drop the segment instead.
    if (rx - ry).abs() < 1e-9 && phi.abs() < 1e-12 {
        CircularArc::try_new(Point2d::from_f64(cx, cy), rx, start, end)
            .ok()
            .map(Curve::Arc)
    } else {
        EllipticalArc::try_new(Point2d::from_f64(cx, cy), rx, ry, phi, start, end)
            .ok()
            .map(Curve::Ellipse)
    }
}

/// Parses attribute text into key/value pairs, accepting single- or
/// double-quoted values and ignoring stray slashes from self-closing tags.
fn parse_attrs(text: &str) -> Vec<(String, String)> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        while i < bytes.len() && (bytes[i].is_ascii_whitespace() || bytes[i] == b'/') {
            i += 1;
        }
        let ks = i;
        while i < bytes.len()
            && bytes[i] != b'='
            && !bytes[i].is_ascii_whitespace()
            && bytes[i] != b'/'
        {
            i += 1;
        }
        let key = text[ks..i].to_string();
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i < bytes.len() && bytes[i] == b'=' {
            i += 1;
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            let val = if i < bytes.len() && (bytes[i] == b'"' || bytes[i] == b'\'') {
                let q = bytes[i];
                i += 1;
                let vs = i;
                while i < bytes.len() && bytes[i] != q {
                    i += 1;
                }
                let v = text[vs..i].to_string();
                if i < bytes.len() {
                    i += 1;
                }
                v
            } else {
                let vs = i;
                while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
                    i += 1;
                }
                text[vs..i].to_string()
            };
            if !key.is_empty() {
                out.push((key, val));
            }
        } else if key.is_empty() {
            i += 1; // guarantee forward progress on malformed input
        }
    }
    out
}

fn attr(svg: &str, name: &str) -> Option<String> {
    let needle = format!("{}=\"", name);
    let start = svg.find(&needle)? + needle.len();
    let end = svg[start..].find('"')? + start;
    Some(svg[start..end].to_string())
}

fn viewbox_height(svg: &str) -> Option<f64> {
    let vb = attr(svg, "viewBox")?;
    let parts: Vec<f64> = vb
        .split_whitespace()
        .filter_map(|p| p.parse().ok())
        .collect();
    parts.get(3).copied()
}

#[derive(Clone, Debug)]
enum Tok {
    Cmd(char),
    Num(f64),
}

fn tokenize_path(d: &str) -> Vec<Tok> {
    let mut out = Vec::new();
    let mut chars = d.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_ascii_alphabetic() {
            out.push(Tok::Cmd(c));
            chars.next();
        } else if c.is_ascii_digit() || c == '-' || c == '+' || c == '.' {
            let mut num = String::new();
            while let Some(&c) = chars.peek() {
                if c.is_ascii_digit()
                    || c == '.'
                    || c == 'e'
                    || c == 'E'
                    || ((c == '-' || c == '+')
                        && (num.is_empty() || num.ends_with('e') || num.ends_with('E')))
                {
                    num.push(c);
                    chars.next();
                } else {
                    break;
                }
            }
            if let Ok(v) = num.parse() {
                out.push(Tok::Num(v));
            }
        } else {
            chars.next();
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxidraft_document::EntityKind;

    fn pt(x: i64, y: i64) -> Point2d {
        Point2d::from_i64(x, y)
    }

    #[test]
    fn export_contains_svg_root_and_line() {
        let mut doc = Document::new();
        doc.add(EntityKind::Curve(Curve::Line(LineSeg::from_endpoints(
            pt(0, 0),
            pt(10, 10),
        ))));
        let svg = export_svg(&doc);
        assert!(svg.contains("<svg"));
        assert!(svg.contains("<line"));
        assert!(svg.contains("</svg>"));
    }

    #[test]
    fn exports_multiline_text_as_one_tspan_per_line() {
        let mut doc = Document::new();
        doc.add(EntityKind::Text {
            anchor: pt(0, 0),
            content: "line one\nline two".into(),
            height: 2.5,
            rotation: 0.0,
            font: None,
        });
        let svg = export_svg(&doc);
        assert_eq!(
            svg.matches("<tspan").count(),
            2,
            "each source line should become its own tspan: {svg}"
        );
        assert!(svg.contains(">line one</tspan>"));
        assert!(svg.contains(">line two</tspan>"));
        assert!(
            !svg.contains("line one\nline two"),
            "content must not appear as a single unbroken text node"
        );
    }

    #[test]
    fn exports_dimensions_as_lines_and_text() {
        let mut doc = Document::new();
        doc.add(EntityKind::OrthoDim {
            p1: pt(0, 0),
            p2: pt(10, 0),
            line: pt(0, 4),
            vertical: false,
            height: 2.5,
            override_text: None,
        });
        doc.add(EntityKind::RadialDim {
            center: pt(0, 0),
            edge: pt(5, 0),
            diameter: true,
            height: 2.5,
            override_text: None,
        });
        let svg = export_svg(&doc);
        assert!(svg.contains("<line"), "dimension lines exported");
        assert!(svg.contains("<text"), "dimension value text exported");
        assert!(svg.contains('\u{00d8}'), "diameter symbol in the label");
    }

    #[test]
    fn roundtrip_line_preserves_geometry() {
        let mut doc = Document::new();
        doc.add(EntityKind::Curve(Curve::Line(LineSeg::from_endpoints(
            pt(2, 3),
            pt(8, 5),
        ))));
        let svg = export_svg(&doc);
        let doc2 = import_svg(&svg);
        assert_eq!(doc2.len(), 1);
        let es: Vec<_> = doc2.iter().collect();
        if let Some(Curve::Line(l)) = es[0].as_curve() {
            let p0 = l.p0.to_f64();
            let p1 = l.p1.to_f64();
            let ok = (close(p0, (2.0, 3.0)) && close(p1, (8.0, 5.0)))
                || (close(p0, (8.0, 5.0)) && close(p1, (2.0, 3.0)));
            assert!(ok, "got {:?} {:?}", p0, p1);
        } else {
            panic!()
        }
    }

    #[test]
    fn reversed_arc_roundtrips_as_its_short_span() {
        // A quarter arc traversed backwards (π/2 → 0), as reverse_curve
        // produces. Export must emit the 90° span (small-arc, opposite
        // sweep), not the 270° complement.
        let mut doc = Document::new();
        doc.add(EntityKind::Curve(Curve::Arc(CircularArc::new(
            pt(0, 0),
            4.0,
            std::f64::consts::FRAC_PI_2,
            0.0,
        ))));
        let svg = export_svg(&doc);
        let doc2 = import_svg(&svg);
        let es: Vec<_> = doc2.iter().collect();
        if let Some(Curve::Arc(a)) = es[0].as_curve() {
            assert!(
                (a.included_angle() - std::f64::consts::FRAC_PI_2).abs() < 1e-3,
                "must reimport as a quarter arc, got {} rad",
                a.included_angle()
            );
            assert!((a.radius - 4.0).abs() < 1e-6);
        } else {
            panic!("expected an arc back");
        }
    }

    #[test]
    fn roundtrip_circle() {
        let mut doc = Document::new();
        doc.add(EntityKind::Curve(Curve::Arc(CircularArc::new(
            pt(5, 5),
            3.0,
            0.0,
            TAU,
        ))));
        let svg = export_svg(&doc);
        assert!(svg.contains("<circle"));
        let doc2 = import_svg(&svg);
        let es: Vec<_> = doc2.iter().collect();
        if let Some(Curve::Arc(a)) = es[0].as_curve() {
            assert!((a.center.x - 5.0).abs() < 1e-6);
            assert!((a.center.y - 5.0).abs() < 1e-6);
            assert!((a.radius - 3.0).abs() < 1e-6);
        } else {
            panic!()
        }
    }

    #[test]
    fn roundtrip_bezier_native_path() {
        let mut doc = Document::new();
        doc.add(EntityKind::Curve(Curve::Bezier(CubicBezier::new(
            pt(0, 0),
            pt(1, 3),
            pt(4, 3),
            pt(5, 0),
        ))));
        let svg = export_svg(&doc);
        assert!(svg.contains(" C "));
        let doc2 = import_svg(&svg);
        let es: Vec<_> = doc2.iter().collect();
        assert!(matches!(es[0].as_curve(), Some(Curve::Bezier(_))));
    }

    #[test]
    fn import_external_path_lines() {
        let svg = "<svg viewBox=\"0 0 10 10\"><path d=\"M 0 0 L 10 0 L 10 10 Z\"/></svg>";
        let doc = import_svg(svg);
        assert_eq!(doc.len(), 3);
    }

    #[test]
    fn import_polyline_and_polygon() {
        let svg = "<svg viewBox=\"0 0 10 10\">\
            <polyline points=\"0,0 5,0 5,5\"/>\
            <polygon points=\"0,0 4,0 4,4\"/></svg>";
        let doc = import_svg(svg);
        // polyline: 2 segments, polygon: 3 segments (closed)
        assert_eq!(doc.len(), 5);
    }

    #[test]
    fn import_rect_makes_closed_quad() {
        let svg = "<svg viewBox=\"0 0 100 100\"><rect x=\"10\" y=\"20\" width=\"30\" height=\"40\"/></svg>";
        let doc = import_svg(svg);
        assert_eq!(doc.len(), 4);
        let curves: Vec<_> = doc.iter().filter_map(|e| e.as_curve().cloned()).collect();
        assert!(curves.iter().all(|c| matches!(c, Curve::Line(_))));
    }

    #[test]
    fn import_relative_and_implicit_lineto() {
        // 'm' moveto then implicit relative linetos, closed
        let svg = "<svg viewBox=\"0 0 20 20\"><path d=\"m 0 0 5 0 0 5 z\"/></svg>";
        let doc = import_svg(svg);
        assert_eq!(doc.len(), 3);
        assert!(
            doc.iter()
                .filter_map(|e| e.as_curve())
                .all(|c| matches!(c, Curve::Line(_)))
        );
    }

    #[test]
    fn import_quadratic_and_smooth_become_beziers() {
        let svg = "<svg viewBox=\"0 0 20 20\"><path d=\"M0 0 Q 5 10 10 0 T 20 0\"/></svg>";
        let doc = import_svg(svg);
        let curves: Vec<_> = doc.iter().filter_map(|e| e.as_curve().cloned()).collect();
        assert_eq!(curves.len(), 2);
        assert!(curves.iter().all(|c| matches!(c, Curve::Bezier(_))));
    }

    #[test]
    fn import_horizontal_vertical_commands() {
        let svg = "<svg viewBox=\"0 0 20 20\"><path d=\"M0 0 H10 V10 H0 Z\"/></svg>";
        let doc = import_svg(svg);
        assert_eq!(doc.len(), 4);
    }

    #[test]
    fn group_translate_offsets_geometry() {
        // line from (0,0)-(2,0) inside a group translated by (5, 3)
        let svg = "<svg viewBox=\"0 0 100 100\">\
            <g transform=\"translate(5,3)\">\
            <line x1=\"0\" y1=\"0\" x2=\"2\" y2=\"0\"/></g></svg>";
        let doc = import_svg(svg);
        let curves: Vec<_> = doc.iter().filter_map(|e| e.as_curve().cloned()).collect();
        // viewbox height 100 -> y_doc = 100 - y_svg; translate adds (5,3)
        if let Curve::Line(l) = &curves[0] {
            let p0 = l.p0.to_f64();
            let p1 = l.p1.to_f64();
            assert!(
                close(p0, (5.0, 97.0)) || close(p1, (5.0, 97.0)),
                "got {p0:?} {p1:?}"
            );
            assert!(
                close(p0, (7.0, 97.0)) || close(p1, (7.0, 97.0)),
                "got {p0:?} {p1:?}"
            );
        } else {
            panic!("expected line");
        }
    }

    #[test]
    fn nested_group_transforms_compose() {
        let svg = "<svg viewBox=\"0 0 100 100\">\
            <g transform=\"translate(10,0)\"><g transform=\"translate(0,10)\">\
            <circle cx=\"0\" cy=\"0\" r=\"1\"/></g></g></svg>";
        let doc = import_svg(svg);
        let c = doc
            .iter()
            .filter_map(|e| e.as_curve().cloned())
            .next()
            .unwrap();
        if let Curve::Arc(a) = c {
            // centre at svg (10,10) -> doc (10, 90)
            assert!((a.center.x - 10.0).abs() < 1e-6, "x={}", a.center.x);
            assert!((a.center.y - 90.0).abs() < 1e-6, "y={}", a.center.y);
        } else {
            panic!("expected arc");
        }
    }

    #[test]
    fn matrix_transform_scales() {
        // matrix(2 0 0 2 0 0) scales geometry by 2
        let svg = "<svg viewBox=\"0 0 100 100\">\
            <g transform=\"matrix(2 0 0 2 0 0)\">\
            <line x1=\"1\" y1=\"0\" x2=\"3\" y2=\"0\"/></g></svg>";
        let doc = import_svg(svg);
        if let Some(Curve::Line(l)) = doc.iter().filter_map(|e| e.as_curve()).next() {
            let len = l.p0.dist_f64(&l.p1);
            assert!((len - 4.0).abs() < 1e-6, "scaled length {len}");
        } else {
            panic!("expected line");
        }
    }

    #[test]
    fn stroke_attribute_sets_entity_color() {
        let svg = "<svg viewBox=\"0 0 10 10\">\
            <line x1=\"0\" y1=\"0\" x2=\"1\" y2=\"1\" stroke=\"#ff0000\"/>\
            <line x1=\"0\" y1=\"0\" x2=\"1\" y2=\"1\" style=\"stroke: rgb(0,255,0); fill:none\"/></svg>";
        let doc = import_svg(svg);
        let colors: Vec<_> = doc.iter().map(|e| e.color.clone()).collect();
        assert_eq!(colors[0], oxidraft_document::Color::Rgb(255, 0, 0));
        assert_eq!(colors[1], oxidraft_document::Color::Rgb(0, 255, 0));
    }

    #[test]
    fn comments_and_cdata_are_skipped() {
        let svg = "<svg viewBox=\"0 0 10 10\"><!-- a comment with <line/> inside -->\
            <line x1=\"0\" y1=\"0\" x2=\"1\" y2=\"1\"/></svg>";
        let doc = import_svg(svg);
        assert_eq!(doc.len(), 1);
    }

    #[test]
    fn import_arc_command_roundtrips_through_centre_param() {
        // semicircle from (0,0) to (10,0), radius 5
        let svg = "<svg viewBox=\"0 0 10 10\"><path d=\"M0 0 A5 5 0 0 1 10 0\"/></svg>";
        let doc = import_svg(svg);
        let c = doc
            .iter()
            .filter_map(|e| e.as_curve().cloned())
            .next()
            .unwrap();
        match c {
            Curve::Arc(a) => assert!((a.radius - 5.0).abs() < 1e-6, "r={}", a.radius),
            other => panic!("expected circular arc, got {other:?}"),
        }
    }

    #[test]
    fn malformed_input_never_panics() {
        let cases = [
            "",
            "<",
            "<svg",
            "<svg>",
            "<svg><line x1=",
            "<svg><line x1=\"5",
            "<svg><!-- unterminated",
            "<svg><![CDATA[ junk",
            "<svg><line x1=\"abc\" y1=\"\" x2=\"px\"/></svg>",
            "<svg><path d=\"\"/></svg>",
            "<svg><path d=\"M\"/></svg>",
            "<svg><path d=\"MLLCSQTAZ\"/></svg>",
            "<svg><path d=\"M 0 0 C 1\"/></svg>",
            "<svg><path d=\"A A A A A A A\"/></svg>",
            "<svg><path d=\"m1e9 1e-9 z z z\"/></svg>",
            "<svg><polygon points=\"0\"/></svg>",
            "<svg><polyline points=\",,, ,\"/></svg>",
            "<svg><rect width=\"-5\" height=\"x\"/></svg>",
            "<svg><circle r=\"0\"/></svg>",
            "<svg><g transform=\"matrix(\"><line/></g></svg>",
            "<svg><g transform=\"rotate() scale(2 skew(\"></g></svg>",
            "</g></g></svg>",
            "<svg><g><g><g></svg>",
            "<svg><text>caf\u{e9} \u{4e2d}\u{6587}</text></svg>",
        ];
        for c in cases {
            let _ = import_svg(c); // must not panic
        }
    }

    fn close(a: (f64, f64), b: (f64, f64)) -> bool {
        (a.0 - b.0).abs() < 1e-5 && (a.1 - b.1).abs() < 1e-5
    }
}
