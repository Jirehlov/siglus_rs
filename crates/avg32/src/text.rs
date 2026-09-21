//! Stateful AVG32 message-window model.

use crate::vm::VmAction;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageWindow {
    lines: Vec<String>,
    current: String,
    pub visible: bool,
    pub doubled_font: bool,
    pub color_index: i32,
}

impl Default for MessageWindow {
    fn default() -> Self {
        Self {
            lines: Vec::new(),
            current: String::new(),
            visible: true,
            doubled_font: false,
            color_index: 0,
        }
    }
}

impl MessageWindow {
    pub fn lines(&self) -> impl Iterator<Item = &str> {
        self.lines
            .iter()
            .map(String::as_str)
            .chain((!self.current.is_empty()).then_some(self.current.as_str()))
    }

    pub fn clear(&mut self, hide: bool) {
        self.lines.clear();
        self.current.clear();
        self.visible = !hide;
    }

    pub fn apply(&mut self, action: &VmAction) {
        match action {
            VmAction::Text(text) => {
                self.visible = true;
                self.current.push_str(text);
            }
            VmAction::LineBreak => {
                self.lines.push(std::mem::take(&mut self.current));
            }
            VmAction::ClearText { hide_window } => self.clear(*hide_window),
            VmAction::SetFontSize { doubled } => self.doubled_font = *doubled,
            VmAction::SetFontColor(color) => self.color_index = *color,
            VmAction::WaitForInput { clears_text: true } => self.clear(false),
            _ => {}
        }
    }
}
