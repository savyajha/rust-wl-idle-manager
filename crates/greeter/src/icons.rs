//! The bottom row's icons (Cancel's and the power buttons'), drawn as cairo paths in the
//! indicators' style: white, with lines two logical pixels wide and round ends, each in a
//! circle 18 across.

use std::f64::consts::{PI, TAU};

use cairo::{Context, FillRule, LineCap};
use ui::draw;

use crate::power::Action;

/// Draw `action`'s icon centred at `(x, y)`, in physical pixels, at `f` physical pixels
/// per logical one.
pub fn power(cr: &Context, action: Action, (x, y): (f64, f64), f: f64) -> Result<(), cairo::Error> {
    let r = 9.0 * f;
    draw::set_color(cr, [1.0; 4]);
    cr.set_line_width(2.0 * f);
    cr.set_line_cap(LineCap::Round);
    match action {
        // A crescent: the disc less another, up and to the right.
        Action::Sleep => {
            cr.save()?;
            cr.arc(x, y, r, 0.0, TAU);
            cr.clip();
            cr.arc(x, y, r, 0.0, TAU);
            cr.new_sub_path();
            cr.arc(x + 0.45 * r, y - 0.35 * r, 0.8 * r, 0.0, TAU);
            cr.set_fill_rule(FillRule::EvenOdd);
            cr.fill()?;
            cr.restore()
        }
        // A snowflake: three strokes through the middle.
        Action::Hibernate => {
            for k in 0..3 {
                let (sin, cos) = (PI / 2.0 + f64::from(k) * PI / 3.0).sin_cos();
                cr.move_to(x + r * cos, y + r * sin);
                cr.line_to(x - r * cos, y - r * sin);
            }
            cr.stroke()
        }
        // An arrow going round, clockwise, its head at the top.
        Action::Restart => {
            let r = 0.8 * r;
            cr.arc(x, y, r, -0.25 * PI, 1.5 * PI);
            cr.stroke()?;
            cr.move_to(x - 0.1 * r, y - 1.4 * r);
            cr.line_to(x + 0.45 * r, y - r);
            cr.line_to(x - 0.1 * r, y - 0.6 * r);
            cr.close_path();
            cr.fill()
        }
        // The power symbol: a ring open at the top, and a stroke down into it.
        Action::ShutDown => {
            cr.arc(x, y, 0.8 * r, -PI / 2.0 + 0.7, 1.5 * PI - 0.7);
            cr.stroke()?;
            cr.move_to(x, y - r);
            cr.line_to(x, y - 0.1 * r);
            cr.stroke()
        }
    }
}

/// Draw the Cancel button's icon, a circle with a cross in it, as `power` draws its icons.
pub fn cancel(cr: &Context, (x, y): (f64, f64), f: f64) -> Result<(), cairo::Error> {
    let r = 9.0 * f;
    draw::set_color(cr, [1.0; 4]);
    cr.set_line_width(2.0 * f);
    cr.set_line_cap(LineCap::Round);
    cr.arc(x, y, 0.8 * r, 0.0, TAU);
    cr.stroke()?;
    let a = 0.32 * r;
    cr.move_to(x - a, y - a);
    cr.line_to(x + a, y + a);
    cr.move_to(x + a, y - a);
    cr.line_to(x - a, y + a);
    cr.stroke()
}
