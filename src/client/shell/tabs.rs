use super::*;

const TAB_SCROLL_BUTTON_WIDTH: u16 = 3;
const MIN_TAB_STRIP_WIDTH: u16 =
    MIN_TAB_WIDTH + NEW_TAB_WIDTH + TAB_SCROLL_BUTTON_WIDTH.saturating_mul(2);

pub(crate) fn render_tab_bar(
    buffer: &mut Buffer,
    area: Rect,
    snapshot: &ClientShellSnapshot,
    config: &ClientShellConfig,
    tab_scroll: &mut usize,
    reveal_focused_tab: &mut bool,
    tab_drag_insert_index: Option<usize>,
    hits: &mut ShellHitMap,
) {
    let palette = &config.palette;
    let row_bg = tab_bar_bg(palette);
    buffer.set_style(area, Style::default().bg(row_bg));
    let tabs = snapshot
        .tabs
        .iter()
        .filter(|tab| Some(tab.workspace_id.as_str()) == snapshot.focused_workspace_id.as_deref())
        .collect::<Vec<_>>();
    let desired_widths = tabs
        .iter()
        .map(|tab| {
            let label = tab_label(tab);
            display_width(&label).saturating_add(4).max(MIN_TAB_WIDTH)
        })
        .collect::<Vec<_>>();
    let content = tab_bar_content_area(snapshot, area);
    let mouse_chrome = config.mouse_capture;
    let new_tab_width = if mouse_chrome { NEW_TAB_WIDTH } else { 0 };
    let desired_total = desired_widths
        .iter()
        .copied()
        .fold(0_u16, u16::saturating_add)
        .saturating_add(tabs.len().saturating_sub(1).min(u16::MAX as usize) as u16)
        .saturating_add(new_tab_width);
    let overflow =
        desired_total > content.width && (!mouse_chrome || content.width >= MIN_TAB_STRIP_WIDTH);
    let available = if overflow && mouse_chrome {
        content
            .width
            .saturating_sub(NEW_TAB_WIDTH)
            .saturating_sub(TAB_SCROLL_BUTTON_WIDTH.saturating_mul(2))
    } else {
        content.width.saturating_sub(new_tab_width)
    };
    let max_scroll = max_tab_scroll(&desired_widths, available);
    if !overflow {
        *tab_scroll = 0;
    } else if *reveal_focused_tab {
        if let Some(focused) = tabs.iter().position(|tab| tab.focused) {
            *tab_scroll = centered_tab_scroll(focused, &desired_widths, available).min(max_scroll);
        }
    } else {
        *tab_scroll = (*tab_scroll).min(max_scroll);
    }
    *reveal_focused_tab = false;

    let mut x = content.x;
    let tab_right = if overflow && mouse_chrome {
        hits.tab_scroll_left = Rect::new(
            content.x,
            content.y,
            TAB_SCROLL_BUTTON_WIDTH.min(content.width),
            1,
        );
        put_text(
            buffer,
            hits.tab_scroll_left.x,
            content.y,
            hits.tab_scroll_left.width,
            " < ",
            Style::default()
                .fg(if *tab_scroll > 0 {
                    palette.overlay1
                } else {
                    palette.overlay0
                })
                .bg(palette.surface0),
        );
        x = hits.tab_scroll_left.right();
        content
            .right()
            .saturating_sub(NEW_TAB_WIDTH + TAB_SCROLL_BUTTON_WIDTH)
    } else {
        content.right().saturating_sub(new_tab_width)
    };

    let mut first_visible = None;
    let mut last_visible = None;
    for (index, tab) in tabs.iter().enumerate().skip(*tab_scroll) {
        let name = tab_label(tab);
        let desired = desired_widths[index];
        let remaining = tab_right.saturating_sub(x);
        let width = desired.min(remaining);
        if width == 0 {
            break;
        }
        let rect = Rect::new(x, area.y, width, 1);
        let style = if tab.focused {
            let base = Style::default()
                .fg(palette
                    .tab_active_fg
                    .unwrap_or_else(|| panel_contrast_fg(palette)))
                .bg(palette.tab_active_bg.unwrap_or(palette.accent));
            if tab.custom_label || palette.tab_active_bold {
                base.add_modifier(Modifier::BOLD)
            } else {
                base
            }
        } else {
            let fg = palette.tab_inactive_fg.unwrap_or(if tab.custom_label {
                palette.overlay1
            } else {
                palette.overlay0
            });
            Style::default().fg(fg).bg(palette.surface0)
        };
        let padding = width.saturating_sub(display_width(&name));
        let left = padding / 2;
        let text = format!(
            "{empty:left$}{name}{empty:right_padding$}",
            empty = "",
            left = left as usize,
            right_padding = padding.saturating_sub(left) as usize,
        );
        put_text(buffer, rect.x, rect.y, rect.width, &text, style);
        hits.tabs.push((rect, tab.tab_id.clone()));
        first_visible.get_or_insert(index);
        last_visible = Some(index);
        x = x.saturating_add(width + 1);
        if width < desired {
            break;
        }
    }

    if overflow && mouse_chrome {
        hits.tab_scroll_right = Rect::new(tab_right, area.y, TAB_SCROLL_BUTTON_WIDTH, 1);
        let can_scroll_right = *tab_scroll < max_scroll;
        put_text(
            buffer,
            hits.tab_scroll_right.x,
            area.y,
            hits.tab_scroll_right.width,
            " > ",
            Style::default()
                .fg(if can_scroll_right {
                    palette.overlay1
                } else {
                    palette.overlay0
                })
                .bg(palette.surface0),
        );
        hits.new_tab = Rect::new(
            hits.tab_scroll_right.right(),
            area.y,
            content
                .right()
                .saturating_sub(hits.tab_scroll_right.right())
                .min(NEW_TAB_WIDTH),
            1,
        );
    } else if mouse_chrome {
        hits.new_tab = Rect::new(
            x.min(content.right()),
            area.y,
            content.right().saturating_sub(x).min(NEW_TAB_WIDTH),
            1,
        );
    }
    if mouse_chrome {
        put_text(
            buffer,
            hits.new_tab.x,
            area.y,
            hits.new_tab.width,
            " + ",
            Style::default().fg(palette.overlay1).bg(row_bg),
        );
    }

    if first_visible.is_some_and(|index| index > 0) {
        let ellipsis_x = if hits.tab_scroll_left.width > 0 {
            hits.tab_scroll_left.right()
        } else {
            content.x
        };
        put_text(
            buffer,
            ellipsis_x,
            area.y,
            u16::from(ellipsis_x < content.right()),
            "…",
            Style::default().fg(palette.overlay0),
        );
    }
    if last_visible.is_some_and(|index| index + 1 < tabs.len()) {
        let ellipsis_x = if hits.tab_scroll_right.width > 0 {
            hits.tab_scroll_right.x.saturating_sub(1)
        } else {
            content.right().saturating_sub(1)
        };
        put_text(
            buffer,
            ellipsis_x,
            area.y,
            u16::from(ellipsis_x >= content.x && ellipsis_x < content.right()),
            "…",
            Style::default().fg(palette.overlay0),
        );
    }

    if let Some(insert_index) = tab_drag_insert_index {
        if let Some(indicator_x) = tab_drop_indicator_x(hits, &tabs, insert_index) {
            put_text(
                buffer,
                indicator_x.min(content.right().saturating_sub(1)),
                area.y,
                1,
                "│",
                Style::default().fg(palette.accent),
            );
        }
    }
    render_tab_bar_status(
        buffer,
        tab_bar_status_left_area(snapshot, area),
        &snapshot.tab_bar_left,
        &snapshot.tab_bar_left_separator,
        palette,
    );
    render_tab_bar_status(
        buffer,
        tab_bar_status_right_area(snapshot, area),
        &snapshot.tab_bar_right,
        &snapshot.tab_bar_right_separator,
        palette,
    );
}

fn tab_bar_bg(palette: &Palette) -> ratatui::style::Color {
    palette.tab_bar_bg.unwrap_or(palette.panel_bg)
}

/// Combined width of both status areas, used to notice a tab-layout change.
pub(crate) fn tab_bar_status_width(snapshot: &ClientShellSnapshot) -> u16 {
    status_width(&snapshot.tab_bar_left, &snapshot.tab_bar_left_separator).saturating_add(
        status_width(&snapshot.tab_bar_right, &snapshot.tab_bar_right_separator),
    )
}

fn status_width(segments: &[ClientShellTabStatusSegment], separator: &str) -> u16 {
    let content = segments.iter().fold(0u16, |width, segment| {
        width.saturating_add(display_width(&segment.text))
    });
    let separators = segments.len().saturating_sub(1);
    content.saturating_add(
        display_width(separator).saturating_mul(separators.min(u16::MAX as usize) as u16),
    )
}

fn tab_bar_status_right_area(snapshot: &ClientShellSnapshot, area: Rect) -> Option<Rect> {
    let width = status_width(&snapshot.tab_bar_right, &snapshot.tab_bar_right_separator);
    if width == 0 {
        return None;
    }
    let reserved = width.saturating_add(1);
    (area.width.saturating_sub(reserved) >= MIN_TAB_STRIP_WIDTH)
        .then(|| Rect::new(area.right().saturating_sub(width), area.y, width, 1))
}

/// The left area is reserved after the right one, so a narrowing row drops the
/// left status first and the tabs keep their minimum strip either way.
fn tab_bar_status_left_area(snapshot: &ClientShellSnapshot, area: Rect) -> Option<Rect> {
    let width = status_width(&snapshot.tab_bar_left, &snapshot.tab_bar_left_separator);
    if width == 0 {
        return None;
    }
    let right_reserved = tab_bar_status_right_area(snapshot, area)
        .map(|status| status.width.saturating_add(1))
        .unwrap_or(0);
    let remaining = area.width.saturating_sub(right_reserved);
    (remaining.saturating_sub(width.saturating_add(1)) >= MIN_TAB_STRIP_WIDTH)
        .then(|| Rect::new(area.x, area.y, width, 1))
}

fn tab_bar_content_area(snapshot: &ClientShellSnapshot, area: Rect) -> Rect {
    let right_reserved = tab_bar_status_right_area(snapshot, area)
        .map(|status| status.width.saturating_add(1))
        .unwrap_or(0);
    let left_reserved = tab_bar_status_left_area(snapshot, area)
        .map(|status| status.width.saturating_add(1))
        .unwrap_or(0);
    Rect {
        x: area.x.saturating_add(left_reserved),
        width: area
            .width
            .saturating_sub(right_reserved)
            .saturating_sub(left_reserved),
        ..area
    }
}

fn render_tab_bar_status(
    buffer: &mut Buffer,
    status: Option<Rect>,
    segments: &[ClientShellTabStatusSegment],
    separator: &str,
    palette: &Palette,
) {
    let Some(status) = status else {
        return;
    };
    let row_bg = tab_bar_bg(palette);
    let separator_width = display_width(separator);
    let mut x = status.x;
    for (index, segment) in segments.iter().enumerate() {
        if index > 0 && separator_width > 0 {
            put_text(
                buffer,
                x,
                status.y,
                separator_width,
                separator,
                Style::default().fg(palette.overlay0).bg(row_bg),
            );
            x = x.saturating_add(separator_width);
        }
        if segment.accent {
            let width = display_width(&segment.text);
            put_text(
                buffer,
                x,
                status.y,
                width,
                &segment.text,
                Style::default()
                    .fg(panel_contrast_fg(palette))
                    .bg(palette.accent)
                    .add_modifier(Modifier::BOLD),
            );
            x = x.saturating_add(width);
            continue;
        }
        if segment.spans.is_empty() {
            let width = display_width(&segment.text);
            put_text(
                buffer,
                x,
                status.y,
                width,
                &segment.text,
                Style::default().fg(palette.overlay1).bg(row_bg),
            );
            x = x.saturating_add(width);
            continue;
        }
        for span in &segment.spans {
            let width = display_width(&span.text);
            let mut style = Style::default()
                .fg(status_color(span.fg).unwrap_or(palette.overlay1))
                .bg(status_color(span.bg).unwrap_or(row_bg));
            if span.bold {
                style = style.add_modifier(Modifier::BOLD);
            }
            put_text(buffer, x, status.y, width, &span.text, style);
            x = x.saturating_add(width);
        }
    }
}

fn status_color(color: Option<ClientShellStatusColor>) -> Option<ratatui::style::Color> {
    let color = color?;
    match (color.rgb, color.indexed) {
        (Some((red, green, blue)), _) => Some(ratatui::style::Color::Rgb(red, green, blue)),
        (None, Some(index)) => Some(ratatui::style::Color::Indexed(index)),
        (None, None) => None,
    }
}

fn tab_drop_indicator_x(
    hits: &ShellHitMap,
    tabs: &[&ClientShellTab],
    insert_index: usize,
) -> Option<u16> {
    let visible = hits
        .tabs
        .iter()
        .filter_map(|(rect, tab_id)| {
            tabs.iter()
                .position(|tab| tab.tab_id == *tab_id)
                .map(|index| (index, *rect))
        })
        .collect::<Vec<_>>();
    let (first_index, first_rect) = *visible.first()?;
    let (last_index, last_rect) = *visible.last()?;
    if insert_index == 0 {
        return Some(if first_index == 0 {
            first_rect.x
        } else {
            hits.tab_scroll_left.right()
        });
    }
    if let Some((_, rect)) = visible.iter().find(|(index, _)| *index == insert_index) {
        return Some(rect.x.saturating_sub(1));
    }
    if insert_index >= tabs.len() {
        return Some(if last_index + 1 >= tabs.len() {
            last_rect.right()
        } else {
            hits.tab_scroll_right.x.saturating_sub(1)
        });
    }
    None
}

fn centered_tab_scroll(focused: usize, widths: &[u16], available: u16) -> usize {
    let mut best = focused;
    let mut best_distance = u16::MAX;
    for start in 0..=focused {
        let before = widths
            .iter()
            .copied()
            .enumerate()
            .skip(start)
            .take(focused.saturating_sub(start))
            .fold(0u16, |width, (_, tab)| width.saturating_add(tab + 1));
        if before >= available {
            continue;
        }
        let focused_width = widths[focused].min(available.saturating_sub(before));
        let center = before.saturating_mul(2).saturating_add(focused_width);
        let distance = center.abs_diff(available);
        if distance <= best_distance {
            best_distance = distance;
            best = start;
        }
    }
    best
}

fn max_tab_scroll(widths: &[u16], available: u16) -> usize {
    let Some((&last, preceding)) = widths.split_last() else {
        return 0;
    };
    let mut start = preceding.len();
    let mut used = u32::from(last);
    // Keep the longest fully visible suffix, not merely a sliver of the last tab.
    // An oversized last tab must still be reachable at the start of the strip.
    for width in preceding.iter().rev() {
        let required = used + 1 + u32::from(*width);
        if required > u32::from(available) {
            break;
        }
        used = required;
        start -= 1;
    }
    start
}

fn tab_label(tab: &ClientShellTab) -> String {
    if tab.zoomed {
        format!("{} Z", tab.label)
    } else {
        tab.label.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::max_tab_scroll;

    #[test]
    fn trailing_scroll_limit_accounts_for_full_widths_and_separators() {
        for (widths, available, expected) in [
            (&[][..], 0, 0),
            (&[8, 13][..], 0, 1),
            (&[8, 13][..], 1, 1),
            (&[8, 13][..], 12, 1),
            (&[8, 13][..], 21, 1),
            (&[8, 13][..], 22, 0),
            (&[8, 13][..], 30, 0),
            (&[8, u16::MAX][..], u16::MAX, 1),
        ] {
            assert_eq!(
                max_tab_scroll(widths, available),
                expected,
                "widths={widths:?}, available={available}"
            );
        }
    }
}
