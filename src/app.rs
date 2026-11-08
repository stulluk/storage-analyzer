//! Interface state: where the user is, what is selected and how keys change it.

use crate::actions::{
  change_refusal, delete_in_background, is_valid_folder_name, move_without_replacing,
};
use crate::disks::{Disk, current_mount_points, refresh_usage};
use crate::layout::{Direction, Rect, neighbour};
use crate::scan::{Item, ItemKind, ROOT, Scan, default_threads, disk_bytes};
use crate::smart::{Health, read_in_background};
use crate::view::{
  Canvas, Entry, EntryKind, GROUP_KEY, Target, draw_dialog, draw_disks, draw_folder, fit_text,
  format_size,
};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::style::{Color, Modifier, Style};
use ratatui::{DefaultTerminal, Frame};
use rayon::ThreadPool;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

const TICK: Duration = Duration::from_millis(33);
const DRAIN_BUDGET: Duration = Duration::from_millis(25);
const SCAN_REDRAW: Duration = Duration::from_millis(100);
const STATUS_TIME: Duration = Duration::from_secs(5);
const HELP_MAP: &str = "arrows/hjkl move  Enter open  q quit";
const HELP_FOLDER: &str =
  "arrows move  Enter open  Esc back  d delete  m mkdir  s mark  c move  +/-/0 zoom  q quit";
const HELP_FOLDER_SHORT: &str = "d del  m mkdir  s mark  c move  +/-/0 zoom  q quit";
/// Room the selected item's description needs before the long help is worth showing.
const MIN_SELECTION_WIDTH: usize = 40;
const MIN_TITLE_WIDTH: usize = 20;
const DELETE_CHOICES: [&str; 3] = ["no", "yes", "don't ask me again"];
const MOVE_DIALOG_ROWS: usize = 10;

/// A folder the user has opened below the scan root.
struct Opened {
  node: u32,
  name: String,
  /// The zoom of the folder above, restored when coming back.
  zoom: usize,
}

/// An item waiting to be deleted.
#[derive(Clone)]
struct Deletion {
  path: PathBuf,
  name: String,
  is_dir: bool,
  size: u64,
  node: u32,
  rect: Rect,
}

/// A deletion running in the background; its item is already gone from the map.
struct Job {
  receiver: Receiver<io::Result<()>>,
  root: PathBuf,
  deletion: Deletion,
  taken: Option<(Item, u64)>,
  removed: Arc<AtomicU64>,
  started: Instant,
}

/// A window on top of the map that takes the keyboard.
enum Dialog {
  Delete { target: Deletion, choice: usize },
  Mkdir { input: String },
  Move { folders: Vec<String>, index: usize },
}

/// A short message at the bottom of the screen.
struct Status {
  text: String,
  error: bool,
  shown_at: Instant,
}

/// The whole interface state.
pub struct App {
  disks: Vec<Disk>,
  health_updates: Receiver<(String, Health)>,
  threads: Option<usize>,
  ascii: bool,
  scans: HashMap<PathBuf, Scan>,
  /// The scan on screen; None means the disk map.
  current: Option<PathBuf>,
  trail: Vec<Opened>,
  selected: Option<String>,
  /// False while the selection simply follows the largest box of a growing folder.
  pinned: bool,
  /// Where a deleted box was; the box nearest to it becomes the selection.
  anchor: Option<Rect>,
  targets: Vec<Target>,
  entries: Vec<Entry>,
  /// How many of the largest items of this folder are hidden to make room for the others.
  zoom: usize,
  /// Names marked in the folder on screen.
  marked: HashSet<String>,
  dialog: Option<Dialog>,
  /// Chosen in the delete prompt; lasts until the program ends.
  skip_delete_prompt: bool,
  jobs: Vec<Job>,
  /// Deletions share one pool per thread count, so that several never oversubscribe the cores.
  delete_pools: HashMap<usize, Arc<ThreadPool>>,
  /// When the user was warned that quitting abandons a running deletion.
  quit_warned: Option<Instant>,
  status: Option<Status>,
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

/// Return the entries left after hiding the largest named ones.
fn zoomed_entries(entries: &[Entry], zoom: usize) -> Vec<Entry> {
  let mut hidden = 0;
  entries
    .iter()
    .filter(|entry| {
      let hide = !matches!(entry.kind, EntryKind::Group(_)) && hidden < zoom;
      hidden += hide as usize;
      !hide
    })
    .cloned()
    .collect()
}

/// Return the first index of a window of rows that keeps index visible.
fn window_start(index: usize, count: usize, rows: usize) -> usize {
  index
    .saturating_sub(rows / 2)
    .min(count.saturating_sub(rows))
}

impl App {
  /// Create the interface on the disk map.
  pub fn on_disks(disks: Vec<Disk>, threads: Option<usize>, ascii: bool) -> Self {
    let names = disks
      .iter()
      .map(|disk| (disk.name.clone(), disk.model.clone()))
      .collect();
    Self {
      disks,
      health_updates: read_in_background(names),
      threads,
      ascii,
      scans: HashMap::new(),
      current: None,
      trail: Vec::new(),
      selected: None,
      pinned: false,
      anchor: None,
      targets: Vec::new(),
      entries: Vec::new(),
      zoom: 0,
      marked: HashSet::new(),
      dialog: None,
      skip_delete_prompt: false,
      jobs: Vec::new(),
      delete_pools: HashMap::new(),
      quit_warned: None,
      status: None,
      error: None,
      redraw: true,
      quit: false,
    }
  }

  /// Create the interface directly on one folder, without a disk map to go back to.
  pub fn on_folder(root: &Path, threads: Option<usize>, ascii: bool) -> io::Result<Self> {
    let mut app = Self::on_disks(Vec::new(), threads, ascii);
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

  /// Return the path of the folder on screen.
  fn folder_path(&self) -> Option<PathBuf> {
    let root = self.scan()?.root.clone();
    Some(
      self
        .trail
        .iter()
        .fold(root, |path, opened| path.join(&opened.name)),
    )
  }

  /// Show a short message at the bottom of the screen.
  fn set_status(&mut self, text: String, error: bool) {
    self.status = Some(Status {
      text,
      error,
      shown_at: Instant::now(),
    });
    self.redraw = true;
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
    self.zoom = 0;
    self.marked.clear();
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
      self.trail.push(Opened {
        node,
        name: key,
        zoom: self.zoom,
      });
      self.selected = None;
      self.pinned = false;
      self.zoom = 0;
      self.marked.clear();
    }
  }

  /// Go one level up, ending on the disk map when the program started there.
  fn go_back(&mut self) {
    self.pinned = true;
    self.marked.clear();
    if let Some(opened) = self.trail.pop() {
      self.selected = Some(opened.name);
      self.zoom = opened.zoom;
    } else if !self.disks.is_empty()
      && let Some(root) = self.current.take()
    {
      self.selected = Some(root.to_string_lossy().into_owned());
      self.zoom = 0;
      refresh_usage(&mut self.disks);
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

  /// Return the number of items that zoom can hide one by one.
  fn named_count(&self) -> usize {
    self
      .entries
      .iter()
      .filter(|entry| !matches!(entry.kind, EntryKind::Group(_)))
      .count()
  }

  /// Hide one more of the largest items, as long as one stays on screen.
  fn zoom_in(&mut self) {
    if self.current.is_some() && self.zoom + 1 < self.named_count() {
      self.zoom += 1;
    }
  }

  /// Return whether the disk can be changed now, saying why when it cannot.
  fn ready_to_change(&mut self) -> bool {
    let Some(scan) = self.scan() else {
      return false;
    };
    if scan.finished.is_none() {
      self.set_status("wait for the scan to finish".to_string(), true);
      return false;
    }
    true
  }

  /// Return the selected entry, unless it is the box of merged small items.
  fn selected_entry(&self) -> Option<&Entry> {
    let key = self.selected.as_deref().filter(|&key| key != GROUP_KEY)?;
    self.entries.iter().find(|entry| entry.name == key)
  }

  /// Return why an entry of the folder on screen must not be deleted or moved.
  fn refusal(&self, entry: &Entry, path: &Path) -> Option<String> {
    let is_dir = matches!(entry.kind, EntryKind::Dir(_));
    let scan = self.scan()?;
    if let EntryKind::Dir(node) = entry.kind
      && scan.tree.contains_other_filesystem(node)
    {
      return Some("another filesystem is inside it".to_string());
    }
    change_refusal(path, is_dir, &current_mount_points())
  }

  /// Start the delete flow for the selected item.
  fn prompt_delete(&mut self) {
    if !self.ready_to_change() {
      return;
    }
    let Some(entry) = self.selected_entry().cloned() else {
      if self.selected.as_deref() == Some(GROUP_KEY) {
        self.set_status("zoom in with + to reach these items".to_string(), true);
      }
      return;
    };
    let Some(path) = self.folder_path().map(|folder| folder.join(&entry.name)) else {
      return;
    };
    if let Some(reason) = self.refusal(&entry, &path) {
      self.set_status(format!("cannot delete: {reason}"), true);
      return;
    }
    let rect = self
      .targets
      .iter()
      .find(|target| target.key == entry.name)
      .map_or(
        Rect {
          x: 0,
          y: 0,
          width: 0,
          height: 0,
        },
        |target| target.rect,
      );
    let target = Deletion {
      path,
      name: entry.name,
      is_dir: matches!(entry.kind, EntryKind::Dir(_)),
      size: entry.size,
      node: self.node(),
      rect,
    };
    if self.skip_delete_prompt {
      self.start_deletion(target);
    } else {
      self.dialog = Some(Dialog::Delete { target, choice: 0 });
    }
  }

  /// Return the thread pool for deleting on the device that holds a path.
  fn delete_pool(&mut self, path: &Path) -> io::Result<Arc<ThreadPool>> {
    let device = fs::symlink_metadata(path)?.dev();
    let threads = self.threads.unwrap_or_else(|| default_threads(device));
    if let Some(pool) = self.delete_pools.get(&threads) {
      return Ok(Arc::clone(pool));
    }
    let pool = rayon::ThreadPoolBuilder::new()
      .num_threads(threads)
      .build()
      .map(Arc::new)
      .map_err(io::Error::other)?;
    self.delete_pools.insert(threads, Arc::clone(&pool));
    Ok(pool)
  }

  /// Take the item off the map at once and delete it in the background.
  fn start_deletion(&mut self, deletion: Deletion) {
    let Some(root) = self.current.clone() else {
      return;
    };
    let pool = match self.delete_pool(&deletion.path) {
      Ok(pool) => pool,
      Err(error) => {
        self.set_status(format!("{}: {error}", deletion.name), true);
        return;
      }
    };
    let taken = self
      .scans
      .get_mut(&root)
      .and_then(|scan| scan.tree.take_item(deletion.node, &deletion.name));
    self.anchor = Some(deletion.rect);
    self.marked.remove(&deletion.name);
    self.set_status(
      format!("deleting {} in the background", deletion.name),
      false,
    );
    let removed = Arc::new(AtomicU64::new(0));
    self.jobs.push(Job {
      receiver: delete_in_background(
        deletion.path.clone(),
        deletion.is_dir,
        pool,
        Arc::clone(&removed),
      ),
      root,
      deletion,
      taken,
      removed,
      started: Instant::now(),
    });
  }

  /// Handle the deletions that have ended.
  fn finish_jobs(&mut self) {
    let mut index = 0;
    while index < self.jobs.len() {
      let result = match self.jobs[index].receiver.try_recv() {
        Ok(result) => result,
        Err(TryRecvError::Empty) => {
          index += 1;
          continue;
        }
        Err(TryRecvError::Disconnected) => Err(io::Error::other("the deletion stopped")),
      };
      let job = self.jobs.swap_remove(index);
      self.conclude_job(job, result);
    }
  }

  /// Report a finished deletion, putting the item back on the map if it failed.
  fn conclude_job(&mut self, job: Job, result: io::Result<()>) {
    let name = &job.deletion.name;
    let seconds = job.started.elapsed().as_secs_f64();
    let scan = self.scans.get_mut(&job.root);
    match (result, scan, job.taken) {
      (Err(error), scan, taken) => {
        if let (Some(scan), Some((item, size))) = (scan, taken) {
          scan.tree.restore_item(job.deletion.node, item, size);
        }
        self.set_status(format!("{name}: {error}; part of it may be gone"), true);
      }
      (Ok(()), scan, taken) => {
        if let (Some(scan), Some((item, _))) = (scan, taken) {
          scan.tree.forget_item(&item);
        }
        let freed = format_size(job.deletion.size);
        self.set_status(
          format!("deleted {name}, freed {freed} in {seconds:.1}s"),
          false,
        );
      }
    }
    refresh_usage(&mut self.disks);
  }

  /// Quit, asking for a second press while a deletion would be abandoned.
  fn request_quit(&mut self) {
    let warned = self
      .quit_warned
      .is_some_and(|warned| warned.elapsed() < STATUS_TIME);
    if self.jobs.is_empty() || warned {
      self.quit = true;
    } else {
      self.quit_warned = Some(Instant::now());
      let text = "a deletion is still running; press again to abandon it".to_string();
      self.set_status(text, true);
    }
  }

  /// Open the prompt for a new folder.
  fn prompt_mkdir(&mut self) {
    if self.ready_to_change() {
      self.dialog = Some(Dialog::Mkdir {
        input: String::new(),
      });
    }
  }

  /// Create the folder named in the prompt.
  fn submit_mkdir(&mut self, input: &str) {
    let name = input.trim();
    if !is_valid_folder_name(name) {
      self.set_status("that is not a valid folder name".to_string(), true);
      return;
    }
    let Some(path) = self.folder_path().map(|folder| folder.join(name)) else {
      return;
    };
    match fs::create_dir(&path) {
      Ok(()) => {
        let node = self.node();
        let size = fs::symlink_metadata(&path).map_or(0, |metadata| disk_bytes(&metadata));
        if let Some(scan) = self
          .current
          .as_ref()
          .and_then(|root| self.scans.get_mut(root))
        {
          scan.tree.add_directory(node, name, size);
        }
        self.selected = Some(name.to_string());
        self.pinned = true;
        self.set_status(format!("{name} created"), false);
      }
      Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
        self.set_status(format!("{name} already exists"), true);
      }
      Err(error) => self.set_status(format!("{name}: {error}"), true),
    }
  }

  /// Mark or unmark the selected item.
  fn toggle_mark(&mut self) {
    if self.current.is_none() {
      return;
    }
    if let Some(name) = self.selected_entry().map(|entry| entry.name.clone())
      && !self.marked.remove(&name)
    {
      self.marked.insert(name);
    }
  }

  /// Open the list of folders the marked items can be moved into.
  fn prompt_move(&mut self) {
    if self.current.is_none() {
      return;
    }
    if self.marked.is_empty() {
      self.set_status("mark items with s first".to_string(), true);
      return;
    }
    if !self.ready_to_change() {
      return;
    }
    let mut folders: Vec<String> = self
      .entries
      .iter()
      .filter(|entry| matches!(entry.kind, EntryKind::Dir(_)) && !self.marked.contains(&entry.name))
      .map(|entry| entry.name.clone())
      .collect();
    folders.sort();
    if folders.is_empty() {
      self.set_status(
        "there is no folder here to move them into".to_string(),
        true,
      );
    } else {
      self.dialog = Some(Dialog::Move { folders, index: 0 });
    }
  }

  /// Move the marked items into one folder of the folder on screen.
  fn confirm_move(&mut self, target: &str) {
    let destination = self.entries.iter().find_map(|entry| match entry.kind {
      EntryKind::Dir(node) if entry.name == target => Some(node),
      _ => None,
    });
    let (Some(destination), Some(folder)) = (destination, self.folder_path()) else {
      return;
    };
    let mut names: Vec<String> = self.marked.iter().cloned().collect();
    names.sort();
    let node = self.node();
    let mut moved = 0;
    let mut problems = Vec::new();
    for name in names {
      let source = folder.join(&name);
      let refusal = self
        .entries
        .iter()
        .find(|entry| entry.name == name)
        .and_then(|entry| self.refusal(entry, &source));
      let result = match refusal {
        Some(reason) => Err(reason),
        None => move_without_replacing(&source, &folder.join(target).join(&name))
          .map_err(|error| error.to_string()),
      };
      match result {
        Ok(()) => {
          moved += 1;
          self.marked.remove(&name);
          if let Some(scan) = self
            .current
            .as_ref()
            .and_then(|root| self.scans.get_mut(root))
          {
            scan.tree.move_item(node, destination, &name);
          }
        }
        Err(reason) => problems.push(format!("{name}: {reason}")),
      }
    }
    self.selected = Some(target.to_string());
    self.pinned = true;
    match problems.first() {
      None => self.set_status(format!("moved {moved} into {target}"), false),
      Some(first) => self.set_status(
        format!("moved {moved}, {} failed; {first}", problems.len()),
        true,
      ),
    }
  }

  /// Apply one key press to the open dialog.
  fn press_dialog(&mut self, code: KeyCode, modifiers: KeyModifiers) {
    let control = modifiers.contains(KeyModifiers::CONTROL);
    let Some(mut dialog) = self.dialog.take() else {
      return;
    };
    let mut keep = true;
    match &mut dialog {
      Dialog::Delete { target, choice } => match code {
        KeyCode::Char('q') => self.request_quit(),
        KeyCode::Esc => keep = false,
        KeyCode::Char('c') if control => keep = false,
        KeyCode::Left | KeyCode::Char('h') => {
          *choice = (*choice + DELETE_CHOICES.len() - 1) % DELETE_CHOICES.len();
        }
        KeyCode::Right | KeyCode::Char('l') => *choice = (*choice + 1) % DELETE_CHOICES.len(),
        KeyCode::Enter => {
          keep = false;
          if *choice > 0 {
            self.skip_delete_prompt |= *choice == 2;
            self.start_deletion(target.clone());
          }
        }
        _ => {}
      },
      Dialog::Mkdir { input } => match code {
        KeyCode::Char('c') if control => self.request_quit(),
        KeyCode::Esc => keep = false,
        KeyCode::Backspace => {
          input.pop();
        }
        KeyCode::Enter => {
          keep = false;
          self.submit_mkdir(input);
        }
        KeyCode::Char(c)
          if !control && !modifiers.contains(KeyModifiers::ALT) && c != '/' && !c.is_control() =>
        {
          input.push(c);
        }
        _ => {}
      },
      Dialog::Move { folders, index } => match code {
        KeyCode::Char('q') => self.request_quit(),
        KeyCode::Esc => keep = false,
        KeyCode::Left | KeyCode::Up | KeyCode::Char('h') | KeyCode::Char('k') => {
          *index = (*index + folders.len() - 1) % folders.len();
        }
        KeyCode::Right | KeyCode::Down | KeyCode::Char('l') | KeyCode::Char('j') => {
          *index = (*index + 1) % folders.len();
        }
        KeyCode::Enter => {
          keep = false;
          self.confirm_move(&folders[*index]);
        }
        _ => {}
      },
    }
    if keep {
      self.dialog = Some(dialog);
    }
  }

  /// Apply one key press.
  fn press(&mut self, code: KeyCode, modifiers: KeyModifiers) {
    self.redraw = true;
    self.status = None;
    if self.dialog.is_some() {
      self.press_dialog(code, modifiers);
      return;
    }
    match code {
      KeyCode::Char('q') => self.request_quit(),
      KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => self.request_quit(),
      KeyCode::Left | KeyCode::Char('h') => self.move_selection(Direction::Left),
      KeyCode::Right | KeyCode::Char('l') => self.move_selection(Direction::Right),
      KeyCode::Up | KeyCode::Char('k') => self.move_selection(Direction::Up),
      KeyCode::Down | KeyCode::Char('j') => self.move_selection(Direction::Down),
      KeyCode::Enter => self.open_selected(),
      KeyCode::Backspace | KeyCode::Esc => self.go_back(),
      KeyCode::Delete | KeyCode::Char('d') => self.prompt_delete(),
      KeyCode::Char('m') => self.prompt_mkdir(),
      KeyCode::Char('s') => self.toggle_mark(),
      KeyCode::Char('c') => self.prompt_move(),
      KeyCode::Char('+') | KeyCode::Char('=') => self.zoom_in(),
      KeyCode::Char('-') => self.zoom = self.zoom.saturating_sub(1),
      KeyCode::Char('0') => self.zoom = 0,
      _ => {}
    }
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
    if !self.marked.is_empty() {
      status.insert_str(0, &format!("{} marked  ", self.marked.len()));
    }
    if !self.jobs.is_empty() {
      let removed: u64 = self
        .jobs
        .iter()
        .map(|job| job.removed.load(Ordering::Relaxed))
        .sum();
      status.insert_str(0, &format!("deleting: {removed} removed  "));
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

  /// Return the rows of the open dialog.
  fn dialog_lines(&self, width: usize) -> Option<Vec<String>> {
    let lines = match self.dialog.as_ref()? {
      Dialog::Delete { target, choice } => {
        let kind = if target.is_dir { "folder" } else { "file" };
        let buttons: Vec<String> = DELETE_CHOICES
          .iter()
          .enumerate()
          .map(|(index, label)| {
            if index == *choice {
              format!("[ {label} ]")
            } else {
              format!("  {label}  ")
            }
          })
          .collect();
        vec![
          format!("Delete this {kind} permanently?"),
          fit_text(&target.path.to_string_lossy(), width),
          format_size(target.size),
          String::new(),
          buttons.join(" "),
        ]
      }
      Dialog::Mkdir { input } => vec![
        "New folder name:".to_string(),
        format!("> {}_", fit_text(input, width.saturating_sub(3))),
      ],
      Dialog::Move { folders, index } => {
        let start = window_start(*index, folders.len(), MOVE_DIALOG_ROWS);
        let mut lines = vec![format!("Move {} marked item(s) into:", self.marked.len())];
        lines.extend(
          folders
            .iter()
            .enumerate()
            .skip(start)
            .take(MOVE_DIALOG_ROWS)
            .map(|(row, name)| {
              let pointer = if row == *index { ">" } else { " " };
              format!("{pointer} {}", fit_text(name, width.saturating_sub(2)))
            }),
        );
        lines
      }
    };
    Some(lines)
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
    let in_folder = self.scan().is_some();
    if let Some(scan) = self.scan() {
      let node = self.node();
      total = scan.tree.nodes[node as usize].total;
      self.entries = folder_entries(scan, node);
      self.zoom = self.zoom.min(self.named_count().saturating_sub(1));
      let scan = self.scan().expect("the scan was just found");
      let unfinished = scan.finished.is_none();
      let path = self.folder_path().expect("the scan was just found");
      let hidden = if self.zoom > 0 {
        format!("  (+{} larger hidden)", self.zoom)
      } else {
        String::new()
      };
      title = format!("{}  {}{hidden}", path.display(), format_size(total));
      status = self.scan_status(scan);
      let visible = zoomed_entries(&self.entries, self.zoom);
      let (targets, group) = draw_folder(canvas, &visible, total, area, &self.marked);
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
    // a long path gives way to the status, which says what is going on right now
    let status_width = status.chars().count();
    let title_width = width.saturating_sub(status_width + 2);
    if title_width >= MIN_TITLE_WIDTH {
      canvas.put(0, 0, &fit_text(&title, title_width), bold);
      canvas.put(width - status_width, 0, &status, Style::default());
    } else {
      canvas.put(0, 0, &fit_text(&title, width), bold);
    }
    self.compose_footer(canvas, in_folder, merged.as_ref(), total);
    if let Some(lines) = self.dialog_lines(width.saturating_sub(8)) {
      let danger = matches!(self.dialog, Some(Dialog::Delete { .. }));
      draw_dialog(canvas, &lines, danger);
    }
  }

  /// Draw the bottom row: a message on its own, or the selected item and the keys that fit.
  fn compose_footer(
    &self,
    canvas: &mut Canvas,
    in_folder: bool,
    merged: Option<&Entry>,
    total: u64,
  ) {
    let width = canvas.width;
    let footer = canvas.height.saturating_sub(1);
    let bold = Style::new().add_modifier(Modifier::BOLD);
    if let Some(status) = &self.status {
      let style = if status.error {
        bold.fg(Color::Red)
      } else {
        bold
      };
      canvas.put(0, footer, &fit_text(&status.text, width), style);
      return;
    }
    let help = if !in_folder {
      HELP_MAP
    } else if width >= HELP_FOLDER.chars().count() + MIN_SELECTION_WIDTH {
      HELP_FOLDER
    } else {
      HELP_FOLDER_SHORT
    };
    let help_width = help.chars().count();
    let left_width = if width > help_width + 2 {
      canvas.put(width - help_width, footer, help, Style::default());
      width - help_width - 2
    } else {
      width
    };
    let selection = self.selection_text(merged, total);
    canvas.put(0, footer, &fit_text(&selection, left_width), bold);
  }

  /// Keep the selection on an existing box, falling back to the nearest or the first one.
  fn resolve_selection(&mut self) {
    if let Some(anchor) = self.anchor.take() {
      let distance = |rect: Rect| rect.x.abs_diff(anchor.x) + 2 * rect.y.abs_diff(anchor.y);
      self.selected = self
        .targets
        .iter()
        .min_by_key(|target| distance(target.rect))
        .map(|target| target.key.clone());
      self.pinned = true;
      return;
    }
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
    let mut canvas = Canvas::new(area.width as usize, area.height as usize, self.ascii);
    self.compose(&mut canvas);
    canvas.blit(frame.buffer_mut());
  }

  /// Show the health of disks whose reading has finished.
  fn apply_health_updates(&mut self) {
    for (name, health) in self.health_updates.try_iter() {
      if let Some(disk) = self.disks.iter_mut().find(|disk| disk.name == name) {
        disk.health = health;
        self.redraw = true;
      }
    }
  }

  /// Remove a message that has been on screen long enough.
  fn expire_status(&mut self) {
    if self
      .status
      .as_ref()
      .is_some_and(|status| status.shown_at.elapsed() >= STATUS_TIME)
    {
      self.status = None;
      self.redraw = true;
    }
  }

  /// Run until the user quits.
  pub fn run(&mut self, terminal: &mut DefaultTerminal) -> io::Result<()> {
    let mut last_draw = Instant::now() - SCAN_REDRAW;
    let mut stale = false;
    while !self.quit {
      for scan in self.scans.values_mut() {
        stale |= scan.drain(DRAIN_BUDGET);
      }
      self.apply_health_updates();
      self.finish_jobs();
      self.expire_status();
      stale |= !self.jobs.is_empty();
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

#[cfg(test)]
mod tests {
  use super::*;

  /// Return entries with one merged group among them.
  fn sample() -> Vec<Entry> {
    let entry = |name: &str, size, kind| Entry {
      name: name.to_string(),
      size,
      kind,
    };
    vec![
      entry("a", 500, EntryKind::File),
      entry("", 400, EntryKind::Group(9)),
      entry("b", 300, EntryKind::Dir(1)),
      entry("c", 200, EntryKind::File),
    ]
  }

  /// Return the names left after zooming.
  fn names(zoom: usize) -> Vec<String> {
    zoomed_entries(&sample(), zoom)
      .into_iter()
      .map(|entry| entry.name)
      .collect()
  }

  #[test]
  fn zoom_hides_the_largest_named_items_and_keeps_the_group() {
    assert_eq!(names(0), ["a", "", "b", "c"]);
    assert_eq!(names(1), ["", "b", "c"]);
    assert_eq!(names(2), ["", "c"]);
  }

  #[test]
  fn the_list_window_follows_the_cursor() {
    assert_eq!(window_start(0, 30, 10), 0);
    assert_eq!(window_start(12, 30, 10), 7);
    assert_eq!(window_start(29, 30, 10), 20);
    assert_eq!(window_start(2, 3, 10), 0);
  }
}
