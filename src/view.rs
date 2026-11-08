//! Drawing of the disk map and of the folder treemap on a character canvas.

use crate::disks::{Disk, Region, SWAP_MOUNT, UNALLOCATED};
use crate::layout::{Rect, fit_treemap, layout_disks, stack_regions};
use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier, Style};
use unicode_width::UnicodeWidthChar;

const GIB: f64 = (1_u64 << 30) as f64;
const FILL_CHAR: char = '#';
const EMPTY_CHAR: char = '.';

const DISK_HEADER_ROWS: usize = 2;
const MIN_DISK_WIDTH: usize = 34;
const MIN_DISK_HEIGHT: usize = 7;
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
  cells: Vec<char>,
  borders: Vec<u8>,
  styles: Vec<Style>,
}

impl Canvas {
  /// Create an empty canvas.
  pub fn new(width: usize, height: usize) -> Self {
    let count = width * height;
    Self {
      width,
      height,
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
    let border = Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD);
    let inside = Style::new().fg(Color::Black).bg(Color::Yellow);
    for y in rect.y..(rect.y + rect.height).min(self.height) {
      for x in rect.x..(rect.x + rect.width).min(self.width) {
        let edge = y == rect.y
          || x == rect.x
          || y + 1 == rect.y + rect.height
          || x + 1 == rect.x + rect.width;
        self.styles[y * self.width + x] = if edge { border } else { inside };
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

/// Draw a box with its label and, when share is known, a proportional fill.
fn draw_labelled(
  canvas: &mut Canvas,
  rect: Rect,
  (title, detail, short_detail): (&str, &str, Option<String>),
  share: Option<f64>,
  keep_fill_row: bool,
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
    keep_fill_row,
  );
  for (index, line) in lines.iter().enumerate() {
    canvas.put(
      rect.x + 2,
      rect.y + 1 + index,
      &fit_text(line, text_width),
      style,
    );
  }
  let Some(share) = share else {
    return;
  };
  let filled = ((inner_width as f64 * share).round() as usize).min(inner_width);
  let fill: String = std::iter::repeat_n(FILL_CHAR, filled)
    .chain(std::iter::repeat_n(EMPTY_CHAR, inner_width - filled))
    .collect();
  for row in lines.len()..inner_height {
    canvas.put(rect.x + 1, rect.y + 1 + row, &fill, Style::default());
  }
}

/// Draw a region box with its label and its usage fill.
fn draw_region(canvas: &mut Canvas, region: &Region, rect: Rect) {
  let (title, detail) = region_text(region);
  let short = region.percent().map(|percent| format!("{percent:.1}%"));
  let share = region.percent().map(|percent| percent / 100.0);
  draw_labelled(
    canvas,
    rect,
    (&title, &detail, short),
    share,
    true,
    Style::default(),
  );
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

/// Draw one treemap box and return its selection target.
fn draw_entry(canvas: &mut Canvas, entry: &Entry, rect: Rect, total: u64) -> Target {
  let share = entry.size as f64 / total.max(1) as f64;
  let percent = format!("{:.1}%", share * 100.0);
  let detail = format!("{}  {percent}", format_size(entry.size));
  let (title, style, key) = match entry.kind {
    EntryKind::Dir(_) => (
      format!("{}/", entry.name),
      Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
      entry.name.clone(),
    ),
    EntryKind::File => (entry.name.clone(), Style::default(), entry.name.clone()),
    EntryKind::Group(count) => (
      format!("({count} more)"),
      Style::new().fg(Color::DarkGray),
      GROUP_KEY.to_string(),
    ),
  };
  draw_labelled(
    canvas,
    rect,
    (&title, &detail, Some(percent)),
    Some(share),
    false,
    style,
  );
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
    .map(|(entry, &rect)| draw_entry(canvas, entry, rect, total))
    .collect();
  let merged = boxes.get(kept).map(|&rect| {
    let count = (named.len() - kept) as u64 + groups.iter().map(group_count).sum::<u64>();
    let size = weights[kept..].iter().sum::<u64>() + extra;
    let entry = Entry {
      name: GROUP_KEY.to_string(),
      size,
      kind: EntryKind::Group(count),
    };
    targets.push(draw_entry(canvas, &entry, rect, total));
    entry
  });
  (targets, merged)
}

#[cfg(test)]
mod tests {
  use super::*;

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
    let mut canvas = Canvas::new(5, 3);
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
    let mut canvas = Canvas::new(6, 1);
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
    let mut canvas = Canvas::new(42, 8);
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
    );
    let row: String = canvas.text().lines().nth(3).unwrap().to_string();
    assert_eq!(row.matches(FILL_CHAR).count(), 10);
    assert_eq!(row.matches(EMPTY_CHAR).count(), 30);
  }

  #[test]
  fn only_mounted_regions_can_be_opened() {
    let mut canvas = Canvas::new(120, 30);
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
    let mut canvas = Canvas::new(40, 12);
    let list = entries(500);
    let total: u64 = list.iter().map(|entry| entry.size).sum();
    let area = Rect {
      x: 0,
      y: 0,
      width: 40,
      height: 12,
    };
    let (targets, merged) = draw_folder(&mut canvas, &list, total, area);
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
      let mut canvas = Canvas::new(width, height);
      draw_disks(&mut canvas, &disks, area);
      let mut canvas = Canvas::new(width, height);
      draw_folder(&mut canvas, &entries(300), 1 << 40, area);
    }
  }
}
