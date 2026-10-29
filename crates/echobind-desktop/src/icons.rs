//! Stroke icons drawn on a 24-unit grid, so the UI needs no SVG loader or
//! icon font.

use eframe::egui::{pos2, Color32, CornerRadius, Painter, Pos2, Rect, Shape, Stroke, StrokeKind};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Icon {
    Logo,
    Eye,
    Share,
    Monitor,
    Refresh,
    Volume,
    Activity,
    Maximize,
    Close,
    Check,
    Stop,
    Alert,
    AlertCircle,
    ChevronDown,
    ChevronUp,
}

struct Grid {
    origin: Pos2,
    scale: f32,
}

impl Grid {
    fn at(&self, x: f32, y: f32) -> Pos2 {
        pos2(
            self.origin.x + x * self.scale,
            self.origin.y + y * self.scale,
        )
    }

    fn points(&self, points: &[(f32, f32)]) -> Vec<Pos2> {
        points.iter().map(|&(x, y)| self.at(x, y)).collect()
    }

    /// Samples a circular arc; angles are in degrees, clockwise from +x.
    fn arc(&self, center: (f32, f32), radius: f32, from: f32, to: f32) -> Vec<Pos2> {
        let steps = ((to - from).abs() / 8.0).ceil().max(2.0) as usize;
        (0..=steps)
            .map(|step| {
                let angle = (from + (to - from) * step as f32 / steps as f32).to_radians();
                self.at(
                    center.0 + radius * angle.cos(),
                    center.1 + radius * angle.sin(),
                )
            })
            .collect()
    }

    fn cubic(&self, points: [(f32, f32); 4]) -> Vec<Pos2> {
        let [p0, p1, p2, p3] = points;
        (0..=12)
            .map(|step| {
                let t = step as f32 / 12.0;
                let u = 1.0 - t;
                let x = u * u * u * p0.0
                    + 3.0 * u * u * t * p1.0
                    + 3.0 * u * t * t * p2.0
                    + t * t * t * p3.0;
                let y = u * u * u * p0.1
                    + 3.0 * u * u * t * p1.1
                    + 3.0 * u * t * t * p2.1
                    + t * t * t * p3.1;
                self.at(x, y)
            })
            .collect()
    }
}

pub fn paint(painter: &Painter, rect: Rect, icon: Icon, color: Color32) {
    let scale = rect.width().min(rect.height()) / 24.0;
    let grid = Grid {
        origin: rect.center() - eframe::egui::Vec2::splat(12.0 * scale),
        scale,
    };
    let stroke = Stroke::new((2.0 * scale).max(1.25), color);
    let line = |points: Vec<Pos2>| {
        painter.add(Shape::line(points, stroke));
    };
    let segment = |from: (f32, f32), to: (f32, f32)| {
        painter.line_segment([grid.at(from.0, from.1), grid.at(to.0, to.1)], stroke);
    };
    let dot = |x: f32, y: f32, radius: f32| {
        painter.circle_filled(grid.at(x, y), radius * scale, color);
    };
    let rounded_rect = |min: (f32, f32), max: (f32, f32), radius: f32| {
        painter.rect_stroke(
            Rect::from_min_max(grid.at(min.0, min.1), grid.at(max.0, max.1)),
            CornerRadius::same((radius * scale).round() as u8),
            stroke,
            StrokeKind::Middle,
        );
    };

    match icon {
        Icon::Logo => {
            dot(6.5, 12.0, 2.5);
            line(grid.arc((6.81, 12.0), 6.5, -43.8, 43.8));
            line(grid.arc((7.95, 12.0), 11.0, -46.7, 46.7));
        }
        Icon::Eye => {
            let mut outline = grid.cubic([(2.0, 12.0), (2.0, 12.0), (5.5, 5.0), (12.0, 5.0)]);
            outline.extend(grid.cubic([(12.0, 5.0), (18.5, 5.0), (22.0, 12.0), (22.0, 12.0)]));
            outline.extend(grid.cubic([(22.0, 12.0), (22.0, 12.0), (18.5, 19.0), (12.0, 19.0)]));
            outline.extend(grid.cubic([(12.0, 19.0), (5.5, 19.0), (2.0, 12.0), (2.0, 12.0)]));
            painter.add(Shape::closed_line(outline, stroke));
            painter.circle_stroke(grid.at(12.0, 12.0), 3.0 * scale, stroke);
        }
        Icon::Share => {
            let mut outline = grid.points(&[(13.0, 3.0), (4.0, 3.0)]);
            outline.extend(grid.arc((4.0, 5.0), 2.0, -90.0, -180.0));
            outline.extend(grid.arc((4.0, 15.0), 2.0, 180.0, 90.0));
            outline.extend(grid.arc((20.0, 15.0), 2.0, 90.0, 0.0));
            outline.push(grid.at(22.0, 12.0));
            line(outline);
            segment((8.0, 21.0), (16.0, 21.0));
            segment((12.0, 17.0), (12.0, 21.0));
            segment((17.0, 8.0), (22.0, 3.0));
            line(grid.points(&[(17.0, 3.0), (22.0, 3.0), (22.0, 8.0)]));
        }
        Icon::Monitor => {
            rounded_rect((2.0, 3.0), (22.0, 17.0), 2.0);
            segment((8.0, 21.0), (16.0, 21.0));
            segment((12.0, 17.0), (12.0, 21.0));
        }
        Icon::Refresh => {
            let mut arc = grid.arc((12.0, 12.0), 9.0, 0.0, 312.0);
            arc.push(grid.at(21.0, 8.0));
            line(arc);
            line(grid.points(&[(21.0, 3.0), (21.0, 8.0), (16.0, 8.0)]));
        }
        Icon::Volume => {
            painter.add(Shape::closed_line(
                grid.points(&[
                    (11.0, 5.0),
                    (6.0, 9.0),
                    (2.0, 9.0),
                    (2.0, 15.0),
                    (6.0, 15.0),
                    (11.0, 19.0),
                ]),
                stroke,
            ));
            line(grid.arc((11.93, 12.0), 5.0, -44.4, 44.4));
            line(grid.arc((11.86, 12.0), 10.0, -44.4, 44.4));
        }
        Icon::Activity => line(grid.points(&[
            (22.0, 12.0),
            (18.0, 12.0),
            (15.0, 21.0),
            (9.0, 3.0),
            (6.0, 12.0),
            (2.0, 12.0),
        ])),
        Icon::Maximize => {
            for (start, center, from, to, end) in [
                ((8.0, 3.0), (5.0, 5.0), -90.0, -180.0, (3.0, 8.0)),
                ((21.0, 8.0), (19.0, 5.0), 0.0, -90.0, (16.0, 3.0)),
                ((3.0, 16.0), (5.0, 19.0), 180.0, 90.0, (8.0, 21.0)),
                ((16.0, 21.0), (19.0, 19.0), 90.0, 0.0, (21.0, 16.0)),
            ] {
                let mut corner = vec![grid.at(start.0, start.1)];
                corner.extend(grid.arc(center, 2.0, from, to));
                corner.push(grid.at(end.0, end.1));
                line(corner);
            }
        }
        Icon::Close => {
            segment((18.0, 6.0), (6.0, 18.0));
            segment((6.0, 6.0), (18.0, 18.0));
        }
        Icon::Check => line(grid.points(&[(20.0, 6.0), (9.0, 17.0), (4.0, 12.0)])),
        Icon::Stop => rounded_rect((6.0, 6.0), (18.0, 18.0), 2.0),
        Icon::Alert => {
            painter.add(Shape::closed_line(
                grid.points(&[(12.0, 3.0), (22.0, 20.0), (2.0, 20.0)]),
                stroke,
            ));
            segment((12.0, 9.0), (12.0, 13.0));
            dot(12.0, 16.8, 1.2);
        }
        Icon::AlertCircle => {
            painter.circle_stroke(grid.at(12.0, 12.0), 10.0 * scale, stroke);
            segment((12.0, 8.0), (12.0, 12.0));
            dot(12.0, 15.8, 1.2);
        }
        Icon::ChevronDown => line(grid.points(&[(6.0, 9.0), (12.0, 15.0), (18.0, 9.0)])),
        Icon::ChevronUp => line(grid.points(&[(6.0, 15.0), (12.0, 9.0), (18.0, 15.0)])),
    }
}
