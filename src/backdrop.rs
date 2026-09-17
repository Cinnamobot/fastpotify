//! The Windows 11 Acrylic background.
//!
//! DWM draws Acrylic *behind* the window and composites the app's own frames
//! over it. The app therefore has to leave every layer it wants to see through
//! transparent; `App::base_fill` and `App::content_fill` are the two layers
//! that matter.
//!
//! One DWM call picks the material:
//! `DwmSetWindowAttribute(DWMWA_SYSTEMBACKDROP_TYPE, DWMSBT_TRANSIENTWINDOW)`.
//! The window is made transparent, so DWM composites the app's per-pixel alpha
//! over that material.
//!
//! Deliberately NOT used: `DwmExtendFrameIntoClientArea`. Extending the frame
//! is the Vista-era way to get a glass client area, and it drags the *whole*
//! frame in with it: DWM then draws its own Minimize, Maximize and Close
//! caption buttons over the client area. This window is undecorated and draws
//! its own controls (see `ui::window_controls`), so the two sets would sit on
//! top of each other. Windows 11 does not need the extended frame for a
//! material anyway -- a transparent window is enough.
//!
//! Mica and Mica Alt (`DWMSBT_MAINWINDOW` / `DWMSBT_TABBEDWINDOW`) are
//! deliberately not offered. They are opaque materials that carry the wallpaper
//! colour once, so they never show the windows behind; Acrylic is the only one
//! of the three that does what "see-through background" means. Mica is also
//! what Microsoft recommends for a window's base layer, and reintroducing it
//! means adding a `Material` variant back, not just a number here.
//!
//! Deliberately unused: [`egui::ViewportBuilder::with_transparent`]. winit
//! answers that with the legacy `DwmEnableBlurBehindWindow`, which has done
//! nothing for blur since Windows 8 and fights the material. The extended
//! frame above is what hands DWM the app's per-pixel alpha instead.
//!
//! Nothing here is a promise: DWM is free to ignore the material. Windows
//! substitutes a solid colour when transparency effects are off, the window is
//! inactive, Battery Saver is on, or the hardware cannot compose it. That
//! fallback is the OS's to make, so the app never works around it.

use egui::Color32;

/// The material DWM draws behind the window. One material: Acrylic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Material {
    /// A translucent material that shows the windows behind this one.
    Acrylic,
}

impl Material {
    /// The value for `DWMWA_SYSTEMBACKDROP_TYPE`: `DWMSBT_TRANSIENTWINDOW`.
    fn system_backdrop(self) -> i32 {
        match self {
            Self::Acrylic => 3,
        }
    }
}

/// What the user asked for, before the environment has its say.
///
/// Two states, because there is one material. A name that is no longer offered
/// still has to read: an older settings file may say `mica`, `mica-alt` or
/// `acrylic`, and all three now mean the same thing, so they land on
/// [`Choice::Acrylic`] rather than failing the whole file.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Choice {
    /// Acrylic where DWM composes it, the app's own background everywhere else.
    #[default]
    Automatic,
    /// Ask for Acrylic even where Windows would rather substitute a solid
    /// colour -- mostly a way to tell "DWM refused" from "Windows decided not
    /// to", rather than a different look.
    Acrylic,
    /// Paint the window as before, whatever Windows supports.
    Opaque,
}

impl Choice {
    pub const ALL: [Choice; 3] = [Self::Automatic, Self::Acrylic, Self::Opaque];

    pub fn label(self) -> &'static str {
        match self {
            Self::Automatic => "Follow Windows",
            Self::Acrylic => "Acrylic",
            Self::Opaque => "Opaque",
        }
    }
}

impl<'de> serde::Deserialize<'de> for Choice {
    /// Reads a name, including the ones this build no longer offers. `serde`'s
    /// own `alias` cannot map several names onto one variant, and `untagged`
    /// cannot report which name was wrong, so the mapping is written out.
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let name = String::deserialize(deserializer)?;
        match name.as_str() {
            "automatic" => Ok(Self::Automatic),
            // Every material earlier builds offered was a see-through
            // background, which is what Acrylic is.
            "acrylic" | "mica" | "mica-alt" => Ok(Self::Acrylic),
            "opaque" => Ok(Self::Opaque),
            _ => Err(serde::de::Error::unknown_variant(
                &name,
                &["automatic", "acrylic", "opaque"],
            )),
        }
    }
}

/// What this machine and this Windows build can do. Probed once: the answers
/// do not change while the app runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Support {
    /// Build 22621 (22H2) added `DWMWA_SYSTEMBACKDROP_TYPE`.
    pub backdrops: bool,
    /// DWM composition. Effectively always on since Windows 8.
    pub composition: bool,
    /// Windows' own "Transparency effects" switch. With it off, DWM replaces
    /// the material with a solid colour.
    pub transparency: bool,
}

impl Support {
    /// The material to ask DWM for, or `None` to paint the window ourselves.
    ///
    /// Pure: the environment is already in `self`, so the decision can be
    /// checked without a window.
    pub fn material(self, choice: Choice) -> Option<Material> {
        // An unsupported material is not an error: the window keeps the look it
        // has always had.
        if choice == Choice::Opaque || !self.backdrops || !self.composition {
            return None;
        }
        match choice {
            // Windows would draw a solid colour with transparency effects off,
            // so asking would only take the client area out of DWM's ordinary
            // hit testing for nothing. Keep our own background instead.
            Choice::Automatic => self.transparency.then_some(Material::Acrylic),
            Choice::Acrylic => Some(Material::Acrylic),
            Choice::Opaque => None,
        }
    }
}

/// The window's base layer under a live material: paint nothing and let the
/// material be what shows.
pub const BASE_ALPHA: u8 = 0;

/// What share of each layer the app paints, as a percentage of full opacity.
/// The rest of that layer is Acrylic showing through.
///
/// This is not a knob on Acrylic itself. `DWMSBT_TRANSIENTWINDOW` is a fixed
/// material whose density is DWM's -- Windows exposes no "beta" for it, and
/// the WinUI control that does take a tint (`SystemBackdropElement`) needs an
/// XAML island. So the only honest lever is how much of the app covers the
/// material up: at 10% the app is a thin wash over glass, at 90% it is nearly
/// its own colour again.
///
/// One number covers every layer, so the app's contrast does not come apart:
/// each layer keeps its share of the whole, which is what keeps text readable
/// at either end.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct Opacity(pub u8);

impl Opacity {
    /// What a settings file without the field gets. Low enough that the
    /// material is clearly doing something, high enough to read against.
    pub const DEFAULT: Opacity = Opacity(25);

    /// The least and most the app will paint. The floor is what keeps text off
    /// whatever is behind the window; the ceiling still lets the material
    /// show at the edges of a panel-free window.
    pub const RANGE: std::ops::RangeInclusive<u8> = 10..=90;

    /// The window's base fill. Stays fully transparent at every setting: the
    /// material *is* the base layer, and covering it is what the other layers
    /// are for.
    pub fn base_alpha(self) -> u8 {
        BASE_ALPHA
    }

    /// A content panel's alpha.
    pub fn panel_alpha(self) -> u8 {
        // A panel is the full amount; the wash below is a fraction of it.
        self.0
    }

    /// A page header wash's alpha: a little stronger than a panel, so the
    /// gradient still reads as a header rather than a flat panel.
    pub fn wash_alpha(self) -> u8 {
        // 1.5x, saturating: the wash never runs past its own scale.
        self.0.saturating_mul(3) / 2
    }
}

impl Default for Opacity {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// `color` at `alpha`, for a fill that a material shows through.
pub fn layer(color: Color32, alpha: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), alpha)
}

/// The window's base fill: transparent under a material, the palette's own
/// window colour otherwise.
pub fn base_fill(
    palette: &crate::theme::Palette,
    material: Option<Material>,
    opacity: Opacity,
) -> Color32 {
    match material {
        Some(_) => layer(palette.window, opacity.base_alpha()),
        None => palette.window,
    }
}

/// A panel's fill over the base.
pub fn content_fill(
    palette: &crate::theme::Palette,
    material: Option<Material>,
    opacity: Opacity,
) -> Color32 {
    match material {
        Some(_) => layer(palette.panel, opacity.panel_alpha()),
        None => palette.panel,
    }
}

/// A page header wash's fill.
pub fn wash_fill(
    base: Color32,
    material: Option<Material>,
    opacity: Opacity,
) -> Color32 {
    match material {
        Some(_) => layer(base, opacity.wash_alpha()),
        None => base,
    }
}

#[cfg(windows)]
mod win {
    use super::{Material, Support};
    use windows_sys::Win32::Foundation::HWND;

    /// The window this module talks to DWM about. A bare handle, not a
    /// pointer: callers pass `hwnd.get()` and never deal in raw pointers.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Window(pub isize);

    impl Window {
        fn hwnd(self) -> HWND {
            self.0 as HWND
        }
    }
    use windows_sys::Win32::Graphics::Dwm::{
        DWMSBT_NONE, DWMWA_SYSTEMBACKDROP_TYPE, DWMWA_USE_IMMERSIVE_DARK_MODE,
        DwmIsCompositionEnabled, DwmSetWindowAttribute,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SetWindowPos,
    };
    use windows_sys::Win32::System::Registry::{
        HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RRF_RT_REG_SZ, RegGetValueW,
    };

    /// Reads one Personalize DWORD, or `None` when the value is absent or not
    /// a DWORD.
    fn personalize_dword(name: *const u16) -> Option<u32> {
        let mut value: u32 = 0;
        let mut bytes = std::mem::size_of::<u32>() as u32;
        // SAFETY: the subkey, value name, and out-parameters are all live for
        // the call, and `RRF_RT_REG_DWORD` makes the four-byte buffer the size
        // Windows expects to write.
        let read = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                windows_sys::w!(
                    "Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"
                ),
                name,
                RRF_RT_REG_DWORD,
                std::ptr::null_mut(),
                (&mut value as *mut u32).cast(),
                &mut bytes,
            )
        };
        (read == 0 && bytes == 4).then_some(value)
    }

    /// The installed build, from `CurrentBuild`. `GetVersionExW` reports the
    /// compatibility-shimmed version instead of the real one.
    fn build() -> u32 {
        let mut buffer = [0u16; 32];
        let mut bytes = (buffer.len() * 2) as u32;
        // SAFETY: as above; `RRF_RT_REG_SZ` fills the UTF-16 buffer and reports
        // the byte count actually written.
        let read = unsafe {
            RegGetValueW(
                windows_sys::Win32::System::Registry::HKEY_LOCAL_MACHINE,
                windows_sys::w!("SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion"),
                windows_sys::w!("CurrentBuild"),
                RRF_RT_REG_SZ,
                std::ptr::null_mut(),
                buffer.as_mut_ptr().cast(),
                &mut bytes,
            )
        };
        if read != 0 {
            return 0;
        }
        let length = (bytes as usize / 2).saturating_sub(1);
        String::from_utf16_lossy(&buffer[..length])
            .trim()
            .parse()
            .unwrap_or(0)
    }

    /// Whether DWM is composing. A failure reads as "no": the material would
    /// not show anyway.
    fn composition() -> bool {
        let mut enabled = 0;
        // SAFETY: `enabled` is a live BOOL for the duration of the call.
        (unsafe { DwmIsCompositionEnabled(&mut enabled) } >= 0) && enabled != 0
    }

    /// What this machine can do, read once.
    pub fn support() -> Support {
        Support {
            backdrops: build() >= 22_621,
            composition: composition(),
            // Absent reads as on, which is Windows' own default.
            transparency: personalize_dword(windows_sys::w!("EnableTransparency"))
                .is_none_or(|value| value != 0),
        }
    }

    /// Nudges DWM into recomputing the window's non-client rendering.
    ///
    /// `DWMWA_USE_IMMERSIVE_DARK_MODE` and the material's tint are read when
    /// DWM activates the window's frame, not when the attribute is set. On a
    /// window whose client area is already the glass region, the attribute can
    /// land without a frame change ever following it, and the material keeps
    /// the tint it was first drawn with -- which is how a dark theme ends up
    /// with Acrylic's light wash. `SWP_FRAMECHANGED` with everything else
    /// suppressed asks for exactly that recomputation and nothing more: no
    /// move, no resize, no restack, no focus change.
    fn refresh_frame(hwnd: HWND) {
        // SAFETY: `hwnd` is a live window; the call reads no memory of ours and
        // every geometry and ordering flag is suppressed.
        unsafe {
            SetWindowPos(
                hwnd,
                std::ptr::null_mut(),
                0,
                0,
                0,
                0,
                SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
    }

    /// Asks DWM for `material`.
    ///
    /// Returns whether DWM accepted it; a refusal leaves the window as it was,
    /// which is the opaque look. The window must already be transparent --
    /// `App::clear_color` and the base fill see to that -- or the material is
    /// drawn behind an opaque client area and never shows.
    pub fn apply(window: Window, material: Material, dark: bool) -> bool {
        let hwnd = window.hwnd();
        if hwnd.is_null() {
            return false;
        }
        let backdrop = material.system_backdrop();
        // SAFETY: `hwnd` came from this window and both pointers are live,
        // correctly sized values for the attributes named.
        unsafe {
            // Dark mode first: DWM tints the material from it, so setting the
            // material second means the tint is right on the first frame
            // rather than corrected by the refresh below.
            let dark = i32::from(dark);
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWA_USE_IMMERSIVE_DARK_MODE as u32,
                (&dark as *const i32).cast(),
                std::mem::size_of_val(&dark) as u32,
            );
            let accepted = DwmSetWindowAttribute(
                hwnd,
                DWMWA_SYSTEMBACKDROP_TYPE as u32,
                (&backdrop as *const i32).cast(),
                std::mem::size_of_val(&backdrop) as u32,
            ) >= 0;
            if accepted {
                refresh_frame(hwnd);
            }
            accepted
        }
    }

    /// Puts the window back to painting its own background.
    pub fn clear(window: Window) {
        let hwnd = window.hwnd();
        if hwnd.is_null() {
            return;
        }
        let backdrop = DWMSBT_NONE;
        // SAFETY: as in `apply`.
        unsafe {
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWA_SYSTEMBACKDROP_TYPE as u32,
                (&backdrop as *const i32).cast(),
                std::mem::size_of_val(&backdrop) as u32,
            );
            refresh_frame(hwnd);
        }
    }
}

#[cfg(windows)]
pub use win::{Window, apply, clear, support};

#[cfg(test)]
mod tests {
    use super::*;

    fn support(backdrops: bool, composition: bool, transparency: bool) -> Support {
        Support {
            backdrops,
            composition,
            transparency,
        }
    }

    #[test]
    fn automatic_asks_for_acrylic_only_when_windows_would_compose_it() {
        let capable = support(true, true, true);
        assert_eq!(capable.material(Choice::Automatic), Some(Material::Acrylic));
        // Transparency effects off: DWM would draw a solid colour, so the app
        // keeps its own background instead of asking for nothing.
        assert_eq!(support(true, true, false).material(Choice::Automatic), None);
    }

    #[test]
    fn the_choice_defaults_to_automatic_and_acrylic() {
        // Both the settings default and what "automatic" resolves to.
        assert_eq!(Choice::default(), Choice::Automatic);
        assert_eq!(
            support(true, true, true).material(Choice::default()),
            Some(Material::Acrylic)
        );
        // Every choice is offered, or a wrong one could not be undone.
        assert_eq!(Choice::ALL.len(), 3);
        for choice in Choice::ALL {
            let _ = choice.label();
        }
    }

    #[test]
    fn a_settings_file_naming_a_removed_material_still_reads() {
        // The three materials an earlier build offered all meant "see-through
        // background", so they fold onto Acrylic instead of failing the file.
        for name in ["\"acrylic\"", "\"mica\"", "\"mica-alt\""] {
            let choice: Choice = serde_json::from_str(name).unwrap_or_else(|error| {
                panic!("{name} must still read, or an old settings file is lost: {error}")
            });
            assert_eq!(choice, Choice::Acrylic, "{name}");
        }
        assert_eq!(
            serde_json::from_str::<Choice>("\"automatic\"").unwrap(),
            Choice::Automatic
        );
        assert_eq!(
            serde_json::from_str::<Choice>("\"opaque\"").unwrap(),
            Choice::Opaque
        );
        // A name this build has never heard of is still an error, not a silent
        // reset to the default.
        assert!(serde_json::from_str::<Choice>("\"lava-lamp\"").is_err());
        // And a choice round-trips as the name it is.
        for choice in Choice::ALL {
            let text = serde_json::to_string(&choice).unwrap();
            assert_eq!(serde_json::from_str::<Choice>(&text).unwrap(), choice);
        }
    }

    #[test]
    fn an_explicit_material_is_taken_at_its_word() {
        // Even with transparency effects off: the explicit choice is the one
        // way to see whether DWM refused, rather than Windows deciding not to.
        let capable = support(true, true, false);
        assert_eq!(capable.material(Choice::Acrylic), Some(Material::Acrylic));
    }

    #[test]
    fn opaque_never_asks_for_a_material() {
        let capable = support(true, true, true);
        for choice in Choice::ALL.iter().filter(|choice| **choice == Choice::Opaque) {
            assert_eq!(capable.material(*choice), None);
        }
    }

    #[test]
    fn an_older_or_uncomposed_windows_keeps_the_apps_own_background() {
        for support in [
            support(false, true, true),
            support(true, false, true),
            support(false, false, false),
        ] {
            for choice in Choice::ALL {
                assert_eq!(
                    support.material(choice),
                    None,
                    "{choice:?} on {support:?} must not be asked for"
                );
            }
        }
    }

    #[test]
    fn the_material_maps_to_its_dwm_value() {
        // DWMSBT_TRANSIENTWINDOW, pinned so a copy-paste swap is caught.
        assert_eq!(Material::Acrylic.system_backdrop(), 3);
    }

    #[test]
    fn a_layer_at_full_opacity_is_the_colour_it_came_from() {
        let palette = crate::theme::Palette::light();
        for color in [palette.window, palette.panel, palette.surface] {
            assert_eq!(layer(color, 255), color);
        }
    }

    #[test]
    fn a_live_material_leaves_the_base_transparent_and_keeps_panels_readable() {
        let palette = crate::theme::Palette::dark();
        let opacity = Opacity::DEFAULT;
        let opaque = base_fill(&palette, None, opacity);
        assert_eq!(opaque, palette.window);
        assert_eq!(opaque.a(), 255, "with no material the base is the ground");

        let clear = base_fill(&palette, Some(Material::Acrylic), opacity);
        assert_eq!(clear.a(), BASE_ALPHA, "the material is what shows");

        // The colour stays the palette's; only the alpha changes. Compared
        // through `layer` itself rather than by value: `Color32` stores
        // premultiplied linear channels, so a low-alpha colour does not survive
        // an exact round trip.
        let panel = content_fill(&palette, Some(Material::Acrylic), opacity);
        assert_eq!(panel, layer(palette.panel, opacity.panel_alpha()));
        assert_eq!(panel.a(), opacity.0);
        assert_eq!(content_fill(&palette, None, opacity), palette.panel);
    }

    #[test]
    fn the_opacity_keeps_the_layers_in_order_at_every_setting() {
        let palette = crate::theme::Palette::dark();
        let material = Some(Material::Acrylic);
        for value in Opacity::RANGE {
            let opacity = Opacity(value);
            let base = base_fill(&palette, material, opacity);
            let panel = content_fill(&palette, material, opacity);
            let wash = wash_fill(palette.window, material, opacity);
            assert_eq!(base.a(), BASE_ALPHA, "{value}: the base stays clear");
            assert!(
                panel.a() > base.a(),
                "{value}: a panel must sit over the material"
            );
            assert!(
                wash.a() >= panel.a(),
                "{value}: a header must not read lighter than a panel"
            );
        }
        // The range is what keeps text off the wallpaper and the material on
        // screen: a 0 would put text straight on the desktop.
        assert!(Opacity::RANGE.start() > &0);
        assert!(Opacity::RANGE.end() < &100);
        assert!(Opacity::RANGE.contains(&Opacity::DEFAULT.0));
    }

    #[test]
    fn a_higher_opacity_paints_more_of_the_apps_own_colour() {
        let palette = crate::theme::Palette::dark();
        let material = Some(Material::Acrylic);
        let mut last = 0;
        for value in Opacity::RANGE {
            let panel = content_fill(&palette, material, Opacity(value));
            assert!(
                panel.a() >= last,
                "raising the opacity must never thin a layer"
            );
            last = panel.a();
        }
        assert_eq!(last, *Opacity::RANGE.end());
    }

    #[test]
    fn opacity_is_a_plain_number_in_a_settings_file() {
        let settings: Opacity = serde_json::from_str("55").unwrap();
        assert_eq!(settings, Opacity(55));
        assert_eq!(serde_json::to_string(&Opacity(55)).unwrap(), "55");
        // A file written before the field existed keeps the tuned default.
        assert_eq!(Opacity::default(), Opacity::DEFAULT);
    }
}
