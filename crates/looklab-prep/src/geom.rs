// SPDX-License-Identifier: AGPL-3.0-or-later
//! Projective geometry for chart corners and pattern fiducials.
//!
//! Point coordinates are pixel centres: `(0.0, 0.0)` is the centre of the
//! first pixel of the image the point was measured in.

use looklab_color::Mat3;
use thiserror::Error;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

impl Point {
    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }

    pub fn distance(self, other: Self) -> f64 {
        libm::hypot(self.x - other.x, self.y - other.y)
    }

    pub fn lerp(self, other: Self, t: f64) -> Self {
        Self::new(
            self.x + (other.x - self.x) * t,
            self.y + (other.y - self.y) * t,
        )
    }

    pub fn is_finite(self) -> bool {
        self.x.is_finite() && self.y.is_finite()
    }
}

/// Four points in the order top-left, top-right, bottom-right, bottom-left.
pub type Quad = [Point; 4];

pub const UNIT_SQUARE: Quad = [
    Point::new(0.0, 0.0),
    Point::new(1.0, 0.0),
    Point::new(1.0, 1.0),
    Point::new(0.0, 1.0),
];

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum GeometryError {
    #[error("the four points do not define a projective frame")]
    Degenerate,
    #[error("a point coordinate is not finite")]
    NonFinite,
}

/// A projective 3×3 map, normalized so that `m[2][2] == 1`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Homography {
    matrix: Mat3,
}

impl Homography {
    /// The exact homography through four correspondences, by the normalized
    /// direct linear transform (Hartley 1997, "In Defense of the Eight-Point
    /// Algorithm").
    pub fn from_quad(from: &Quad, to: &Quad) -> Result<Self, GeometryError> {
        for point in from.iter().chain(to.iter()) {
            if !point.is_finite() {
                return Err(GeometryError::NonFinite);
            }
        }
        let (from_norm, from_points) = normalize_quad(from)?;
        let (to_norm, to_points) = normalize_quad(to)?;

        let mut rows = [[0.0f64; 8]; 8];
        let mut rhs = [0.0f64; 8];
        for index in 0..4 {
            let (x, y) = (from_points[index].x, from_points[index].y);
            let (u, v) = (to_points[index].x, to_points[index].y);
            rows[2 * index] = [x, y, 1.0, 0.0, 0.0, 0.0, -u * x, -u * y];
            rhs[2 * index] = u;
            rows[2 * index + 1] = [0.0, 0.0, 0.0, x, y, 1.0, -v * x, -v * y];
            rhs[2 * index + 1] = v;
        }
        let solution = solve8(rows, rhs).ok_or(GeometryError::Degenerate)?;
        let normalized = Mat3([
            [solution[0], solution[1], solution[2]],
            [solution[3], solution[4], solution[5]],
            [solution[6], solution[7], 1.0],
        ]);
        let to_inverse = to_norm.inverse().ok_or(GeometryError::Degenerate)?;
        let matrix = to_inverse.mul(&normalized).mul(&from_norm);
        let scale = matrix.0[2][2];
        if !scale.is_finite() || scale.abs() <= 1e-12 {
            return Err(GeometryError::Degenerate);
        }
        let mut matrix = matrix;
        for row in &mut matrix.0 {
            for value in row.iter_mut() {
                *value /= scale;
            }
        }
        if matrix.0.iter().flatten().any(|value| !value.is_finite()) {
            return Err(GeometryError::Degenerate);
        }
        Ok(Self { matrix })
    }

    pub fn matrix(&self) -> &Mat3 {
        &self.matrix
    }

    pub fn map(&self, point: Point) -> Option<Point> {
        let m = &self.matrix.0;
        let w = m[2][0] * point.x + m[2][1] * point.y + m[2][2];
        if !w.is_finite() || w.abs() <= 1e-12 {
            return None;
        }
        let x = (m[0][0] * point.x + m[0][1] * point.y + m[0][2]) / w;
        let y = (m[1][0] * point.x + m[1][1] * point.y + m[1][2]) / w;
        let mapped = Point::new(x, y);
        mapped.is_finite().then_some(mapped)
    }

    pub fn inverse(&self) -> Option<Self> {
        self.matrix.inverse().map(|matrix| Self { matrix })
    }

    /// Root mean square distance between `mapped` and `expected` over four
    /// correspondences, in pixels.
    pub fn reprojection_error(&self, from: &Quad, to: &Quad) -> Option<f64> {
        let mut sum = 0.0;
        for index in 0..4 {
            let mapped = self.map(from[index])?;
            let delta = mapped.distance(to[index]);
            sum += delta * delta;
        }
        Some((sum / 4.0).sqrt())
    }
}

fn normalize_quad(quad: &Quad) -> Result<(Mat3, Quad), GeometryError> {
    let mut cx = 0.0;
    let mut cy = 0.0;
    for point in quad {
        cx += point.x;
        cy += point.y;
    }
    cx *= 0.25;
    cy *= 0.25;
    let mut mean = 0.0;
    for point in quad {
        mean += libm::hypot(point.x - cx, point.y - cy);
    }
    mean *= 0.25;
    if !mean.is_finite() || mean <= 1e-9 {
        return Err(GeometryError::Degenerate);
    }
    let scale = 2.0f64.sqrt() / mean;
    let matrix = Mat3([
        [scale, 0.0, -scale * cx],
        [0.0, scale, -scale * cy],
        [0.0, 0.0, 1.0],
    ]);
    let points = quad.map(|point| Point::new(scale * (point.x - cx), scale * (point.y - cy)));
    Ok((matrix, points))
}

/// Gaussian elimination with partial pivoting.
fn solve8(mut rows: [[f64; 8]; 8], mut rhs: [f64; 8]) -> Option<[f64; 8]> {
    for column in 0..8 {
        let mut pivot = column;
        for row in (column + 1)..8 {
            if rows[row][column].abs() > rows[pivot][column].abs() {
                pivot = row;
            }
        }
        let pivot_value = rows[pivot][column];
        if !pivot_value.is_finite() || pivot_value.abs() <= 1e-12 {
            return None;
        }
        rows.swap(column, pivot);
        rhs.swap(column, pivot);
        let inverse = 1.0 / rows[column][column];
        for value in &mut rows[column][column..] {
            *value *= inverse;
        }
        rhs[column] *= inverse;
        let pivot_row = rows[column];
        for row in 0..8 {
            if row == column {
                continue;
            }
            let factor = rows[row][column];
            if factor == 0.0 {
                continue;
            }
            for (target, source) in rows[row][column..].iter_mut().zip(&pivot_row[column..]) {
                *target -= factor * source;
            }
            rhs[row] -= factor * rhs[column];
        }
    }
    Some(rhs)
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Line {
    pub origin: Point,
    pub direction: Point,
}

impl Line {
    pub fn through(a: Point, b: Point) -> Option<Self> {
        let dx = b.x - a.x;
        let dy = b.y - a.y;
        let length = libm::hypot(dx, dy);
        if !length.is_finite() || length <= 1e-9 {
            return None;
        }
        Some(Self {
            origin: a,
            direction: Point::new(dx / length, dy / length),
        })
    }

    pub fn distance_to(&self, point: Point) -> f64 {
        let dx = point.x - self.origin.x;
        let dy = point.y - self.origin.y;
        (dx * self.direction.y - dy * self.direction.x).abs()
    }

    pub fn intersection(&self, other: &Line) -> Option<Point> {
        let denom = self.direction.x * other.direction.y - self.direction.y * other.direction.x;
        if !denom.is_finite() || denom.abs() <= 1e-9 {
            return None;
        }
        let dx = other.origin.x - self.origin.x;
        let dy = other.origin.y - self.origin.y;
        let t = (dx * other.direction.y - dy * other.direction.x) / denom;
        let point = Point::new(
            self.origin.x + self.direction.x * t,
            self.origin.y + self.direction.y * t,
        );
        point.is_finite().then_some(point)
    }
}

/// Total-least-squares line: the principal axis of the points' covariance.
pub fn fit_line(points: &[Point]) -> Option<Line> {
    if points.len() < 2 {
        return None;
    }
    let count = points.len() as f64;
    let mut cx = 0.0;
    let mut cy = 0.0;
    for point in points {
        cx += point.x;
        cy += point.y;
    }
    cx /= count;
    cy /= count;
    let mut sxx = 0.0;
    let mut syy = 0.0;
    let mut sxy = 0.0;
    for point in points {
        let dx = point.x - cx;
        let dy = point.y - cy;
        sxx += dx * dx;
        syy += dy * dy;
        sxy += dx * dy;
    }
    if !sxx.is_finite() || !syy.is_finite() || !sxy.is_finite() {
        return None;
    }
    if sxx + syy <= 1e-12 {
        return None;
    }
    let angle = 0.5 * libm::atan2(2.0 * sxy, sxx - syy);
    Some(Line {
        origin: Point::new(cx, cy),
        direction: Point::new(libm::cos(angle), libm::sin(angle)),
    })
}

/// Signed area of a quad by the shoelace formula; positive for clockwise order
/// in image coordinates (y down).
pub fn quad_area(quad: &Quad) -> f64 {
    let mut sum = 0.0;
    for index in 0..4 {
        let a = quad[index];
        let b = quad[(index + 1) % 4];
        sum += a.x * b.y - b.x * a.y;
    }
    sum * 0.5
}

/// Convex containment for a quad in either winding order.
pub fn quad_contains(quad: &Quad, point: Point) -> bool {
    if !point.is_finite() {
        return false;
    }
    let area = quad_area(quad);
    if area == 0.0 || !area.is_finite() {
        return false;
    }
    let sign = if area > 0.0 { 1.0 } else { -1.0 };
    for index in 0..4 {
        let a = quad[index];
        let b = quad[(index + 1) % 4];
        let cross = (b.x - a.x) * (point.y - a.y) - (b.y - a.y) * (point.x - a.x);
        if cross * sign < 0.0 {
            return false;
        }
    }
    true
}

pub fn quad_is_convex(quad: &Quad) -> bool {
    let mut positive = false;
    let mut negative = false;
    for index in 0..4 {
        let a = quad[index];
        let b = quad[(index + 1) % 4];
        let c = quad[(index + 2) % 4];
        let cross = (b.x - a.x) * (c.y - b.y) - (b.y - a.y) * (c.x - b.x);
        if cross > 1e-9 {
            positive = true;
        } else if cross < -1e-9 {
            negative = true;
        }
    }
    positive != negative
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn homography_maps_the_unit_square_onto_its_quad() {
        let quad = [
            Point::new(12.0, 20.0),
            Point::new(220.0, 14.0),
            Point::new(240.0, 180.0),
            Point::new(8.0, 172.0),
        ];
        let homography = Homography::from_quad(&UNIT_SQUARE, &quad).expect("quad is valid");
        for index in 0..4 {
            let mapped = homography.map(UNIT_SQUARE[index]).expect("mappable");
            assert!(mapped.distance(quad[index]) < 1e-9);
        }
        let inverse = homography.inverse().expect("invertible");
        let round_trip = inverse.map(Point::new(100.0, 90.0)).expect("mappable");
        let back = homography.map(round_trip).expect("mappable");
        assert!(back.distance(Point::new(100.0, 90.0)) < 1e-6);
        assert!(homography.reprojection_error(&UNIT_SQUARE, &quad).unwrap() < 1e-9);
    }

    #[test]
    fn a_collinear_quad_has_no_homography() {
        let degenerate = [
            Point::new(0.0, 0.0),
            Point::new(1.0, 1.0),
            Point::new(2.0, 2.0),
            Point::new(3.0, 3.0),
        ];
        assert_eq!(
            Homography::from_quad(&UNIT_SQUARE, &degenerate),
            Err(GeometryError::Degenerate)
        );
    }

    #[test]
    fn quad_containment_accepts_both_windings() {
        let clockwise = [
            Point::new(0.0, 0.0),
            Point::new(10.0, 0.0),
            Point::new(10.0, 10.0),
            Point::new(0.0, 10.0),
        ];
        let counter_clockwise = [
            Point::new(0.0, 0.0),
            Point::new(0.0, 10.0),
            Point::new(10.0, 10.0),
            Point::new(10.0, 0.0),
        ];
        for quad in [clockwise, counter_clockwise] {
            assert!(quad_contains(&quad, Point::new(5.0, 5.0)));
            assert!(!quad_contains(&quad, Point::new(15.0, 5.0)));
            assert!(quad_is_convex(&quad));
        }
    }

    #[test]
    fn total_least_squares_fits_vertical_lines() {
        let points: Vec<Point> = (0..10)
            .map(|i| Point::new(5.0, f64::from(i) + 0.1 * f64::from(i % 2)))
            .collect();
        let line = fit_line(&points).expect("points span a line");
        assert!(line.direction.y.abs() > 0.99);
        assert!(line.distance_to(Point::new(5.0, 4.5)) < 0.2);
    }
}
