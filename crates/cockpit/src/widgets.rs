//! Reusable TUI primitives for the cockpit.
//!
//! Pulled out of `lib.rs` so the bespoke overlays (confirm-kill,
//! confirm-promote, confirm-boot-mode, balloon-prompt, …) stop
//! reinventing the same centered-Clear-bordered-Paragraph dance every
//! time. The helpers here intentionally accept already-built
//! `Vec<Line>` bodies — they don't try to be a layout engine, just a
//! consistent frame around whatever the caller assembles.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

/// One self-contained modal: a title bar (in the border), a vertical
/// stack of body lines, and a yellow footer line for keybindings.
/// Sized in absolute character cells; centered in `area`.
pub struct OverlayCard<'a> {
    pub title: &'a str,
    pub headline: Option<Line<'a>>,
    pub body: Vec<Line<'a>>,
    pub footer: Option<Line<'a>>,
    pub width: u16,
    pub height: u16,
    pub accent: Color,
}

/// Render an OverlayCard into the centered absolute rect.
pub fn render_overlay_card(f: &mut Frame, area: Rect, card: OverlayCard<'_>) {
    let overlay = centered_rect_abs(card.width, card.height, area);
    f.render_widget(Clear, overlay);

    let mut lines: Vec<Line> = Vec::with_capacity(card.body.len() + 4);
    if let Some(h) = card.headline {
        lines.push(h);
        lines.push(Line::raw(""));
    }
    lines.extend(card.body);
    if let Some(footer) = card.footer {
        lines.push(Line::raw(""));
        lines.push(footer);
    }

    let title = format!(" {} ", card.title.trim());
    let p = Paragraph::new(lines)
        .block(Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(Style::default().fg(card.accent)));
    f.render_widget(p, overlay);
}

/// Standardised yes/no confirm prompt. Builder produces the OverlayCard;
/// the caller still calls render_overlay_card. Keeping the two steps
/// separate lets callers tweak the card before render (e.g. swap accent
/// to red for destructive ops).
pub fn confirm_card<'a>(
    title: &'a str,
    headline: &'a str,
    body: Vec<Line<'a>>,
    yes_label: &'a str,
    width: u16,
) -> OverlayCard<'a> {
    let height = (body.len() as u16) + 6;
    OverlayCard {
        title,
        headline: Some(Line::from(Span::styled(headline,
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)))),
        body,
        footer: Some(Line::from(Span::styled(
            format!("  [y] {yes_label}   [Esc/n] cancel"),
            Style::default().fg(Color::Yellow)))),
        width,
        height,
        accent: Color::Yellow,
    }
}

/// Centered rect with absolute (character-cell) dimensions. Used for
/// modal overlays where percentage sizing is the wrong abstraction —
/// we know exactly how wide the lines are.
pub fn centered_rect_abs(w: u16, h: u16, r: Rect) -> Rect {
    let x = r.x + (r.width.saturating_sub(w)) / 2;
    let y = r.y + (r.height.saturating_sub(h)) / 2;
    Rect { x, y, width: w.min(r.width), height: h.min(r.height) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn centered_rect_abs_centers_within_area() {
        let area = Rect { x: 0, y: 0, width: 80, height: 24 };
        let r = centered_rect_abs(40, 10, area);
        // Centred horizontally: (80-40)/2 = 20
        assert_eq!(r.x, 20);
        assert_eq!(r.width, 40);
        // Centred vertically: (24-10)/2 = 7
        assert_eq!(r.y, 7);
        assert_eq!(r.height, 10);
    }

    #[test]
    fn centered_rect_abs_clamps_to_available() {
        let area = Rect { x: 0, y: 0, width: 30, height: 10 };
        // Asking for 80x24 in a 30x10 frame — clamp to area, no panic.
        let r = centered_rect_abs(80, 24, area);
        assert_eq!(r.width, 30);
        assert_eq!(r.height, 10);
    }
}
