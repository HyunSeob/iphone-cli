//! Two-finger gestures: pinch and rotate.
//!
//! [`plan`] turns a request into the frames to send. It works in logical
//! points and normalizes each axis on its own at the end, so a circle stays
//! round on a tall screen, and it does no I/O. [`play`] sends the frames and
//! always lifts the pair afterwards, also when a frame failed.

use std::time::Duration;

use anyhow::Result;
use idevice::core_device::hid::TouchscreenContact;

use crate::device::input::normalize;

/// The pair's contact identities. The one-finger path reports identity 2
/// (`TOUCHSCREEN_STATE_CONTACT` is `0xC0 | 2`), so the two never collide.
const IDENTITIES: [u8; 2] = [0, 1];

pub const MIN_SCALE: f64 = 0.25;
pub const MAX_SCALE: f64 = 4.0;
pub const MAX_ROTATE: f64 = 180.0;

/// Room kept between a finger and the screen edge.
const MARGIN: f64 = 24.0;
/// Closest the fingers may start or end.
const MIN_SPAN: f64 = 40.0;
/// The wider of the two spans, as a share of the screen's shorter side.
const SPAN_SHARE: f64 = 0.6;
/// Both fingers rest before they move, so the phone sees a two-finger touch begin.
const HOLD: Duration = Duration::from_millis(40);
/// Frames at the end pose before the lift: the last samples must say "not
/// moving", or the content keeps coasting.
const REST_FRAMES: u32 = 4;
const REST_GAP: Duration = Duration::from_millis(30);

/// What to do, in logical points.
#[derive(Debug, Clone, Copy)]
pub struct Spec {
    pub screen: (f64, f64),
    pub center: (f64, f64),
    /// End span over start span: above 1 spreads the fingers, below 1 pinches.
    pub scale: f64,
    /// Degrees, positive clockwise on the screen.
    pub rotate: f64,
    /// How long the fingers move.
    pub duration: Duration,
}

/// Both contacts at one moment, `at` after the first frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Frame {
    pub(crate) at: Duration,
    pub(crate) contacts: [TouchscreenContact; 2],
}

impl Frame {
    pub(crate) fn touching(&self) -> bool {
        self.contacts[0].touching
    }
}

#[derive(Debug, Clone)]
pub struct Plan {
    /// Down, hold, the moving frames, the rest frames, and the lift last.
    pub(crate) frames: Vec<Frame>,
    pub center: (f64, f64),
    /// The distance between the fingers at the start and at the end, in points.
    pub span: (f64, f64),
}

/// Whether the motion asks for anything the gesture can do.
pub fn check_motion(scale: f64, rotate: f64) -> Result<(), String> {
    if !(MIN_SCALE..=MAX_SCALE).contains(&scale) {
        return Err(format!("scale must be {MIN_SCALE}..{MAX_SCALE}"));
    }
    if !(-MAX_ROTATE..=MAX_ROTATE).contains(&rotate) {
        return Err(format!("rotate must be -{MAX_ROTATE}..{MAX_ROTATE} degrees"));
    }
    if scale == 1.0 && rotate == 0.0 {
        return Err("give a direction, a scale other than 1, or a rotation".into());
    }
    Ok(())
}

fn smoothstep(t: f64) -> f64 {
    t * t * (3.0 - 2.0 * t)
}

/// The frames for `spec`, or why there is no room for them. Nothing has been
/// sent at that point. When the fingers would leave the margin the whole
/// motion is made smaller, never clamped point by point, so the ratio between
/// the two spans stays what was asked for.
pub fn plan(spec: &Spec) -> Result<Plan, String> {
    let Spec { screen: (w, h), center: (cx, cy), scale, rotate, duration } = *spec;
    check_motion(scale, rotate)?;
    if !(w > 0.0 && h > 0.0) {
        return Err("the screen has no size".into());
    }
    let room = (cx.min(w - cx) - MARGIN, cy.min(h - cy) - MARGIN);
    if !(room.0 > 0.0 && room.1 > 0.0) {
        return Err(format!(
            "{cx},{cy} is too close to the edge of the {w}x{h} screen; both fingers need {MARGIN} pt of room"
        ));
    }
    let base = SPAN_SHARE * w.min(h);
    let (mut from, mut to) = if scale >= 1.0 { (base / scale, base) } else { (base, base * scale) };
    let angle = rotate.to_radians();
    let steps = (duration.as_millis() / 16).clamp(4, 240) as u32;
    let eased = |i: u32| smoothstep(i as f64 / steps as f64);
    let offset = |from: f64, to: f64, e: f64| {
        let (r, a) = ((from + (to - from) * e) / 2.0, angle * e);
        (r * a.cos(), r * a.sin())
    };

    let fit = (0..=steps).fold(1.0_f64, |fit, i| {
        let (ox, oy) = offset(from, to, eased(i));
        let fit = if ox.abs() > f64::EPSILON { fit.min(room.0 / ox.abs()) } else { fit };
        if oy.abs() > f64::EPSILON { fit.min(room.1 / oy.abs()) } else { fit }
    });
    (from, to) = (from * fit, to * fit);
    if from.min(to) < MIN_SPAN {
        return Err(format!(
            "no room for a {scale}x pinch at {cx},{cy}: the fingers would be {:.0} pt apart, under {MIN_SPAN}",
            from.min(to)
        ));
    }

    let frame = |at: Duration, e: f64, touching: bool| {
        let (ox, oy) = offset(from, to, e);
        let contact = |identity: u8, sign: f64| TouchscreenContact {
            identity,
            touching,
            x: normalize(cx + sign * ox, w),
            y: normalize(cy + sign * oy, h),
        };
        Frame { at, contacts: [contact(IDENTITIES[0], 1.0), contact(IDENTITIES[1], -1.0)] }
    };
    let moved_until = HOLD + duration;
    let mut frames = Vec::with_capacity(steps as usize + 2 + REST_FRAMES as usize + 1);
    frames.push(frame(Duration::ZERO, 0.0, true));
    frames.push(frame(HOLD, 0.0, true));
    for i in 1..=steps {
        frames.push(frame(HOLD + duration.mul_f64(i as f64 / steps as f64), eased(i), true));
    }
    for j in 1..=REST_FRAMES {
        frames.push(frame(moved_until + REST_GAP * j, 1.0, true));
    }
    frames.push(frame(moved_until + REST_GAP * REST_FRAMES, 1.0, false));
    Ok(Plan { frames, center: (cx, cy), span: (from, to) })
}

/// `pair` lifted where it stands.
pub(crate) fn lifted(pair: &[TouchscreenContact; 2]) -> [TouchscreenContact; 2] {
    pair.map(|c| TouchscreenContact { touching: false, ..c })
}

/// Send `plan` frame by frame through `send`. Once a frame may have gone out
/// the pair is lifted, also when a later frame failed, and `held` says so
/// until the lift went through: a gesture dropped halfway still leaves the
/// pair for [`InputSession::close`](crate::device::input::InputSession::close)
/// to lift.
pub(crate) async fn play(
    plan: &Plan,
    held: &mut Option<[TouchscreenContact; 2]>,
    mut send: impl AsyncFnMut(&[TouchscreenContact]) -> Result<()>,
) -> Result<()> {
    let start = tokio::time::Instant::now();
    let mut result = Ok(());
    for frame in &plan.frames {
        tokio::time::sleep_until(start + frame.at).await;
        if frame.touching() {
            *held = Some(frame.contacts);
        }
        if let Err(e) = send(&frame.contacts).await {
            result = Err(e);
            break;
        }
        if !frame.touching() {
            *held = None;
        }
    }
    if let Some(pair) = *held {
        let lift = send(&lifted(&pair)).await;
        if lift.is_ok() {
            *held = None;
        }
        result = result.and(lift);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;

    const SCREEN: (f64, f64) = (402.0, 874.0);

    fn spec(scale: f64, rotate: f64) -> Spec {
        Spec {
            screen: SCREEN,
            center: (SCREEN.0 / 2.0, SCREEN.1 / 2.0),
            scale,
            rotate,
            duration: Duration::from_millis(500),
        }
    }

    fn point(c: &TouchscreenContact) -> (f64, f64) {
        (c.x as f64 / 65535.0 * SCREEN.0, c.y as f64 / 65535.0 * SCREEN.1)
    }

    fn span(f: &Frame) -> f64 {
        let (a, b) = (point(&f.contacts[0]), point(&f.contacts[1]));
        (a.0 - b.0).hypot(a.1 - b.1)
    }

    fn angle(f: &Frame) -> f64 {
        let (a, b) = (point(&f.contacts[0]), point(&f.contacts[1]));
        (a.1 - b.1).atan2(a.0 - b.0).to_degrees()
    }

    fn turn(degrees: f64) -> f64 {
        (degrees + 180.0).rem_euclid(360.0) - 180.0
    }

    fn touching(plan: &Plan) -> impl Iterator<Item = &Frame> {
        plan.frames.iter().filter(|f| f.touching())
    }

    #[test]
    fn the_end_span_is_the_requested_scale_times_the_start_span() {
        for scale in [0.25, 0.5, 0.8, 1.5, 2.0, 4.0] {
            let plan = plan(&spec(scale, 0.0)).unwrap();
            let (first, last) = (plan.frames.first().unwrap(), touching(&plan).last().unwrap());
            assert!((span(last) - scale * span(first)).abs() < 1.0, "scale {scale}");
            assert!((plan.span.1 - scale * plan.span.0).abs() < 1e-9, "scale {scale}");
        }
    }

    #[test]
    fn the_fingers_stay_centred_on_the_target_in_every_frame() {
        let target = (120.0, 300.0);
        let plan = plan(&Spec { center: target, ..spec(2.0, 60.0) }).unwrap();
        for f in &plan.frames {
            let (a, b) = (point(&f.contacts[0]), point(&f.contacts[1]));
            let mid = ((a.0 + b.0) / 2.0, (a.1 + b.1) / 2.0);
            assert!((mid.0 - target.0).abs() < 0.05 && (mid.1 - target.1).abs() < 0.05, "{mid:?}");
        }
    }

    #[test]
    fn the_angle_turns_by_exactly_the_rotation() {
        for rotate in [90.0, -45.0, 150.0, 180.0, -180.0, 10.0] {
            let plan = plan(&spec(1.0, rotate)).unwrap();
            let turned = angle(touching(&plan).last().unwrap()) - angle(&plan.frames[0]);
            assert!(turn(turned - rotate).abs() < 0.1, "rotate {rotate} turned {turned}");
        }
    }

    #[test]
    fn a_positive_rotation_is_clockwise_with_y_pointing_down() {
        let plan = plan(&spec(1.0, 90.0)).unwrap();
        let centre = (SCREEN.0 / 2.0, SCREEN.1 / 2.0);
        let (start, end) = (point(&plan.frames[0].contacts[0]), point(&touching(&plan).last().unwrap().contacts[0]));
        assert!(start.0 > centre.0 && (start.1 - centre.1).abs() < 0.1, "starts right of the centre: {start:?}");
        assert!(end.1 > centre.1 && (end.0 - centre.0).abs() < 0.1, "ends below the centre: {end:?}");
    }

    #[test]
    fn a_circle_stays_round_on_a_tall_screen() {
        let plan = plan(&spec(1.0, 180.0)).unwrap();
        for f in touching(&plan) {
            assert!((span(f) - plan.span.0).abs() < 0.05, "{} vs {}", span(f), plan.span.0);
        }
    }

    #[test]
    fn the_frames_run_forward_in_time_and_end_with_a_lift_of_both_contacts() {
        let plan = plan(&spec(2.0, 0.0)).unwrap();
        assert!(plan.frames.windows(2).all(|w| w[0].at <= w[1].at));
        assert_eq!(plan.frames[0].at, Duration::ZERO);
        let moving = &plan.frames[2..plan.frames.len() - 1 - REST_FRAMES as usize];
        assert!(moving.iter().all(|f| f.at > HOLD && f.at <= HOLD + Duration::from_millis(500)));
        assert_eq!(plan.frames.last().unwrap().at, HOLD + Duration::from_millis(500) + REST_GAP * REST_FRAMES);
        let (identities, flags): (Vec<u8>, Vec<bool>) = plan
            .frames
            .last()
            .unwrap()
            .contacts
            .iter()
            .map(|c| (c.identity, c.touching))
            .unzip();
        assert_eq!((identities, flags), (vec![0, 1], vec![false, false]));
        assert!(touching(&plan).all(|f| f.contacts.map(|c| c.identity) == [0, 1]));
        let down = &plan.frames[0].contacts;
        let lift = &plan.frames.last().unwrap().contacts;
        assert_eq!(down.map(|c| c.identity), lift.map(|c| c.identity));
    }

    #[test]
    fn a_target_near_an_edge_shrinks_the_motion_and_keeps_every_frame_inside_the_margin() {
        let plan = plan(&Spec { center: (90.0, 437.0), ..spec(2.0, 0.0) }).unwrap();
        let (open, tight) = (self::plan(&spec(2.0, 0.0)).unwrap(), &plan);
        assert!(tight.span.1 < open.span.1);
        assert!((tight.span.1 / tight.span.0 - 2.0).abs() < 1e-9);
        for f in &plan.frames {
            for c in &f.contacts {
                let (x, y) = point(c);
                assert!(x >= MARGIN - 0.05 && x <= SCREEN.0 - MARGIN + 0.05, "x {x}");
                assert!(y >= MARGIN - 0.05 && y <= SCREEN.1 - MARGIN + 0.05, "y {y}");
            }
        }
    }

    #[test]
    fn a_request_that_cannot_fit_is_an_error_before_anything_is_sent() {
        for center in [(5.0, 5.0), (400.0, 437.0), (201.0, 10.0), (30.0, 437.0)] {
            assert!(plan(&Spec { center, ..spec(4.0, 0.0) }).is_err(), "{center:?}");
        }
        assert!(plan(&Spec { center: (60.0, 437.0), ..spec(2.0, 0.0) }).is_err(), "36 pt apart is under the minimum");
        assert!(plan(&Spec { screen: (0.0, 874.0), ..spec(2.0, 0.0) }).is_err());
    }

    #[test]
    fn a_motion_outside_the_limits_or_with_nothing_to_do_is_rejected() {
        for (scale, rotate) in [(1.0, 0.0), (0.1, 0.0), (5.0, 0.0), (2.0, 181.0), (2.0, -200.0), (f64::NAN, 0.0), (1.0, f64::NAN)] {
            assert!(check_motion(scale, rotate).is_err(), "{scale} {rotate}");
            assert!(plan(&spec(scale, rotate)).is_err(), "{scale} {rotate}");
        }
        for (scale, rotate) in [(0.25, 0.0), (4.0, 0.0), (1.0, 180.0), (1.0, -180.0), (2.0, 30.0)] {
            assert!(check_motion(scale, rotate).is_ok(), "{scale} {rotate}");
        }
    }

    #[test]
    fn the_longest_gesture_is_capped_at_240_steps() {
        let plan = plan(&Spec { duration: Duration::from_secs(10), ..spec(2.0, 0.0) }).unwrap();
        assert_eq!(plan.frames.len(), 240 + 2 + REST_FRAMES as usize + 1);
    }

    #[tokio::test(start_paused = true)]
    async fn the_frames_go_out_in_order_and_the_pair_is_not_held_afterwards() {
        let plan = plan(&spec(2.0, 0.0)).unwrap();
        let (mut sent, mut held) = (Vec::new(), None);
        play(&plan, &mut held, async |c: &[TouchscreenContact]| {
            sent.push(c.to_vec());
            Ok(())
        })
        .await
        .unwrap();
        assert_eq!(sent.len(), plan.frames.len());
        assert!(held.is_none());
        assert!(sent.last().unwrap().iter().all(|c| !c.touching));
    }

    #[tokio::test(start_paused = true)]
    async fn a_failure_on_a_frame_still_sends_the_lift_where_the_pair_was() {
        let plan = plan(&spec(2.0, 0.0)).unwrap();
        for failing in [0, 1, 5, plan.frames.len() - 2] {
            let (mut sent, mut held) = (Vec::new(), None);
            let result = play(&plan, &mut held, async |c: &[TouchscreenContact]| {
                sent.push(c.to_vec());
                if sent.len() == failing + 1 { Err(anyhow!("frame {failing} failed")) } else { Ok(()) }
            })
            .await;
            assert_eq!(result.unwrap_err().to_string(), format!("frame {failing} failed"));
            assert_eq!(sent.len(), failing + 2, "the failed frame, then the lift");
            let lift = sent.last().unwrap();
            assert_eq!(lift.iter().map(|c| (c.identity, c.touching)).collect::<Vec<_>>(), [(0, false), (1, false)]);
            let before = &plan.frames[failing].contacts;
            assert_eq!(lift.iter().map(|c| (c.x, c.y)).collect::<Vec<_>>(), before.map(|c| (c.x, c.y)));
            assert!(held.is_none());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_lift_that_fails_leaves_the_pair_held_for_close_to_lift() {
        let plan = plan(&spec(2.0, 0.0)).unwrap();
        let (mut sent, mut held) = (Vec::new(), None);
        let result = play(&plan, &mut held, async |c: &[TouchscreenContact]| {
            sent.push(c.to_vec());
            if c[0].touching && sent.len() < 3 { Ok(()) } else { Err(anyhow!("down")) }
        })
        .await;
        assert!(result.is_err());
        assert!(held.is_some(), "the lift went nowhere, so the pair is still down");
    }

    #[tokio::test(start_paused = true)]
    async fn the_frames_are_paced_by_their_times() {
        let plan = plan(&spec(2.0, 0.0)).unwrap();
        let (start, mut held) = (tokio::time::Instant::now(), None);
        let mut times = Vec::new();
        play(&plan, &mut held, async |_: &[TouchscreenContact]| {
            times.push(start.elapsed());
            Ok(())
        })
        .await
        .unwrap();
        assert_eq!(times.len(), plan.frames.len());
        for (ran, f) in times.iter().zip(&plan.frames) {
            assert!(ran.abs_diff(f.at) <= Duration::from_millis(1), "{ran:?} vs {:?}", f.at);
        }
    }
}
