//! Interface state: where the user is, what is selected and how keys change it.

use crate::disks::Disk;
use crate::layout::{Direction, Rect, neighbour};
use crate::scan::{ItemKind, ROOT, Scan};
use crate::view::{
  Canvas, Entry, EntryKind, GROUP_KEY, Target, draw_disks, draw_folder, fit_text, format_size,
};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::style::{Modifier, Style};
use ratatui::{DefaultTerminal, Frame};
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const TICK: Duration = Duration::from_millis(33);
const DRAIN_BUDGET: Duration = Duration::from_millis(25);
const SCAN_REDRAW: Duration = Duration::from_millis(100);
const KEY_HELP: &str = "arrows/hjkl move  Enter open  Backspace back  q quit";

/// A folder the user has opened below the scan root.
struct Opened {
  node: u32,
  name: String,
}

/// The whole interface state.
pub struct App {
  disks: Vec<Disk>,
  threads: Option<usize>,
  scans: HashMap<PathBuf, Scan>,
  /// The scan on screen; None means the disk map.
  current: Option<PathBuf>,
  trail: Vec<Opened>,
  selected: Option<String>,
  /// False while the selection simply follows the largest box of a growing folder.
  pinned: bool,
  targets: Vec<Target>,
  entries: Vec<Entry>,
  error: Option<String>,
  redraw: bool,
  quit: bool,
}

/// Convert the children of a directory to screen entries, largest first.
fn folder_entries(scan: &Scan, node: u32) -> Vec<Entry> {
  let tree = &scan.tree;
  let mut entries: Vec<Entry> = tree.nodes[node as usize]
    .children
    .iter()
    .map(|item| Entry {
      name: item.name.to_string(),
      size: tree.item_size(item),
      kind: match item.kind {
        ItemKind::File => EntryKind::File,
        ItemKind::Dir(child) => EntryKind::Dir(child),
        ItemKind::Group(count) => EntryKind::Group(count as u64),
      },
    })
    .collect();
  entries.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.name.cmp(&b.name)));
  entries
}

impl App {
  /// Create the interface on the disk map.
  pub fn on_disks(disks: Vec<Disk>, threads: Option<usize>) -> Self {
    Self {
      disks,
      threads,
      scans: HashMap::new(),
      current: None,
      trail: Vec::new(),
      selected: None,
      pinned: false,
      targets: Vec::new(),
      entries: Vec::new(),
      error: None,
      redraw: true,
      quit: false,
    }
  }

  /// Create the interface directly on one folder, without a disk map to go back to.
  pub fn on_folder(root: &Path, threads: Option<usize>) -> io::Result<Self> {
    let mut app = Self::on_disks(Vec::new(), threads);
    let scan = Scan::start(root, threads)?;
    let key = scan.root.clone();
    app.scans.insert(key.clone(), scan);
    app.current = Some(key);
    Ok(app)
  }

  /// Return the scan on screen.
  fn scan(&self) -> Option<&Scan> {
    self.scans.get(self.current.as_ref()?)
  }

  /// Return the directory on screen.
  fn node(&self) -> u32 {
    self.trail.last().map_or(ROOT, |opened| opened.node)
  }

  /// Open a partition; only one unfinished scan runs at a time, finished ones are kept.
  fn open_mount(&mut self, mount: &str) {
    let path = PathBuf::from(mount);
    self
      .scans
      .retain(|root, scan| scan.finished.is_some() || *root == path);
    if !self.scans.contains_key(&path) {
      match Scan::start(&path, self.threads) {
        Ok(mut scan) => {
          // the scan knows its real path, but the map addresses it by mount point
          scan.root = path.clone();
          self.scans.insert(path.clone(), scan);
        }
        Err(error) => {
          self.error = Some(format!("{mount}: {error}"));
          return;
        }
      }
    }
    self.error = None;
    self.current = Some(path);
    self.trail.clear();
    self.selected = None;
    self.pinned = false;
  }

  /// Open what is selected, if it can be opened.
  fn open_selected(&mut self) {
    let Some(key) = self.selected.clone() else {
      return;
    };
    if self.current.is_none() {
      self.open_mount(&key);
      return;
    }
    let child = self
      .entries
      .iter()
      .find(|entry| entry.name == key)
      .map(|entry| entry.kind);
    if let Some(EntryKind::Dir(node)) = child {
      self.trail.push(Opened { node, name: key });
      self.selected = None;
      self.pinned = false;
    }
  }

  /// Go one level up, ending on the disk map when the program started there.
  fn go_back(&mut self) {
    self.pinned = true;
    if let Some(opened) = self.trail.pop() {
      self.selected = Some(opened.name);
    } else if !self.disks.is_empty()
      && let Some(root) = self.current.take()
    {
      self.selected = Some(root.to_string_lossy().into_owned());
    }
  }

  /// Move the selection to the box next to it.
  fn move_selection(&mut self, direction: Direction) {
    let rects: Vec<Rect> = self.targets.iter().map(|target| target.rect).collect();
    let from = self
      .targets
      .iter()
      .position(|target| Some(&target.key) == self.selected.as_ref());
    if let Some(next) = from.and_then(|from| neighbour(&rects, from, direction)) {
      self.selected = Some(self.targets[next].key.clone());
      self.pinned = true;
    }
  }

  /// Apply one key press.
  fn press(&mut self, code: KeyCode, modifiers: KeyModifiers) {
    match code {
      KeyCode::Char('q') => self.quit = true,
      KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => self.quit = true,
      KeyCode::Left | KeyCode::Char('h') => self.move_selection(Direction::Left),
      KeyCode::Right | KeyCode::Char('l') => self.move_selection(Direction::Right),
      KeyCode::Up | KeyCode::Char('k') => self.move_selection(Direction::Up),
      KeyCode::Down | KeyCode::Char('j') => self.move_selection(Direction::Down),
      KeyCode::Enter => self.open_selected(),
      KeyCode::Backspace | KeyCode::Esc => self.go_back(),
      _ => return,
    }
    self.redraw = true;
  }

  /// Return the scan progress shown at the top right.
  fn scan_status(&self, scan: &Scan) -> String {
    let tree = &scan.tree;
    let done = format_size(tree.nodes[ROOT as usize].total);
    let seconds = scan.elapsed().as_secs_f64();
    let mut status = match scan.finished {
      Some(_) => format!("{done}  {} files  scanned in {seconds:.1}s", tree.files),
      None => {
        let root = scan.root.to_string_lossy();
        let expected = self
          .disks
          .iter()
          .flat_map(|disk| &disk.regions)
          .find(|region| region.mountpoint.as_deref() == Some(&root))
          .and_then(|region| region.used);
        let progress = expected.map_or(String::new(), |used| {
          let percent = 100.0 * tree.nodes[ROOT as usize].total as f64 / used.max(1) as f64;
          format!(" of {} ({:.0}%)", format_size(used), percent.min(100.0))
        });
        format!(
          "scanning {done}{progress}  {} files  {seconds:.0}s",
          tree.files
        )
      }
    };
    if tree.denied > 0 {
      status.push_str(&format!("  {} unreadable", tree.denied));
    }
    status
  }

  /// Return the text describing the selected box.
  fn selection_text(&self, merged: Option<&Entry>, total: u64) -> String {
    let Some(key) = self.selected.as_deref() else {
      return String::new();
    };
    if self.current.is_none() {
      return format!("{key}  Enter scans this partition");
    }
    let named = self.entries.iter().find(|entry| entry.name == key);
    let Some(entry) = (if key == GROUP_KEY { merged } else { named }) else {
      return String::new();
    };
    let share = 100.0 * entry.size as f64 / total.max(1) as f64;
    let size = format_size(entry.size);
    match entry.kind {
      EntryKind::Group(count) => format!("{count} small items  {size}  {share:.1}%"),
      EntryKind::Dir(_) => format!("{}/  {size}  {share:.1}%", entry.name),
      EntryKind::File => format!("{}  {size}  {share:.1}%", entry.name),
    }
  }

  /// Draw the current screen on a canvas and remember its boxes for the cursor keys.
  fn compose(&mut self, canvas: &mut Canvas) {
    let area = Rect {
      x: 0,
      y: 1,
      width: canvas.width,
      height: canvas.height.saturating_sub(2),
    };
    let mut merged = None;
    let mut total = 0;
    let (title, status);
    if let Some(scan) = self.scan() {
      let node = self.node();
      total = scan.tree.nodes[node as usize].total;
      self.entries = folder_entries(scan, node);
      let scan = self.scan().expect("the scan was just found");
      let unfinished = scan.finished.is_none();
      let names: Vec<&str> = self
        .trail
        .iter()
        .map(|opened| opened.name.as_str())
        .collect();
      let path = names
        .iter()
        .fold(scan.root.clone(), |path, name| path.join(name));
      title = format!("{}  {}", path.display(), format_size(total));
      status = self.scan_status(scan);
      let (targets, group) = draw_folder(canvas, &self.entries, total, area);
      self.targets = targets;
      merged = group;
      if self.entries.is_empty() {
        let scanning = unfinished && self.trail.is_empty();
        let text = if scanning { "scanning..." } else { "(empty)" };
        canvas.put(2, 2, text, Style::default());
      }
    } else {
      self.targets = draw_disks(canvas, &self.disks, area);
      title = "storage-analyzer  choose a partition".to_string();
      status = self.error.clone().unwrap_or_default();
    }
    self.resolve_selection();
    if let Some(target) = self
      .targets
      .iter()
      .find(|target| Some(&target.key) == self.selected.as_ref())
    {
      canvas.highlight(target.rect);
    }
    let width = canvas.width;
    let bold = Style::new().add_modifier(Modifier::BOLD);
    canvas.put(0, 0, &fit_text(&title, width), bold);
    let status_width = status.chars().count();
    if title.chars().count() + status_width + 2 <= width {
      canvas.put(width - status_width, 0, &status, Style::default());
    }
    let footer = canvas.height.saturating_sub(1);
    let help_width = KEY_HELP.chars().count();
    let selection = self.selection_text(merged.as_ref(), total);
    if width > help_width + 2 {
      canvas.put(width - help_width, footer, KEY_HELP, Style::default());
      canvas.put(
        0,
        footer,
        &fit_text(&selection, width - help_width - 2),
        bold,
      );
    } else {
      canvas.put(0, footer, &fit_text(&selection, width), bold);
    }
  }

  /// Keep the selection on an existing box, falling back to the first one.
  fn resolve_selection(&mut self) {
    let exists = self
      .targets
      .iter()
      .any(|target| Some(&target.key) == self.selected.as_ref());
    if !exists || !self.pinned {
      self.selected = self.targets.first().map(|target| target.key.clone());
    }
  }

  /// Draw one frame.
  fn draw(&mut self, frame: &mut Frame) {
    let area = frame.area();
    let mut canvas = Canvas::new(area.width as usize, area.height as usize);
    self.compose(&mut canvas);
    canvas.blit(frame.buffer_mut());
  }

  /// Run until the user quits.
  pub fn run(&mut self, terminal: &mut DefaultTerminal) -> io::Result<()> {
    let mut last_draw = Instant::now() - SCAN_REDRAW;
    let mut stale = false;
    while !self.quit {
      for scan in self.scans.values_mut() {
        stale |= scan.drain(DRAIN_BUDGET);
      }
      if self.redraw || (stale && last_draw.elapsed() >= SCAN_REDRAW) {
        terminal.draw(|frame| self.draw(frame))?;
        self.redraw = false;
        stale = false;
        last_draw = Instant::now();
      }
      if event::poll(TICK)? {
        match event::read()? {
          Event::Key(key) if key.kind != KeyEventKind::Release => {
            self.press(key.code, key.modifiers);
          }
          Event::Resize(..) => self.redraw = true,
          _ => {}
        }
      }
    }
    Ok(())
  }
}
