//! Drawing of the disk map and of the folder treemap on a character canvas.

use crate::disks::{Disk, Region, SWAP_MOUNT, UNALLOCATED};
use crate::layout::{Rect, fit_treemap, layout_disks, stack_regions};
use crate::smart::{Level, fit_fields, health_fields};
use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier, Style};
use std::collections::HashSet;
use unicode_width::UnicodeWidthChar;

const GIB: f64 = (1_u64 << 30) as f64;
const FILL_CHAR: char = '#';
const EMPTY_CHAR: char = '.';

const DISK_HEADER_ROWS: usize = 3;
const MIN_DISK_WIDTH: usize = 34;
const MIN_DISK_HEIGHT: usize = 8;
const MIN_REGION_WIDTH: usize = 18;
const MIN_MOUNTED_HEIGHT: usize = 4;
const MIN_UNMOUNTED_HEIGHT: usize = 3;
const MIN_ITEM_BOX: (usize, usize) = (12, 3);

const BORDER_UP: u8 = 1;
const BORDER_DOWN: u8 = 2;
const BORDER_LEFT: u8 = 4;
const BORDER_RIGHT: u8 = 8;
/// Line drawing characters, indexed by the combination of border directions.
const BORDER_CHARS: [char; 16] = [
  ' ', '│', '│', '│', '─', '┘', '┐', '┤', '─', '└', '┌', '├', '─', '┴', '┬', '┼',
];

const USED_LOW: Color = Color::Rgb(0x97, 0xC4, 0x59);
const USED_HIGH: Color = Color::Rgb(0xEF, 0x9F, 0x27);
const USED_FULL: Color = Color::Rgb(0xE2, 0x4B, 0x4A);
const USED_HIGH_FROM: f64 = 0.70;
const USED_FULL_FROM: f64 = 0.90;
const FREE_SPACE: Color = Color::Rgb(0x2C, 0x2C, 0x2A);
const TEXT_ON_USED: Color = Color::Rgb(0x17, 0x17, 0x16);
const TEXT_ON_DARK: Color = Color::Rgb(0xE8, 0xE6, 0xDF);
const TEXT_MUTED: Color = Color::Rgb(0xB4, 0xB2, 0xA9);
const FOLDER_SMALL: (u8, u8, u8) = (0x16, 0x2C, 0x45);
const FOLDER_LARGE: (u8, u8, u8) = (0x18, 0x5F, 0xA5);
const FILE_SMALL: (u8, u8, u8) = (0x26, 0x26, 0x24);
const FILE_LARGE: (u8, u8, u8) = (0x5F, 0x5E, 0x5A);
const GROUP_TINT: Color = Color::Rgb(0x1F, 0x1F, 0x1E);
const MARKED_TINT: Color = Color::Rgb(0x2E, 0x7D, 0x32);
const SELECTION_FRAME: Color = Color::Rgb(0xFF, 0x1E, 0x1E);
const DIALOG_BACKGROUND: Color = Color::Rgb(0x17, 0x17, 0x16);
/// Partial block characters, indexed by the eighths of a cell they cover.
const EIGHTHS: [char; 8] = [' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉'];

/// How the inside of a box is painted.
#[derive(Clone, Copy, Debug)]
enum Fill {
  Empty,
  Hashes(f64),
  Bar(f64),
  Tint(Color),
}

/// Key of the box that merges the smallest items.
pub const GROUP_KEY: &str = "";

/// A selectable box of the current screen.
#[derive(Clone, Debug)]
pub struct Target {
  pub key: String,
  pub rect: Rect,
}

/// What a box of the folder treemap stands for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
  File,
  Dir(u32),
  Group(u64),
}

/// One child of the folder on screen.
#[derive(Clone, Debug)]
pub struct Entry {
  pub name: String,
  pub size: u64,
  pub kind: EntryKind,
}

/// Format bytes as whole gibibytes; anything below 1G is shown as 0G.
pub fn format_gib(size: u64) -> String {
  let gib = size as f64 / GIB;
  if gib < 1.0 {
    "0G".to_string()
  } else {
    format!("{gib:.0}G")
  }
}

/// Format bytes with the largest unit that keeps the number readable.
pub fn format_size(size: u64) -> String {
  let mut value = size as f64;
  let mut units = ["B", "K", "M", "G", "T", "P"].iter().peekable();
  let mut unit = "B";
  while let Some(next) = units.next() {
    unit = next;
    if value < 1024.0 || units.peek().is_none() {
      break;
    }
    value /= 1024.0;
  }
  match unit {
    "B" => format!("{value:.0}B"),
    _ if value < 100.0 => format!("{value:.1}{unit}"),
    _ => format!("{value:.0}{unit}"),
  }
}

/// Shorten text to width by replacing its middle with a tilde.
pub fn fit_text(text: &str, width: usize) -> String {
  let chars: Vec<char> = text.chars().collect();
  if chars.len() <= width {
    return text.to_string();
  }
  if width == 0 {
    return String::new();
  }
  let head = width / 2;
  let tail = width - head - 1;
  chars[..head]
    .iter()
    .chain(std::iter::once(&'~'))
    .chain(&chars[chars.len() - tail..])
    .collect()
}

/// A character grid whose crossing box borders join up automatically.
pub struct Canvas {
  pub width: usize,
  pub height: usize,
  /// Draw with hashes and basic colours instead of solid colour fills.
  ascii: bool,
  cells: Vec<char>,
  borders: Vec<u8>,
  styles: Vec<Style>,
}

impl Canvas {
  /// Create an empty canvas.
  pub fn new(width: usize, height: usize, ascii: bool) -> Self {
    let count = width * height;
    Self {
      width,
      height,
      ascii,
      cells: vec![' '; count],
      borders: vec![0; count],
      styles: vec![Style::default(); count],
    }
  }

  /// Add border directions to one cell, ignoring cells outside the canvas.
  fn add_border(&mut self, x: usize, y: usize, bits: u8) {
    if x < self.width && y < self.height {
      self.borders[y * self.width + x] |= bits;
    }
  }

  /// Draw the outline of a box.
  pub fn draw_box(&mut self, rect: Rect) {
    if rect.width == 0 || rect.height == 0 {
      return;
    }
    let right = rect.x + rect.width - 1;
    let bottom = rect.y + rect.height - 1;
    for x in rect.x..=right {
      let bits =
        if x > rect.x { BORDER_LEFT } else { 0 } | if x < right { BORDER_RIGHT } else { 0 };
      self.add_border(x, rect.y, bits);
      self.add_border(x, bottom, bits);
    }
    for y in rect.y..=bottom {
      let bits = if y > rect.y { BORDER_UP } else { 0 } | if y < bottom { BORDER_DOWN } else { 0 };
      self.add_border(rect.x, y, bits);
      self.add_border(right, y, bits);
    }
  }

  /// Write text starting at a cell; names cannot break the grid or send terminal controls.
  pub fn put(&mut self, x: usize, y: usize, text: &str, style: Style) {
    if y >= self.height {
      return;
    }
    for (offset, c) in text.chars().enumerate() {
      if x + offset < self.width {
        let index = y * self.width + x + offset;
        self.cells[index] = if c.width() == Some(1) { c } else { '?' };
        self.styles[index] = style;
      }
    }
  }

  /// Mark a box as selected, keeping its text.
  pub fn highlight(&mut self, rect: Rect) {
    if rect.width == 0 || rect.height == 0 {
      return;
    }
    let right = rect.x + rect.width - 1;
    let bottom = rect.y + rect.height - 1;
    let inside = Style::new().fg(Color::Black).bg(Color::Yellow);
    let edge_color = if self.ascii {
      Color::LightRed
    } else {
      SELECTION_FRAME
    };
    let edge = Style::new().fg(edge_color).add_modifier(Modifier::BOLD);
    for y in rect.y..=bottom.min(self.height.saturating_sub(1)) {
      for x in rect.x..=right.min(self.width.saturating_sub(1)) {
        let index = y * self.width + x;
        let glyph = match (x == rect.x, x == right, y == rect.y, y == bottom) {
          (true, _, true, _) => '┏',
          (_, true, true, _) => '┓',
          (true, _, _, true) => '┗',
          (_, true, _, true) => '┛',
          (_, _, true, _) | (_, _, _, true) => '━',
          (true, _, _, _) | (_, true, _, _) => '┃',
          _ => {
            if self.ascii {
              self.styles[index] = inside;
            }
            continue;
          }
        };
        self.styles[index] = edge;
        if !self.ascii {
          self.borders[index] = 0;
          self.cells[index] = glyph;
        }
      }
    }
  }

  /// Blank an area, including any borders crossing it.
  fn clear(&mut self, rect: Rect, style: Style) {
    for y in rect.y..(rect.y + rect.height).min(self.height) {
      for x in rect.x..(rect.x + rect.width).min(self.width) {
        let index = y * self.width + x;
        self.cells[index] = ' ';
        self.borders[index] = 0;
        self.styles[index] = style;
      }
    }
  }

  /// Return the character shown in one cell.
  fn shown(&self, index: usize) -> char {
    match self.borders[index] {
      0 => self.cells[index],
      bits => BORDER_CHARS[bits as usize],
    }
  }

  /// Copy the canvas to a terminal buffer of at least the same size.
  pub fn blit(&self, buffer: &mut Buffer) {
    for y in 0..self.height.min(buffer.area.height as usize) {
      for x in 0..self.width.min(buffer.area.width as usize) {
        let index = y * self.width + x;
        buffer[(x as u16, y as u16)]
          .set_char(self.shown(index))
          .set_style(self.styles[index]);
      }
    }
  }

  /// Return the canvas as plain text.
  #[cfg(test)]
  pub fn text(&self) -> String {
    (0..self.height)
      .map(|y| {
        (0..self.width)
          .map(|x| self.shown(y * self.width + x))
          .collect::<String>()
      })
      .collect::<Vec<_>>()
      .join("\n")
  }
}

/// Return the title and detail text of a region.
fn region_text(region: &Region) -> (String, String) {
  let size = format_gib(region.size);
  if let (Some(percent), Some(total), Some(mount)) =
    (region.percent(), region.total, &region.mountpoint)
  {
    return (
      mount.clone(),
      format!("{}  {percent:.1}%", format_gib(total)),
    );
  }
  match &region.mountpoint {
    Some(mount) if mount == SWAP_MOUNT => ("swap".to_string(), format!("{}  {size}", region.name)),
    Some(mount) => (mount.clone(), size),
    None if region.name == UNALLOCATED => (UNALLOCATED.to_string(), size),
    None => {
      let mut details = vec!["not mounted".to_string()];
      details.extend(region.fstype.clone());
      details.push(size);
      (region.name.clone(), details.join("  "))
    }
  }
}

/// Return one label line, or the title and detail on two lines when one is too long.
///
/// With `keep_fill_row` a box of two rows keeps its second row for the fill.
fn label_lines(
  title: &str,
  detail: &str,
  short_detail: Option<String>,
  width: usize,
  height: usize,
  keep_fill_row: bool,
) -> Vec<String> {
  let line = format!("{title}  {detail}");
  if line.chars().count() <= width {
    return vec![line];
  }
  if height < if keep_fill_row { 3 } else { 2 } {
    return vec![title.to_string()];
  }
  let too_long = detail.chars().count() > width;
  let detail = short_detail
    .filter(|_| too_long)
    .unwrap_or_else(|| detail.to_string());
  vec![title.to_string(), detail]
}

/// Return the bar colour for a used share.
fn usage_color(share: f64) -> Color {
  if share >= USED_FULL_FROM {
    USED_FULL
  } else if share >= USED_HIGH_FROM {
    USED_HIGH
  } else {
    USED_LOW
  }
}

/// Return a colour between two others.
fn mix(low: (u8, u8, u8), high: (u8, u8, u8), amount: f64) -> Color {
  let channel = |from: u8, to: u8| (from as f64 + (to as f64 - from as f64) * amount).round() as u8;
  Color::Rgb(
    channel(low.0, high.0),
    channel(low.1, high.1),
    channel(low.2, high.2),
  )
}

/// Return the columns fully used by a bar and the eighths used of the next column.
fn bar_extent(width: usize, share: f64) -> (usize, usize) {
  let eighths = ((width * 8) as f64 * share.clamp(0.0, 1.0)).round() as usize;
  (eighths / 8, eighths % 8)
}

/// Paint a usage bar over a whole area, with a partial block at its end.
fn paint_bar(
  canvas: &mut Canvas,
  (x, y): (usize, usize),
  (width, height): (usize, usize),
  share: f64,
) {
  let color = usage_color(share);
  let (full, edge) = bar_extent(width, share);
  for row in y..y + height {
    canvas.put(x, row, &" ".repeat(full), Style::new().bg(color));
    canvas.put(
      x + full,
      row,
      &" ".repeat(width - full),
      Style::new().bg(FREE_SPACE),
    );
    if edge > 0 {
      let glyph = EIGHTHS[edge].to_string();
      canvas.put(x + full, row, &glyph, Style::new().fg(color).bg(FREE_SPACE));
    }
  }
}

/// Write a label over a bar, with a text colour that suits each cell's background.
fn put_on_bar(canvas: &mut Canvas, (x, y): (usize, usize), text: &str, bar: (usize, usize, f64)) {
  let (bar_x, width, share) = bar;
  let color = usage_color(share);
  let (full, _) = bar_extent(width, share);
  for (offset, c) in text.chars().enumerate() {
    let column = x + offset;
    let style = if column < bar_x + full {
      Style::new().fg(TEXT_ON_USED).bg(color)
    } else {
      Style::new().fg(TEXT_ON_DARK).bg(FREE_SPACE)
    };
    canvas.put(
      column,
      y,
      &c.to_string(),
      style.add_modifier(Modifier::BOLD),
    );
  }
}

/// Draw a box with its label and its fill.
fn draw_labelled(
  canvas: &mut Canvas,
  rect: Rect,
  (title, detail, short_detail): (&str, &str, Option<String>),
  fill: Fill,
  style: Style,
) {
  canvas.draw_box(rect);
  let inner_width = rect.width.saturating_sub(2);
  let inner_height = rect.height.saturating_sub(2);
  if inner_width == 0 || inner_height == 0 {
    return;
  }
  let text_width = inner_width.saturating_sub(1);
  let lines = label_lines(
    title,
    detail,
    short_detail,
    text_width,
    inner_height,
    matches!(fill, Fill::Hashes(_)),
  );
  let (left, top) = (rect.x + 1, rect.y + 1);
  match fill {
    Fill::Bar(share) => paint_bar(canvas, (left, top), (inner_width, inner_height), share),
    Fill::Tint(color) => {
      for row in top..top + inner_height {
        canvas.put(left, row, &" ".repeat(inner_width), Style::new().bg(color));
      }
    }
    Fill::Empty | Fill::Hashes(_) => {}
  }
  for (index, line) in lines.iter().take(inner_height).enumerate() {
    let line = fit_text(line, text_width);
    let position = (left + 1, top + index);
    match fill {
      Fill::Bar(share) => put_on_bar(canvas, position, &line, (left, inner_width, share)),
      Fill::Tint(color) => canvas.put(position.0, position.1, &line, style.bg(color)),
      Fill::Empty | Fill::Hashes(_) => canvas.put(position.0, position.1, &line, style),
    }
  }
  if let Fill::Hashes(share) = fill {
    let filled = ((inner_width as f64 * share).round() as usize).min(inner_width);
    let row: String = std::iter::repeat_n(FILL_CHAR, filled)
      .chain(std::iter::repeat_n(EMPTY_CHAR, inner_width - filled))
      .collect();
    for index in lines.len()..inner_height {
      canvas.put(left, top + index, &row, Style::default());
    }
  }
}

/// Draw a region box with its label and its usage fill.
fn draw_region(canvas: &mut Canvas, region: &Region, rect: Rect) {
  let (title, detail) = region_text(region);
  let short = region.percent().map(|percent| format!("{percent:.1}%"));
  let fill = match region.percent() {
    Some(percent) if canvas.ascii => Fill::Hashes(percent / 100.0),
    Some(percent) => Fill::Bar(percent / 100.0),
    None => Fill::Empty,
  };
  draw_labelled(
    canvas,
    rect,
    (&title, &detail, short),
    fill,
    Style::default(),
  );
}

/// Write the health values of a disk on one row, coloured by how much attention they need.
fn draw_health(canvas: &mut Canvas, disk: &Disk, (x, y): (usize, usize), width: usize) {
  let mut column = x;
  for field in fit_fields(health_fields(&disk.health), width) {
    let style = match field.level {
      Level::Muted => Style::new().fg(Color::DarkGray),
      Level::Normal => Style::default(),
      Level::Warning => Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
      Level::Alarm => Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
    };
    let text = fit_text(&field.text, width);
    canvas.put(column, y, &text, style);
    column += text.chars().count() + 2;
  }
}

/// Draw a disk box with its details and regions, collecting the regions that can be opened.
fn draw_disk(canvas: &mut Canvas, disk: &Disk, rect: Rect, targets: &mut Vec<Target>) {
  canvas.draw_box(rect);
  let text_width = rect.width.saturating_sub(3);
  let title = format!("{}  {}  {}", disk.name, disk.model, format_gib(disk.size));
  let link = format!("{}  {}", disk.bus, disk.link);
  let title_style = Style::new().add_modifier(Modifier::BOLD);
  for (index, line) in [title, link].iter().enumerate() {
    if index + 2 < rect.height {
      canvas.put(
        rect.x + 2,
        rect.y + 1 + index,
        &fit_text(line, text_width),
        title_style,
      );
    }
  }
  if DISK_HEADER_ROWS + 1 < rect.height {
    draw_health(
      canvas,
      disk,
      (rect.x + 2, rect.y + DISK_HEADER_ROWS),
      text_width,
    );
  }
  let header_rows = DISK_HEADER_ROWS + 1;
  if rect.height <= header_rows + 1 {
    return;
  }
  let area = Rect {
    y: rect.y + header_rows,
    height: rect.height - header_rows,
    ..rect
  };
  let weights: Vec<u64> = disk.regions.iter().map(|region| region.size).collect();
  let heights: Vec<usize> = disk
    .regions
    .iter()
    .map(|region| {
      if region.percent().is_some() {
        MIN_MOUNTED_HEIGHT
      } else {
        MIN_UNMOUNTED_HEIGHT
      }
    })
    .collect();
  for (region, boxed) in
    disk
      .regions
      .iter()
      .zip(stack_regions(area, &weights, MIN_REGION_WIDTH, &heights))
  {
    draw_region(canvas, region, boxed);
    if let Some(mount) = region.browsable_mount() {
      targets.push(Target {
        key: mount.to_string(),
        rect: boxed,
      });
    }
  }
}

/// Draw every disk and return the partitions that can be opened.
pub fn draw_disks(canvas: &mut Canvas, disks: &[Disk], area: Rect) -> Vec<Target> {
  let sizes: Vec<u64> = disks.iter().map(|disk| disk.size).collect();
  let mut targets = Vec::new();
  for (disk, rect) in disks
    .iter()
    .zip(layout_disks(&sizes, area, MIN_DISK_WIDTH, MIN_DISK_HEIGHT))
  {
    draw_disk(canvas, disk, rect, &mut targets);
  }
  targets
}

/// Return the title, fill and text style of one treemap box.
fn entry_paint(entry: &Entry, share: f64, ascii: bool, marked: bool) -> (String, Fill, Style) {
  if marked {
    let dir_suffix = if matches!(entry.kind, EntryKind::Dir(_)) {
      "/"
    } else {
      ""
    };
    let title = format!("{}{dir_suffix}", entry.name);
    return if ascii {
      (
        title,
        Fill::Hashes(share),
        Style::new().fg(Color::Green).add_modifier(Modifier::BOLD),
      )
    } else {
      (
        title,
        Fill::Tint(MARKED_TINT),
        Style::new().fg(TEXT_ON_DARK).add_modifier(Modifier::BOLD),
      )
    };
  }
  let bold = Modifier::BOLD;
  let strength = share.clamp(0.0, 1.0).sqrt();
  let hashes = Fill::Hashes(share);
  match entry.kind {
    EntryKind::Dir(_) if ascii => (
      format!("{}/", entry.name),
      hashes,
      Style::new().fg(Color::Cyan).add_modifier(bold),
    ),
    EntryKind::Dir(_) => (
      format!("{}/", entry.name),
      Fill::Tint(mix(FOLDER_SMALL, FOLDER_LARGE, strength)),
      Style::new().fg(TEXT_ON_DARK).add_modifier(bold),
    ),
    EntryKind::File if ascii => (entry.name.clone(), hashes, Style::default()),
    EntryKind::File => (
      entry.name.clone(),
      Fill::Tint(mix(FILE_SMALL, FILE_LARGE, strength)),
      Style::new().fg(TEXT_ON_DARK),
    ),
    EntryKind::Group(count) if ascii => (
      format!("({count} more)"),
      hashes,
      Style::new().fg(Color::DarkGray),
    ),
    EntryKind::Group(count) => (
      format!("({count} more)"),
      Fill::Tint(GROUP_TINT),
      Style::new().fg(TEXT_MUTED),
    ),
  }
}

/// Draw one treemap box and return its selection target.
fn draw_entry(canvas: &mut Canvas, entry: &Entry, rect: Rect, total: u64, marked: bool) -> Target {
  let share = entry.size as f64 / total.max(1) as f64;
  let percent = format!("{:.1}%", share * 100.0);
  let detail = format!("{}  {percent}", format_size(entry.size));
  let (title, fill, style) = entry_paint(entry, share, canvas.ascii, marked);
  let key = if matches!(entry.kind, EntryKind::Group(_)) {
    GROUP_KEY.to_string()
  } else {
    entry.name.clone()
  };
  draw_labelled(canvas, rect, (&title, &detail, Some(percent)), fill, style);
  Target { key, rect }
}

/// Draw the children of a folder, sorted by descending size, and return their boxes.
///
/// The second value is the entry shown in the merged box of small items, if there is one.
pub fn draw_folder(
  canvas: &mut Canvas,
  entries: &[Entry],
  total: u64,
  area: Rect,
  marked: &HashSet<String>,
) -> (Vec<Target>, Option<Entry>) {
  let (groups, named): (Vec<&Entry>, Vec<&Entry>) = entries
    .iter()
    .partition(|entry| matches!(entry.kind, EntryKind::Group(_)));
  let group_count = |entry: &&Entry| {
    if let EntryKind::Group(count) = entry.kind {
      count
    } else {
      0
    }
  };
  let extra: u64 = groups.iter().map(|entry| entry.size).sum();
  let weights: Vec<u64> = named.iter().map(|entry| entry.size).collect();
  let (kept, boxes) = fit_treemap(&weights, extra, area, MIN_ITEM_BOX);
  let mut targets: Vec<Target> = named
    .iter()
    .zip(&boxes)
    .take(kept)
    .map(|(entry, &rect)| draw_entry(canvas, entry, rect, total, marked.contains(&entry.name)))
    .collect();
  let merged = boxes.get(kept).map(|&rect| {
    let count = (named.len() - kept) as u64 + groups.iter().map(group_count).sum::<u64>();
    let size = weights[kept..].iter().sum::<u64>() + extra;
    let entry = Entry {
      name: GROUP_KEY.to_string(),
      size,
      kind: EntryKind::Group(count),
    };
    targets.push(draw_entry(canvas, &entry, rect, total, false));
    entry
  });
  (targets, merged)
}

/// Draw a centred window with one row of text per line.
pub fn draw_dialog(canvas: &mut Canvas, lines: &[String], danger: bool) {
  let widest = lines
    .iter()
    .map(|line| line.chars().count())
    .max()
    .unwrap_or(0);
  let width = (widest + 4).min(canvas.width);
  let height = (lines.len() + 2).min(canvas.height);
  if width == 0 || height == 0 {
    return;
  }
  let rect = Rect {
    x: (canvas.width - width) / 2,
    y: (canvas.height - height) / 2,
    width,
    height,
  };
  let text = Style::new()
    .fg(TEXT_ON_DARK)
    .bg(DIALOG_BACKGROUND)
    .add_modifier(Modifier::BOLD);
  canvas.clear(rect, text);
  canvas.draw_box(rect);
  if danger {
    let frame = text.fg(SELECTION_FRAME);
    for x in rect.x..rect.x + rect.width {
      canvas.styles[rect.y * canvas.width + x] = frame;
      canvas.styles[(rect.y + rect.height - 1) * canvas.width + x] = frame;
    }
    for y in rect.y..rect.y + rect.height {
      canvas.styles[y * canvas.width + rect.x] = frame;
      canvas.styles[y * canvas.width + rect.x + rect.width - 1] = frame;
    }
  }
  for (index, line) in lines.iter().take(height.saturating_sub(2)).enumerate() {
    let fitted = fit_text(line, width.saturating_sub(4));
    canvas.put(rect.x + 2, rect.y + 1 + index, &fitted, text);
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::smart::{Health, Report};

  const GIB: u64 = 1 << 30;

  /// Return a disk fixture with mounted, unmounted and tiny partitions.
  fn disk(name: &str, gib: u64) -> Disk {
    let mounted = |name: &str, mount: &str, size: u64| Region {
      name: name.to_string(),
      size,
      mountpoint: Some(mount.to_string()),
      fstype: Some("ext4".to_string()),
      total: Some(size),
      used: Some(size / 4),
    };
    Disk {
      name: name.to_string(),
      size: gib * GIB,
      model: "Model".to_string(),
      bus: "NVMe".to_string(),
      link: "PCIe Gen4 x4".to_string(),
      health: Health::Reading,
      regions: vec![
        mounted("p1", "/media/a/very/long/mount/point", gib * GIB / 2),
        Region {
          name: "p2".to_string(),
          size: 1 << 20,
          ..Region::default()
        },
        mounted("p3", "/b", gib * GIB / 2),
      ],
    }
  }

  /// Return folder entries, largest first.
  fn entries(count: u64) -> Vec<Entry> {
    (0..count)
      .map(|index| Entry {
        name: format!("item-{index}"),
        size: (count - index) * 1024 * 1024,
        kind: if index % 2 == 0 {
          EntryKind::Dir(index as u32)
        } else {
          EntryKind::File
        },
      })
      .collect()
  }

  #[test]
  fn sizes_follow_the_epydf_rules_on_the_disk_map() {
    assert_eq!(format_gib(256 * 1024), "0G");
    assert_eq!(format_gib(1073 * 1024 * 1024), "1G");
    assert_eq!(format_gib(257 * GIB + GIB / 4), "257G");
  }

  #[test]
  fn folder_sizes_use_the_largest_readable_unit() {
    assert_eq!(format_size(0), "0B");
    assert_eq!(format_size(1023), "1023B");
    assert_eq!(format_size(1536), "1.5K");
    assert_eq!(format_size(200 * 1024 * 1024), "200M");
    assert_eq!(format_size(3 * GIB / 2), "1.5G");
  }

  #[test]
  fn fit_text_keeps_width_and_marks_the_cut() {
    assert_eq!(fit_text("short", 10), "short");
    assert_eq!(fit_text("/media/EXTERNAL", 8).chars().count(), 8);
    assert!(fit_text("/media/EXTERNAL", 8).contains('~'));
    assert_eq!(fit_text("text", 0), "");
    assert_eq!(fit_text("text", 1).chars().count(), 1);
  }

  #[test]
  fn canvas_joins_shared_borders() {
    let mut canvas = Canvas::new(5, 3, false);
    canvas.draw_box(Rect {
      x: 0,
      y: 0,
      width: 3,
      height: 3,
    });
    canvas.draw_box(Rect {
      x: 2,
      y: 0,
      width: 3,
      height: 3,
    });
    assert_eq!(canvas.text(), "┌─┬─┐\n│ │ │\n└─┴─┘");
  }

  #[test]
  fn hostile_file_names_cannot_escape_their_cells() {
    let mut canvas = Canvas::new(6, 1, false);
    canvas.put(0, 0, "a\x1b[\n界", Style::default());
    assert_eq!(canvas.text(), "a?[?? ");
  }

  #[test]
  fn narrow_labels_stay_readable() {
    let label = |height, keep| label_lines("frameworks/", "3.1G  0.6%", None, 12, height, keep);
    assert_eq!(label(1, false), ["frameworks/"]);
    assert_eq!(label(2, false), ["frameworks/", "3.1G  0.6%"]);
    assert_eq!(label(2, true), ["frameworks/"]);
    assert_eq!(label(3, true), ["frameworks/", "3.1G  0.6%"]);
  }

  #[test]
  fn fill_matches_the_share_of_the_folder() {
    let mut canvas = Canvas::new(42, 8, true);
    let entry = Entry {
      name: "x".to_string(),
      size: 25,
      kind: EntryKind::File,
    };
    draw_entry(
      &mut canvas,
      &entry,
      Rect {
        x: 0,
        y: 0,
        width: 42,
        height: 8,
      },
      100,
      false,
    );
    let row: String = canvas.text().lines().nth(3).unwrap().to_string();
    assert_eq!(row.matches(FILL_CHAR).count(), 10);
    assert_eq!(row.matches(EMPTY_CHAR).count(), 30);
  }

  /// Return the background of one cell.
  fn background(canvas: &Canvas, x: usize, y: usize) -> Option<Color> {
    canvas.styles[y * canvas.width + x].bg
  }

  #[test]
  fn bar_is_painted_up_to_the_used_share_with_a_partial_edge() {
    let mut canvas = Canvas::new(22, 6, false);
    let region = Region {
      mountpoint: Some("/x".to_string()),
      total: Some(1000),
      used: Some(425),
      ..Region::default()
    };
    let rect = Rect {
      x: 0,
      y: 0,
      width: 22,
      height: 6,
    };
    draw_region(&mut canvas, &region, rect);
    assert_eq!(background(&canvas, 8, 3), Some(USED_LOW));
    assert_eq!(background(&canvas, 9, 3), Some(FREE_SPACE));
    assert_eq!(background(&canvas, 20, 3), Some(FREE_SPACE));
    assert_eq!(canvas.shown(3 * 22 + 9), EIGHTHS[4]);
    assert_eq!(canvas.shown(22 + 2), '/');
  }

  #[test]
  fn bar_colour_follows_how_full_a_filesystem_is() {
    assert_eq!(usage_color(0.34), USED_LOW);
    assert_eq!(usage_color(0.843), USED_HIGH);
    assert_eq!(usage_color(0.95), USED_FULL);
  }

  #[test]
  fn folder_boxes_are_tinted_darker_when_larger() {
    let entry = |size| Entry {
      name: "x".to_string(),
      size,
      kind: EntryKind::Dir(1),
    };
    let brightness = |size| match entry_paint(&entry(size), size as f64 / 100.0, false, false).1 {
      Fill::Tint(Color::Rgb(_, _, blue)) => blue,
      other => panic!("unexpected fill {other:?}"),
    };
    assert!(brightness(90) > brightness(4));
  }

  #[test]
  fn selection_is_a_heavy_frame_that_keeps_the_box_paint() {
    let mut canvas = Canvas::new(6, 4, false);
    let rect = Rect {
      x: 0,
      y: 0,
      width: 6,
      height: 4,
    };
    canvas.draw_box(rect);
    canvas.put(1, 1, "    ", Style::new().bg(USED_LOW));
    canvas.highlight(rect);
    assert_eq!(canvas.text().lines().next(), Some("┏━━━━┓"));
    assert_eq!(background(&canvas, 2, 1), Some(USED_LOW));
  }

  #[test]
  fn health_row_is_written_under_the_link_row() {
    let mut canvas = Canvas::new(60, 12, false);
    let mut fixture = disk("a", 1000);
    fixture.health = Health::Report(Report {
      wear_percent: Some(12),
      media_errors: Some(0),
      ..Report::default()
    });
    let area = Rect {
      x: 0,
      y: 0,
      width: 60,
      height: 12,
    };
    draw_disks(&mut canvas, &[fixture], area);
    assert!(
      canvas
        .text()
        .lines()
        .nth(3)
        .unwrap()
        .contains("Wear 12%  Errors 0")
    );
  }

  #[test]
  fn marked_boxes_are_green_and_selection_is_red() {
    let mut canvas = Canvas::new(40, 8, false);
    let list = entries(1);
    let marked = HashSet::from(["item-0".to_string()]);
    let area = Rect {
      x: 0,
      y: 0,
      width: 40,
      height: 8,
    };
    let (targets, _) = draw_folder(&mut canvas, &list, 1 << 20, area, &marked);
    assert_eq!(background(&canvas, 5, 4), Some(MARKED_TINT));
    canvas.highlight(targets[0].rect);
    assert_eq!(canvas.styles[0].fg, Some(SELECTION_FRAME));
  }

  #[test]
  fn dialog_covers_what_is_under_it_at_any_size() {
    for (width, height) in [(0, 0), (1, 1), (6, 3), (30, 9), (80, 24)] {
      let mut canvas = Canvas::new(width, height, false);
      canvas.draw_box(Rect {
        x: 0,
        y: 0,
        width,
        height,
      });
      let lines = [
        "Delete this very long named thing?".to_string(),
        "no".to_string(),
      ];
      draw_dialog(&mut canvas, &lines, true);
    }
    let mut canvas = Canvas::new(30, 9, false);
    draw_dialog(&mut canvas, &["hello".to_string()], false);
    assert!(canvas.text().contains("hello"));
  }

  #[test]
  fn only_mounted_regions_can_be_opened() {
    let mut canvas = Canvas::new(120, 30, false);
    let area = Rect {
      x: 0,
      y: 0,
      width: 120,
      height: 30,
    };
    let targets = draw_disks(&mut canvas, &[disk("a", 1000)], area);
    let keys: Vec<&str> = targets.iter().map(|target| target.key.as_str()).collect();
    assert_eq!(keys, ["/media/a/very/long/mount/point", "/b"]);
  }

  #[test]
  fn merged_box_holds_everything_that_did_not_fit() {
    let mut canvas = Canvas::new(40, 12, false);
    let list = entries(500);
    let total: u64 = list.iter().map(|entry| entry.size).sum();
    let area = Rect {
      x: 0,
      y: 0,
      width: 40,
      height: 12,
    };
    let (targets, merged) = draw_folder(&mut canvas, &list, total, area, &HashSet::new());
    let merged = merged.unwrap();
    assert_eq!(targets.last().unwrap().key, GROUP_KEY);
    let EntryKind::Group(count) = merged.kind else {
      panic!("not a group");
    };
    assert_eq!(count as usize, list.len() - (targets.len() - 1));
  }

  #[test]
  fn every_terminal_size_draws_without_panicking() {
    let disks = [disk("a", 2000), disk("b", 1000), disk("c", 500)];
    for (width, height) in [
      (0, 0),
      (1, 1),
      (2, 2),
      (5, 3),
      (20, 5),
      (80, 24),
      (199, 49),
      (300, 90),
    ] {
      let area = Rect {
        x: 0,
        y: 0,
        width,
        height,
      };
      for ascii in [false, true] {
        let mut canvas = Canvas::new(width, height, ascii);
        let targets = draw_disks(&mut canvas, &disks, area);
        if let Some(target) = targets.first() {
          canvas.highlight(target.rect);
        }
        let mut canvas = Canvas::new(width, height, ascii);
        let (targets, _) = draw_folder(&mut canvas, &entries(300), 1 << 40, area, &HashSet::new());
        if let Some(target) = targets.first() {
          canvas.highlight(target.rect);
        }
      }
    }
  }
}
