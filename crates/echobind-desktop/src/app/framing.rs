//! Fits the remote video inside its stage, or fills the stage and crops what
//! overflows. A filled picture pans to keep the pointer's surroundings in
//! view: a viewer in control pushes against a cropped edge to scroll it, and
//! a viewer who only watches follows the host's pointer.

use super::EchobindApp;
use eframe::egui::{self, vec2, Pos2, Rect, Vec2};

/// The band along a cropped edge that pans the view, as a share of the view
/// and then clamped, in points.
const EDGE_SHARE: f32 = 0.1;
const MIN_EDGE: f32 = 24.0;
const MAX_EDGE: f32 = 96.0;
/// Pan speed with the pointer at the very edge, in points per second.
const EDGE_SPEED: f32 = 1600.0;
/// How quickly the view catches up with the host's pointer, per second.
const FOLLOW_RATE: f32 = 10.0;

pub(super) struct Framing {
    pub(super) fill: bool,
    /// Where the view sits in the cropped overflow on each axis: 0 shows
    /// the left or top edge, 1 the right or bottom one.
    pan: Vec2,
}

impl Default for Framing {
    fn default() -> Self {
        Self {
            fill: false,
            pan: Vec2::splat(0.5),
        }
    }
}

impl Framing {
    pub(super) fn recenter(&mut self) {
        self.pan = Vec2::splat(0.5);
    }
}

impl EchobindApp {
    /// Places this frame's video in `area`, first panning a filled picture
    /// toward the pointer. The result may reach past `area`.
    pub(super) fn frame_video(
        &mut self,
        context: &egui::Context,
        area: Rect,
        source: Vec2,
    ) -> Rect {
        let video = place(source, area, self.framing.fill, self.framing.pan);
        let overflow = video.size() - area.size();
        if !self.framing.fill || (overflow.x < 0.5 && overflow.y < 0.5) {
            return video;
        }
        let dt = context.input(|input| input.stable_dt).min(0.1);
        let pointer = self
            .controlling()
            .then(|| self.steering_pointer(context, area));
        let delta = match pointer {
            Some(Some(pointer)) => vec2(
                edge_push(pointer.x, area.left(), area.right(), dt),
                edge_push(pointer.y, area.top(), area.bottom(), dt),
            ),
            Some(None) => return video,
            None => {
                let Some(cursor) = self
                    .remote_cursor
                    .filter(|cursor| cursor.visible && self.stream_width > 0)
                else {
                    return video;
                };
                let host = video.min
                    + vec2(
                        cursor.x as f32 / self.stream_width as f32,
                        cursor.y as f32 / self.stream_height.max(1) as f32,
                    ) * video.size();
                vec2(
                    follow(host.x, area.left(), area.right(), dt),
                    follow(host.y, area.top(), area.bottom(), dt),
                )
            }
        };

        let mut pan = self.framing.pan;
        for axis in 0..2 {
            if overflow[axis] >= 0.5 {
                pan[axis] = (pan[axis] + delta[axis] / overflow[axis]).clamp(0.0, 1.0);
            }
        }
        if pan == self.framing.pan {
            return video;
        }
        self.framing.pan = pan;
        context.request_repaint();
        let video = place(source, area, true, pan);
        if let Some(Some(pointer)) = pointer {
            // The picture moved under a still pointer, so the host's must too.
            self.repoint(video, pointer);
        }
        video
    }

    /// The viewer's pointer, when it is over the video or dragging there.
    fn steering_pointer(&self, context: &egui::Context, area: Rect) -> Option<Pos2> {
        let pointer = context.input(|input| input.pointer.hover_pos())?;
        let over = area.contains(pointer)
            && !self.control.blocked(pointer)
            && context
                .layer_id_at(pointer)
                .is_none_or(|layer| layer.order == egui::Order::Background);
        (over || self.control.dragging()).then_some(pointer)
    }
}

/// The rect a `source`-sized frame takes in `area`. Fit shows all of it;
/// fill covers `area`, with `pan` choosing what the crop keeps.
pub(super) fn place(source: Vec2, area: Rect, fill: bool, pan: Vec2) -> Rect {
    let scale_x = area.width() / source.x;
    let scale_y = area.height() / source.y;
    let scale = if fill {
        scale_x.max(scale_y)
    } else {
        scale_x.min(scale_y)
    }
    .max(0.01);
    let size = source * scale;
    let spare = area.size() - size;
    let anchor = vec2(
        if spare.x < 0.0 { pan.x } else { 0.5 },
        if spare.y < 0.0 { pan.y } else { 0.5 },
    );
    Rect::from_min_size(area.min + spare * anchor, size)
}

/// The part of the frame (0 to 1 on each axis) that shows in `view`.
pub(super) fn visible_uv(video: Rect, view: Rect) -> Rect {
    let size = video.size().max(Vec2::splat(1.0));
    Rect::from_min_max(
        ((view.min - video.min) / size).to_pos2(),
        ((view.max - video.min) / size).to_pos2(),
    )
}

fn edge_band(start: f32, end: f32) -> f32 {
    ((end - start) * EDGE_SHARE).clamp(MIN_EDGE, MAX_EDGE)
}

/// How far a pointer at `position` pushes the view this frame, in points.
/// It speeds up the deeper the pointer is into the edge band.
fn edge_push(position: f32, start: f32, end: f32, dt: f32) -> f32 {
    let band = edge_band(start, end);
    let depth = if position < start + band {
        (position - (start + band)) / band
    } else if position > end - band {
        (position - (end - band)) / band
    } else {
        0.0
    }
    .clamp(-1.0, 1.0);
    depth * depth.abs() * EDGE_SPEED * dt
}

/// How far the view moves this frame to bring `position` back out of the
/// edge band, easing in so a jump in the pointer is not a jump cut.
fn follow(position: f32, start: f32, end: f32, dt: f32) -> f32 {
    let band = edge_band(start, end);
    let gap = if position < start + band {
        position - (start + band)
    } else if position > end - band {
        position - (end - band)
    } else {
        0.0
    };
    gap * (1.0 - (-FOLLOW_RATE * dt).exp())
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::pos2;

    fn stage() -> Rect {
        Rect::from_min_size(pos2(0.0, 0.0), vec2(1920.0, 1080.0))
    }

    #[test]
    fn fit_letterboxes_an_ultrawide_frame() {
        let video = place(vec2(3440.0, 1440.0), stage(), false, Vec2::splat(0.5));
        assert_eq!(video.width(), 1920.0);
        assert!((video.height() - 803.7).abs() < 0.1);
        assert!((video.top() - 138.1).abs() < 0.1);
    }

    #[test]
    fn fill_covers_the_stage_and_pans_across_the_overflow() {
        let source = vec2(3440.0, 1440.0);
        let left = place(source, stage(), true, vec2(0.0, 0.5));
        let right = place(source, stage(), true, vec2(1.0, 0.5));
        assert_eq!(left.height(), 1080.0);
        assert_eq!(left.top(), 0.0);
        assert_eq!(left.left(), 0.0);
        assert!((right.right() - 1920.0).abs() < 0.01);
        let uv = visible_uv(left, left.intersect(stage()));
        assert_eq!(uv.min, pos2(0.0, 0.0));
        assert!((uv.max.x - 1920.0 / 2580.0).abs() < 0.001);
        assert!((uv.max.y - 1.0).abs() < 0.001);
    }

    #[test]
    fn fill_matches_fit_when_the_shapes_agree() {
        let source = vec2(2560.0, 1440.0);
        let fit = place(source, stage(), false, Vec2::splat(0.5));
        let fill = place(source, stage(), true, vec2(0.0, 1.0));
        assert_eq!(fit, fill);
    }

    #[test]
    fn only_the_edge_band_pushes_the_view() {
        assert_eq!(edge_push(960.0, 0.0, 1920.0, 0.1), 0.0);
        assert!(edge_push(1919.0, 0.0, 1920.0, 0.1) > 150.0);
        assert!(edge_push(1.0, 0.0, 1920.0, 0.1) < -150.0);
        let shallow = edge_push(1920.0 - 90.0, 0.0, 1920.0, 0.1);
        assert!(shallow > 0.0 && shallow < 1.0);
    }

    #[test]
    fn following_closes_the_gap_to_the_host_pointer() {
        assert_eq!(follow(960.0, 0.0, 1920.0, 0.016), 0.0);
        let step = follow(2400.0, 0.0, 1920.0, 0.016);
        let gap = 2400.0 - (1920.0 - 96.0);
        assert!(step > 0.0 && step < gap);
    }
}
