//! Parallel folder scan feeding a compact tree that the interface reads while it grows.

use rayon::Scope;
use std::collections::HashSet;
use std::fs::{self, Metadata};
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

pub const ROOT: u32 = 0;
/// Files kept by name in one directory; a treemap cannot show more than this anyway.
const KEPT_FILES_PER_DIR: usize = 128;
const HARDLINK_SHARDS: usize = 64;
const BLOCK_SIZE: u64 = 512;

/// What one directory child is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ItemKind {
  File,
  Dir(u32),
  /// The smallest files of a directory, merged to save memory; holds their count.
  Group(u32),
}

/// One child of a directory.
#[derive(Debug)]
pub struct Item {
  pub name: Box<str>,
  pub size: u64,
  pub kind: ItemKind,
}

/// One directory of the tree.
#[derive(Debug, Default)]
pub struct Node {
  pub parent: u32,
  pub total: u64,
  pub children: Vec<Item>,
}

/// Everything found in one directory.
#[derive(Debug, Default)]
struct Batch {
  node: u32,
  items: Vec<Item>,
  files: u64,
  denied: u64,
  other_filesystem: u64,
}

/// A message from the scanning threads, in the order the tree needs them.
enum Message {
  Batch(Batch),
  Done(Duration),
}

/// The directory tree, kept as an arena indexed by node number.
#[derive(Debug, Default)]
pub struct Tree {
  pub nodes: Vec<Node>,
  pub files: u64,
  pub directories: u64,
  pub denied: u64,
  pub other_filesystem: u64,
}

impl Tree {
  /// Return a tree holding only the root directory with its own size.
  fn new(root_size: u64) -> Self {
    let root = Node {
      parent: ROOT,
      total: root_size,
      children: Vec::new(),
    };
    Self {
      nodes: vec![root],
      directories: 1,
      ..Self::default()
    }
  }

  /// Return the current size of a child, including everything below it.
  pub fn item_size(&self, item: &Item) -> u64 {
    match item.kind {
      ItemKind::Dir(node) => self.nodes[node as usize].total,
      ItemKind::File | ItemKind::Group(_) => item.size,
    }
  }

  /// Merge one directory listing and add its size to every ancestor.
  fn apply(&mut self, batch: Batch) {
    let parent = batch.node;
    let added: u64 = batch.items.iter().map(|item| item.size).sum();
    for item in &batch.items {
      if let ItemKind::Dir(node) = item.kind {
        let index = node as usize;
        if index >= self.nodes.len() {
          self.nodes.resize_with(index + 1, Node::default);
        }
        self.nodes[index].parent = parent;
        self.nodes[index].total = item.size;
        self.directories += 1;
      }
    }
    self.files += batch.files;
    self.denied += batch.denied;
    self.other_filesystem += batch.other_filesystem;
    self.nodes[parent as usize].children = batch.items;
    let mut current = parent;
    loop {
      let node = &mut self.nodes[current as usize];
      node.total += added;
      if current == ROOT {
        break;
      }
      current = node.parent;
    }
  }
}

/// State shared by the scanning threads.
struct Context {
  device: u64,
  next_node: AtomicU32,
  stop: Arc<AtomicBool>,
  sender: Sender<Message>,
  hardlinks: Vec<Mutex<HashSet<(u64, u64)>>>,
}

impl Context {
  /// Return whether this is the first time a file with several links is seen.
  fn first_link(&self, metadata: &Metadata) -> bool {
    let key = (metadata.dev(), metadata.ino());
    let shard = (metadata.ino() as usize) % HARDLINK_SHARDS;
    self.hardlinks[shard]
      .lock()
      .map_or(true, |mut seen| seen.insert(key))
  }
}

/// Return the bytes a file really occupies on the disk.
fn disk_bytes(metadata: &Metadata) -> u64 {
  metadata.blocks() * BLOCK_SIZE
}

/// Merge all but the largest files into one item, so that huge directories stay cheap.
fn fold_small_files(files: &mut Vec<Item>) {
  if files.len() <= KEPT_FILES_PER_DIR {
    return;
  }
  files.select_nth_unstable_by_key(KEPT_FILES_PER_DIR - 1, |item| std::cmp::Reverse(item.size));
  let size = files[KEPT_FILES_PER_DIR..]
    .iter()
    .map(|item| item.size)
    .sum();
  let count = (files.len() - KEPT_FILES_PER_DIR) as u32;
  files.truncate(KEPT_FILES_PER_DIR);
  files.push(Item {
    name: "".into(),
    size,
    kind: ItemKind::Group(count),
  });
}

/// List one directory, report it and queue its subdirectories.
fn walk<'scope>(context: &'scope Context, scope: &Scope<'scope>, node: u32, path: PathBuf) {
  if context.stop.load(Ordering::Relaxed) {
    return;
  }
  let mut batch = Batch {
    node,
    ..Batch::default()
  };
  let mut files = Vec::new();
  let mut folders = Vec::new();
  let mut subdirectories = Vec::new();
  match fs::read_dir(&path) {
    Err(_) => batch.denied += 1,
    Ok(listing) => {
      for entry in listing {
        let Ok((entry, metadata)) = entry.and_then(|entry| {
          let metadata = entry.metadata()?;
          Ok((entry, metadata))
        }) else {
          batch.denied += 1;
          continue;
        };
        if metadata.dev() != context.device {
          batch.other_filesystem += 1;
          continue;
        }
        let name: Box<str> = entry.file_name().to_string_lossy().into();
        let size = disk_bytes(&metadata);
        if metadata.is_dir() {
          let child = context.next_node.fetch_add(1, Ordering::Relaxed);
          folders.push(Item {
            name,
            size,
            kind: ItemKind::Dir(child),
          });
          subdirectories.push((child, entry.path()));
        } else if metadata.nlink() <= 1 || context.first_link(&metadata) {
          batch.files += 1;
          files.push(Item {
            name,
            size,
            kind: ItemKind::File,
          });
        }
      }
    }
  }
  fold_small_files(&mut files);
  batch.items = folders;
  batch.items.append(&mut files);
  // a parent must reach the tree before any of its children, so it is sent before they start
  if context.sender.send(Message::Batch(batch)).is_err() {
    return;
  }
  for (child, child_path) in subdirectories {
    scope.spawn(move |scope| walk(context, scope, child, child_path));
  }
}

/// Return whether a device id belongs to a rotating disk.
fn is_rotational(device: u64) -> bool {
  let base = format!(
    "/sys/dev/block/{}:{}",
    rustix::fs::major(device),
    rustix::fs::minor(device)
  );
  // a partition has no queue directory of its own; its parent disk has
  [
    format!("{base}/queue/rotational"),
    format!("{base}/../queue/rotational"),
  ]
  .iter()
  .find_map(|path| fs::read_to_string(path).ok())
  .is_some_and(|text| text.trim() == "1")
}

/// Return the thread count for a device: one on a rotating disk, else 80% of the cores.
fn default_threads(device: u64) -> usize {
  if is_rotational(device) {
    return 1;
  }
  let cores = thread::available_parallelism().map_or(1, usize::from);
  (cores * 8 / 10).max(1)
}

/// A running or finished scan of one folder.
pub struct Scan {
  pub root: PathBuf,
  pub tree: Tree,
  pub threads: usize,
  pub finished: Option<Duration>,
  receiver: Receiver<Message>,
  stop: Arc<AtomicBool>,
  started: Instant,
}

impl Scan {
  /// Start scanning a folder in the background, staying on its filesystem.
  pub fn start(root: &Path, threads: Option<usize>) -> io::Result<Self> {
    let root = root.canonicalize()?;
    let metadata = fs::metadata(&root)?;
    if !metadata.is_dir() {
      return Err(io::Error::other("not a directory"));
    }
    let threads = threads.unwrap_or_else(|| default_threads(metadata.dev()));
    let pool = rayon::ThreadPoolBuilder::new()
      .num_threads(threads)
      .build()
      .map_err(io::Error::other)?;
    let (sender, receiver) = mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let started = Instant::now();
    let context = Context {
      device: metadata.dev(),
      next_node: AtomicU32::new(ROOT + 1),
      stop: Arc::clone(&stop),
      sender,
      hardlinks: (0..HARDLINK_SHARDS).map(|_| Mutex::default()).collect(),
    };
    let walked = root.clone();
    thread::spawn(move || {
      pool.scope(|scope| walk(&context, scope, ROOT, walked));
      let _ = context.sender.send(Message::Done(started.elapsed()));
    });
    Ok(Self {
      tree: Tree::new(disk_bytes(&metadata)),
      root,
      threads,
      finished: None,
      receiver,
      stop,
      started,
    })
  }

  /// Merge waiting results for at most the given time and return whether anything changed.
  pub fn drain(&mut self, budget: Duration) -> bool {
    let deadline = Instant::now() + budget;
    let mut changed = false;
    while self.finished.is_none() {
      match self.receiver.try_recv() {
        Ok(Message::Batch(batch)) => self.tree.apply(batch),
        Ok(Message::Done(elapsed)) => self.finished = Some(elapsed),
        Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
      }
      changed = true;
      if Instant::now() >= deadline {
        break;
      }
    }
    changed
  }

  /// Return how long the scan has been running, or how long it took.
  pub fn elapsed(&self) -> Duration {
    self.finished.unwrap_or_else(|| self.started.elapsed())
  }
}

impl Drop for Scan {
  /// Tell the scanning threads to give up when nobody needs the result any more.
  fn drop(&mut self) {
    self.stop.store(true, Ordering::Relaxed);
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::os::unix::fs as unix_fs;

  /// Create an empty folder that is unique to one test.
  fn test_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("storage-analyzer-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
  }

  /// Run a scan to its end.
  fn finished_scan(root: &Path, threads: usize) -> Scan {
    let mut scan = Scan::start(root, Some(threads)).unwrap();
    while scan.finished.is_none() {
      scan.drain(Duration::from_millis(10));
      thread::sleep(Duration::from_millis(1));
    }
    scan
  }

  /// Return a child of the root by name.
  fn child<'a>(scan: &'a Scan, name: &str) -> &'a Item {
    scan.tree.nodes[ROOT as usize]
      .children
      .iter()
      .find(|item| &*item.name == name)
      .unwrap()
  }

  #[test]
  fn totals_match_the_files_and_count_hardlinks_once() {
    let dir = test_dir("totals");
    fs::create_dir_all(dir.join("a/b")).unwrap();
    fs::write(dir.join("a/one"), vec![1; 8192]).unwrap();
    fs::write(dir.join("a/b/two"), vec![2; 16384]).unwrap();
    fs::hard_link(dir.join("a/one"), dir.join("a/b/one-link")).unwrap();
    unix_fs::symlink("a/one", dir.join("link")).unwrap();
    let scan = finished_scan(&dir, 4);
    let expected: u64 = [
      dir.clone(),
      dir.join("a"),
      dir.join("a/b"),
      dir.join("a/one"),
      dir.join("a/b/two"),
      dir.join("link"),
    ]
    .iter()
    .map(|path| disk_bytes(&fs::symlink_metadata(path).unwrap()))
    .sum();
    assert_eq!(scan.tree.nodes[ROOT as usize].total, expected);
    assert_eq!(scan.tree.files, 3);
    assert_eq!(scan.tree.directories, 3);
    let folder = child(&scan, "a");
    assert!(matches!(folder.kind, ItemKind::Dir(_)));
    assert_eq!(
      scan.tree.item_size(folder),
      expected
        - disk_bytes(&fs::symlink_metadata(dir.join("link")).unwrap())
        - disk_bytes(&fs::metadata(&dir).unwrap())
    );
    fs::remove_dir_all(&dir).unwrap();
  }

  #[test]
  fn thread_count_does_not_change_the_result() {
    let dir = test_dir("threads");
    for folder in 0..40 {
      let path = dir.join(format!("d{folder}/e"));
      fs::create_dir_all(&path).unwrap();
      fs::write(path.join("data"), vec![0; 4096 * (folder + 1)]).unwrap();
    }
    let one = finished_scan(&dir, 1);
    let many = finished_scan(&dir, 8);
    assert_eq!(
      one.tree.nodes[ROOT as usize].total,
      many.tree.nodes[ROOT as usize].total
    );
    assert_eq!(one.tree.directories, many.tree.directories);
    assert_eq!(one.tree.files, 40);
    fs::remove_dir_all(&dir).unwrap();
  }

  #[test]
  fn small_files_are_merged_without_losing_bytes() {
    let mut files: Vec<Item> = (0..KEPT_FILES_PER_DIR as u64 + 72)
      .map(|size| Item {
        name: size.to_string().into(),
        size,
        kind: ItemKind::File,
      })
      .collect();
    let before: u64 = files.iter().map(|item| item.size).sum();
    fold_small_files(&mut files);
    assert_eq!(files.len(), KEPT_FILES_PER_DIR + 1);
    assert_eq!(files.iter().map(|item| item.size).sum::<u64>(), before);
    assert_eq!(files.last().unwrap().kind, ItemKind::Group(72));
    assert!(
      files[..KEPT_FILES_PER_DIR]
        .iter()
        .all(|item| item.size >= 72)
    );
  }

  #[test]
  fn unreadable_folder_is_counted() {
    let dir = test_dir("denied");
    let locked = dir.join("locked");
    fs::create_dir_all(&locked).unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    let scan = finished_scan(&dir, 2);
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(scan.tree.denied, 1);
    fs::remove_dir_all(&dir).unwrap();
  }
}
