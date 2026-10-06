use std::f64::consts::PI;
use std::time::Duration;

use cairo::{
    Antialias, Context, Extend, Filter, Format, HintMetrics, HintStyle, ImageSurface,
    LinearGradient, Operator,
};
use pango::FontDescription;
use pango::Weight;
use pango::glib::{self, DateTime};
use pango::prelude::FontMapExt;
use tracing::error;

use crate::config::{Anchor, LockScreen, Offset, Place, Style, Text};
use crate::entry::{Look, Status};
use crate::gtk::{self, Palette, Rgba};
use crate::wallpaper::Blurred;

/// Logical pixels between the field and the caps lock line, and the field's edge and button.
const GAP: f64 = 10.0;
const BUTTON_INSET: f64 = 5.0;

/// Text sizes in logical pixels; the hint's is also the arrow's and the countdown's, and
/// the initial's is a fraction of the avatar's diameter. `CAPS_HEIGHT` stands in for a caps
/// lock line that could not be rendered.
const TEXT_SIZE: f64 = 15.0;
const HINT_SIZE: f64 = 13.0;
const CAPS_SIZE: f64 = 12.0;
const CAPS_HEIGHT: f64 = 15.0;
const INITIAL_SIZE: f64 = 26.0 / 64.0;

const HINT_ALPHA: f64 = 0.75;
const CAPS_ALPHA: f64 = 0.8;

/// The opacity of white in frosted shapes (field, button, avatar) and in their outlines.
const FIELD: f64 = 0.2;
const BUTTON: f64 = 0.3;
const AVATAR: f64 = 0.22;
const FIELD_OUTLINE: f64 = 0.28;
const AVATAR_OUTLINE: f64 = 0.35;

/// How much of their opacity the field and its dots keep while the password is checked.
const CHECKING: f64 = 0.5;

/// Where the top gradient ends and the bottom one starts, as fractions of the height.
const GRADIENTS: (f64, f64) = (0.4, 0.62);

/// How much smaller than a frame the background is kept; blurred, it loses nothing.
const BACKGROUND_SHRINK: i32 = 4;

/// The widest and tallest a rendered text may be, in physical pixels; more is cut off.
const MAX_TEXT: i32 = 8192;

/// Prepares each output's `Scene` ahead of time, so that drawing a frame only composites.
pub struct Painter {
    config: LockScreen,
    fonts: pango::Context,
    /// GTK's font, whose size and weight each text sets.
    font: FontDescription,
    palette: Palette,
    /// The user's full name.
    name: String,
    wallpaper: Option<ImageSurface>,
}

/// How a text is rendered, in logical pixels.
struct Font {
    size: f64,
    weight: Weight,
    spacing: f64,
    colour: Rgba,
}

/// How an output is drawn: its logical size, its scale in 120ths, and its frames' size.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Layout {
    pub logical: (i32, i32),
    pub scale: u32,
    pub pixels: (i32, i32),
}

/// A rectangle in physical pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Rect {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Circle {
    centre: (f64, f64),
    radius: f64,
}

/// Text rendered at an output's scale, with its top-left corner in physical pixels.
struct Label {
    surface: ImageSurface,
    at: (f64, f64),
}

/// A label whose text changes, such as the clock's.
struct Rendered {
    text: String,
    label: Label,
}

/// One output's lock screen, ready to draw; a part that could not be prepared is left out.
pub struct Scene {
    pub layout: Layout,
    /// The background, gradients included, at 1/`BACKGROUND_SHRINK` of a frame's size.
    background: Option<ImageSurface>,
    /// Drawn if the background cannot be.
    plain: Rgba,
    date: Option<Rendered>,
    clock: Option<Rendered>,
    name: Option<Label>,
    avatar: Option<Avatar>,
    field: Field,
}

struct Avatar {
    circle: Circle,
    initial: Option<Label>,
}

struct Field {
    pill: Rect,
    radius: f64,
    /// A dot's diameter, which is also the gap between dots.
    dot: f64,
    button: Circle,
    colour: Rgba,
    error: Rgba,
    hint: Option<Label>,
    arrow: Option<Label>,
    caps: Option<Label>,
    countdown: Option<Rendered>,
}

impl Painter {
    pub fn new(config: LockScreen) -> Self {
        // GLib's name for a user without one in the passwd database.
        let name = glib::real_name().into_string().ok();
        let name = name.filter(|name| name != "Unknown");
        let name = name.unwrap_or_else(|| glib::user_name().to_string_lossy().into_owned());
        let palette = Palette::load(&config);
        Self::with(config, palette, &gtk::font(), name)
    }

    /// A painter with `palette`, `font` (as Pango describes one) and the user's `name`.
    fn with(config: LockScreen, palette: Palette, font: &str, name: String) -> Self {
        let fonts = pangocairo::FontMap::default().create_context();
        let mut options = cairo::FontOptions::new().expect("cairo font options");
        options.set_antialias(Antialias::Gray);
        options.set_hint_style(HintStyle::Slight);
        options.set_hint_metrics(HintMetrics::Off);
        pangocairo::functions::context_set_font_options(&fonts, Some(&options));
        Self {
            config,
            fonts,
            font: FontDescription::from_string(font),
            palette,
            name,
            wallpaper: None,
        }
    }

    /// Read GTK's colours again, and use `wallpaper` from now on if there is one.
    pub fn reload(&mut self, wallpaper: Option<Blurred>) {
        if let Some(wallpaper) = wallpaper {
            let (width, height) = (wallpaper.width, wallpaper.height);
            let pixels = wallpaper.pixels;
            let surface =
                ImageSurface::create_for_data(pixels, Format::Rgb24, width, height, width * 4);
            self.wallpaper = surface
                .inspect_err(|e| error!("using the wallpaper: {e}"))
                .ok();
        }
        self.palette = Palette::load(&self.config);
    }

    pub fn scene(&self, layout: Layout) -> Scene {
        let background = self
            .background(layout.pixels)
            .inspect_err(|e| error!("preparing the lock screen's background: {e}"))
            .ok();
        let plain = self.config.background.as_ref();
        let name = self.config.name.as_ref().and_then(|name| {
            let font = self.font_of(&name.style);
            let place = |size| layout.place(&name.place, size);
            self.label(&self.name, &font, layout.factor(), place)
        });
        let mut scene = Scene {
            layout,
            background,
            plain: plain.map_or([0.0, 0.0, 0.0, 1.0], |b| self.palette.get(&b.color)),
            date: None,
            clock: None,
            name,
            avatar: self.avatar(layout),
            field: self.field(layout),
        };
        self.refresh(&mut scene);
        scene
    }

    fn avatar(&self, layout: Layout) -> Option<Avatar> {
        let avatar = self.config.avatar.as_ref()?;
        let d = avatar.diameter.0;
        let rect = layout.physical(layout.place_logical(&avatar.place, (d, d)), (d, d));
        let circle = Circle {
            centre: rect.centre(),
            radius: rect.w / 2.0,
        };
        let initial: String = self
            .name
            .chars()
            .take(1)
            .flat_map(char::to_uppercase)
            .collect();
        let colour = self.palette.get(&avatar.color);
        let font = Font::new(d * INITIAL_SIZE, Weight::Semibold, colour);
        let place = |size| centred(size, circle.centre);
        let initial = self.label(&initial, &font, layout.factor(), place);
        Some(Avatar { circle, initial })
    }

    fn field(&self, layout: Layout) -> Field {
        let factor = layout.factor();
        let password = &self.config.password;
        let colour = self.palette.get(&password.color);
        let faded = |size, weight, alpha| Font::new(size, weight, fade(colour, alpha));
        let caps_font = faded(CAPS_SIZE, Weight::Medium, CAPS_ALPHA);
        let caps = self.label("⇪ Caps Lock is on", &caps_font, factor, |_| (0.0, 0.0));
        let caps_height = caps
            .as_ref()
            .map_or(CAPS_HEIGHT, |caps| caps.size().1 / factor);
        let (width, height) = (password.width.0, password.height.0);
        let (x, y) = layout.place_logical(&password.place, (width, height + GAP + caps_height));
        let pill = layout.physical((x, y), (width, height));
        let radius = (height / 2.0 - BUTTON_INSET) * factor;
        let button = Circle {
            centre: (
                pill.x + pill.w - BUTTON_INSET * factor - radius,
                pill.centre().1,
            ),
            radius,
        };
        let caps = caps.map(|caps| {
            let top = ((y + height + GAP) * factor).round();
            let at = (centred(caps.size(), pill.centre()).0, top);
            Label { at, ..caps }
        });
        let hint = faded(HINT_SIZE, Weight::Normal, HINT_ALPHA);
        let arrow = faded(HINT_SIZE, Weight::Bold, 1.0);
        Field {
            pill,
            radius: password.radius.0.min(height / 2.0) * factor,
            dot: password.dot_size.0 * factor,
            button,
            colour,
            error: self.palette.get(&password.error_color),
            hint: self.label("Enter Password", &hint, factor, |size| {
                centred(size, pill.centre())
            }),
            arrow: self.label("→", &arrow, factor, |size| centred(size, button.centre)),
            caps,
            countdown: None,
        }
    }

    /// Render the date and clock again where their text has changed; returns whether any did.
    pub fn refresh(&self, scene: &mut Scene) -> bool {
        let now = DateTime::now_local().ok();
        let (date, clock) = (self.config.date.as_ref(), self.config.clock.as_ref());
        let layout = scene.layout;
        let date = self.time(&mut scene.date, date, "%A %-d %B", now.as_ref(), layout);
        let clock = self.time(&mut scene.clock, clock, "%H:%M", now.as_ref(), layout);
        date || clock
    }

    /// Render `config`'s time (by `default` format) into `shown`; returns whether it changed.
    fn time(
        &self,
        shown: &mut Option<Rendered>,
        config: Option<&Text>,
        default: &str,
        now: Option<&DateTime>,
        layout: Layout,
    ) -> bool {
        let Some(config) = config else { return false };
        let format = config.format.as_ref().map_or(default, |format| &format.0);
        let text = now.and_then(|now| now.format(format).ok());
        let text = text.as_deref().unwrap_or_default();
        if shown.as_ref().is_some_and(|shown| shown.text == text) {
            return false;
        }
        let font = self.font_of(&config.style);
        let place = |size| layout.place(&config.place, size);
        let label = self.label(text, &font, layout.factor(), place);
        *shown = label.map(|label| Rendered {
            text: text.to_owned(),
            label,
        });
        true
    }

    /// Render "Try again in `seconds` s" for the password field, unless it already is.
    pub fn countdown(&self, scene: &mut Scene, seconds: u64) {
        let text = format!("Try again in {seconds} s");
        let field = &mut scene.field;
        if field
            .countdown
            .as_ref()
            .is_some_and(|shown| shown.text == text)
        {
            return;
        }
        let font = Font::new(HINT_SIZE, Weight::Normal, fade(field.colour, HINT_ALPHA));
        let centre = field.pill.centre();
        let label = self.label(&text, &font, scene.layout.factor(), |size| {
            centred(size, centre)
        });
        field.countdown = label.map(|label| Rendered { text, label });
    }

    /// The background at 1/`BACKGROUND_SHRINK` of frames of `pixels`: the wallpaper covering
    /// it or the plain colour, darkened by the gradients; black without a `background`.
    fn background(&self, pixels: (i32, i32)) -> Result<ImageSurface, cairo::Error> {
        let width = (pixels.0 / BACKGROUND_SHRINK).max(1);
        let height = (pixels.1 / BACKGROUND_SHRINK).max(1);
        let surface = ImageSurface::create(Format::Rgb24, width, height)?;
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

    fn font_of(&self, style: &Style) -> Font {
        Font {
            size: style.size.map_or(TEXT_SIZE, |size| size.0),
            weight: style.weight.map_or(Weight::Normal, |weight| weight.0),
            spacing: style.letter_spacing.map_or(0.0, |spacing| spacing.0),
            colour: style
                .color
                .as_ref()
                .map_or([1.0; 4], |c| self.palette.get(c)),
        }
    }

    /// `text` at `factor` physical pixels per logical one, where `place` puts a box of its
    /// size. A failure (the environment's, such as a broken fontconfig) is logged.
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
        Some(Label { surface, at })
    }

    /// `text` rendered in GTK's font, in its logical box.
    fn render(&self, text: &str, font: &Font, factor: f64) -> Result<ImageSurface, cairo::Error> {
        let layout = pango::Layout::new(&self.fonts);
        let mut description = self.font.clone();
        description.set_absolute_size(font.size * factor * f64::from(pango::SCALE));
        description.set_weight(font.weight);
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

impl Font {
    fn new(size: f64, weight: Weight, colour: Rgba) -> Self {
        Self {
            size,
            weight,
            spacing: 0.0,
            colour,
        }
    }
}

impl Layout {
    /// An output of `logical` size at `scale`, with frames rounded as wp-fractional-scale-v1
    /// asks, or of the output's `mode` where that is within a pixel of it (1707 × 1.5 is
    /// 2560.5, on a 2560 px panel).
    pub fn new(logical: (i32, i32), scale: u32, mode: Option<(i32, i32)>) -> Self {
        let round = |size: i32| (size * scale as i32 + 60) / 120;
        let rounded = (round(logical.0), round(logical.1));
        let pixels = match mode {
            Some(mode) if rounded.0.abs_diff(mode.0) <= 1 && rounded.1.abs_diff(mode.1) <= 1 => {
                mode
            }
            _ => rounded,
        };
        Self {
            logical,
            scale,
            pixels,
        }
    }

    /// Physical pixels per logical one.
    fn factor(self) -> f64 {
        f64::from(self.scale) / 120.0
    }

    /// The logical top-left corner of a widget of logical `size` at `place`.
    fn place_logical(self, place: &Place, size: (f64, f64)) -> (f64, f64) {
        let Offset(x, y) = place.offset.unwrap_or_default();
        let screen = (f64::from(self.logical.0), f64::from(self.logical.1));
        self::place(place.anchor.unwrap_or_default(), (x.0, y.0), size, screen)
    }

    /// The physical top-left corner, on a whole pixel, of a widget of physical `size`.
    fn place(self, place: &Place, size: (f64, f64)) -> (f64, f64) {
        let factor = self.factor();
        let (x, y) = self.place_logical(place, (size.0 / factor, size.1 / factor));
        ((x * factor).round(), (y * factor).round())
    }

    /// A rectangle at logical `at` of logical `size`, its corners on whole physical pixels.
    fn physical(self, at: (f64, f64), size: (f64, f64)) -> Rect {
        let f = self.factor();
        let (x, y) = ((at.0 * f).round(), (at.1 * f).round());
        let (right, bottom) = (((at.0 + size.0) * f).round(), ((at.1 + size.1) * f).round());
        Rect {
            x,
            y,
            w: right - x,
            h: bottom - y,
        }
    }
}

impl Rect {
    fn centre(self) -> (f64, f64) {
        (self.x + self.w / 2.0, self.y + self.h / 2.0)
    }
}

impl Circle {
    fn trace(self, cr: &Context) {
        cr.arc(self.centre.0, self.centre.1, self.radius, 0.0, 2.0 * PI);
    }
}

impl Scene {
    /// Draw the lock screen showing `look` into `canvas`, a frame in `Xrgb8888`. A part
    /// that fails is logged and left out; if even the background fails, the frame is the
    /// plain colour, so a lock never shows nothing.
    pub fn draw(&self, canvas: &mut [u8], look: Look) {
        let (width, height) = self.layout.pixels;
        let len = width as usize * 4 * height as usize;
        assert!(canvas.len() >= len, "a canvas too small for the frame");
        // SAFETY: `canvas` holds `height` rows of `width * 4` bytes (asserted above),
        // cairo's Rgb24 has Xrgb8888's layout, and the surface is finished before `canvas`
        // is used again.
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
        let (width, height) = self.layout.pixels;
        match &self.background {
            Some(background) => {
                let x = f64::from(width) / f64::from(background.width());
                cr.scale(x, f64::from(height) / f64::from(background.height()));
                stretch(&cr, background)?;
            }
            None => {
                set_colour(&cr, self.plain);
                cr.paint()?;
            }
        }
        let line = self.layout.factor();
        let part = |paint: &dyn Fn(&Context) -> Result<(), cairo::Error>| {
            if let Err(e) = Context::new(target).and_then(|cr| paint(&cr)) {
                error!("drawing part of the lock screen: {e}");
            }
        };
        let times = [&self.date, &self.clock].into_iter().flatten();
        for label in times.map(|time| &time.label).chain(&self.name) {
            part(&|cr| label.paint(cr, 1.0));
        }
        if let Some(avatar) = &self.avatar {
            part(&|cr| {
                avatar.circle.trace(cr);
                frost(cr, AVATAR, [1.0, 1.0, 1.0, AVATAR_OUTLINE], line)?;
                match &avatar.initial {
                    Some(initial) => initial.paint(cr, 1.0),
                    None => Ok(()),
                }
            });
        }
        part(&|cr| self.field.paint(cr, look, line));
        Ok(())
    }
}

impl Field {
    /// Draw the field showing `look`; `line` is a logical pixel's width.
    fn paint(&self, cr: &Context, look: Look, line: f64) -> Result<(), cairo::Error> {
        let Rect { x, y, w, h } = self.pill;
        let dim = if look.status == Status::Checking {
            CHECKING
        } else {
            1.0
        };
        match look.status {
            Status::Idle => {
                if let Some(hint) = &self.hint {
                    hint.paint(cr, 1.0)?;
                }
            }
            Status::Cooldown(_) => {
                if let Some(countdown) = &self.countdown {
                    countdown.label.paint(cr, 1.0)?;
                }
            }
            Status::Typing | Status::Checking | Status::Failed => {
                let outline = if look.status == Status::Failed {
                    self.error
                } else {
                    [1.0, 1.0, 1.0, FIELD_OUTLINE * dim]
                };
                rounded(cr, self.pill, self.radius);
                frost(cr, FIELD * dim, outline, line)?;
                set_colour(cr, fade(self.colour, dim));
                let max = ((w - 2.0 * h + self.dot) / (2.0 * self.dot)).max(1.0);
                let dots = look.chars.min(max as usize);
                // Centred on whole pixels, so that a dot is the same at any position.
                let row = (2 * dots) as f64 * self.dot - self.dot;
                let first = x + w / 2.0 - row / 2.0 + self.dot / 2.0;
                let cy = (y + h / 2.0).floor() + 0.5;
                for i in 0..dots {
                    let cx = (first + (2 * i) as f64 * self.dot).floor() + 0.5;
                    cr.arc(cx, cy, self.dot / 2.0, 0.0, 2.0 * PI);
                    cr.fill()?;
                }
                self.button.trace(cr);
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
    fn paint(&self, cr: &Context, alpha: f64) -> Result<(), cairo::Error> {
        cr.set_source_surface(&self.surface, self.at.0, self.at.1)?;
        cr.paint_with_alpha(alpha)
    }

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

fn rounded(cr: &Context, Rect { x, y, w, h }: Rect, radius: f64) {
    let (right, bottom) = (x + w - radius, y + h - radius);
    cr.new_sub_path();
    cr.arc(right, y + radius, radius, -PI / 2.0, 0.0);
    cr.arc(right, bottom, radius, 0.0, PI / 2.0);
    cr.arc(x + radius, bottom, radius, PI / 2.0, PI);
    cr.arc(x + radius, y + radius, radius, PI, 1.5 * PI);
    cr.close_path();
}

fn set_colour(cr: &Context, [r, g, b, a]: Rgba) {
    cr.set_source_rgba(r, g, b, a);
}

fn fade([r, g, b, a]: Rgba, alpha: f64) -> Rgba {
    [r, g, b, a * alpha]
}

/// Where a box of `size` goes for its centre to be at `centre`, on whole pixels.
fn centred(size: (f64, f64), centre: (f64, f64)) -> (f64, f64) {
    (
        (centre.0 - size.0 / 2.0).round(),
        (centre.1 - size.1 / 2.0).round(),
    )
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

/// The scale (in 120ths) the compositor likely prefers for `logical` pixels on a `mode`.
pub fn scale_of(mode: i32, logical: i32) -> u32 {
    (f64::from(mode) * 120.0 / f64::from(logical.max(1)))
        .round()
        .max(1.0) as u32
}

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
        let pixels = |logical, scale, mode| Layout::new(logical, scale, mode).pixels;
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

    /// The default lock screen, prepared for a 1280×720 output at `scale`, with the
    /// built-in colours and the font the package's tests provide.
    fn scene(scale: u32) -> (Painter, Scene) {
        let config: Config = knuffel::parse("default", DEFAULT_LOCK_SCREEN).unwrap();
        let config = config.lock_screen.unwrap();
        let painter = Painter::with(config, Palette::parse(""), "Adwaita Sans", "Ann".into());
        let scene = painter.scene(Layout::new((1280, 720), scale, None));
        (painter, scene)
    }

    #[test]
    fn scenes_scale_with_the_output() {
        for (scale, factor) in [(120, 1.0), (180, 1.5), (240, 2.0)] {
            let (_, scene) = scene(scale);
            let background = scene.background.as_ref().unwrap();
            let small = ((320.0 * factor) as i32, (180.0 * factor) as i32);
            assert_eq!((background.width(), background.height()), small);
            let pill = scene.field.pill;
            // 176 × 30 at the bottom centre, with its bottom 56 px plus the caps line up.
            assert_eq!((pill.w, pill.h), (176.0 * factor, 30.0 * factor));
            assert_eq!(pill.x, (640.0 - 88.0) * factor);
            let caps = scene.field.caps.as_ref().unwrap().size().1 / factor;
            let top = 720.0 - 56.0 - caps - GAP - 30.0;
            assert!((pill.y - top * factor).abs() <= 1.0, "{scale}: {pill:?}");
            let circle = scene.avatar.as_ref().unwrap().circle;
            let [x, y, d] =
                [640.0 - 32.0, 720.0 - 149.0 - 64.0, 64.0].map(|v| (v * factor).round());
            let want = Circle {
                centre: (x + d / 2.0, y + d / 2.0),
                radius: d / 2.0,
            };
            assert_eq!(circle, want);
        }
    }

    #[test]
    fn the_clock_is_rendered_again_only_when_its_text_changes() {
        let (painter, mut scene) = scene(120);
        assert!(!painter.refresh(&mut scene));
        scene.clock.as_mut().unwrap().text = "stale".into();
        assert!(painter.refresh(&mut scene));
        assert_ne!(scene.clock.as_ref().unwrap().text, "stale");
        assert!(!painter.refresh(&mut scene));
    }

    /// The colour of pixel `(x, y)` of a 1280 px wide frame.
    fn pixel(canvas: &[u8], (x, y): (usize, usize)) -> u32 {
        u32::from_ne_bytes(canvas.as_chunks().0[y * 1280 + x]) & 0xffffff
    }

    fn look(status: Status, chars: usize) -> Look {
        Look {
            status,
            chars,
            caps_lock: false,
        }
    }

    fn frame(scene: &Scene, look: Look) -> Vec<u8> {
        let mut canvas = vec![0; 1280 * 720 * 4];
        scene.draw(&mut canvas, look);
        canvas
    }

    #[test]
    fn the_countdown_shows_the_seconds_left() {
        let (painter, mut scene) = scene(120);
        painter.countdown(&mut scene, 30);
        let thirty = frame(&scene, look(Status::Cooldown(30), 0));
        painter.countdown(&mut scene, 30);
        assert!(frame(&scene, look(Status::Cooldown(30), 0)) == thirty);
        painter.countdown(&mut scene, 29);
        assert!(frame(&scene, look(Status::Cooldown(29), 0)) != thirty);
    }

    #[test]
    fn each_state_draws_what_only_it_shows() {
        let (painter, mut scene) = scene(120);
        painter.countdown(&mut scene, 3);
        let states = [
            (Status::Idle, 0),
            (Status::Typing, 1),
            (Status::Checking, 1),
            (Status::Failed, 0),
            (Status::Cooldown(3), 0),
        ];
        let frames = states.map(|(status, chars)| frame(&scene, look(status, chars)));
        let pill = scene.field.pill;
        let (left, top) = (pill.x as usize, pill.y as usize);
        let (cx, cy) = pill.centre();
        let (centre, edge) = ((cx as usize, cy as usize), (cx as usize, top));
        // Half-way down the left edge, the plain background, between the gradients.
        assert!(frames.iter().all(|f| pixel(f, (5, 360)) == 0x202428));
        // Typing: a dot in the middle; checking: dimmed; a failure: the error outline.
        assert_eq!(pixel(&frames[1], centre), 0xffffff);
        assert_ne!(pixel(&frames[2], centre), pixel(&frames[1], centre));
        assert_eq!(pixel(&frames[3], edge), 0xffb4ab);
        assert_ne!(pixel(&frames[1], edge), 0xffb4ab);
        // Each state has a pixel in the field that no other state draws so.
        let field = (top..top + pill.h as usize)
            .flat_map(|y| (left..left + pill.w as usize).map(move |x| (x, y)));
        for (i, (status, _)) in states.iter().enumerate() {
            let only = |at: (usize, usize)| {
                let mine = pixel(&frames[i], at);
                (frames.iter().enumerate()).all(|(j, f)| j == i || pixel(f, at) != mine)
            };
            assert!(field.clone().any(only), "{status:?}");
        }
    }

    #[test]
    fn caps_lock_shows_under_the_field() {
        let (_, scene) = scene(120);
        let idle = look(Status::Idle, 0);
        let caps = frame(
            &scene,
            Look {
                caps_lock: true,
                ..idle
            },
        );
        let line = scene.field.caps.as_ref().unwrap();
        let (x, y) = (line.at.0 as usize, line.at.1 as usize);
        let (w, h) = line.size();
        let area = (y..y + h as usize).flat_map(|y| (x..x + w as usize).map(move |x| (x, y)));
        let without = frame(&scene, idle);
        assert!(
            area.into_iter()
                .any(|at| pixel(&caps, at) != pixel(&without, at))
        );
    }

    #[test]
    fn a_part_that_fails_leaves_the_rest_of_the_frame() {
        let (_, mut scene) = scene(120);
        let broken = ImageSurface::create(Format::ARgb32, 10, 10).unwrap();
        broken.finish();
        scene.name.as_mut().unwrap().surface = broken.clone();
        let canvas = frame(&scene, look(Status::Typing, 1));
        assert_eq!(pixel(&canvas, (5, 360)), 0x202428);
        // The field, frosted, is drawn after the name.
        let pill = scene.field.pill;
        let inside = (pill.x as usize + 20, pill.y as usize + 15);
        assert_ne!(pixel(&canvas, inside), 0x202428);
        // Without the background, the frame is the plain colour.
        scene.background = Some(broken);
        let canvas = frame(&scene, look(Status::Typing, 1));
        let plain = |p: &[u8; 4]| u32::from_ne_bytes(*p) & 0xffffff == 0x202428;
        assert!(canvas.as_chunks().0.iter().all(plain));
    }

    #[test]
    #[should_panic(expected = "a canvas too small")]
    fn a_canvas_too_small_for_the_frame_is_refused() {
        let (_, scene) = scene(120);
        scene.draw(&mut [0; 4], look(Status::Idle, 0));
    }
}
