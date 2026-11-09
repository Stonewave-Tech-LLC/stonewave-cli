//! Small building blocks shared by the screens: margin, key-value rows,
//! the wave spinner, step rows and bars.

use std::time::Duration;

use crate::style::{ColorMode, pal, width};

/// Left margin of every line.
pub const MARGIN: &str = "  ";
/// Indent of a screen's heading.
pub const INDENT: &str = "    ";
/// Indent of the lines under a heading.
pub const DETAIL: &str = "      ";

const LEVELS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// A small wave of `cells` bars, moving with time `t` (seconds).
/// Without animation it is a calm, fixed wave.
pub fn wave_spinner(mode: ColorMode, t: f32, cells: usize) -> String {
    let mut s = String::new();
    for i in 0..cells {
        let phase = t * 7.0 - i as f32 * 0.9;
        let h = (phase.sin() + 1.0) / 2.0;
        let idx = ((h * (LEVELS.len() - 1) as f32).round() as usize).min(LEVELS.len() - 1);
        let c = pal::DEEP.mix(pal::SKY, h);
        mode.cell(&mut s, LEVELS[idx], c, None);
    }
    s
}

/// A dot that slowly pulses between deep cyan and sky: "working on this".
/// A one-cell wave reads as a stray block, so single cells use this instead.
pub fn pulse_dot(mode: ColorMode, t: f32) -> String {
    let h = ((t * 5.0).sin() + 1.0) / 2.0;
    mode.fg(pal::DEEP.mix(pal::SKY, h), "●")
}

/// `label` padded to `w` columns in grey, then `value`: the host/user rows.
pub fn kv(mode: ColorMode, indent: &str, label: &str, w: usize, value: &str) -> String {
    let pad = " ".repeat(w.saturating_sub(label.chars().count()));
    format!("{indent}{}{pad}{value}", mode.fg(pal::GREY, label))
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StepState {
    Pending,
    Running,
    Done,
    /// Was already in place: the setup checks first and leaves it as it is.
    Kept,
}

#[derive(Clone, Debug)]
pub struct Step {
    pub label: &'static str,
    pub detail: &'static str,
    /// Shown instead of `detail` when the step was already in place.
    pub kept_detail: &'static str,
    /// How long the demo pretends this step takes.
    pub takes: Duration,
    /// Whether the demo shows this step as already in place.
    pub already_done: bool,
    pub state: StepState,
}

impl Step {
    pub const fn new(label: &'static str, detail: &'static str, millis: u64) -> Step {
        Step {
            label,
            detail,
            kept_detail: "",
            takes: Duration::from_millis(millis),
            already_done: false,
            state: StepState::Pending,
        }
    }

    /// A step the demo finds already in place.
    pub const fn kept(label: &'static str, kept_detail: &'static str) -> Step {
        Step {
            label,
            detail: "",
            kept_detail,
            takes: Duration::from_millis(250),
            already_done: true,
            state: StepState::Pending,
        }
    }
}

/// One row of the step list. `t` drives the spinner of the running step.
pub fn step_row(mode: ColorMode, step: &Step, label_w: usize, t: f32) -> String {
    let pad = " ".repeat(label_w.saturating_sub(step.label.chars().count()) + 2);
    let (icon, label, detail) = match step.state {
        StepState::Pending => (
            mode.fg(pal::DIM, "○"),
            mode.fg(pal::DIM, step.label),
            String::new(),
        ),
        StepState::Running => (
            pulse_dot(mode, t),
            mode.bold(step.label),
            mode.fg(pal::CYAN, step.detail),
        ),
        StepState::Done => (
            mode.fg_bold(pal::GREEN, "✓"),
            step.label.to_owned(),
            mode.fg(pal::GREY, step.detail),
        ),
        StepState::Kept => (
            mode.fg(pal::GREY, "✓"),
            step.label.to_owned(),
            mode.fg(pal::GREY, step.kept_detail),
        ),
    };
    format!("{DETAIL}{icon} {label}{pad}{detail}")
}

/// A thin progress bar: `━` done, `─` to go. `fraction` in 0..=1.
pub fn progress_bar(mode: ColorMode, fraction: f32, cells: usize) -> String {
    let filled = ((fraction.clamp(0.0, 1.0) * cells as f32).round() as usize).min(cells);
    let mut s = String::new();
    for i in 0..cells {
        if i < filled {
            let c = pal::DEEP.mix(pal::CYAN, i as f32 / cells.max(1) as f32);
            mode.cell(&mut s, '━', c, None);
        } else {
            mode.cell(&mut s, '─', pal::DIM, None);
        }
    }
    s
}

/// A small meter for a percentage, e.g. disk use: `━━━━━━────────  41 %`.
/// Thin lines like the progress bar: full blocks on stacked rows melt into one blob.
pub fn meter(mode: ColorMode, percent: u8, cells: usize) -> String {
    let p = f32::from(percent.min(100)) / 100.0;
    let filled = ((p * cells as f32).round() as usize).min(cells);
    let color = if percent >= 90 {
        pal::RED
    } else if percent >= 75 {
        pal::AMBER
    } else {
        pal::CYAN
    };
    let mut bar = String::new();
    for i in 0..cells {
        if i < filled {
            mode.cell(&mut bar, '━', color, None);
        } else {
            mode.cell(&mut bar, '─', pal::DIM, None);
        }
    }
    format!("{bar} {}", mode.fg(pal::GREY, &format!("{percent:>3} %")))
}

/// Pads `left` and right-aligns `right` within `w` columns.
pub fn spread(left: &str, right: &str, w: usize) -> String {
    let gap = w.saturating_sub(width(left) + width(right)).max(1);
    format!("{left}{}{right}", " ".repeat(gap))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meter_is_fixed_width() {
        for p in [0, 1, 41, 99, 100] {
            assert_eq!(width(&meter(ColorMode::None, p, 10)), 10 + 1 + 5, "{p}");
        }
        assert_eq!(meter(ColorMode::None, 100, 4), "━━━━ 100 %");
        assert_eq!(
            meter(ColorMode::None, 41, 4),
            "━━── 41 %".replacen(' ', "  ", 1)
        );
    }

    #[test]
    fn progress_bar_fills() {
        assert_eq!(progress_bar(ColorMode::None, 0.0, 4), "────");
        assert_eq!(progress_bar(ColorMode::None, 0.5, 4), "━━──");
        assert_eq!(progress_bar(ColorMode::None, 1.0, 4), "━━━━");
    }

    #[test]
    fn spinner_has_one_char_per_cell() {
        assert_eq!(width(&wave_spinner(ColorMode::TrueColor, 1.3, 5)), 5);
    }

    #[test]
    fn spread_right_aligns() {
        assert_eq!(spread("ab", "cd", 7), "ab   cd");
        assert_eq!(spread("abcd", "ef", 3), "abcd ef");
    }
}
