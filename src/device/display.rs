//! Phones with more than one display (iPhone Duo: a cover display, and an
//! inner one that unfolds). Each display has its own touchscreen, and a touch
//! lands only on the display whose touchscreen gets it. Which display is in
//! use, and how it is turned, comes from CoreDevice's `getdisplayinfo`. A
//! phone with one touchscreen is never asked.
//!
//! Points come in two spaces (measured on an iPhone Duo, iOS 27.2.1). The
//! frames of the system's own surfaces (home screen, Control Center) are in
//! the panel's points, however the display is turned. The frames of apps,
//! and the screen a direction gesture is laid out on, are in the points of
//! the screen as turned: the panel with width and height swapped at a
//! quarter turn.

use std::borrow::Cow;

use idevice::{IdeviceError, ReadWrite, RemoteXpcClient, RsdService, rsd::RsdHandshake, xpc::Dictionary};

use crate::{
    device::{media, session::bounded},
    tcp::handle::AdapterHandle,
};

const SERVICE: &str = "com.apple.coredevice.deviceinfo";
const FEATURE: &str = "com.apple.coredevice.feature.getdisplayinfo";
/// Longest wait for the connection, and for one answer. Answers took 15 ms
/// (median) to 150 ms over Wi-Fi.
const WITHIN_SECS: u64 = 5;

/// How the display is turned from its panel's own orientation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rotation {
    R0,
    R90,
    R180,
    R270,
}

impl Rotation {
    /// `currentOrientation` of a display: `rot0`, `rot90`, `rot180` or `rot270`.
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "rot0" => Ok(Self::R0),
            "rot90" => Ok(Self::R90),
            "rot180" => Ok(Self::R180),
            "rot270" => Ok(Self::R270),
            other => Err(format!("unknown display orientation `{other}`")),
        }
    }

    /// Where a point of the turned screen, given as shares of its width and
    /// height, lies on the panel. `R180` is not measured.
    pub(crate) fn turn(self, (u, v): (f64, f64)) -> (f64, f64) {
        match self {
            Self::R0 => (u, v),
            Self::R90 => (v, 1.0 - u),
            Self::R180 => (1.0 - u, 1.0 - v),
            Self::R270 => (1.0 - v, u),
        }
    }
}

/// Which space a point is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Space {
    /// The panel's own points: the system's surfaces.
    Panel,
    /// The screen as the display is turned: apps, and direction gestures.
    Screen,
}

/// The space a gesture's points are in, with that space's size as the
/// reading the points came from saw it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Canvas {
    pub space: Space,
    pub size: (f64, f64),
}

/// The display in use.
#[derive(Debug, Clone, PartialEq)]
pub struct ActiveDisplay {
    /// Also in the name of the display's touchscreen.
    pub unique_id: String,
    /// The number the screen-sharing service streams the display's video
    /// by, when the phone gives one.
    pub display_id: Option<i64>,
    /// The panel's size in points: its pixel bounds over its point scale.
    pub panel: (f64, f64),
    pub rotation: Rotation,
}

impl ActiveDisplay {
    /// The size of `space` in points.
    pub fn size(&self, space: Space) -> (f64, f64) {
        let (w, h) = self.panel;
        match (space, self.rotation) {
            (Space::Screen, Rotation::R90 | Rotation::R270) => (h, w),
            _ => (w, h),
        }
    }

    /// Where a point of `space` lies on the panel, as shares of its width
    /// and height.
    fn share_of_panel(&self, space: Space, (x, y): (f64, f64)) -> (f64, f64) {
        let (w, h) = self.size(space);
        let share = (x / w, y / h);
        match space {
            Space::Panel => share,
            Space::Screen => self.rotation.turn(share),
        }
    }

    /// Where a point of `space` lies on the panel, in the panel's points,
    /// where points of both spaces can be compared.
    pub fn on_panel(&self, space: Space, p: (f64, f64)) -> (f64, f64) {
        let (u, v) = self.share_of_panel(space, p);
        (u * self.panel.0, v * self.panel.1)
    }
}

/// Touch-report coordinates (0..=65535 across the panel) for a point of
/// `space` on `display`.
pub fn report_point(p: (f64, f64), space: Space, display: &ActiveDisplay) -> (u16, u16) {
    let unit = |share: f64| (share * 65535.0).round().clamp(0.0, 65535.0) as u16;
    let (u, v) = display.share_of_panel(space, p);
    (unit(u), unit(v))
}

/// The display in use, from `getdisplayinfo`'s answer. Of the displays with
/// a size, exactly one must be active; the phone also lists displays of no
/// size, and those are passed over.
pub fn parse(answer: plist::Value) -> Result<ActiveDisplay, String> {
    let output = media::output(answer).map_err(|e| format!("{e:#}"))?;
    let displays = output
        .as_dictionary()
        .and_then(|o| o.get("displays"))
        .and_then(|d| d.as_array())
        .ok_or("the answer lists no displays")?;
    let number = |v: &plist::Value| v.as_real().or_else(|| v.as_signed_integer().map(|i| i as f64));
    let size = |d: &plist::Dictionary| -> Option<(f64, f64)> {
        let corner = d.get("bounds")?.as_array()?.get(1)?.as_array()?;
        Some((number(corner.first()?)?, number(corner.get(1)?)?))
    };
    let active: Vec<&plist::Dictionary> = displays
        .iter()
        .filter_map(|d| d.as_dictionary())
        .filter(|d| size(d).is_some_and(|(w, h)| w > 0.0 && h > 0.0))
        .filter(|d| d.get("active").and_then(|a| a.as_boolean()) == Some(true))
        .collect();
    let [display] = active[..] else {
        return Err(format!("{} displays are active, not one", active.len()));
    };
    let text = |key: &str| display.get(key).and_then(|v| v.as_string()).ok_or(format!("the active display has no {key}"));
    let scale = display
        .get("pointScale")
        .and_then(number)
        .filter(|s| *s > 0.0)
        .ok_or("the active display has no pointScale")?;
    let (w, h) = size(display).ok_or("the active display has no bounds")?;
    Ok(ActiveDisplay {
        unique_id: text("uniqueId")?.to_string(),
        display_id: display.get("displayId").and_then(|v| v.as_signed_integer()),
        panel: (w / scale, h / scale),
        rotation: Rotation::parse(text("currentOrientation")?)?,
    })
}

/// A connection to the phone's device-information service. After an error
/// it is not used again: a request cut off leaves its answer on the
/// connection for the next one to read.
pub struct DisplayInfo(RemoteXpcClient<Box<dyn ReadWrite>>);

impl RsdService for DisplayInfo {
    fn rsd_service_name() -> Cow<'static, str> {
        SERVICE.into()
    }

    async fn from_stream(stream: Box<dyn ReadWrite>) -> Result<Self, IdeviceError> {
        let mut xpc = RemoteXpcClient::new(stream).await?;
        xpc.do_handshake().await?;
        Ok(Self(xpc))
    }
}

impl DisplayInfo {
    pub async fn open(adapter: &AdapterHandle, rsd: &RsdHandshake) -> Result<Self, String> {
        let (mut adapter, mut rsd) = (adapter.clone(), rsd.clone());
        bounded("display info", WITHIN_SECS, Self::connect_rsd(&mut adapter, &mut rsd)).await
    }

    /// The display in use now.
    pub async fn active(&mut self) -> Result<ActiveDisplay, String> {
        let answer = bounded("display info", WITHIN_SECS, async {
            self.0.send_object(media::request(FEATURE, Dictionary::new(), None), true).await?;
            self.0.recv().await
        })
        .await?;
        parse(answer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The inner display of an iPhone Duo: 2007x2853 pixels at 3x.
    fn inner(rotation: Rotation) -> ActiveDisplay {
        ActiveDisplay { unique_id: "123-456-789".into(), display_id: Some(2), panel: (669.0, 951.0), rotation }
    }

    /// The cover display: 1398x2034 pixels at 3x, never turned.
    fn cover() -> ActiveDisplay {
        ActiveDisplay { unique_id: "987-654-321".into(), display_id: Some(1), panel: (466.0, 678.0), rotation: Rotation::R0 }
    }

    /// Shares of the panel in report coordinates. 0.2 and its multiples land
    /// on whole report units.
    fn report(u: f64, v: f64) -> (u16, u16) {
        ((u * 65535.0).round() as u16, (v * 65535.0).round() as u16)
    }

    #[test]
    fn folded_the_cover_panel_and_the_screen_are_the_same_space() {
        let d = cover();
        assert_eq!(d.size(Space::Panel), (466.0, 678.0));
        assert_eq!(d.size(Space::Screen), (466.0, 678.0));
        let p = (466.0 * 0.6, 678.0 * 0.8);
        assert_eq!(report_point(p, Space::Panel, &d), report(0.6, 0.8));
        assert_eq!(report_point(p, Space::Screen, &d), report(0.6, 0.8));
    }

    #[test]
    fn the_home_screen_on_the_turned_inner_display_is_in_panel_points_and_not_turned() {
        for rotation in [Rotation::R0, Rotation::R90, Rotation::R270] {
            let d = inner(rotation);
            assert_eq!(d.size(Space::Panel), (669.0, 951.0), "{rotation:?}");
            let p = (669.0 * 0.2, 951.0 * 0.6);
            assert_eq!(report_point(p, Space::Panel, &d), report(0.2, 0.6), "{rotation:?}");
        }
    }

    #[test]
    fn an_app_on_the_inner_display_at_rot270_is_turned_to_one_minus_v_u() {
        let d = inner(Rotation::R270);
        assert_eq!(d.size(Space::Screen), (951.0, 669.0));
        let p = (951.0 * 0.2, 669.0 * 0.6);
        assert_eq!(report_point(p, Space::Screen, &d), report(1.0 - 0.6, 0.2));
    }

    #[test]
    fn an_app_on_the_inner_display_at_rot90_is_turned_to_v_one_minus_u() {
        let d = inner(Rotation::R90);
        assert_eq!(d.size(Space::Screen), (951.0, 669.0));
        let p = (951.0 * 0.2, 669.0 * 0.6);
        assert_eq!(report_point(p, Space::Screen, &d), report(0.6, 1.0 - 0.2));
    }

    #[test]
    fn an_app_on_the_inner_display_at_rot0_is_not_turned() {
        let d = inner(Rotation::R0);
        assert_eq!(d.size(Space::Screen), (669.0, 951.0));
        let p = (669.0 * 0.2, 951.0 * 0.6);
        assert_eq!(report_point(p, Space::Screen, &d), report(0.2, 0.6));
    }

    #[test]
    fn rot180_is_expected_to_mirror_both_axes() {
        let d = inner(Rotation::R180);
        assert_eq!(d.size(Space::Screen), (669.0, 951.0));
        let p = (669.0 * 0.2, 951.0 * 0.6);
        assert_eq!(report_point(p, Space::Screen, &d), report(1.0 - 0.2, 1.0 - 0.6));
        assert_eq!(report_point(p, Space::Panel, &d), report(0.2, 0.6));
    }

    #[test]
    fn the_same_point_of_the_panel_is_reached_from_either_space() {
        let d = inner(Rotation::R270);
        let on_screen = (951.0 * 0.2, 669.0 * 0.6);
        let on_panel = d.on_panel(Space::Screen, on_screen);
        assert_eq!(report_point(on_panel, Space::Panel, &d), report_point(on_screen, Space::Screen, &d));
        assert_eq!(d.on_panel(Space::Panel, (10.0, 20.0)), (10.0, 20.0));
    }

    #[test]
    fn points_outside_the_space_clamp_to_the_edge() {
        let d = cover();
        assert_eq!(report_point((-5.0, 700.0), Space::Panel, &d), (0, 65535));
    }

    fn real_pair(a: f64, b: f64) -> plist::Value {
        plist::Value::Array(vec![plist::Value::Real(a), plist::Value::Real(b)])
    }

    fn display(id: Option<&str>, number: i64, size: (f64, f64), active: Option<bool>, orientation: &str) -> plist::Value {
        let mut d = plist::Dictionary::new();
        if let Some(id) = id {
            d.insert("uniqueId".into(), plist::Value::String(id.into()));
        }
        d.insert("displayId".into(), plist::Value::Integer(number.into()));
        d.insert("bounds".into(), plist::Value::Array(vec![real_pair(0.0, 0.0), real_pair(size.0, size.1)]));
        d.insert("pointScale".into(), plist::Value::Integer(3.into()));
        if let Some(active) = active {
            d.insert("active".into(), plist::Value::Boolean(active));
        }
        d.insert("currentOrientation".into(), plist::Value::String(orientation.into()));
        plist::Value::Dictionary(d)
    }

    fn answer(displays: Vec<plist::Value>) -> plist::Value {
        let mut output = plist::Dictionary::new();
        output.insert("displays".into(), plist::Value::Array(displays));
        let mut answer = plist::Dictionary::new();
        answer.insert("CoreDevice.output".into(), plist::Value::Dictionary(output));
        plist::Value::Dictionary(answer)
    }

    #[test]
    fn folded_the_cover_display_is_the_active_one() {
        let folded = answer(vec![
            display(Some("987-654-321"), 1, (1398.0, 2034.0), Some(true), "rot0"),
            display(Some("123-456-789"), 2, (2007.0, 2853.0), Some(false), "rot90"),
            display(None, 4, (0.0, 0.0), None, "rot0"),
        ]);
        assert_eq!(parse(folded), Ok(cover()));
    }

    #[test]
    fn unfolded_the_inner_display_is_the_active_one_with_its_own_turn() {
        let unfolded = answer(vec![
            display(Some("987-654-321"), 1, (1398.0, 2034.0), Some(false), "rot90"),
            display(Some("123-456-789"), 2, (2007.0, 2853.0), Some(true), "rot270"),
            display(None, 4, (0.0, 0.0), Some(true), "rot0"),
        ]);
        assert_eq!(parse(unfolded), Ok(inner(Rotation::R270)));
    }

    #[test]
    fn no_active_display_two_active_displays_or_an_unknown_turn_is_an_error() {
        let none = answer(vec![display(Some("987-654-321"), 1, (1398.0, 2034.0), Some(false), "rot0")]);
        assert!(parse(none).is_err());
        let two = answer(vec![
            display(Some("987-654-321"), 1, (1398.0, 2034.0), Some(true), "rot0"),
            display(Some("123-456-789"), 2, (2007.0, 2853.0), Some(true), "rot0"),
        ]);
        assert!(parse(two).is_err());
        let odd = answer(vec![display(Some("987-654-321"), 1, (1398.0, 2034.0), Some(true), "rot45")]);
        assert!(parse(odd).is_err());
        let nameless = answer(vec![display(None, 1, (1398.0, 2034.0), Some(true), "rot0")]);
        assert!(parse(nameless).is_err());
        let failed = plist::Value::Dictionary(plist::Dictionary::new());
        assert!(parse(failed).is_err());
    }

    #[test]
    fn an_active_display_without_a_number_is_still_the_display_in_use() {
        let mut unnumbered = display(Some("987-654-321"), 1, (1398.0, 2034.0), Some(true), "rot0");
        unnumbered.as_dictionary_mut().unwrap().remove("displayId");
        assert_eq!(parse(answer(vec![unnumbered])), Ok(ActiveDisplay { display_id: None, ..cover() }));
    }
}
