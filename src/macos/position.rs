//! AppKit uses points and a bottom-left screen origin, including on Retina displays.

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WorkArea {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl WorkArea {
    pub fn contains(self, point: Point) -> bool {
        point.x >= self.x
            && point.x < self.x + self.width
            && point.y >= self.y
            && point.y < self.y + self.height
    }
}

pub fn clamp(point: Point, width: f64, height: f64, work: WorkArea) -> Point {
    Point {
        x: point
            .x
            .max(work.x)
            .min((work.x + work.width - width).max(work.x)),
        y: point
            .y
            .max(work.y)
            .min((work.y + work.height - height).max(work.y)),
    }
}

/// Open below and to the right, switching sides before clamping to the usable screen.
pub fn near_cursor(cursor: Point, width: f64, height: f64, gap: f64, work: WorkArea) -> Point {
    let mut origin = Point {
        x: cursor.x + gap,
        y: cursor.y - gap - height,
    };
    if origin.x + width > work.x + work.width {
        origin.x = cursor.x - gap - width;
    }
    if origin.y < work.y {
        origin.y = cursor.y + gap;
    }
    clamp(origin, width, height, work)
}

#[cfg(test)]
mod tests {
    use super::*;

    const WORK: WorkArea = WorkArea {
        x: 0.0,
        y: 30.0,
        width: 1440.0,
        height: 850.0,
    };

    #[test]
    fn opens_below_cursor_in_appkit_coordinates() {
        assert_eq!(
            near_cursor(Point { x: 400.0, y: 700.0 }, 340.0, 200.0, 18.0, WORK),
            Point { x: 418.0, y: 482.0 }
        );
    }

    #[test]
    fn switches_sides_near_edges() {
        assert_eq!(
            near_cursor(Point { x: 1400.0, y: 40.0 }, 340.0, 200.0, 18.0, WORK),
            Point { x: 1042.0, y: 58.0 }
        );
    }

    #[test]
    fn handles_monitors_left_of_primary_display() {
        let work = WorkArea {
            x: -1920.0,
            y: -200.0,
            width: 1920.0,
            height: 1080.0,
        };
        let cursor = Point {
            x: -1500.0,
            y: 600.0,
        };
        let placed = near_cursor(cursor, 340.0, 200.0, 18.0, work);
        assert_eq!(
            placed,
            Point {
                x: -1482.0,
                y: 382.0
            }
        );
        assert!(work.contains(placed));
    }

    #[test]
    fn reclamps_pin_when_monitor_disappears() {
        assert_eq!(
            clamp(
                Point {
                    x: -1700.0,
                    y: -400.0
                },
                340.0,
                200.0,
                WORK
            ),
            Point { x: 0.0, y: 30.0 }
        );
    }

    #[test]
    fn leaves_cursor_uncovered_when_there_is_room() {
        for x in (0..1440).step_by(97) {
            for y in (30..880).step_by(71) {
                let cursor = Point {
                    x: f64::from(x),
                    y: f64::from(y),
                };
                let origin = near_cursor(cursor, 340.0, 200.0, 18.0, WORK);
                assert!(
                    !WorkArea {
                        x: origin.x,
                        y: origin.y,
                        width: 340.0,
                        height: 200.0
                    }
                    .contains(cursor)
                );
                assert!(origin.x >= WORK.x && origin.x + 340.0 <= WORK.x + WORK.width);
                assert!(origin.y >= WORK.y && origin.y + 200.0 <= WORK.y + WORK.height);
            }
        }
    }
}
