//! A touch keyboard for the search field (Settings: optional, for a touch screen), in the
//! system's keyboard layout (AZERTY, QWERTY or QWERTZ), or the one chosen in Settings.

use eframe::egui::{Pos2, Rect, Ui, Vec2};

use super::skin::*;
use crate::settings::KeyboardLayout;

/// The layout to draw: the one chosen, or (Auto) the system's.
pub fn layout(chosen: KeyboardLayout) -> KeyboardLayout {
    match chosen {
        KeyboardLayout::Auto => system_layout(),
        l => l,
    }
}

/// The system's keyboard layout (asked once): macOS's input source, Linux's X keyboard map;
/// QWERTY when it can't tell.
pub fn system_layout() -> KeyboardLayout {
    static L: std::sync::OnceLock<KeyboardLayout> = std::sync::OnceLock::new();
    *L.get_or_init(|| layout_named(&system_layout_name().unwrap_or_default()))
}

fn system_layout_name() -> Option<String> {
    let run = |cmd: &str, args: &[&str]| {
        std::process::Command::new(cmd).args(args).output().ok().filter(|o| o.status.success()).map(|o| String::from_utf8_lossy(&o.stdout).to_string())
    };
    if cfg!(target_os = "macos") {
        // e.g. "com.apple.keylayout.Belgian"
        run("defaults", &["read", "com.apple.HIToolbox", "AppleCurrentKeyboardLayoutInputSourceID"])
    } else if cfg!(all(unix, not(target_os = "macos"))) {
        // e.g. "layout:     be"
        run("setxkbmap", &["-query"]).and_then(|o| o.lines().find(|l| l.starts_with("layout")).map(|l| l.to_string()))
    } else {
        None
    }
}

/// The layout a system layout name means.
pub fn layout_named(name: &str) -> KeyboardLayout {
    let n = name.to_ascii_lowercase();
    let word = |w: &str| n.split(|c: char| !c.is_ascii_alphanumeric()).any(|x| x == w);
    if n.contains("belgian") || n.contains("french") || word("be") || word("fr") {
        KeyboardLayout::Azerty
    } else if n.contains("german") || n.contains("swiss") || n.contains("austrian") || word("de") || word("ch") || word("at") {
        KeyboardLayout::Qwertz
    } else {
        KeyboardLayout::Qwerty
    }
}

/// The letter rows of a layout (digits above).
pub fn rows(l: KeyboardLayout) -> [&'static str; 4] {
    match l {
        KeyboardLayout::Azerty => ["1234567890", "azertyuiop", "qsdfghjklm", "wxcvbn'-"],
        KeyboardLayout::Qwertz => ["1234567890", "qwertzuiop", "asdfghjkl", "yxcvbnm'-"],
        _ => ["1234567890", "qwertyuiop", "asdfghjkl", "zxcvbnm'-"],
    }
}

/// What a press on the keyboard asks for, besides typing.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Action {
    None,
    Hide,
}

/// The keys of the keyboard in `rect`: each label and where it is (the letter rows, then
/// Clear, space, Back, Hide).
pub fn keys(rect: Rect, l: KeyboardLayout) -> Vec<(String, Rect)> {
    let rows = rows(l);
    let n_rows = rows.len() as f32 + 1.0;
    let gap = (rect.height() * 0.025).clamp(3.0, 8.0);
    let key_h = (rect.height() - gap * (n_rows - 1.0)) / n_rows;
    let widest = rows.iter().map(|r| r.chars().count()).max().unwrap_or(10) as f32;
    let key_w = ((rect.width() - gap * (widest - 1.0)) / widest).min(key_h * 1.6);
    let mut out = Vec::new();
    for (k, row) in rows.iter().enumerate() {
        let n = row.chars().count() as f32;
        let w = n * key_w + (n - 1.0) * gap;
        let y = rect.top() + k as f32 * (key_h + gap);
        for (j, ch) in row.chars().enumerate() {
            let x = rect.center().x - w / 2.0 + j as f32 * (key_w + gap);
            out.push((ch.to_string(), Rect::from_min_size(Pos2::new(x, y), Vec2::new(key_w, key_h))));
        }
    }
    let y = rect.top() + (n_rows - 1.0) * (key_h + gap);
    let row_w = (widest * key_w + (widest - 1.0) * gap).min(rect.width());
    let parts = [("Clear", 1.6), ("space", 4.2), ("Back", 1.6), ("Hide", 1.6)];
    let unit = (row_w - gap * (parts.len() as f32 - 1.0)) / parts.iter().map(|p| p.1).sum::<f32>();
    let mut x = rect.center().x - row_w / 2.0;
    for (label, share) in parts {
        out.push((label.to_string(), Rect::from_min_size(Pos2::new(x, y), Vec2::new(unit * share, key_h))));
        x += unit * share + gap;
    }
    out
}

/// The keyboard in `rect`, typing into `text`.
pub fn show(ui: &mut Ui, rect: Rect, text: &mut String, l: KeyboardLayout) -> Action {
    let keys = keys(rect, l);
    let key_h = keys.first().map_or(30.0, |k| k.1.height());
    let font = bold((key_h * 0.42).clamp(12.0, 30.0));
    let mut action = Action::None;
    for (label, r) in keys {
        let style = if label == "Hide" { Style::Light } else { Style::Dark };
        if button(ui, r, &label, style, font.clone()).clicked() {
            match label.as_str() {
                "Clear" => text.clear(),
                "space" => text.push(' '),
                "Back" => {
                    text.pop();
                }
                "Hide" => action = Action::Hide,
                k => text.push_str(k),
            }
        }
    }
    action
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layouts_from_system_names() {
        assert_eq!(layout_named("com.apple.keylayout.Belgian"), KeyboardLayout::Azerty);
        assert_eq!(layout_named("com.apple.keylayout.French"), KeyboardLayout::Azerty);
        assert_eq!(layout_named("com.apple.keylayout.German"), KeyboardLayout::Qwertz);
        assert_eq!(layout_named("com.apple.keylayout.US"), KeyboardLayout::Qwerty);
        assert_eq!(layout_named("com.apple.keylayout.British"), KeyboardLayout::Qwerty);
        assert_eq!(layout_named("layout:     be"), KeyboardLayout::Azerty);
        assert_eq!(layout_named("layout:     de"), KeyboardLayout::Qwertz);
        assert_eq!(layout_named("layout:     us"), KeyboardLayout::Qwerty);
        assert_eq!(layout_named(""), KeyboardLayout::Qwerty);
        assert_eq!(rows(KeyboardLayout::Azerty)[1], "azertyuiop");
    }
}
