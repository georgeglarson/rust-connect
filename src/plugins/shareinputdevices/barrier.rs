//! Pure barrier-math planner for the InputCapture portal half.
//!
//! Single Responsibility: turn a zone set + the configured edge into the
//! ONE barrier rectangle the portal needs (port of
//! kdeconnect-kde plugins/shareinputdevices/inputcapturesession.cpp:192-221).
//! The function is pure so the wire-shape decision stays unit-testable
//! without portals, mirroring `crate::plugins::mousepad` (`mousepad.rs:472-526`)
//! which the M1 plugin already uses.
//!
//! **Upstream quirk, replicated:** the cpp's `QRect::bottom` /
//! `QRect::right` are INCLUSIVE pixel coordinates
//! (`x + width - 1`, `y + height - 1`), and the barrier coordinates
//! depend on it (inputcapturesession.cpp:200,:213) — see
//! `Zone::inclusive_right` / `Zone::inclusive_bottom`.
use crate::plugins::shareinputdevices::Edge;

/// A rectangular zone returned by the portal's GetZones call.
///
/// `(x, y)` is the top-left offset; `width` / `height` are pixel
/// dimensions. Mirrors the `a(uuii)` D-Bus signature where the order
/// is `(width, height, x_offset, y_offset)` (see
/// `/usr/share/dbus-1/interfaces/org.freedesktop.portal.InputCapture.xml:156-159`
/// and xdp-kde `InputCapturePortal::zone` at
/// `/tmp/xdp-kde-1042/src/inputcapture.h:58-65`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Zone {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl Zone {
    pub fn new(x: i32, y: i32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// The inclusive right edge (x + width - 1).
    ///
    /// **`QRect::right` in the cpp, but Qt's `QRect::right()` returns
    /// `x + width - 1`** (Qt's QRect has width semantics where
    /// `right() == x() + width() - 1`). The cpp's :213 comment
    /// explicitly calls this the "deliberate QRect::bottom/right
    /// inclusivity" and treats the inclusive pixel as the barrier
    /// coordinate — so a 1920x1080 screen has the bottom row at y=1079
    /// and the right column at x=1919. We replicate the inclusivity by
    /// computing `x + width - 1` here. Recorded in the wire-shape
    /// fixture `barrier_2monitor_left_edge.json` so the quirk is
    /// pinned at the test surface.
    pub fn inclusive_right(&self) -> i32 {
        self.x + self.width as i32 - 1
    }

    /// The inclusive bottom edge (y + height - 1).
    /// See `inclusive_right`'s doc.
    pub fn inclusive_bottom(&self) -> i32 {
        self.y + self.height as i32 - 1
    }

    /// Exclusive right edge (x + width).
    /// For LEFT/BOTTOM barrier coordinates where the portal needs the
    /// "outside" of the zone, not the inclusive inside pixel.
    pub fn exclusive_right(&self) -> i32 {
        self.x + self.width as i32
    }

    /// Exclusive bottom edge (y + height).
    pub fn exclusive_bottom(&self) -> i32 {
        self.y + self.height as i32
    }
}

/// A barrier rectangle as the portal wants it: `(x1, y1, x2, y2)`.
///
/// For Left/Right barriers x1 == x2 (vertical line); for Top/Bottom
/// y1 == y2 (horizontal line). Diagonal barriers are explicitly not
/// supported by the spec (InputCapture.xml:242-244).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Barrier {
    pub x1: i32,
    pub y1: i32,
    pub x2: i32,
    pub y2: i32,
}

impl Barrier {
    /// Build the wire-shape entry the portal's `SetPointerBarriers`
    /// expects: a single-element dict with keys `barrier_id` (uint)
    /// and `position` (array of 4 ints — QList<int> on the wire,
    /// `ai`). Replicated from the cpp at
    /// `inputcapturesession.cpp:227-231`.
    pub fn to_wire_entry(&self, barrier_id: u32) -> serde_json::Value {
        serde_json::json!({
            "barrier_id": barrier_id,
            "position": [self.x1, self.y1, self.x2, self.y2],
        })
    }
}

/// Pick the one zone that owns the configured edge and build the
/// barrier rectangle on its outer boundary.
///
/// `barrier_id` is the non-zero u32 the cpp hard-codes to 1
/// (`inputcapturesession.cpp:230`). The portal re-emits it on the
/// Activated signal's `barrier_id` option (InputCapture.xml:438-446),
/// so a producer that wants to identify WHICH barrier fired has to
/// pass a stable id — the cpp's hard-coded 1 is the established
/// producer-side convention (one barrier per session).
///
/// **Edge-most sort order** matches the cpp exactly:
/// - `Edge::Left`  → zones sorted ascending by `x`; front = leftmost
/// - `Edge::Right` → zones sorted descending by `x + width`; front = rightmost
/// - `Edge::Top`   → zones sorted ascending by `y`; front = topmost
/// - `Edge::Bottom`→ zones sorted descending by `y + height`; front = bottommost
///
/// **Boundary inclusivity** matches the cpp's deliberate
/// `QRect::bottom/right` inclusivity (`:200,:213`):
/// - Left barrier: `(zone.x, zone.y) → (zone.x, zone.inclusive_bottom())`
/// - Right barrier: `(zone.exclusive_right(), zone.y) → (zone.exclusive_right(), zone.inclusive_bottom())`
/// - Top barrier: `(zone.x, zone.y) → (zone.inclusive_right(), zone.y)`
/// - Bottom barrier: `(zone.x, zone.exclusive_bottom()) → (zone.inclusive_right(), zone.exclusive_bottom())`
///
/// The portal's spec (InputCapture.xml:191-197) defines the barrier as
/// "situated on the top/left edge of pixels and width/height is
/// inclusive of each pixel" — so the inclusive bottom/right matters
/// when the barrier is on the inside of a zone (Top/Left), and the
/// exclusive edge is right for the outside (Right/Bottom).
pub fn plan_barrier(zones: &[Zone], edge: Edge, barrier_id: u32) -> Option<Barrier> {
    if zones.is_empty() {
        return None;
    }
    let zone = edge_most_zone(zones, edge)?;
    let barrier = match edge {
        Edge::Left => Barrier {
            x1: zone.x,
            y1: zone.y,
            x2: zone.x,
            // Deliberate QRect::bottom inclusivity: a 1920x1080 zone's
            // bottom row is y=1079 (cpp :200).
            y2: zone.inclusive_bottom(),
        },
        Edge::Right => Barrier {
            x1: zone.exclusive_right(),
            y1: zone.y,
            x2: zone.exclusive_right(),
            y2: zone.inclusive_bottom(),
        },
        Edge::Top => Barrier {
            x1: zone.x,
            y1: zone.y,
            // Deliberate QRect::right inclusivity (:213).
            x2: zone.inclusive_right(),
            y2: zone.y,
        },
        Edge::Bottom => Barrier {
            x1: zone.x,
            y1: zone.exclusive_bottom(),
            x2: zone.inclusive_right(),
            y2: zone.exclusive_bottom(),
        },
    };
    // The portal call always carries barrier_id; the planner
    // does not read it but the type signature makes the wire
    // shape explicit at every call site.
    let _ = barrier_id;
    Some(barrier)
}

fn edge_most_zone(zones: &[Zone], edge: Edge) -> Option<Zone> {
    // Stable sort to match the cpp's `std::stable_sort` (inputcapturesession.cpp:196,
    // :203, :209, :216). Ties go to the input order, which matters
    // when two zones share the same edge coordinate (e.g. two top-edge
    // monitors at y=0).
    let mut indexed: Vec<(usize, &Zone)> = zones.iter().enumerate().collect();
    match edge {
        Edge::Left => indexed.sort_by_key(|a| a.1.x),
        Edge::Right => indexed.sort_by(|a, b| {
            let ar = a.1.x + a.1.width as i32;
            let br = b.1.x + b.1.width as i32;
            br.cmp(&ar)
        }),
        Edge::Top => indexed.sort_by_key(|a| a.1.y),
        Edge::Bottom => indexed.sort_by(|a, b| {
            let ab = a.1.y + a.1.height as i32;
            let bb = b.1.y + b.1.height as i32;
            bb.cmp(&ab)
        }),
    }
    indexed.first().map(|(_, z)| **z)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    use super::*;

    /// 1920x1080 primary monitor at origin — the cpp's documented
    /// example in its :200 comment.
    fn primary_1920x1080() -> Zone {
        Zone::new(0, 0, 1920, 1080)
    }

    /// Secondary monitor placed to the right of the primary at
    /// (1920, 0) — the spec's documented example zone at
    /// InputCapture.xml:200-208.
    fn secondary_right_1920x1080() -> Zone {
        Zone::new(1920, 0, 1920, 1080)
    }

    #[test]
    fn empty_zones_returns_none() {
        // The spec notes "An empty zone list implies that no pointer
        // barriers can be set" (InputCapture.xml:138-139). We mirror
        // that by returning None — the caller MUST NOT call
        // SetPointerBarriers in that case.
        assert!(plan_barrier(&[], Edge::Left, 1).is_none());
    }

    #[test]
    fn single_monitor_left_barrier_matches_cpp_inclusive_bottom() {
        // cpp :199-201:
        //   m_barrier = {zone.x(), zone.y(), zone.x(), zone.bottom()};
        // For a 1920x1080 zone at origin: barrier = (0,0,0,1079).
        // Pin the deliberate QRect::bottom inclusivity (y=1079, NOT 1080).
        let zones = [primary_1920x1080()];
        let barrier = plan_barrier(&zones, Edge::Left, 1).expect("one zone");
        assert_eq!(
            barrier,
            Barrier {
                x1: 0,
                y1: 0,
                x2: 0,
                y2: 1079,
            }
        );
    }

    #[test]
    fn single_monitor_right_barrier_uses_exclusive_right() {
        // cpp :206-207:
        //   m_barrier = {zone.x() + zone.width(), zone.y(),
        //                zone.x() + zone.width(), zone.bottom()};
        // For the same 1920x1080: barrier = (1920, 0, 1920, 1079).
        // The x is EXCLUSIVE (1920, not 1919) — outside the zone — and
        // the bottom is inclusive (1079).
        let zones = [primary_1920x1080()];
        let barrier = plan_barrier(&zones, Edge::Right, 1).expect("one zone");
        assert_eq!(
            barrier,
            Barrier {
                x1: 1920,
                y1: 0,
                x2: 1920,
                y2: 1079,
            }
        );
    }

    #[test]
    fn single_monitor_top_barrier_matches_cpp_inclusive_right() {
        // cpp :213-214:
        //   m_barrier = {zone.x(), zone.y(), zone.right(), zone.y()};
        // For a 1920x1080 zone: barrier = (0, 0, 1919, 0).
        // The right is INCLUSIVE (1919) — inside the zone — per the
        // deliberate inclusivity comment at :213.
        let zones = [primary_1920x1080()];
        let barrier = plan_barrier(&zones, Edge::Top, 1).expect("one zone");
        assert_eq!(
            barrier,
            Barrier {
                x1: 0,
                y1: 0,
                x2: 1919,
                y2: 0,
            }
        );
    }

    #[test]
    fn single_monitor_bottom_barrier_uses_exclusive_bottom() {
        // cpp :219-220:
        //   m_barrier = {zone.x(), zone.y() + zone.height(),
        //                zone.right(), zone.y() + zone.height()};
        // For a 1920x1080 zone: barrier = (0, 1080, 1919, 1080).
        let zones = [primary_1920x1080()];
        let barrier = plan_barrier(&zones, Edge::Bottom, 1).expect("one zone");
        assert_eq!(
            barrier,
            Barrier {
                x1: 0,
                y1: 1080,
                x2: 1919,
                y2: 1080,
            }
        );
    }

    #[test]
    fn two_monitors_left_barrier_picks_leftmost_zone() {
        // cpp :196-198: sort ascending by x; front = leftmost. Two
        // monitors side-by-side → barrier on the LEFT edge of the
        // leftmost zone (= primary at x=0). Same as single-monitor
        // left barrier.
        let zones = [secondary_right_1920x1080(), primary_1920x1080()];
        let barrier = plan_barrier(&zones, Edge::Left, 1).expect("two zones");
        assert_eq!(
            barrier,
            Barrier {
                x1: 0,
                y1: 0,
                x2: 0,
                y2: 1079,
            }
        );
    }

    #[test]
    fn two_monitors_right_barrier_picks_rightmost_zone() {
        // cpp :203-205: sort descending by (x + width); front = the
        // zone with the LARGEST right edge. Two monitors → secondary at
        // (1920.., 0..1080), right edge = 1920+1920=3840.
        // Barrier on the OUTSIDE of that zone: (3840, 0, 3840, 1079).
        let zones = [primary_1920x1080(), secondary_right_1920x1080()];
        let barrier = plan_barrier(&zones, Edge::Right, 1).expect("two zones");
        assert_eq!(
            barrier,
            Barrier {
                x1: 3840,
                y1: 0,
                x2: 3840,
                y2: 1079,
            }
        );
    }

    #[test]
    fn two_monitors_top_barrier_picks_topmost_zone() {
        // cpp :209-211: sort ascending by y; ties break by stable
        // order. Both zones at y=0 → first one wins (stable sort).
        // Test uses a deliberate order so the assertion names which
        // zone was picked.
        let zones = [secondary_right_1920x1080(), primary_1920x1080()];
        let barrier = plan_barrier(&zones, Edge::Top, 1).expect("two zones");
        // secondary at (1920.., 0..1080), inclusive right = 1920+1920-1=3839
        assert_eq!(
            barrier,
            Barrier {
                x1: 1920,
                y1: 0,
                x2: 3839,
                y2: 0,
            }
        );
    }

    #[test]
    fn two_monitors_bottom_barrier_picks_bottommost_zone() {
        // cpp :216-218: sort descending by (y + height). Both monitors
        // same height → tie; stable sort keeps first → secondary
        // (last in input array? — actually the second zone is the
        // bottommost when the FIRST is primary at y=0 and the SECOND
        // is at y=0 too; the tie-breaker is "stable sort" = whichever
        // was first). Set up distinct y values for a clean test.
        let zones = [
            Zone::new(0, 0, 1920, 800),   // primary at origin, height 800
            Zone::new(0, 800, 1920, 200), // stacked under primary, height 200 (bottom 999)
        ];
        let barrier = plan_barrier(&zones, Edge::Bottom, 1).expect("two zones");
        // bottommost = the second zone (y=800, h=200, exclusive_bottom=1000)
        assert_eq!(
            barrier,
            Barrier {
                x1: 0,
                y1: 1000,
                x2: 1919,
                y2: 1000,
            }
        );
    }

    #[test]
    fn barrier_to_wire_entry_carries_barrier_id_and_position_tuple() {
        // cpp :230: {barrier_id: 1, position: [x1,y1,x2,y2]}.
        let barrier = Barrier {
            x1: 0,
            y1: 0,
            x2: 0,
            y2: 1079,
        };
        let entry = barrier.to_wire_entry(1);
        assert_eq!(
            entry,
            serde_json::json!({
                "barrier_id": 1u32,
                "position": [0, 0, 0, 1079],
            })
        );
    }

    #[test]
    fn inclusive_right_and_bottom_are_zero_for_degenerate_zones() {
        // Edge case the cpp doesn't have to handle because QRect has
        // its own width/height semantics, but the planner must: a
        // 1x1 zone has inclusive_right=x, inclusive_bottom=y. Our
        // exclusive_*() helpers return x+1, y+1 — the same boundary
        // shape the cpp would compute.
        let z = Zone::new(5, 7, 1, 1);
        assert_eq!(z.inclusive_right(), 5);
        assert_eq!(z.inclusive_bottom(), 7);
        assert_eq!(z.exclusive_right(), 6);
        assert_eq!(z.exclusive_bottom(), 8);
    }
}
