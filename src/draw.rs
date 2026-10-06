use std::f64::consts::PI;
use std::time::Duration;

use cairo::{
    Antialias, Context, Extend, Filter, Format, HintMetrics, HintStyle, ImageSurface,
    LinearGradient, Operator,
};
use pango::FontDescription;
use pango::glib::{self, DateTime};
use pango::prelude::FontMapExt;
use tracing::error;

use crate::config::{Anchor, LockScreen, Offset, Place, Style, Weight};
use crate::gtk::{self, Palette, Rgba};
use crate::password::{Look, Status};
use crate::wallpaper::Blurred;

/// Logical pixels between the password field and the caps lock line, and between the
/// field's edge and its arrow button.
const GAP: f64 = 10.0;
const BUTTON_INSET: f64 = 5.0;

/// The opacities of white over the background that make a shape frosted: the password
/// field, its arrow button, and the avatar; and their outlines.
const FIELD: f64 = 0.2;
const BUTTON: f64 = 0.3;
const AVATAR: f64 = 0.22;
const FIELD_OUTLINE: f64 = 0.28;
const AVATAR_OUTLINE: f64 = 0.35;

/// How much of their opacity the field and its dots keep while the password is checked.
const CHECKING: f64 = 0.5;

/// Where the top gradient ends and the bottom one starts, as fractions of the height.
const GRADIENTS: (f64, f64) = (0.4, 0.62);

/// How much smaller than a frame the background is kept; it is blurred, so it loses
/// nothing when scaled back up as each frame is drawn.
const SHRINK: u32 = 4;

/// The widest and tallest a rendered text may be, in physical pixels; more is cut off.
const MAX_TEXT: i32 = 8192;

/// Prepares each output's `Scene` ahead of time (at startup, on SIGHUP, when the
/// wallpaper or the clock changes), so that drawing a frame only composites.
pub struct Painter {
    config: LockScreen,
    fonts: pango::Context,
    /// GTK's font, whose size and weight each text sets.
    font: FontDescription,
    palette: Palette,
    /// The user's full name.
    name: String,
    /// The blurred wallpaper, if there is one.
    wallpaper: Option<ImageSurface>,
}

/// How a text is rendered: its size in logical pixels, weight, spacing between
/// characters, and colour.
struct Font {
    size: f64,
    weight: Weight,
    spacing: f64,
    colour: Rgba,
}

/// Text rendered at an output's scale, with its top-left corner in physical pixels.
struct Label {
    /// What it says, where that can change (the date and clock); empty elsewhere.
    text: String,
    surface: ImageSurface,
    at: (f64, f64),
}

/// A rectangle in physical pixels: x, y, width, height.
type Rect = (f64, f64, f64, f64);

/// One output's lock screen, ready to draw: the background and every text rendered,
/// every shape placed, in physical pixels. A part that could not be prepared is left out.
pub struct Scene {
    /// The output's logical size, the scale everything is drawn at (in 120ths), and the
    /// size of its frames in physical pixels.
    pub size: (u32, u32),
    pub scale: u32,
    pub pixels: (u32, u32),
    /// The background, gradients included, at 1/`SHRINK` of a frame's size.
    background: Option<ImageSurface>,
    /// What is drawn if the background cannot be.
    plain: Rgba,
    /// The date and the clock.
    times: [Option<Label>; 2],
    name: Option<Label>,
    /// The circle and the initial.
    avatar: Option<(Rect, Option<Label>)>,
    field: Field,
}

/// The password field, placed.
struct Field {
    pill: Rect,
    radius: f64,
    /// A dot's diameter, which is also the gap between dots.
    dot: f64,
    /// The arrow button's centre and radius.
    button: (f64, f64, f64),
    colour: Rgba,
    error: Rgba,
    /// "Enter Password", the arrow and "⇪ Caps Lock is on", placed.
    hint: Option<Label>,
    arrow: Option<Label>,
    caps: Option<Label>,
    /// "Try again in N s", with its N.
    countdown: Option<(u64, Label)>,
}

impl Painter {
    /// Read GTK's font and colours, and the user's name.
    pub fn new(config: LockScreen) -> Self {
        let fonts = pangocairo::FontMap::default().create_context();
        let mut options = cairo::FontOptions::new().expect("cairo font options");
        options.set_antialias(Antialias::Gray);
        options.set_hint_style(HintStyle::Slight);
        options.set_hint_metrics(HintMetrics::Off);
        pangocairo::functions::context_set_font_options(&fonts, Some(&options));
        // GLib's name for a user without one in the passwd database.
        let name = glib::real_name().into_string().ok();
        let name = name.filter(|name| name != "Unknown");
        Self {
            config,
            fonts,
            font: FontDescription::from_string(&gtk::font()),
            palette: Palette::load(),
            name: name.unwrap_or_else(|| glib::user_name().to_string_lossy().into_owned()),
            wallpaper: None,
        }
    }

    /// Use `wallpaper` from now on.
    pub fn set_wallpaper(&mut self, wallpaper: Blurred) {
        let (width, height) = (wallpaper.width as i32, wallpaper.height as i32);
        let pixels = wallpaper.pixels;
        self.wallpaper =
            ImageSurface::create_for_data(pixels, Format::Rgb24, width, height, width * 4).ok();
    }

    /// Read GTK's colours again.
    pub fn reload_colours(&mut self) {
        self.palette = Palette::load();
    }

    /// Prepare the lock screen for an output of logical `size`, drawn at `scale` (in
    /// 120ths) into frames of `pixels`.
    pub fn scene(&self, size: (u32, u32), scale: u32, pixels: (u32, u32)) -> Scene {
        let factor = f64::from(scale) / 120.0;
        let password = &self.config.password;
        let colour = self.palette.get(&password.color);
        let background = self.config.background.as_ref();
        let mut scene = Scene {
            size,
            scale,
            pixels,
            background: self
                .background(pixels)
                .inspect_err(|e| error!("preparing the lock screen's background: {e}"))
                .ok(),
            plain: background.map_or([0.0, 0.0, 0.0, 1.0], |b| self.palette.get(&b.color)),
            times: [None, None],
            name: None,
            avatar: None,
            field: Field {
                pill: (0.0, 0.0, 0.0, 0.0),
                radius: password.radius.0.min(password.height.0 / 2.0) * factor,
                dot: password.dot_size.0 * factor,
                button: (0.0, 0.0, 0.0),
                colour,
                error: self.palette.get(&password.error_color),
                hint: None,
                arrow: None,
                caps: None,
                countdown: None,
            },
        };
        self.refresh(&mut scene);
        if let Some(name) = &self.config.name {
            let font = self.font_of(&name.style);
            scene.name = self.label(&self.name, &font, factor, |s| scene.place(&name.place, s));
        }
        if let Some(avatar) = &self.config.avatar {
            let d = avatar.diameter.0;
            let circle = scene.physical(scene.place_logical(&avatar.place, (d, d)), (d, d));
            let initial: String = self
                .name
                .chars()
                .take(1)
                .flat_map(char::to_uppercase)
                .collect();
            let font = small(
                d * 26.0 / 64.0,
                Weight::Semibold,
                self.palette.get(&avatar.color),
            );
            let initial = self.label(&initial, &font, factor, |s| centred(s, middle(circle)));
            scene.avatar = Some((circle, initial));
        }
        let small = |size, weight, alpha| small(size, weight, fade(colour, alpha));
        let caps_font = small(12.0, Weight::Medium, 0.8);
        let caps = self.label("⇪ Caps Lock is on", &caps_font, factor, |_| (0.0, 0.0));
        let caps_height = caps.as_ref().map_or(15.0, |caps| caps.size().1 / factor);
        let (width, height) = (password.width.0, password.height.0);
        let (x, y) = scene.place_logical(&password.place, (width, height + GAP + caps_height));
        let pill = scene.physical((x, y), (width, height));
        let field = &mut scene.field;
        field.pill = pill;
        let radius = (height / 2.0 - BUTTON_INSET) * factor;
        let inset = BUTTON_INSET * factor;
        field.button = (pill.0 + pill.2 - inset - radius, middle(pill).1, radius);
        field.caps = caps.map(|caps| {
            let top = ((y + height + GAP) * factor).round();
            let at = (centred(caps.size(), middle(pill)).0, top);
            Label { at, ..caps }
        });
        let hint = small(13.0, Weight::Normal, 0.75);
        field.hint = self.label("Enter Password", &hint, factor, |s| {
            centred(s, middle(pill))
        });
        let (cx, cy, _) = field.button;
        let arrow = small(13.0, Weight::Bold, 1.0);
        field.arrow = self.label("→", &arrow, factor, |s| centred(s, (cx, cy)));
        scene
    }

    /// Render the date and clock again where what they show has changed (or never was);
    /// returns whether anything did.
    pub fn refresh(&self, scene: &mut Scene) -> bool {
        let now = DateTime::now_local().ok();
        let factor = scene.factor();
        let mut changed = false;
        let times = [
            (&self.config.date, "%A %-d %B"),
            (&self.config.clock, "%H:%M"),
        ];
        for (i, (config, default)) in times.into_iter().enumerate() {
            let Some(config) = config else { continue };
            let format = config.format.as_ref().map_or(default, |format| &format.0);
            let text = now.as_ref().and_then(|now| now.format(format).ok());
            let text = text.as_deref().unwrap_or_default();
            if scene.times[i]
                .as_ref()
                .is_none_or(|label| label.text != text)
            {
                let font = self.font_of(&config.style);
                let label = self.label(text, &font, factor, |s| scene.place(&config.place, s));
                let text = text.to_owned();
                scene.times[i] = label.map(|label| Label { text, ..label });
                changed = true;
            }
        }
        changed
    }

    /// Render "Try again in `seconds` s" for the password field, unless it already is.
    pub fn countdown(&self, scene: &mut Scene, seconds: u64) {
        let field = &scene.field;
        if field
            .countdown
            .as_ref()
            .is_some_and(|(shown, _)| *shown == seconds)
        {
            return;
        }
        let font = small(13.0, Weight::Normal, fade(field.colour, 0.75));
        let text = format!("Try again in {seconds} s");
        let centre = middle(field.pill);
        let label = self.label(&text, &font, scene.factor(), |s| centred(s, centre));
        scene.field.countdown = label.map(|label| (seconds, label));
    }

    /// The background for frames of `pixels`, at 1/`SHRINK` of their size: the wallpaper
    /// scaled to cover it (or the plain colour), darkened by the gradients; black without a
    /// `background`.
    fn background(&self, pixels: (u32, u32)) -> Result<ImageSurface, cairo::Error> {
        let (width, height) = ((pixels.0 / SHRINK).max(1), (pixels.1 / SHRINK).max(1));
        let surface = ImageSurface::create(Format::Rgb24, width as i32, height as i32)?;
        let Some(config) = &self.config.background else {
            return Ok(surface);
        };
        let (width, height) = (f64::from(width), f64::from(height));
        let cr = Context::new(&surface)?;
        if let Some(wallpaper) = &self.wallpaper {
            let (w, h) = (f64::from(wallpaper.width()), f64::from(wallpaper.height()));
            let cover = (width / w).max(height / h);
            cr.translate((width - w * cover) / 2.0, (height - h * cover) / 2.0);
            cr.scale(cover, cover);
            stretch(&cr, wallpaper)?;
            cr.identity_matrix();
        } else {
            set_colour(&cr, self.palette.get(&config.color));
            cr.paint()?;
        }
        let (top_end, bottom_start) = (GRADIENTS.0 * height, GRADIENTS.1 * height);
        let (top, bottom) = (config.gradient.0.0, config.gradient.1.0);
        for (from, to, alpha) in [(0.0, top_end, top), (height, bottom_start, bottom)] {
            let gradient = LinearGradient::new(0.0, from, 0.0, to);
            gradient.add_color_stop_rgba(0.0, 0.0, 0.0, 0.0, alpha);
            gradient.add_color_stop_rgba(1.0, 0.0, 0.0, 0.0, 0.0);
            cr.set_source(&gradient)?;
            cr.rectangle(0.0, from.min(to), width, (to - from).abs());
            cr.fill()?;
        }
        drop(cr);
        Ok(surface)
    }

    /// How `style` renders text, with its defaults.
    fn font_of(&self, style: &Style) -> Font {
        Font {
            size: style.size.map_or(15.0, |size| size.0),
            weight: style.weight.unwrap_or_default(),
            spacing: style.letter_spacing.map_or(0.0, |spacing| spacing.0),
            colour: style
                .color
                .as_ref()
                .map_or([1.0; 4], |c| self.palette.get(c)),
        }
    }

    /// `text` rendered as `font` says at `factor` physical pixels per logical one, at the
    /// top-left corner `place` gives a box of its physical size. A failure (only the
    /// environment's, such as a broken fontconfig) is logged, and leaves it out.
    fn label(
        &self,
        text: &str,
        font: &Font,
        factor: f64,
        place: impl FnOnce((f64, f64)) -> (f64, f64),
    ) -> Option<Label> {
        let surface = self
            .render(text, font, factor)
            .inspect_err(|e| error!("rendering {text:?}: {e}"))
            .ok()?;
        let at = place((f64::from(surface.width()), f64::from(surface.height())));
        let text = String::new();
        Some(Label { text, surface, at })
    }

    /// `text` rendered in GTK's font, in its logical box.
    fn render(&self, text: &str, font: &Font, factor: f64) -> Result<ImageSurface, cairo::Error> {
        let layout = pango::Layout::new(&self.fonts);
        let mut description = self.font.clone();
        description.set_absolute_size(font.size * factor * f64::from(pango::SCALE));
        description.set_weight(weight(font.weight));
        layout.set_font_description(Some(&description));
        if font.spacing != 0.0 {
            let attributes = pango::AttrList::new();
            let spacing = (font.spacing * factor * f64::from(pango::SCALE)) as i32;
            attributes.insert(pango::AttrInt::new_letter_spacing(spacing));
            layout.set_attributes(Some(&attributes));
        }
        layout.set_text(text);
        let (_, logical) = layout.pixel_extents();
        let fit = |size: i32| size.clamp(1, MAX_TEXT);
        let surface =
            ImageSurface::create(Format::ARgb32, fit(logical.width()), fit(logical.height()))?;
        let cr = Context::new(&surface)?;
        cr.move_to(-f64::from(logical.x()), -f64::from(logical.y()));
        set_colour(&cr, font.colour);
        pangocairo::functions::show_layout(&cr, &layout);
        cr.status()?;
        drop(cr);
        Ok(surface)
    }
}

impl Scene {
    /// Physical pixels per logical one.
    fn factor(&self) -> f64 {
        f64::from(self.scale) / 120.0
    }

    /// The logical top-left corner of a widget of logical `size` at `place`.
    fn place_logical(&self, place: &Place, size: (f64, f64)) -> (f64, f64) {
        let Offset(x, y) = place.offset.unwrap_or_default();
        let screen = (f64::from(self.size.0), f64::from(self.size.1));
        self::place(place.anchor.unwrap_or_default(), (x.0, y.0), size, screen)
    }

    /// The physical top-left corner, on a whole pixel, of a widget of physical `size` at
    /// `place`.
    fn place(&self, place: &Place, size: (f64, f64)) -> (f64, f64) {
        let factor = self.factor();
        let (x, y) = self.place_logical(place, (size.0 / factor, size.1 / factor));
        ((x * factor).round(), (y * factor).round())
    }

    /// A rectangle at logical `at` of logical `size`, in physical pixels, with its corners
    /// on whole pixels.
    fn physical(&self, at: (f64, f64), size: (f64, f64)) -> Rect {
        let f = self.factor();
        let (left, top) = ((at.0 * f).round(), (at.1 * f).round());
        let (right, bottom) = (((at.0 + size.0) * f).round(), ((at.1 + size.1) * f).round());
        (left, top, right - left, bottom - top)
    }

    /// Draw the lock screen showing `look` into `canvas`, a frame of `pixels` in
    /// `Xrgb8888`: the background scaled up, the prepared texts, and a few shapes. A part
    /// that fails is logged and left out; if even the background fails, the frame is the
    /// plain colour, so a lock never shows nothing.
    pub fn draw(&self, canvas: &mut [u8], look: Look) {
        let (width, height) = (self.pixels.0 as i32, self.pixels.1 as i32);
        // SAFETY: `canvas` holds `height` rows of `width * 4` bytes, cairo's Rgb24 has
        // Xrgb8888's layout, and the surface is finished before `canvas` is used again.
        let target = unsafe {
            ImageSurface::create_for_data_unsafe(
                canvas.as_mut_ptr(),
                Format::Rgb24,
                width,
                height,
                width * 4,
            )
        };
        let drawn = target.and_then(|target| {
            let drawn = self.paint(&target, look);
            target.finish();
            drawn
        });
        if let Err(e) = drawn {
            error!("drawing the lock screen: {e}; drawing it plain");
            let [r, g, b, _] = self.plain.map(|c| (c * 255.0).round() as u8);
            let pixel = u32::from_be_bytes([0, r, g, b]).to_ne_bytes();
            canvas.as_chunks_mut().0.fill(pixel);
        }
    }

    /// Paint the background, then each other part with a context of its own: cairo errors
    /// stick to a context, and one part's must not stop the others.
    fn paint(&self, target: &ImageSurface, look: Look) -> Result<(), cairo::Error> {
        let cr = Context::new(target)?;
        cr.set_operator(Operator::Source);
        match &self.background {
            Some(background) => {
                let x = f64::from(self.pixels.0) / f64::from(background.width());
                cr.scale(x, f64::from(self.pixels.1) / f64::from(background.height()));
                stretch(&cr, background)?;
            }
            None => {
                set_colour(&cr, self.plain);
                cr.paint()?;
            }
        }
        let line = self.factor();
        let part = |paint: &dyn Fn(&Context) -> Result<(), cairo::Error>| {
            if let Err(e) = Context::new(target).and_then(|cr| paint(&cr)) {
                error!("drawing part of the lock screen: {e}");
            }
        };
        for label in self.times.iter().chain([&self.name]).flatten() {
            part(&|cr| label.paint(cr, 1.0));
        }
        if let Some(((x, y, d, _), initial)) = &self.avatar {
            part(&|cr| {
                cr.arc(x + d / 2.0, y + d / 2.0, d / 2.0, 0.0, 2.0 * PI);
                frost(cr, AVATAR, [1.0, 1.0, 1.0, AVATAR_OUTLINE], line)?;
                initial
                    .as_ref()
                    .map_or(Ok(()), |initial| initial.paint(cr, 1.0))
            });
        }
        part(&|cr| self.field.paint(cr, look, line));
        Ok(())
    }
}

impl Field {
    /// Draw the field showing `look`; `line` is a logical pixel's width.
    fn paint(&self, cr: &Context, look: Look, line: f64) -> Result<(), cairo::Error> {
        let (x, y, width, height) = self.pill;
        let dim = if look.status == Status::Checking {
            CHECKING
        } else {
            1.0
        };
        match look.status {
            Status::Idle => self
                .hint
                .as_ref()
                .map_or(Ok(()), |hint| hint.paint(cr, 1.0))?,
            Status::Cooldown(_) => {
                if let Some((_, countdown)) = &self.countdown {
                    countdown.paint(cr, 1.0)?;
                }
            }
            Status::Typing | Status::Checking | Status::Failed => {
                let outline = match look.status {
                    Status::Failed => self.error,
                    _ => [1.0, 1.0, 1.0, FIELD_OUTLINE * dim],
                };
                rounded(cr, self.pill, self.radius);
                frost(cr, FIELD * dim, outline, line)?;
                set_colour(cr, fade(self.colour, dim));
                let max = ((width - 2.0 * height + self.dot) / (2.0 * self.dot)).max(1.0);
                let dots = look.chars.min(max as usize);
                // Centred on whole pixels, so that a dot is the same at any position.
                let row = (2 * dots) as f64 * self.dot - self.dot;
                let first = x + width / 2.0 - row / 2.0 + self.dot / 2.0;
                let cy = (y + height / 2.0).floor() + 0.5;
                for i in 0..dots {
                    let cx = (first + (2 * i) as f64 * self.dot).floor() + 0.5;
                    cr.arc(cx, cy, self.dot / 2.0, 0.0, 2.0 * PI);
                    cr.fill()?;
                }
                let (cx, cy, radius) = self.button;
                cr.arc(cx, cy, radius, 0.0, 2.0 * PI);
                frost(cr, BUTTON * dim, [0.0; 4], line)?;
                if let Some(arrow) = &self.arrow {
                    arrow.paint(cr, dim)?;
                }
            }
        }
        match &self.caps {
            Some(caps) if look.caps_lock => caps.paint(cr, 1.0),
            _ => Ok(()),
        }
    }
}

impl Label {
    /// Draw it in its place, at `alpha` of its opacity.
    fn paint(&self, cr: &Context, alpha: f64) -> Result<(), cairo::Error> {
        cr.set_source_surface(&self.surface, self.at.0, self.at.1)?;
        cr.paint_with_alpha(alpha)
    }

    /// Its size in physical pixels.
    fn size(&self) -> (f64, f64) {
        (
            f64::from(self.surface.width()),
            f64::from(self.surface.height()),
        )
    }
}

/// Paint `surface` with bilinear filtering, its edge pixels repeated beyond it.
fn stretch(cr: &Context, surface: &ImageSurface) -> Result<(), cairo::Error> {
    cr.set_source_surface(surface, 0.0, 0.0)?;
    let source = cr.source();
    source.set_filter(Filter::Bilinear);
    source.set_extend(Extend::Pad);
    cr.paint()
}

/// Fill the current path with white at `alpha` over what is there, and give it a
/// one-`line` inner outline in `outline`.
fn frost(cr: &Context, alpha: f64, outline: Rgba, line: f64) -> Result<(), cairo::Error> {
    cr.set_source_rgba(1.0, 1.0, 1.0, alpha);
    cr.save()?;
    cr.clip_preserve();
    cr.fill_preserve()?;
    // Half of a two-line stroke falls inside the clip.
    set_colour(cr, outline);
    cr.set_line_width(2.0 * line);
    cr.stroke()?;
    cr.restore()
}

/// Trace a rectangle with corners of `radius`.
fn rounded(cr: &Context, (x, y, width, height): Rect, radius: f64) {
    let (right, bottom) = (x + width - radius, y + height - radius);
    cr.new_sub_path();
    cr.arc(right, y + radius, radius, -PI / 2.0, 0.0);
    cr.arc(right, bottom, radius, 0.0, PI / 2.0);
    cr.arc(x + radius, bottom, radius, PI / 2.0, PI);
    cr.arc(x + radius, y + radius, radius, PI, 1.5 * PI);
    cr.close_path();
}

/// Text of `size` and `weight` in `colour`, with no extra spacing.
fn small(size: f64, weight: Weight, colour: Rgba) -> Font {
    let spacing = 0.0;
    Font {
        size,
        weight,
        spacing,
        colour,
    }
}

fn set_colour(cr: &Context, [r, g, b, a]: Rgba) {
    cr.set_source_rgba(r, g, b, a);
}

/// `colour` with its opacity multiplied by `alpha`.
fn fade([r, g, b, a]: Rgba, alpha: f64) -> Rgba {
    [r, g, b, a * alpha]
}

/// The centre of `rect`.
fn middle((x, y, width, height): Rect) -> (f64, f64) {
    (x + width / 2.0, y + height / 2.0)
}

/// Where a box of `size` goes for its centre to be at `centre`, on whole pixels.
fn centred(size: (f64, f64), centre: (f64, f64)) -> (f64, f64) {
    (
        (centre.0 - size.0 / 2.0).round(),
        (centre.1 - size.1 / 2.0).round(),
    )
}

/// Pango's weight for `weight`.
fn weight(weight: Weight) -> pango::Weight {
    match weight {
        Weight::Thin => pango::Weight::Thin,
        Weight::Ultralight => pango::Weight::Ultralight,
        Weight::Light => pango::Weight::Light,
        Weight::Semilight => pango::Weight::Semilight,
        Weight::Book => pango::Weight::Book,
        Weight::Normal => pango::Weight::Normal,
        Weight::Medium => pango::Weight::Medium,
        Weight::Semibold => pango::Weight::Semibold,
        Weight::Bold => pango::Weight::Bold,
        Weight::Ultrabold => pango::Weight::Ultrabold,
        Weight::Heavy => pango::Weight::Heavy,
        Weight::Ultraheavy => pango::Weight::Ultraheavy,
    }
}

/// The top-left corner of a box of `size` placed on a `screen` at `anchor`, moved by
/// `offset` away from the edges it is anchored to (right and down along a centred axis).
fn place(anchor: Anchor, offset: (f64, f64), size: (f64, f64), screen: (f64, f64)) -> (f64, f64) {
    use Anchor::*;
    let (h, v) = match anchor {
        TopLeft => (0.0, 0.0),
        Top => (0.5, 0.0),
        TopRight => (1.0, 0.0),
        Left => (0.0, 0.5),
        Center => (0.5, 0.5),
        Right => (1.0, 0.5),
        BottomLeft => (0.0, 1.0),
        Bottom => (0.5, 1.0),
        BottomRight => (1.0, 1.0),
    };
    let along = |at: f64, offset: f64, size: f64, space: f64| {
        at * (space - size) + if at == 1.0 { -offset } else { offset }
    };
    (
        along(h, offset.0, size.0, screen.0),
        along(v, offset.1, size.1, screen.1),
    )
}

/// The size in physical pixels of a frame for an output of logical `size` at `scale` (in
/// 120ths): rounded as wp-fractional-scale-v1 asks, or the output's `mode` where that is
/// within a pixel of it (1707 × 1.5 is 2560.5, on a 2560 px panel).
pub fn pixels(size: (u32, u32), scale: u32, mode: Option<(u32, u32)>) -> (u32, u32) {
    let round = |logical: u32| (logical * scale + 60) / 120;
    let rounded = (round(size.0), round(size.1));
    match mode {
        Some(mode) if rounded.0.abs_diff(mode.0) <= 1 && rounded.1.abs_diff(mode.1) <= 1 => mode,
        _ => rounded,
    }
}

/// The scale (in 120ths) of an output whose mode is `mode` pixels wide and shows
/// `logical` ones, as the compositor will most likely prefer it.
pub fn scale_of(mode: u32, logical: u32) -> u32 {
    (f64::from(mode) * 120.0 / f64::from(logical.max(1)))
        .round()
        .max(1.0) as u32
}

/// How long until the clock next changes, at the next minute.
pub fn until_next_minute() -> Duration {
    let seconds = DateTime::now_local().map_or(0.0, |now| now.seconds());
    Duration::from_secs_f64((60.0 - seconds).clamp(0.001, 60.0))
}

/// The current minute, counted from the epoch.
pub fn minute() -> i64 {
    DateTime::now_local().map_or(0, |now| now.to_unix() / 60)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, DEFAULT_LOCK_SCREEN};

    #[test]
    fn widgets_are_placed_inwards_from_their_anchor() {
        let screen = (1280.0, 720.0);
        let at = |anchor| place(anchor, (10.0, 20.0), (100.0, 50.0), screen);
        assert_eq!(at(Anchor::TopLeft), (10.0, 20.0));
        assert_eq!(at(Anchor::Top), (600.0, 20.0));
        assert_eq!(at(Anchor::TopRight), (1170.0, 20.0));
        assert_eq!(at(Anchor::Left), (10.0, 355.0));
        assert_eq!(at(Anchor::Center), (600.0, 355.0));
        assert_eq!(at(Anchor::Right), (1170.0, 355.0));
        assert_eq!(at(Anchor::BottomLeft), (10.0, 650.0));
        assert_eq!(at(Anchor::Bottom), (600.0, 650.0));
        assert_eq!(at(Anchor::BottomRight), (1170.0, 650.0));
    }

    #[test]
    fn frames_round_like_the_protocol_and_fit_the_panel() {
        let at = |scale| pixels((1280, 720), scale, None);
        assert_eq!(
            [120, 180, 240].map(at),
            [(1280, 720), (1920, 1080), (2560, 1440)]
        );
        assert_eq!(pixels((853, 480), 180, None), (1280, 720));
        // niri's 1707 × 1067 at 1.5, on a 2560 × 1600 panel.
        assert_eq!(pixels((1707, 1067), 180, None), (2561, 1601));
        assert_eq!(pixels((1707, 1067), 180, Some((2560, 1600))), (2560, 1600));
        // A mode further off is not the frame's size.
        assert_eq!(pixels((1280, 720), 120, Some((1920, 1080))), (1280, 720));
        assert_eq!(scale_of(2560, 1707), 180);
        assert_eq!(scale_of(2560, 1280), 240);
        assert_eq!(scale_of(1920, 1920), 120);
    }

    /// The default lock screen, prepared for a 1280×720 output at `scale`.
    fn scene(scale: u32) -> (Painter, Scene) {
        let config: Config = knuffel::parse("default", DEFAULT_LOCK_SCREEN).unwrap();
        let painter = Painter::new(config.lock_screen.unwrap());
        let scene = painter.scene((1280, 720), scale, pixels((1280, 720), scale, None));
        (painter, scene)
    }

    #[test]
    fn scenes_scale_with_the_output() {
        for (scale, factor) in [(120, 1.0), (180, 1.5), (240, 2.0)] {
            let (_, scene) = scene(scale);
            let background = scene.background.as_ref().unwrap();
            let small = ((320.0 * factor) as i32, (180.0 * factor) as i32);
            assert_eq!((background.width(), background.height()), small);
            let field = &scene.field;
            // 176 × 30 at the bottom centre, with its bottom 56 px plus the caps line up.
            assert_eq!(
                (field.pill.2, field.pill.3),
                (176.0 * factor, 30.0 * factor)
            );
            assert_eq!(field.pill.0, (640.0 - 88.0) * factor);
            let caps = field.caps.as_ref().unwrap().size().1 / factor;
            let top = 720.0 - 56.0 - caps - GAP - 30.0;
            assert!(
                (field.pill.1 - top * factor).abs() <= 1.0,
                "{scale}: {:?}",
                field.pill
            );
            let ((x, y, d, _), _) = scene.avatar.as_ref().unwrap();
            let expected = [640.0 - 32.0, 720.0 - 149.0 - 64.0, 64.0].map(|v| (v * factor).round());
            assert_eq!([*x, *y, *d], expected);
        }
    }

    #[test]
    fn the_clock_is_rendered_again_only_when_its_text_changes() {
        let (painter, mut scene) = scene(120);
        assert!(!painter.refresh(&mut scene));
        scene.times[1].as_mut().unwrap().text = "stale".into();
        assert!(painter.refresh(&mut scene));
        assert_ne!(scene.times[1].as_ref().unwrap().text, "stale");
        assert!(!painter.refresh(&mut scene));
    }

    #[test]
    fn the_countdown_is_rendered_once_per_second_shown() {
        let (painter, mut scene) = scene(120);
        let shown = |scene: &Scene| {
            let (seconds, label) = scene.field.countdown.as_ref().unwrap();
            (*seconds, label.surface.to_raw_none())
        };
        painter.countdown(&mut scene, 30);
        let first = shown(&scene);
        painter.countdown(&mut scene, 30);
        // The same surface, not rendered again.
        assert_eq!(shown(&scene), first);
        painter.countdown(&mut scene, 29);
        assert_eq!(shown(&scene).0, 29);
    }

    /// The colour of pixel `(x, y)` of a 1280 px wide frame.
    fn pixel(canvas: &[u8], x: usize, y: usize) -> u32 {
        u32::from_ne_bytes(canvas.as_chunks().0[y * 1280 + x]) & 0xffffff
    }

    const TYPING: Look = Look {
        status: Status::Typing,
        chars: 1,
        caps_lock: false,
    };

    #[test]
    fn every_state_draws() {
        let (_, scene) = scene(120);
        let mut canvas = vec![0; 1280 * 720 * 4];
        let states = [
            Status::Idle,
            Status::Typing,
            Status::Checking,
            Status::Failed,
        ];
        for status in states.into_iter().chain([Status::Cooldown(3)]) {
            scene.draw(
                &mut canvas,
                Look {
                    status,
                    chars: 40,
                    caps_lock: true,
                },
            );
            // Half-way down the left edge, the plain background, between the gradients.
            assert_eq!(pixel(&canvas, 5, 360), 0x202428);
        }
    }

    #[test]
    fn a_part_that_fails_leaves_the_rest_of_the_frame() {
        let (_, mut scene) = scene(120);
        let broken = ImageSurface::create(Format::ARgb32, 10, 10).unwrap();
        broken.finish();
        scene.name.as_mut().unwrap().surface = broken.clone();
        let mut canvas = vec![0; 1280 * 720 * 4];
        scene.draw(&mut canvas, TYPING);
        assert_eq!(pixel(&canvas, 5, 360), 0x202428);
        // The field, frosted, is drawn after the name.
        let (x, y, ..) = scene.field.pill;
        assert_ne!(pixel(&canvas, x as usize + 20, y as usize + 15), 0x202428);
        // Without the background, the frame is the plain colour.
        scene.background = Some(broken);
        scene.draw(&mut canvas, TYPING);
        let plain = |p: &[u8; 4]| u32::from_ne_bytes(*p) & 0xffffff == 0x202428;
        assert!(canvas.as_chunks().0.iter().all(plain));
    }
}
