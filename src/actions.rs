//! Changes to the disk: the safety checks and the file operations behind them.

use rayon::{Scope, ThreadPool};
use rustix::fs::{
  AtFlags, CWD, Dir, FileType, Mode, OFlags, RenameFlags, ResolveFlags, openat, openat2,
  renameat_with, statat, unlinkat,
};
use rustix::io::Errno;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;

/// Return why an item must not be deleted or moved, if there is a reason.
pub fn change_refusal(path: &Path, is_dir: bool, mounts: &[String]) -> Option<String> {
  if path.to_string_lossy().contains('\u{FFFD}') {
    return Some("the path is not valid UTF-8, change it with another tool".to_string());
  }
  if is_dir && path.parent() == Some(Path::new("/")) {
    return Some(format!("{} is a top-level system folder", path.display()));
  }
  mounts
    .iter()
    .find(|mount| Path::new(mount).starts_with(path))
    .map(|mount| format!("{mount} is mounted inside it"))
}

/// A folder being deleted, waiting for its listing and its subfolders to finish.
struct PendingFolder {
  parent: Option<Arc<PendingFolder>>,
  relative: PathBuf,
  remaining: AtomicUsize,
}

/// State shared by the threads deleting one folder.
struct Deletion<'a> {
  root: OwnedFd,
  removed: &'a AtomicU64,
  first_error: Mutex<Option<io::Error>>,
}

impl Deletion<'_> {
  /// Remember the first problem, ignoring items that are already gone.
  fn note_error(&self, path: &Path, error: Errno) {
    if error == Errno::NOENT {
      return;
    }
    if let Ok(mut first) = self.first_error.lock()
      && first.is_none()
    {
      let message = format!("{}: {error}", path.display());
      *first = Some(io::Error::other(message));
    }
  }

  /// Open a folder below the root; the kernel refuses to pass a symlink or a mount on the way.
  fn open(&self, relative: &Path) -> Result<OwnedFd, Errno> {
    open_beneath(&self.root, relative)
  }

  /// Remove an empty folder that has finished.
  fn remove_folder(&self, folder: &PendingFolder) {
    let (Some(parent), Some(name)) = (folder.relative.parent(), folder.relative.file_name()) else {
      return;
    };
    let parent = if parent.as_os_str().is_empty() {
      Path::new(".")
    } else {
      parent
    };
    let removed = self
      .open(parent)
      .and_then(|parent_fd| unlinkat(&parent_fd, name, AtFlags::REMOVEDIR));
    match removed {
      Ok(()) => {
        self.removed.fetch_add(1, Ordering::Relaxed);
      }
      Err(error) => self.note_error(&folder.relative, error),
    }
  }

  /// Count one finished piece of a folder, removing the folders that are now empty.
  fn finish_piece(&self, mut folder: Arc<PendingFolder>) {
    while folder.remaining.fetch_sub(1, Ordering::AcqRel) == 1 {
      let Some(parent) = folder.parent.clone() else {
        return;
      };
      self.remove_folder(&folder);
      folder = parent;
    }
  }

  /// Remove the files of a folder and return the names of its subfolders.
  fn clear_files(&self, relative: &Path) -> Result<Vec<PathBuf>, Errno> {
    let mut listing = Dir::new(self.open(relative)?)?;
    let mut subfolders = Vec::new();
    while let Some(entry) = listing.next() {
      let entry = entry?;
      let name = entry.file_name();
      if name == c"." || name == c".." {
        continue;
      }
      let fd = listing.fd()?;
      let kind = match entry.file_type() {
        FileType::Unknown => {
          FileType::from_raw_mode(statat(fd, name, AtFlags::SYMLINK_NOFOLLOW)?.st_mode)
        }
        kind => kind,
      };
      if kind == FileType::Directory {
        subfolders.push(PathBuf::from(OsStr::from_bytes(name.to_bytes())));
        continue;
      }
      match unlinkat(fd, name, AtFlags::empty()) {
        Ok(()) => {
          self.removed.fetch_add(1, Ordering::Relaxed);
        }
        Err(error) => self.note_error(&relative.join(OsStr::from_bytes(name.to_bytes())), error),
      }
    }
    Ok(subfolders)
  }

  /// Delete one folder's files and hand its subfolders to the thread pool.
  fn walk<'scope>(&'scope self, scope: &Scope<'scope>, folder: Arc<PendingFolder>) {
    let subfolders = self.clear_files(&folder.relative).unwrap_or_else(|error| {
      self.note_error(&folder.relative, error);
      Vec::new()
    });
    // one extra piece stands for this listing, so that no subfolder can finish the folder early
    folder
      .remaining
      .store(subfolders.len() + 1, Ordering::Release);
    for name in subfolders {
      let child = Arc::new(PendingFolder {
        relative: folder.relative.join(name),
        parent: Some(Arc::clone(&folder)),
        remaining: AtomicUsize::new(0),
      });
      scope.spawn(move |scope| self.walk(scope, child));
    }
    self.finish_piece(folder);
  }
}

/// Open a folder without following symlinks or leaving the filesystem of the root.
fn open_beneath(root: &OwnedFd, relative: &Path) -> Result<OwnedFd, Errno> {
  let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
  let resolve = ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_XDEV;
  loop {
    match openat2(root, relative, flags, Mode::empty(), resolve) {
      Err(Errno::AGAIN) | Err(Errno::INTR) => continue,
      result => return result,
    }
  }
}

/// Delete a folder using every thread of a pool.
fn delete_folder(path: &Path, pool: &ThreadPool, removed: &AtomicU64) -> io::Result<()> {
  let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
  let root = openat(CWD, path, flags, Mode::empty()).map_err(io::Error::from)?;
  // kernels and containers without openat2 keep the standard single-threaded deletion
  match open_beneath(&root, Path::new(".")) {
    Err(Errno::NOSYS | Errno::INVAL | Errno::PERM) => return fs::remove_dir_all(path),
    Err(error) => return Err(error.into()),
    Ok(_) => {}
  }
  let deletion = Deletion {
    root,
    removed,
    first_error: Mutex::new(None),
  };
  let top = Arc::new(PendingFolder {
    parent: None,
    relative: PathBuf::from("."),
    remaining: AtomicUsize::new(0),
  });
  pool.scope(|scope| deletion.walk(scope, top));
  if let Some(error) = deletion.first_error.into_inner().ok().flatten() {
    return Err(error);
  }
  match fs::remove_dir(path) {
    Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
    _ => {
      removed.fetch_add(1, Ordering::Relaxed);
      Ok(())
    }
  }
}

/// Delete a file or a whole folder in the background, counting what has been removed.
pub fn delete_in_background(
  path: PathBuf,
  is_dir: bool,
  pool: Arc<ThreadPool>,
  removed: Arc<AtomicU64>,
) -> Receiver<io::Result<()>> {
  let (sender, receiver) = mpsc::channel();
  thread::spawn(move || {
    let result = if is_dir {
      delete_folder(&path, &pool, &removed)
    } else {
      fs::remove_file(&path)
    };
    let _ = sender.send(result);
  });
  receiver
}

/// Move an item, failing instead of replacing anything that already has the new name.
pub fn move_without_replacing(from: &Path, to: &Path) -> io::Result<()> {
  renameat_with(CWD, from, CWD, to, RenameFlags::NOREPLACE).map_err(io::Error::from)
}

/// Return whether text can be used as the name of one new folder.
pub fn is_valid_folder_name(name: &str) -> bool {
  !name.is_empty()
    && name != "."
    && name != ".."
    && !name.contains('/')
    && !name.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::os::unix::fs as unix_fs;
  use std::time::Duration;

  /// Create an empty folder that is unique to one test.
  fn test_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("sa-actions-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
  }

  /// Delete something with a small pool and wait for the result.
  fn delete(path: PathBuf, is_dir: bool) -> (io::Result<()>, u64) {
    let pool = Arc::new(
      rayon::ThreadPoolBuilder::new()
        .num_threads(4)
        .build()
        .unwrap(),
    );
    let removed = Arc::new(AtomicU64::new(0));
    let receiver = delete_in_background(path, is_dir, pool, Arc::clone(&removed));
    let result = receiver.recv_timeout(Duration::from_secs(20)).unwrap();
    (result, removed.load(Ordering::Relaxed))
  }
  #[test]
  fn a_folder_with_a_mount_inside_is_refused() {
    let mounts = vec!["/".to_string(), "/data/a/b".to_string()];
    let refusal = change_refusal(Path::new("/data/a"), true, &mounts);
    assert!(refusal.unwrap().contains("/data/a/b"));
    assert!(change_refusal(Path::new("/data/a/b"), true, &mounts).is_some());
    assert!(change_refusal(Path::new("/data/ab"), true, &mounts).is_none());
    assert!(change_refusal(Path::new("/data/a/c"), true, &mounts).is_none());
  }

  #[test]
  fn top_level_system_folders_are_refused_but_files_are_not() {
    let mounts = vec!["/".to_string()];
    assert!(change_refusal(Path::new("/usr"), true, &mounts).is_some());
    assert!(change_refusal(Path::new("/swapfile"), false, &mounts).is_none());
    assert!(change_refusal(Path::new("/usr/share"), true, &mounts).is_none());
  }

  #[test]
  fn damaged_names_are_refused() {
    assert!(change_refusal(Path::new("/a/b\u{FFFD}"), false, &[]).is_some());
  }

  #[test]
  fn deleting_a_folder_leaves_the_target_of_a_symlink_alone() {
    let dir = test_dir("symlink");
    fs::create_dir_all(dir.join("gone/sub")).unwrap();
    fs::create_dir_all(dir.join("kept")).unwrap();
    fs::write(dir.join("kept/file"), b"data").unwrap();
    unix_fs::symlink(dir.join("kept"), dir.join("gone/sub/link")).unwrap();
    delete(dir.join("gone"), true).0.unwrap();
    assert!(!dir.join("gone").exists());
    assert!(dir.join("kept/file").exists());
    fs::remove_dir_all(&dir).unwrap();
  }

  #[test]
  fn deleting_a_symlink_removes_only_the_link() {
    let dir = test_dir("link");
    fs::write(dir.join("real"), b"data").unwrap();
    unix_fs::symlink(dir.join("real"), dir.join("link")).unwrap();
    delete(dir.join("link"), false).0.unwrap();
    assert!(dir.join("real").exists());
    assert!(fs::symlink_metadata(dir.join("link")).is_err());
    fs::remove_dir_all(&dir).unwrap();
  }

  #[test]
  fn a_wide_and_deep_tree_is_removed_completely() {
    let dir = test_dir("tree");
    let top = dir.join("top");
    let mut expected = 1;
    for branch in 0..30 {
      let mut folder = top.join(format!("b{branch}"));
      for depth in 0..6 {
        folder = folder.join(format!("d{depth}"));
      }
      fs::create_dir_all(&folder).unwrap();
      expected += 7;
      for file in 0..5 {
        fs::write(folder.join(format!("f{file}")), b"x").unwrap();
        unix_fs::symlink("/nonexistent", folder.join(format!("l{file}"))).unwrap();
        expected += 2;
      }
    }
    let (result, removed) = delete(top.clone(), true);
    result.unwrap();
    assert!(!top.exists());
    assert_eq!(removed, expected);
    fs::remove_dir_all(&dir).unwrap();
  }

  #[test]
  fn a_symlink_given_as_the_folder_is_not_followed() {
    let dir = test_dir("rootlink");
    fs::create_dir_all(dir.join("real")).unwrap();
    fs::write(dir.join("real/file"), b"data").unwrap();
    unix_fs::symlink(dir.join("real"), dir.join("alias")).unwrap();
    assert!(delete(dir.join("alias"), true).0.is_err());
    assert!(dir.join("real/file").exists());
    fs::remove_dir_all(&dir).unwrap();
  }

  #[test]
  fn moving_never_replaces_an_existing_item() {
    let dir = test_dir("move");
    fs::create_dir_all(dir.join("into")).unwrap();
    fs::write(dir.join("a"), b"new").unwrap();
    fs::write(dir.join("into/a"), b"old").unwrap();
    assert!(move_without_replacing(&dir.join("a"), &dir.join("into/a")).is_err());
    assert_eq!(fs::read(dir.join("into/a")).unwrap(), b"old");
    assert_eq!(fs::read(dir.join("a")).unwrap(), b"new");
    fs::write(dir.join("b"), b"b").unwrap();
    move_without_replacing(&dir.join("b"), &dir.join("into/b")).unwrap();
    assert!(dir.join("into/b").exists());
    fs::remove_dir_all(&dir).unwrap();
  }

  #[test]
  fn folder_names_are_checked() {
    assert!(is_valid_folder_name("backup 2024"));
    for bad in ["", ".", "..", "a/b", "a\nb"] {
      assert!(!is_valid_folder_name(bad), "{bad}");
    }
  }
}
