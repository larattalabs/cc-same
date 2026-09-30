//! The headline picture: one ring per account, drawn like the app icon.
//!
//! Rings that share every session overlap, and the translucent fills deepen where they do, so a
//! group in sync reads as one warm core. A ring still missing sessions drifts out until it
//! catches up; an excluded one stands apart, faded and dashed. Positions are springs, so a
//! change of state glides and a sync visibly pulls the group together.

use crate::i18n::t;
use crate::model::{Account, Mood, Placement};
use crate::theme::RingColors;
use gpui_kit::base::{
    Easing, IterationCount, Keyframe, Keyframes, PlaybackDirection, Spring, Timing, Transition, animate_keyframes,
    spring, transition,
};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::{
    AnyElement, App, FontWeight, Hsla, InteractiveElement as _, IntoElement, ParentElement as _, PathBuilder, Pixels,
    Point, SharedString, StatefulInteractiveElement as _, Styled as _, Window, canvas, div, linear_color_stop,
    linear_gradient, point, px, relative,
};
use std::f32::consts::{FRAC_1_SQRT_2, FRAC_PI_2, TAU};
use std::rc::Rc;
use std::time::Duration;

/// Height of the picture.
pub const HEIGHT: f32 = 150.;
/// Spacing of the dot grid behind the rings.
const GRID: f32 = 13.;

/// The motion of a ring finding its place: quick, with a small settle at the end.
const PLACE: Spring = Spring::new(Duration::from_millis(620)).with_damping(0.72).with_epsilon(0.05);

pub type OnHover = Rc<dyn Fn(Option<SharedString>, &mut Window, &mut App)>;

/// A ring as the picture needs it.
#[derive(Clone)]
struct Ring {
    key: SharedString,
    initials: SharedString,
    tip: SharedString,
    placement: Placement,
    open: bool,
    /// A stand-in: loading, or the hint that another account can join.
    ghost: bool,
}

impl Ring {
    fn from_account(a: &Account) -> Ring {
        Ring {
            key: a.id.clone().into(),
            initials: a.initials().into(),
            tip: format!("{}\n{}", a.name, a.detail()).into(),
            placement: a.ring(),
            open: a.open,
            ghost: false,
        }
    }

    fn ghost(key: &str, initials: &str, tip: &str) -> Ring {
        Ring {
            key: key.to_string().into(),
            initials: initials.to_string().into(),
            tip: tip.to_string().into(),
            placement: Placement::Apart,
            open: false,
            ghost: true,
        }
    }
}

/// Everything painted for one ring this frame.
#[derive(Clone, Copy)]
struct Painted {
    center: Point<Pixels>,
    radius: f32,
    opacity: f32,
    fill: Hsla,
    stroke: Hsla,
    dashed: bool,
    open_dot: Option<(Point<Pixels>, Hsla)>,
}

fn radius(n: usize) -> f32 {
    match n {
        0..=2 => 40.,
        3 => 37.,
        4 => 34.,
        5 => 31.,
        _ => 28.,
    }
}

/// Distance of a joined ring's center from the middle, in radii.
fn spread(n: usize) -> f32 {
    match n {
        0 | 1 => 0.,
        2 => 0.56,
        3 => 0.6,
        4 => 0.7,
        5 => 0.78,
        _ => 0.84,
    }
}

/// Which way ring `i` of `n` sits from the middle.
fn direction(i: usize, n: usize) -> (f32, f32) {
    match n {
        0 | 1 => (0., 0.),
        2 => {
            if i == 0 {
                (-1., 0.)
            } else {
                (1., 0.)
            }
        }
        _ => {
            let angle = -FRAC_PI_2 + TAU * i as f32 / n as f32;
            (angle.cos(), angle.sin())
        }
    }
}

/// How far a ring drifts out from its joined spot, in radii.
fn drift(placement: Placement, mood: Mood) -> f32 {
    match placement {
        Placement::Joined => 0.,
        // While a sync runs, the rings it is filling in are pulled in already.
        Placement::Behind if matches!(mood, Mood::Syncing { .. }) => 0.,
        Placement::Behind | Placement::Broken => 0.36,
        Placement::Apart => 0.95,
    }
}

fn circle(center: Point<Pixels>, r: f32, mut builder: PathBuilder) -> Option<gpui_kit::Path<Pixels>> {
    let r = px(r);
    builder.move_to(point(center.x + r, center.y));
    builder.arc_to(point(r, r), px(0.), false, true, point(center.x - r, center.y));
    builder.arc_to(point(r, r), px(0.), false, true, point(center.x + r, center.y));
    builder.close();
    builder.build().ok()
}

/// The picture for `accounts` in `mood`. `gathered` is false on the first frame, when the rings
/// start scattered so they can fly in.
#[allow(clippy::too_many_arguments)]
pub fn rings(
    accounts: &[Account],
    mood: Mood,
    hovered: Option<&str>,
    gathered: bool,
    on_hover: OnHover,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let colors = RingColors::new(cx);
    let mut rings: Vec<Ring> = accounts.iter().map(Ring::from_account).collect();
    match mood {
        Mood::Loading => {
            rings = (0..3).map(|i| Ring::ghost(&format!("loading-{i}"), "", "")).collect();
        }
        Mood::Missing => rings = vec![Ring::ghost("missing", "?", &t("ring.missing"))],
        Mood::Single if rings.len() < 2 => {
            rings.push(Ring::ghost("invite", "+", &t("ring.invite")));
        }
        _ => {}
    }
    let n = rings.len();
    let r = radius(n);

    // A slow breath while copying: the overlaps brighten and fade.
    let breath = if matches!(mood, Mood::Syncing { .. } | Mood::Loading) {
        let frames = Keyframes::try_new([Keyframe::new(0., 0f32), Keyframe::new(1., 1f32)]).expect("valid keyframes");
        let timing = Timing::new(Duration::from_millis(900))
            .iterations(IterationCount::Infinite)
            .direction(PlaybackDirection::Alternate)
            .ease(Easing::EaseInOut);
        animate_keyframes("hero-breath", &frames, timing, window, cx).value
    } else {
        0.
    };

    let mut painted: Vec<Painted> = Vec::with_capacity(n);
    let mut labels: Vec<AnyElement> = Vec::with_capacity(n);
    let mut targets: Vec<AnyElement> = Vec::with_capacity(n);
    for (i, ring) in rings.iter().enumerate() {
        let (dx, dy) = direction(i, n);
        let reach = if gathered { spread(n) + drift(ring.placement, mood) } else { spread(n) + 1.5 };
        let key = ring.key.clone();
        let x = spring((key.clone(), "x"), dx * reach * r, PLACE, window, cx);
        let y = spring((key.clone(), "y"), dy * reach * r, PLACE, window, cx);
        let shown = match (gathered, ring.placement, ring.ghost) {
            (false, _, _) => 0.,
            (_, _, true) => 1.,
            (_, Placement::Apart, _) => 0.75,
            _ => 1.,
        };
        let opacity = transition(
            (key.clone(), "opacity"),
            shown,
            Transition::new(Duration::from_millis(360)).easing(Easing::EaseOut),
            window,
            cx,
        );
        let hover = transition(
            (key.clone(), "hover"),
            if hovered == Some(ring.key.as_ref()) { 1. } else { 0. },
            Transition::new(Duration::from_millis(140)),
            window,
            cx,
        );

        let base_stroke = match (ring.placement, ring.open, ring.ghost) {
            (_, _, true) => colors.stroke,
            (Placement::Broken, _, _) => colors.danger,
            (_, true, _) => colors.open,
            _ => colors.stroke,
        };
        let stroke = if ring.open || ring.placement == Placement::Broken {
            base_stroke
        } else {
            let mut s = base_stroke;
            s.a += (colors.stroke_hover.a - s.a) * hover;
            s
        };
        let fill = match (ring.placement, ring.ghost) {
            (_, true) | (Placement::Apart, _) => colors.fill.opacity(0.),
            (Placement::Behind, _) => colors.fill.opacity(0.7 + 0.6 * breath),
            _ => colors.fill.opacity(1. + 0.6 * breath),
        };
        let center = point(px(x), px(y));
        let open_dot = ring.open.then(|| {
            let (ox, oy) = if n == 1 { (FRAC_1_SQRT_2, -FRAC_1_SQRT_2) } else { (dx, dy) };
            (point(px(x + ox * r), px(y + oy * r)), colors.open)
        });
        painted.push(Painted {
            center,
            radius: r,
            opacity,
            fill,
            stroke,
            dashed: ring.ghost || matches!(ring.placement, Placement::Apart | Placement::Broken),
            open_dot,
        });

        // Initials sit in the part of the ring nobody else covers.
        let label_at = if n == 1 { (x, y) } else { (x + dx * r * 0.42, y + dy * r * 0.42) };
        let label_color = if ring.open { colors.open } else { colors.label };
        labels.push(
            div()
                .absolute()
                .top(px(HEIGHT / 2. + label_at.1 - 9.))
                .left(relative(0.5))
                .ml(px(label_at.0 - 16.))
                .w(px(32.))
                .h(px(18.))
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(12.))
                .font_weight(FontWeight::MEDIUM)
                .text_color(label_color)
                .opacity(opacity)
                .child(ring.initials.clone())
                .into_any_element(),
        );

        if !ring.tip.is_empty() {
            let tip = ring.tip.clone();
            let hover_key = ring.key.clone();
            let on_hover = on_hover.clone();
            targets.push(
                div()
                    .id(("ring", i))
                    .absolute()
                    .top(px(HEIGHT / 2. + y - r * 0.8))
                    .left(relative(0.5))
                    .ml(px(x - r * 0.8))
                    .size(px(r * 1.6))
                    .rounded_full()
                    .on_hover(move |inside: &bool, window, cx| {
                        on_hover(inside.then(|| hover_key.clone()), window, cx);
                    })
                    .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
                    .into_any_element(),
            );
        }
    }

    let grid = colors.grid;
    div()
        .id("hero-rings")
        .relative()
        .w_full()
        .h(px(HEIGHT))
        .flex_none()
        .child(
            canvas(
                |_, _, _| (),
                move |bounds, _, window, _| {
                    let middle = bounds.center();
                    let at = |p: Point<Pixels>| point(middle.x + p.x, middle.y + p.y);
                    // A dot grid that fades out from the middle, like paper under the rings.
                    let reach = f32::from(bounds.size.width).min(360.) * 0.5;
                    let steps = (reach / GRID) as i32;
                    for gx in -steps..=steps {
                        for gy in -steps..=steps {
                            let (x, y) = (gx as f32 * GRID, gy as f32 * GRID);
                            let fade = 1. - (x.hypot(y * 1.6) / reach);
                            if fade <= 0. || y.abs() > HEIGHT / 2. - 4. {
                                continue;
                            }
                            let dot = gpui_kit::Bounds::new(
                                at(point(px(x - 0.75), px(y - 0.75))),
                                gpui_kit::size(px(1.5), px(1.5)),
                            );
                            window.paint_quad(gpui_kit::fill(dot, grid.opacity(grid.a * fade)));
                        }
                    }
                    for ring in &painted {
                        if ring.fill.a > 0.
                            && let Some(path) = circle(at(ring.center), ring.radius, PathBuilder::fill())
                        {
                            let a = ring.fill.a * ring.opacity;
                            window.paint_path(
                                path,
                                linear_gradient(
                                    155.,
                                    linear_color_stop(ring.fill.opacity(a * 0.7), 0.),
                                    linear_color_stop(ring.fill.opacity(a * 1.3), 1.),
                                ),
                            );
                        }
                    }
                    for ring in &painted {
                        let mut builder = PathBuilder::stroke(px(1.25));
                        if ring.dashed {
                            builder = builder.dash_array(&[px(3.), px(3.)]);
                        }
                        if let Some(path) = circle(at(ring.center), ring.radius, builder) {
                            // The rim catches the light at the top.
                            let a = ring.stroke.a * ring.opacity;
                            window.paint_path(
                                path,
                                linear_gradient(
                                    180.,
                                    linear_color_stop(ring.stroke.opacity((a * 1.25).min(1.)), 0.),
                                    linear_color_stop(ring.stroke.opacity(a * 0.7), 1.),
                                ),
                            );
                        }
                        if let Some((dot, color)) = ring.open_dot
                            && let Some(path) = circle(at(dot), 3.5, PathBuilder::fill())
                        {
                            window.paint_path(path, color.opacity(ring.opacity));
                        }
                    }
                },
            )
            .absolute()
            .inset_0(),
        )
        .children(labels)
        .children(targets)
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joined_rings_overlap_and_apart_ones_do_not() {
        for n in 2..=6 {
            let r = radius(n);
            let (ax, ay) = direction(0, n);
            let (bx, by) = direction(1, n);
            let joined = spread(n) * r;
            let distance = ((ax - bx) * joined).hypot((ay - by) * joined);
            assert!(distance < 2. * r * 0.62, "{n} joined rings should overlap well: {distance} vs r {r}");
            let apart = (spread(n) + drift(Placement::Apart, Mood::Single)) * r;
            let away = ((ax * apart) - bx * joined).hypot((ay * apart) - by * joined);
            assert!(away > distance, "an excluded ring sits further out");
        }
    }

    #[test]
    fn syncing_pulls_behind_rings_in() {
        assert_eq!(drift(Placement::Behind, Mood::Syncing { sessions: 3 }), 0.);
        assert!(drift(Placement::Behind, Mood::Pending { sessions: 3, other: 0 }) > 0.);
    }
}
