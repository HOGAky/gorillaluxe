//! Zone C — Visual Drag-and-Drop Keymapper Canvas (spec §3.6).
//! Widgets are placed on the game surface and bound to host keys;
//! layout persists to `%LOCALAPPDATA%\GorillaLuxe\keymap.json`.

use crate::config::data_dir;
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WidgetKind {
    Tap,
    Wasd,
    FpsAim,
    Macro,
}

impl WidgetKind {
    pub fn code(self) -> &'static str {
        match self {
            WidgetKind::Tap => "tap",
            WidgetKind::Wasd => "wasd",
            WidgetKind::FpsAim => "fps_aim",
            WidgetKind::Macro => "macro",
        }
    }
    pub fn from_code(s: &str) -> Option<Self> {
        match s {
            "tap" => Some(WidgetKind::Tap),
            "wasd" => Some(WidgetKind::Wasd),
            "fps_aim" => Some(WidgetKind::FpsAim),
            "macro" => Some(WidgetKind::Macro),
            _ => None,
        }
    }
    /// Palette ordering for Zone C.
    pub const PALETTE: [WidgetKind; 4] =
        [WidgetKind::Tap, WidgetKind::Wasd, WidgetKind::FpsAim, WidgetKind::Macro];
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Widget {
    pub kind: WidgetKind,
    /// Normalized position 0.0..1.0 on the game surface.
    pub x: f32,
    pub y: f32,
    /// Bound virtual-key code (None = unbound).
    pub key: Option<u16>,
}

#[derive(Default)]
pub struct Keymapper {
    pub open: bool,
    pub widgets: Vec<Widget>,
    /// Index of the widget currently dragged with the mouse.
    pub dragging: Option<usize>,
    /// Index selected for key binding (next WM_KEYDOWN assigns VK).
    pub selected: Option<usize>,
}

impl Keymapper {
    pub fn toggle(&mut self) {
        self.open = !self.open;
        if !self.open {
            self.dragging = None;
            self.selected = None;
        }
    }

    pub fn place(&mut self, kind: WidgetKind, x: f32, y: f32) -> usize {
        let w = Widget { kind, x: x.clamp(0.02, 0.98), y: y.clamp(0.02, 0.98), key: None };
        self.widgets.push(w);
        self.widgets.len() - 1
    }

    /// Hit-test placed widgets (pixel space, `sw`/`sh` = surface).
    /// Returns the widget index under `(px, py)` if within `r`.
    pub fn hit(&self, px: f32, py: f32, sw: f32, sh: f32, r: f32) -> Option<usize> {
        self.widgets.iter().enumerate().rev().find_map(|(i, w)| {
            let dx = px - w.x * sw;
            let dy = py - w.y * sh;
            (dx * dx + dy * dy <= r * r).then_some(i)
        })
    }

    pub fn move_to(&mut self, i: usize, x: f32, y: f32) {
        if let Some(w) = self.widgets.get_mut(i) {
            w.x = x.clamp(0.02, 0.98);
            w.y = y.clamp(0.02, 0.98);
        }
    }

    pub fn bind(&mut self, i: usize, vk: u16) {
        if let Some(w) = self.widgets.get_mut(i) {
            w.key = Some(vk);
        }
        self.selected = None;
    }

    pub fn remove(&mut self, i: usize) {
        if i < self.widgets.len() {
            self.widgets.remove(i);
            self.dragging = None;
            self.selected = None;
        }
    }

    pub fn path() -> PathBuf {
        data_dir().join("keymap.json")
    }

    /// Serialize to the `keymap.json` schema.
    pub fn to_json(&self) -> String {
        let mut s = String::from("{\n  \"widgets\": [");
        for (i, w) in self.widgets.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            s.push_str(&format!(
                "\n    {{\"kind\": \"{}\", \"x\": {:.4}, \"y\": {:.4}, \"key\": {}}}",
                w.kind.code(),
                w.x,
                w.y,
                w.key.map(|k| k.to_string()).unwrap_or_else(|| "null".into())
            ));
        }
        s.push_str("\n  ]\n}\n");
        s
    }

    /// Parse `keymap.json` content (flat schema — tolerant parser for
    /// our own emitted format).
    pub fn from_json(text: &str) -> Keymapper {
        let mut km = Keymapper::default();
        let Some(a) = text.find("\"widgets\"") else { return km };
        let Some(start) = text[a..].find('[').map(|i| a + i) else { return km };
        let Some(end) = text[start..].find(']').map(|i| start + i) else { return km };
        let body = &text[start + 1..end];
        let mut rest = body;
        while let Some(ob) = rest.find('{') {
            let Some(cb) = rest[ob..].find('}').map(|i| ob + i) else { break };
            let obj = &rest[ob + 1..cb];
            let kind = get_str(obj, "kind").and_then(WidgetKind::from_code);
            let x = get_num(obj, "x").unwrap_or(0.5) as f32;
            let y = get_num(obj, "y").unwrap_or(0.5) as f32;
            let key = get_num(obj, "key").map(|v| v as u16);
            if let Some(kind) = kind {
                km.widgets.push(Widget { kind, x, y, key });
            }
            rest = &rest[cb + 1..];
        }
        km
    }

    pub fn save(&self) -> std::io::Result<()> {
        let dir = data_dir();
        std::fs::create_dir_all(&dir)?;
        std::fs::write(Self::path(), self.to_json())
    }

    pub fn load() -> Keymapper {
        std::fs::read_to_string(Self::path())
            .map(|t| Self::from_json(&t))
            .unwrap_or_default()
    }
}

fn get_str<'a>(obj: &'a str, key: &str) -> Option<&'a str> {
    let pat = format!("\"{}\"", key);
    let i = obj.find(&pat)? + pat.len();
    let q1 = obj[i..].find('"')? + i;
    let q2 = obj[q1 + 1..].find('"')? + q1 + 1;
    Some(&obj[q1 + 1..q2])
}

fn get_num(obj: &str, key: &str) -> Option<f64> {
    let pat = format!("\"{}\"", key);
    let i = obj.find(&pat)? + pat.len();
    let c = obj[i..].find(':')? + i + 1;
    let tail = obj[c..].trim_start();
    if tail.starts_with("null") {
        return None;
    }
    let end = tail
        .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-' || c == '+' || c == 'e' || c == 'E'))
        .unwrap_or(tail.len());
    tail[..end].parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let mut km = Keymapper::default();
        km.place(WidgetKind::Tap, 0.3, 0.4);
        km.bind(0, 'W' as u16);
        km.place(WidgetKind::FpsAim, 0.75, 0.5);
        let km2 = Keymapper::from_json(&km.to_json());
        assert_eq!(km.widgets, km2.widgets);
    }
}
