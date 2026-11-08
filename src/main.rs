//! storage-analyzer: browse disks, partitions and folders as a proportional box map.

mod actions;
mod app;
mod disks;
mod layout;
mod scan;
mod smart;
mod view;

use app::App;
use scan::{ROOT, Scan};
use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::thread;
use std::time::Duration;

#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

const USAGE: &str = "\
Browse disks, partitions and folders as a proportional box map.

Usage: storage-analyzer [OPTIONS] [PATH]

Without PATH the program starts on the map of all disks.

Options:
      --summary      Scan PATH, print the totals and exit
      --health       Print the health of every disk and exit (needs root)
      --ascii        Draw with # and . instead of colour fills
      --threads N    Number of scanning and deleting threads-h, --help         Print this help
  -V, --version      Print the version";

/// The parsed command line.
#[derive(Debug, Default, PartialEq, Eq)]
struct Options {
  path: Option<PathBuf>,
  summary: bool,
  health: bool,
  ascii: bool,
  threads: Option<usize>,
  help: bool,
  version: bool,
}

/// Parse command line arguments, without the program name.
fn parse_options(arguments: impl IntoIterator<Item = String>) -> Result<Options, String> {
  let mut options = Options::default();
  let mut arguments = arguments.into_iter();
  while let Some(argument) = arguments.next() {
    match argument.as_str() {
      "--summary" => options.summary = true,
      "--health" => options.health = true,
      "--ascii" => options.ascii = true,
      "-h" | "--help" => options.help = true,
      "-V" | "--version" => options.version = true,
      "--threads" => {
        let value = arguments.next().ok_or("--threads needs a number")?;
        let threads = value.parse().ok().filter(|&threads| threads > 0);
        options.threads = Some(threads.ok_or(format!("invalid thread count: {value}"))?);
      }
      option if option.starts_with('-') && option.len() > 1 => {
        return Err(format!("unknown option: {option}"));
      }
      _ if options.path.is_some() => return Err("only one PATH is accepted".to_string()),
      path => options.path = Some(PathBuf::from(path)),
    }
  }
  if options.summary && options.path.is_none() {
    return Err("--summary needs a PATH".to_string());
  }
  Ok(options)
}

/// Scan a folder to its end and print the totals.
fn print_summary(path: &Path, threads: Option<usize>) -> io::Result<()> {
  let mut scan = Scan::start(path, threads)?;
  while scan.finished.is_none() {
    if !scan.drain(Duration::from_millis(100)) {
      thread::sleep(Duration::from_millis(1));
    }
  }
  let tree = &scan.tree;
  println!("path: {}", scan.root.display());
  println!("bytes: {}", tree.nodes[ROOT as usize].total);
  println!("files: {}", tree.files);
  println!("directories: {}", tree.directories);
  println!("unreadable: {}", tree.denied);
  println!("other filesystems skipped: {}", tree.other_filesystem);
  println!("threads: {}", scan.threads);
  println!("seconds: {:.3}", scan.elapsed().as_secs_f64());
  Ok(())
}

/// Print the health of every disk, one disk per line.
fn print_health() -> Result<(), String> {
  for disk in disks::read_disks().map_err(|error| format!("cannot read disks: {error}"))? {
    let health = smart::read_health(&disk.name, &disk.model);
    let fields: Vec<String> = smart::health_fields(&health)
      .into_iter()
      .map(|field| field.text)
      .collect();
    println!("{}  {}  {}", disk.name, disk.model, fields.join("  "));
  }
  Ok(())
}

/// Return whether this terminal cannot be trusted with 24-bit colour and block characters.
fn terminal_needs_ascii() -> bool {
  matches!(
    std::env::var("TERM").as_deref(),
    Ok("linux" | "dumb" | "vt100")
  )
}

/// Run the interactive interface until the user quits.
fn run_interface(options: &Options) -> Result<(), String> {
  if !io::stdout().is_terminal() || !io::stdin().is_terminal() {
    return Err("the interface needs a terminal; use --summary PATH otherwise".to_string());
  }
  let ascii = options.ascii || terminal_needs_ascii();
  let mut app = match &options.path {
    Some(path) => App::on_folder(path, options.threads, ascii)
      .map_err(|error| format!("{}: {error}", path.display()))?,
    None => {
      let disks = disks::read_disks().map_err(|error| format!("cannot read disks: {error}"))?;
      if disks.is_empty() {
        return Err("no physical disks found".to_string());
      }
      App::on_disks(disks, options.threads, ascii)
    }
  };
  let mut terminal = ratatui::init();
  let result = app.run(&mut terminal);
  ratatui::restore();
  result.map_err(|error| error.to_string())
}

/// Do what the command line asks for.
fn run() -> Result<(), String> {
  let options = parse_options(std::env::args().skip(1))?;
  if options.help {
    println!("{USAGE}");
    return Ok(());
  }
  if options.version {
    println!("storage-analyzer {}", env!("CARGO_PKG_VERSION"));
    return Ok(());
  }
  if options.health {
    return print_health();
  }
  match (&options.path, options.summary) {
    (Some(path), true) => {
      print_summary(path, options.threads).map_err(|error| format!("{}: {error}", path.display()))
    }
    _ => run_interface(&options),
  }
}

/// Program entry point.
fn main() -> ExitCode {
  match run() {
    Ok(()) => ExitCode::SUCCESS,
    Err(error) => {
      eprintln!("storage-analyzer: {error}");
      ExitCode::FAILURE
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Parse a command line given as string slices.
  fn parse(arguments: &[&str]) -> Result<Options, String> {
    parse_options(arguments.iter().map(ToString::to_string))
  }

  #[test]
  fn no_arguments_start_on_the_disk_map() {
    assert_eq!(parse(&[]).unwrap(), Options::default());
  }

  #[test]
  fn summary_with_threads_is_understood() {
    let options = parse(&["--summary", "/tmp", "--threads", "4"]).unwrap();
    assert!(options.summary);
    assert_eq!(options.threads, Some(4));
    assert_eq!(options.path, Some(PathBuf::from("/tmp")));
  }

  #[test]
  fn bad_command_lines_are_rejected() {
    assert!(parse(&["--summary"]).is_err());
    assert!(parse(&["--threads", "0"]).is_err());
    assert!(parse(&["--threads"]).is_err());
    assert!(parse(&["--nope"]).is_err());
    assert!(parse(&["/a", "/b"]).is_err());
  }
}
