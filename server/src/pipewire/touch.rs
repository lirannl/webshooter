use std::collections::HashSet;

use reis::ei;

use super::eis::with_emulation;

/// How many touch slots one session may occupy in the compositor's namespace.
///
/// The client picks its own slot ids (0, 1, 2 …) and every session starts at
/// zero, so concurrent sessions collide unless their ranges are kept apart.
pub(crate) const SLOTS_PER_SESSION: u32 = 64;

/// A touch slot is identified by a plain integer, and libei does not say those
/// integers have to be unique across devices — so the same id arriving from two
/// devices means whatever the compositor decides it means. Qt and Wayland treat
/// a touch point id as one touch point for the whole seat, not one per emulated
/// device, which is why two sessions both pressing "slot 0" cancel each other:
/// the second press is the first slot being pressed again.
///
/// Giving each session its own range of ids keeps the two apart without
/// changing anything the client sends, and without asking the compositor to
/// behave differently — the ids stay opaque. The cost is a bounded one: a
/// client that claims a slot outside its range is ignored, because honouring it
/// would put it back in another session's range.
pub(crate) fn shift_slots(event: EisTouchEvent, base: u32) -> Option<EisTouchEvent> {
    let within =
        |index: u32| -> Option<u32> { (index < SLOTS_PER_SESSION).then_some(base + index) };
    Some(match event {
        EisTouchEvent::Down { x, y, index } => EisTouchEvent::Down {
            x,
            y,
            index: within(index)?,
        },
        EisTouchEvent::Motion { x, y, index } => EisTouchEvent::Motion {
            x,
            y,
            index: within(index)?,
        },
        EisTouchEvent::Up { index } => EisTouchEvent::Up {
            index: within(index)?,
        },
    })
}

#[derive(Debug, PartialEq, Eq)]
pub enum EisTouchEvent {
    Down { x: u16, y: u16, index: u32 },
    Motion { x: u16, y: u16, index: u32 },
    Up { index: u32 },
}

pub struct TouchState {
    active_slots: HashSet<u8>,
}

impl TouchState {
    pub fn new() -> Self {
        Self {
            active_slots: Default::default(),
        }
    }

    pub fn handle_touch(&mut self, index: u8, x: u16, y: u16) -> Vec<EisTouchEvent> {
        let is_new = self.active_slots.insert(index);
        let index = index.into();
        vec![if is_new {
            EisTouchEvent::Down { x, y, index }
        } else {
            EisTouchEvent::Motion { x, y, index }
        }]
    }

    pub fn handle_release(&mut self, index: u8) -> Option<EisTouchEvent> {
        if self.active_slots.remove(&index) {
            Some(EisTouchEvent::Up {
                index: index.into(),
            })
        } else {
            None
        }
    }

    pub fn release_all(&mut self) -> Vec<EisTouchEvent> {
        self.active_slots
            .drain()
            .map(|index| EisTouchEvent::Up {
                index: index.into(),
            })
            .collect()
    }
}

/// Maps coordinates the client reports into the space libei accepts.
///
/// The client reports in **frame pixels**: it normalises each touch against its
/// canvas, whose backing store it sizes from the frames it decodes, so a
/// coordinate is a fraction of the picture the user is looking at. That is the
/// right space to receive — it survives the client's resolution and its device
/// pixel ratio, both of which say nothing about the host's input devices.
///
/// libei, however, accepts coordinates in the space the portal *reported* for
/// the shared region, and those are not the same numbers. The portal reports a
/// region's size in logical units, divided by the output's scale, while the
/// frames themselves stay physical. Measured on a 150%-scaled desktop: the
/// monitor and the encoder's frames are both 1080x2255, and the portal reports
/// that region as 720x1503 — a factor of exactly 1.5. Passing the client's
/// coordinates through untouched therefore lands every touch at two thirds of
/// where it was aimed, with no error anywhere to explain it.
///
/// Both sizes are needed to bridge the two spaces, and only the server has both.
/// A `None` means they were the same, or that one was missing; either way the
/// identity is the safe answer, and is what this did before there was any
/// scaling to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CoordMap {
    frame_w: u16,
    frame_h: u16,
    portal_w: u16,
    portal_h: u16,
}

impl CoordMap {
    pub fn new(frame: (u16, u16), portal: (u16, u16)) -> Self {
        Self {
            frame_w: frame.0,
            frame_h: frame.1,
            portal_w: portal.0,
            portal_h: portal.1,
        }
    }

    /// Whether this maps anything. Logged when a capture starts, because a
    /// client whose touches land in the wrong place needs this to be visible.
    pub fn is_identity(&self) -> bool {
        self.frame_w == self.portal_w && self.frame_h == self.portal_h
    }

    pub fn x(&self, x: u16) -> u16 {
        scale(x, self.frame_w, self.portal_w)
    }

    pub fn y(&self, y: u16) -> u16 {
        scale(y, self.frame_h, self.portal_h)
    }
}

fn scale(value: u16, from: u16, to: u16) -> u16 {
    if from == to || from == 0 {
        return value;
    }
    // u32 throughout: the product of two u16 overflows u16 for anything larger
    // than 256x256, and a debug build would panic on the arithmetic rather than
    // on the input.
    let scaled = u32::from(value) * u32::from(to) / u32::from(from);
    // Clamped to the far edge rather than wrapped: a coordinate that overshoots
    // is at the end of the screen, not back at the start.
    scaled.min(u32::from(to.saturating_sub(1))) as u16
}

/// One region the compositor declares for a touch device: a monitor's size in
/// logical pixels, and where that monitor sits on the desktop.
///
/// A device advertises *one region per monitor*, so a list of these is the
/// device's whole input space — not a single answer to "where do touches land".
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct InputRegion {
    pub w: u16,
    pub h: u16,
    pub x: i32,
    pub y: i32,
    /// The compositor's name for the monitor this region covers. Diagnostic
    /// only: it is the display name the server asked for, but the compositor
    /// uniquifies collisions by appending to it, so every concurrent session
    /// requests the same name and it cannot identify which region is whose.
    pub id: Option<String>,
}

/// Whether a region describes the same *shape* as the frames, and so could be
/// the monitor those frames came from.
///
/// Size differences are expected and fine — a 150%-scaled monitor reports its
/// region in logical pixels while the frames stay physical, a clean factor of
/// 1.5 apart. Shape differences mean it is a different monitor, and scaling
/// frame coordinates by it is meaningless: it silently compresses every touch,
/// with nothing in the log to explain it.
fn is_same_shape(frame: (u16, u16), region: &InputRegion) -> bool {
    if frame.0 == 0 || frame.1 == 0 || region.w == 0 || region.h == 0 {
        return false;
    }
    let want = f64::from(frame.0) / f64::from(frame.1);
    let got = f64::from(region.w) / f64::from(region.h);
    ((got - want) / want).abs() <= super::video::ASPECT_TOLERANCE
}

/// The logical input space belonging to *this* session's stream, or `None` when
/// no region can be identified as its monitor.
///
/// A touch device is not per-session: it is bound once and advertises a region
/// for every monitor on the desktop. Concurrent sessions each run their own
/// capture on their own virtual monitor, so the device's region list holds an
/// entry for every session — and taking the first one, as this did, picks
/// whichever monitor the compositor happened to list first. When that was
/// another session's, the map was built from a foreign monitor's size: a
/// 1080x2255 session mapped onto a 1080x960 region sent every touch to 43% of
/// its intended height, and because the ratio depends on whichever foreign
/// monitor was picked, the error moved around as sessions came, went and
/// resized.
///
/// Two signals identify the right region, and they are checked in order of how
/// much they can be trusted:
///
/// 1. **Position.** `stream_pos` is where this session's stream sits on the
///    desktop, and a region carries the position of its monitor in the same
///    space. This is an exact fact rather than a similarity, and it is what
///    separates two sessions whose windows happen to be the same size — the
///    common case, since both are the same page.
/// 2. **Shape.** Used when the position finds nothing, and as a contradiction
///    check on the position: a region at our position but a different shape is
///    not our monitor, and guessing anyway is what produced the bug.
///
/// The whole region is returned, not just its size, because the caller needs
/// its position too: the region defines where input is scaled *and* which
/// monitor it lands on, and taking those from two different sources is how they
/// come to disagree.
///
/// `None` means "do not scale". A missing map leaves coordinates where they
/// always were before any of this existed, which is a visible wrong answer on a
/// scaled desktop; a *wrong* map is an invisible one, quietly aimed at the wrong
/// place. The caller logs which case it hit.
pub(crate) fn input_space(
    regions: &[InputRegion],
    frame: (u16, u16),
    stream_pos: (i32, i32),
) -> Option<InputRegion> {
    match regions {
        // No region: nothing declares an input space at all.
        [] => None,
        // One monitor, so the only region is ours. Still shape-checked, because
        // "the only one" is not the same as "the right one" once a second
        // session's monitor has been created and destroyed underneath us.
        [only] => is_same_shape(frame, only).then(|| only.clone()),
        many => {
            let here: Vec<&InputRegion> =
                many.iter().filter(|r| (r.x, r.y) == stream_pos).collect();
            match here.as_slice() {
                [only] => is_same_shape(frame, only).then(|| (*only).clone()),
                // Several regions claim our position (mirrored outputs), so fall
                // through and let the shape pick.
                _ => {
                    // Exactly one shape matches, or nothing: with two matching
                    // shapes and no position to separate them there is no way to
                    // tell which monitor is ours, and picking one would be the
                    // original bug with extra steps.
                    let mut same_shape = many.iter().filter(|r| is_same_shape(frame, r)).cloned();
                    match (same_shape.next(), same_shape.next()) {
                        (Some(only), None) => Some(only),
                        _ => None,
                    }
                }
            }
        }
    }
}

pub(crate) fn map_touch_event(event: EisTouchEvent, map: CoordMap) -> EisTouchEvent {
    match event {
        EisTouchEvent::Down { x, y, index } => EisTouchEvent::Down {
            x: map.x(x),
            y: map.y(y),
            index,
        },
        EisTouchEvent::Motion { x, y, index } => EisTouchEvent::Motion {
            x: map.x(x),
            y: map.y(y),
            index,
        },
        EisTouchEvent::Up { index } => EisTouchEvent::Up { index },
    }
}

pub(crate) fn offset_touch_event(event: EisTouchEvent, (ox, oy): (i32, i32)) -> EisTouchEvent {
    match event {
        EisTouchEvent::Down { x, y, index } => EisTouchEvent::Down {
            x: (x as i32 + ox).max(0).min(u16::MAX as i32) as u16,
            y: (y as i32 + oy).max(0).min(u16::MAX as i32) as u16,
            index,
        },
        EisTouchEvent::Motion { x, y, index } => EisTouchEvent::Motion {
            x: (x as i32 + ox).max(0).min(u16::MAX as i32) as u16,
            y: (y as i32 + oy).max(0).min(u16::MAX as i32) as u16,
            index,
        },
        EisTouchEvent::Up { .. } => event,
    }
}

/// Send a run of touch events as **one** emulated frame.
///
/// Every event sent on its own costs a `start_emulating`, a `frame`, a
/// `stop_emulating` and a `flush` — four protocol messages and a syscall to
/// move one finger by a few pixels. That is invisible at human event rates and
/// ruinous under a flood: the compositor's own per-event work (a window lookup
/// per touch point, per frame) is what stalls rendering, so paying four messages
/// per event is what makes a multitouch flood freeze the display as well as the
/// input. EIS has an explicit frame concept precisely so a run of events shares
/// one, and the input gate already coalesces a burst into a batch — this is where
/// that batch is spent.
///
/// Ordering within the batch is preserved, so a finger lifted in the same batch
/// it moved is still lifted last.
pub(crate) fn send_touch_events(
    connection: &reis::event::Connection,
    device: &reis::event::Device,
    touchscreen: &ei::Touchscreen,
    sequence: &mut u32,
    events: &[EisTouchEvent],
) {
    if events.is_empty() {
        return;
    }
    with_emulation(connection, device, sequence, "touchscreen", || {
        for event in events {
            match *event {
                EisTouchEvent::Down { x, y, index } => {
                    touchscreen.down(index, x as f32, y as f32);
                }
                EisTouchEvent::Motion { x, y, index } => {
                    touchscreen.motion(index, x as f32, y as f32);
                }
                EisTouchEvent::Up { index } => {
                    touchscreen.up(index);
                }
            }
        }
    });
}

#[cfg(test)]
mod coord_map_tests {
    use super::{CoordMap, EisTouchEvent, map_touch_event};

    /// The measured case: a 150%-scaled desktop where the monitor and the
    /// frames are both 1080 wide but the portal reports the region as 720.
    /// Without this the far edge of the screen is unreachable — every touch
    /// lands at two thirds of where it was aimed.
    #[test]
    fn frame_pixels_are_scaled_into_the_portals_units() {
        let map = CoordMap::new((1080, 2255), (720, 1503));
        assert_eq!(map.x(0), 0);
        assert_eq!(map.x(540), 360);
        // The client's coordinates run to one less than the width, and so do
        // the mapped ones: 1079 is the last addressable column of 720.
        assert_eq!(map.x(1079), 719);
    }

    /// The other axis is not special; a portrait region has the taller side
    /// scaled too.
    #[test]
    fn both_axes_are_mapped() {
        let map = CoordMap::new((1080, 2255), (720, 1503));
        assert_eq!(map.y(0), 0);
        assert_eq!(map.y(1127), 751);
        assert_eq!(map.y(2254), 1502);
    }

    /// When the two spaces agree — an unscaled desktop — this must be the
    /// identity, so a machine without scaling is unaffected.
    #[test]
    fn an_unscaled_desktop_is_left_alone() {
        let map = CoordMap::new((1920, 1080), (1920, 1080));
        assert!(map.is_identity());
        assert_eq!(map.x(137), 137);
        assert_eq!(map.y(900), 900);
    }

    /// A scale above 1 the other way — a portal reporting a *larger* region
    /// than the frame, which would otherwise clamp everything to the far edge.
    #[test]
    fn scaling_up_is_applied_too() {
        let map = CoordMap::new((720, 1280), (1080, 1920));
        assert_eq!(map.x(360), 540);
        // 719 is the last input column and scales to 1078.99, truncated. The
        // shortfall is a pixel at the far edge and is the price of not rounding
        // up into a coordinate the region does not have.
        assert_eq!(map.x(719), 1078);
        assert_eq!(map.x(720), 1079);
    }

    /// Coordinates past the end must clamp to the last addressable pixel, not
    /// wrap round to the start. Wrapping is the failure that looks like a touch
    /// landing in the opposite corner.
    #[test]
    fn overshooting_coordinates_clamp_rather_than_wrap() {
        let map = CoordMap::new((1080, 2255), (720, 1503));
        assert_eq!(map.x(2000), 719);
        assert_eq!(map.y(5000), 1502);
    }

    /// Rounding must not drift: over the whole range, no output may land below
    /// the input's own position, or touches would creep toward the origin.
    #[test]
    fn scaling_never_moves_a_coordinate_backwards() {
        let map = CoordMap::new((1080, 2255), (720, 1503));
        let mut previous = 0;
        for x in (0..=1079).step_by(7) {
            let mapped = map.x(x);
            assert!(mapped >= previous, "{x} mapped back to {mapped}");
            assert!(mapped <= 720);
            previous = mapped;
        }
    }

    /// A degenerate width cannot produce a scale, and must leave coordinates
    /// alone rather than divide by zero.
    #[test]
    fn a_degenerate_width_is_the_identity() {
        let map = CoordMap::new((0, 0), (720, 1503));
        assert_eq!(map.x(42), 42);
    }

    /// The mapping applies to every touch event that carries a position, and to
    /// none that does not.
    #[test]
    fn every_positioned_event_is_mapped_and_up_is_untouched() {
        let map = CoordMap::new((1080, 2255), (720, 1503));
        assert_eq!(
            map_touch_event(
                EisTouchEvent::Down {
                    x: 540,
                    y: 0,
                    index: 3
                },
                map
            ),
            EisTouchEvent::Down {
                x: 360,
                y: 0,
                index: 3
            }
        );
        assert_eq!(
            map_touch_event(
                EisTouchEvent::Motion {
                    x: 108,
                    y: 76,
                    index: 0
                },
                map
            ),
            EisTouchEvent::Motion {
                x: 72,
                y: 50,
                index: 0
            }
        );
        assert_eq!(
            map_touch_event(EisTouchEvent::Up { index: 7 }, map),
            EisTouchEvent::Up { index: 7 }
        );
    }
}

#[cfg(test)]
mod slot_tests {
    use super::{EisTouchEvent, SLOTS_PER_SESSION, shift_slots};

    fn slot_of(event: &EisTouchEvent) -> u32 {
        match event {
            EisTouchEvent::Down { index, .. }
            | EisTouchEvent::Motion { index, .. }
            | EisTouchEvent::Up { index } => *index,
        }
    }

    /// The whole point: two sessions both pressing "slot 0" must not land on the
    /// same touch point, or the second press cancels the first.
    #[test]
    fn two_sessions_never_collide_on_slot_zero() {
        let one = shift_slots(
            EisTouchEvent::Down {
                x: 1,
                y: 2,
                index: 0,
            },
            0,
        )
        .unwrap();
        let two = shift_slots(
            EisTouchEvent::Down {
                x: 1,
                y: 2,
                index: 0,
            },
            SLOTS_PER_SESSION,
        )
        .unwrap();
        assert_ne!(slot_of(&one), slot_of(&two));
    }

    /// And no slot of one session may reach another's, at any depth.
    #[test]
    fn session_ranges_do_not_overlap() {
        let base = SLOTS_PER_SESSION * 3;
        let mine: Vec<u32> = (0..SLOTS_PER_SESSION)
            .map(|i| slot_of(&shift_slots(EisTouchEvent::Up { index: i }, base).unwrap()))
            .collect();
        let theirs: Vec<u32> = (0..SLOTS_PER_SESSION)
            .map(|i| {
                slot_of(
                    &shift_slots(EisTouchEvent::Up { index: i }, base + SLOTS_PER_SESSION).unwrap(),
                )
            })
            .collect();
        assert!(
            mine.iter().all(|s| !theirs.contains(s)),
            "{mine:?} vs {theirs:?}"
        );
    }

    /// A lift must reach the same slot its press did, or the compositor holds a
    /// finger down forever.
    #[test]
    fn up_lands_on_the_slot_down_used() {
        let base = SLOTS_PER_SESSION * 2;
        for index in [0u32, 1, 7, SLOTS_PER_SESSION - 1] {
            let down = shift_slots(EisTouchEvent::Down { x: 0, y: 0, index }, base).unwrap();
            let up = shift_slots(EisTouchEvent::Up { index }, base).unwrap();
            assert_eq!(slot_of(&down), slot_of(&up), "index {index}");
            assert_eq!(slot_of(&down), base + index);
        }
    }

    /// Coordinates are untouched: only the identifier moves.
    #[test]
    fn coordinates_are_left_alone() {
        let base = SLOTS_PER_SESSION * 5;
        let shifted = shift_slots(
            EisTouchEvent::Down {
                x: 640,
                y: 480,
                index: 2,
            },
            base,
        )
        .unwrap();
        assert_eq!(
            shifted,
            EisTouchEvent::Down {
                x: 640,
                y: 480,
                index: base + 2
            }
        );
    }

    /// A client claiming a slot past its range is dropped rather than folded
    /// into the next session's range, which is how the ranges would stop being
    /// ranges at all.
    #[test]
    fn a_slot_past_the_range_is_dropped() {
        for index in [SLOTS_PER_SESSION, SLOTS_PER_SESSION + 1, 255] {
            assert!(
                shift_slots(EisTouchEvent::Down { x: 0, y: 0, index }, 0).is_none(),
                "index {index} should be refused"
            );
        }
    }
}

#[cfg(test)]
mod touch_state_tests {
    use super::TouchState;

    /// A finger resting on the glass sends nothing, so a held slot must simply
    /// stay held. It used not to: the input task released every touch after a
    /// second of silence, which cancelled any gesture held still — and with two
    /// sessions it meant touching the second display killed the first's touches
    /// one second later. The state itself has no clock, and must not grow one;
    /// only a lift or the session ending releases a slot.
    #[test]
    fn a_held_slot_survives_arbitrary_silence() {
        let mut state = TouchState::new();
        assert!(matches!(
            state.handle_touch(0, 100, 200)[0],
            super::EisTouchEvent::Down { .. }
        ));
        // Any number of further reports, however long they take to arrive.
        for _ in 0..1000 {
            assert!(matches!(
                state.handle_touch(0, 100, 200)[0],
                super::EisTouchEvent::Motion { .. }
            ));
        }
        // Still held: only an explicit release lifts it.
        assert_eq!(
            state
                .handle_release(0)
                .map(|e| matches!(e, super::EisTouchEvent::Up { .. })),
            Some(true)
        );
        assert!(state.handle_release(0).is_none(), "released twice");
    }

    /// Several fingers are independent slots, so lifting one leaves the others
    /// down — the case that made multi-touch worth having.
    #[test]
    fn slots_are_independent() {
        let mut state = TouchState::new();
        for index in 0..3u8 {
            assert!(matches!(
                state.handle_touch(index, 10, 10)[0],
                super::EisTouchEvent::Down { .. }
            ));
        }
        state.handle_release(1);
        assert!(matches!(
            state.handle_touch(0, 11, 11)[0],
            super::EisTouchEvent::Motion { .. }
        ));
        assert!(matches!(
            state.handle_touch(2, 12, 12)[0],
            super::EisTouchEvent::Motion { .. }
        ));
        // The lifted one is free to press again.
        assert!(matches!(
            state.handle_touch(1, 13, 13)[0],
            super::EisTouchEvent::Down { .. }
        ));
    }

    /// The teardown path: everything still held goes up at once, so a client
    /// that vanishes mid-gesture does not leave fingers on the compositor.
    #[test]
    fn release_all_lifts_everything_held() {
        let mut state = TouchState::new();
        for index in 0..3u8 {
            state.handle_touch(index, 10, 10);
        }
        let released = state.release_all();
        assert_eq!(released.len(), 3);
        assert!(
            released
                .iter()
                .all(|e| matches!(e, super::EisTouchEvent::Up { .. }))
        );
        assert!(state.release_all().is_empty(), "nothing left to release");
    }
}

#[cfg(test)]
mod input_space_tests {
    use super::{InputRegion, input_space};

    /// The size the caller scales by, which is what these tests are about.
    fn space(regions: &[InputRegion], frame: (u16, u16), at: (i32, i32)) -> Option<(u16, u16)> {
        input_space(regions, frame, at).map(|r| (r.w, r.h))
    }

    const HDMI: InputRegion = InputRegion {
        w: 720,
        h: 1503,
        x: 0,
        y: 0,
        id: None,
    };
    /// A second session's virtual monitor: shorter, and placed to the right of
    /// the physical output. Both numbers are taken from a real capture.
    const VIRTUAL: InputRegion = InputRegion {
        w: 1080,
        h: 959,
        x: 720,
        y: 0,
        id: None,
    };

    /// The bug, exactly as it happened: two sessions, one device, and the
    /// foreign monitor listed first. Taking `.first()` gave 1080x959 for
    /// 1080x2255 frames and every touch landed at 43% of its height.
    #[test]
    fn a_foreign_monitor_is_not_mistaken_for_this_stream() {
        let regions = [VIRTUAL, HDMI];
        assert_eq!(space(&regions, (1080, 2255), (0, 0)), Some((720, 1503)));
    }

    /// The same list in the other order must give the same answer, since the
    /// compositor does not promise a stable order and the old code read it as
    /// though it did.
    #[test]
    fn the_answer_does_not_depend_on_region_order() {
        let forwards = [HDMI, VIRTUAL];
        let backwards = [VIRTUAL, HDMI];
        assert_eq!(
            space(&forwards, (1080, 2255), (0, 0)),
            space(&backwards, (1080, 2255), (0, 0))
        );
    }

    /// Two sessions with the same window shape — the ordinary case, both being
    /// the same page — are only separable by position. Position is checked
    /// first precisely so this works.
    #[test]
    fn identical_shapes_are_separated_by_position() {
        let session_one = InputRegion {
            w: 720,
            h: 1503,
            x: 0,
            y: 0,
            id: None,
        };
        let session_two = InputRegion {
            w: 720,
            h: 1503,
            x: 1920,
            y: 0,
            id: None,
        };
        let regions = [session_one, session_two];
        assert_eq!(space(&regions, (1080, 2255), (0, 0)), Some((720, 1503)));
        assert_eq!(space(&regions, (1080, 2255), (1920, 0)), Some((720, 1503)));
    }

    /// A single region is the whole desktop, so it is used — but only if it is
    /// the right shape. "The only one" stops being true once a second session's
    /// monitor has been created and destroyed while this capture was up.
    #[test]
    fn one_region_is_used_when_it_is_the_right_shape() {
        assert_eq!(space(&[HDMI], (1080, 2255), (0, 0)), Some((720, 1503)));
    }

    /// And refused when it is not, rather than mapped through.
    #[test]
    fn one_region_of_the_wrong_shape_is_refused() {
        assert_eq!(input_space(&[VIRTUAL], (1080, 2255), (0, 0)), None);
    }

    /// Nothing declared, nothing to map.
    #[test]
    fn no_regions_means_no_input_space() {
        assert_eq!(input_space(&[], (1080, 2255), (0, 0)), None);
    }

    /// A region at our position whose shape disagrees is a contradiction. This
    /// used to be resolved by scaling anyway, which is how a 0.425 factor got
    /// into a live session.
    #[test]
    fn position_and_shape_must_agree() {
        // VIRTUAL sits at x=720, so ask for that stream with portrait frames.
        assert_eq!(input_space(&[HDMI, VIRTUAL], (1080, 2255), (720, 0)), None);
    }

    /// With no position match, an unambiguous shape still identifies the
    /// monitor: the position can be stale while the shape is not.
    #[test]
    fn shape_can_identify_the_region_when_position_does_not() {
        let regions = [VIRTUAL, HDMI];
        assert_eq!(
            space(&regions, (1080, 2255), (9999, 9999)),
            Some((720, 1503))
        );
    }

    /// Two regions of the right shape and no position to separate them: there is
    /// no answer, and guessing is the bug. Returning `None` leaves coordinates
    /// unscaled, which is visibly wrong rather than quietly aimed elsewhere.
    #[test]
    fn an_ambiguous_shape_is_refused() {
        let left = InputRegion {
            w: 720,
            h: 1503,
            x: 0,
            y: 0,
            id: None,
        };
        let right = InputRegion {
            w: 720,
            h: 1503,
            x: 1920,
            y: 0,
            id: None,
        };
        assert_eq!(
            input_space(&[left, right], (1080, 2255), (9999, 9999)),
            None
        );
    }

    /// The size difference that made this necessary stays allowed: a 150%-scaled
    /// monitor reports logical pixels while the frames are physical.
    #[test]
    fn a_scaled_monitor_still_counts_as_the_same_shape() {
        let scaled = InputRegion {
            w: 720,
            h: 744,
            x: 0,
            y: 0,
            id: None,
        };
        assert_eq!(space(&[scaled], (1080, 1116), (0, 0)), Some((720, 744)));
    }

    /// A zero-sized frame has no shape to compare, so nothing can be confirmed
    /// and nothing is claimed.
    #[test]
    fn a_degenerate_frame_identifies_nothing() {
        assert_eq!(input_space(&[HDMI], (0, 2255), (0, 0)), None);
        assert_eq!(input_space(&[HDMI], (1080, 0), (0, 0)), None);
    }
}
