//! The Roch family filled crescent on an 18 by 18 logical-pixel canvas.

use std::sync::OnceLock;

use eframe::egui::{Color32, Mesh, Painter, Pos2, Rect, Shape, Stroke, pos2};

const STEPS: usize = 128;

/// Same 257-point closed contour as output/shared-controls/moon-geometry.json:
/// circle (11.875, 9), r=8, minus circle (16.375, 9), r=8.
pub fn points() -> &'static [[f32; 2]] {
    static POINTS: OnceLock<Vec<[f32; 2]>> = OnceLock::new();
    POINTS.get_or_init(build_points)
}

fn build_points() -> Vec<[f32; 2]> {
    let angle = (4.5f64 / 16.0).acos();
    let mut points = Vec::with_capacity(STEPS * 2 + 1);
    for i in 0..=STEPS {
        let t = -angle - (2.0 * std::f64::consts::PI - 2.0 * angle) * i as f64 / STEPS as f64;
        points.push([(11.875 + 8.0 * t.cos()) as f32, (9.0 + 8.0 * t.sin()) as f32]);
    }
    for i in 1..=STEPS {
        let t = std::f64::consts::PI - angle + 2.0 * angle * i as f64 / STEPS as f64;
        points.push([(16.375 + 8.0 * t.cos()) as f32, (9.0 + 8.0 * t.sin()) as f32]);
    }
    points
}

/// Fills the crescent centred in `bounds` (one logical pixel per contour unit; egui already
/// scales logical pixels by the DPI, which is what .NET's `dpi / 96` did).
pub fn paint(painter: &Painter, bounds: Rect, color: Color32) {
    let origin = pos2(bounds.min.x + (bounds.width() - 18.0) / 2.0, bounds.min.y + (bounds.height() - 18.0) / 2.0);
    let at = |p: [f32; 2]| Pos2::new(origin.x + p[0], origin.y + p[1]);
    let pts = points();
    // The crescent is concave, so it is filled as a strip between the outer arc (top tip to
    // bottom tip) and the inner arc walked back the same way; both arcs share their end points.
    let mut mesh = Mesh::default();
    for i in 0..=STEPS {
        mesh.colored_vertex(at(pts[i]), color);
        // Inner arc index STEPS + j walks bottom (j = 0, shared with outer[STEPS]) to top.
        mesh.colored_vertex(at(pts[2 * STEPS - i]), color);
    }
    for i in 0..STEPS as u32 {
        let (o0, n0, o1, n1) = (2 * i, 2 * i + 1, 2 * i + 2, 2 * i + 3);
        mesh.add_triangle(o0, o1, n0);
        mesh.add_triangle(n0, o1, n1);
    }
    painter.add(Shape::mesh(mesh));
    // A hairline outline in the same colour gives the filled mesh an anti-aliased edge.
    painter.add(Shape::closed_line(pts.iter().map(|p| at(*p)).collect(), Stroke::new(0.6, color)));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contour_has_257_points_and_closes_on_the_tips() {
        let p = points();
        assert_eq!(p.len(), 257);
        // Both arcs end on the circles' intersection points (x = 14.125).
        assert!((p[0][0] - 14.125).abs() < 1e-4 && (p[256][0] - 14.125).abs() < 1e-4);
        assert!((p[0][1] - p[256][1]).abs() < 1e-4);
        assert!((p[128][0] - 14.125).abs() < 1e-4 && (p[128][1] - p[256][1]).abs() > 1.0);
        // The outer arc passes through the left extreme of the big circle.
        assert!(p.iter().any(|q| (q[0] - 3.875).abs() < 0.01));
        // Every point lies inside the 18 x 18 canvas.
        assert!(p.iter().all(|q| q[0] >= 0.0 && q[0] <= 18.0 && q[1] >= 0.0 && q[1] <= 18.0));
    }

    #[test]
    fn strip_pairs_outer_and_inner_arcs_between_the_same_tips() {
        let p = points();
        // Outer index 0 (top tip) pairs with the inner arc's last point (top tip);
        // outer index 128 (bottom tip) pairs with the inner arc's start, which is point 128 itself.
        assert!((p[0][1] - p[256][1]).abs() < 1e-4);
        assert!(p[128][1] > 9.0 && p[0][1] < 9.0);
    }
}
