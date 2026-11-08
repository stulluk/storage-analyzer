//! Physical disks and partitions, read straight from sysfs, procfs and statvfs.

use crate::smart::Health;
use std::fs;
use std::path::{Path, PathBuf};

pub const SWAP_MOUNT: &str = "[SWAP]";
pub const UNALLOCATED: &str = "unallocated";
const IGNORED_DISK_PREFIXES: [&str; 5] = ["zram", "ram", "loop", "sr", "fd"];
const PCIE_LANE_GBPS: [f64; 5] = [0.250, 0.500, 0.985, 1.969, 3.938];
const SECTOR_BYTES: u64 = 512;

/// One box inside a disk: a partition, a whole-disk filesystem or free space.
#[derive(Clone, Debug, Default)]
pub struct Region {
  pub name: String,
  pub size: u64,
  pub mountpoint: Option<String>,
  pub fstype: Option<String>,
  pub total: Option<u64>,
  pub used: Option<u64>,
}

impl Region {
  /// Return the used percentage, or None when usage is unknown.
  pub fn percent(&self) -> Option<f64> {
    match (self.total, self.used) {
      (Some(total), Some(used)) if total > 0 => Some(100.0 * used as f64 / total as f64),
      _ => None,
    }
  }

  /// Return the mount point when the region is a filesystem that can be browsed.
  pub fn browsable_mount(&self) -> Option<&str> {
    self
      .mountpoint
      .as_deref()
      .filter(|&mount| mount != SWAP_MOUNT)
  }
}

/// A physical disk with its regions, connection details and health.
#[derive(Clone, Debug)]
pub struct Disk {
  pub name: String,
  pub size: u64,
  pub model: String,
  pub bus: String,
  pub link: String,
  pub regions: Vec<Region>,
  pub health: Health,
}

/// Return the trimmed text of a sysfs file, or an empty string on failure.
fn read_sysfs_text(path: impl AsRef<Path>) -> String {
  fs::read_to_string(path)
    .map(|text| text.trim().to_string())
    .unwrap_or_default()
}

/// Return the number at the start of a string such as "16.0 GT/s PCIe".
fn leading_number(text: &str) -> Option<f64> {
  let end = text
    .find(|c: char| !c.is_ascii_digit() && c != '.')
    .unwrap_or(text.len());
  text[..end].parse().ok()
}

/// Map a max_link_speed line to a PCIe generation from 1 to 5.
fn pcie_generation(speed_line: &str) -> Option<usize> {
  let rate = leading_number(speed_line.trim())?;
  Some(
    [4.0, 6.0, 10.0, 20.0]
      .iter()
      .position(|&limit| rate < limit)
      .unwrap_or(4)
      + 1,
  )
}

/// Return a static ~GB/s hint for a PCIe generation and lane count.
fn pcie_throughput_label(generation: usize, lanes: u32) -> String {
  let total = PCIE_LANE_GBPS[generation - 1] * lanes as f64;
  if total >= 10.0 {
    format!("~{total:.0} GB/s")
  } else if total >= 1.0 {
    format!("~{total:.1} GB/s")
  } else {
    format!("~{total:.2} GB/s")
  }
}

/// Return the maximum PCIe link of an NVMe namespace such as nvme0n1.
fn nvme_link(name: &str) -> Option<String> {
  let controller = &name[..name.rfind('n').filter(|&index| index > 3)?];
  let base = format!("/sys/class/nvme/{controller}/device");
  let speed = read_sysfs_text(format!("{base}/max_link_speed"));
  let width = read_sysfs_text(format!("{base}/max_link_width"));
  let generation = pcie_generation(&speed)?;
  let lanes: u32 = width.parse().ok()?;
  Some(format!(
    "PCIe Gen{generation} x{width} ({})",
    pcie_throughput_label(generation, lanes)
  ))
}

/// Read the negotiated sata_spd of an sdX disk from sysfs.
fn sata_speed_raw(name: &str) -> Option<String> {
  let device = fs::canonicalize(format!("/sys/block/{name}/device")).ok()?;
  let device = device.to_string_lossy().into_owned();
  let host_start = device.find("/host")?;
  let host_end = device[host_start + 1..].find('/')? + host_start + 1;
  let host = &device[host_start + 5..host_end];
  let ata_dir = &device[..host_start];
  let host_dir = format!("/sys/class/scsi_host/host{host}");
  let mut port = read_sysfs_text(format!("{host_dir}/port_no"));
  if port.parse::<u32>().is_err() {
    port = read_sysfs_text(format!("{host_dir}/unique_id"));
  }
  if port.parse::<u32>().is_ok() {
    let speed = read_sysfs_text(format!("{ata_dir}/link{port}/ata_link/link{port}/sata_spd"));
    if !speed.is_empty() {
      return Some(speed);
    }
  }
  let speeds: Vec<String> = fs::read_dir(ata_dir)
    .ok()?
    .flatten()
    .map(|entry| entry.file_name().to_string_lossy().into_owned())
    .filter(|entry| entry.starts_with("link"))
    .map(|entry| read_sysfs_text(format!("{ata_dir}/{entry}/ata_link/{entry}/sata_spd")))
    .filter(|speed| !speed.is_empty() && speed != "<unknown>")
    .collect();
  (speeds.len() == 1).then(|| speeds[0].clone())
}

/// Return the human readable SATA link speed of a disk.
fn sata_link(name: &str) -> String {
  let Some(raw) = sata_speed_raw(name).filter(|raw| raw != "<unknown>") else {
    return "link n/a".to_string();
  };
  let Some(rate) = leading_number(&raw) else {
    return raw;
  };
  let generation = [(1.6, "I"), (3.1, "II"), (6.1, "III")]
    .iter()
    .find(|(limit, _)| rate <= *limit);
  generation.map_or(raw.clone(), |(_, label)| format!("{raw} (SATA {label})"))
}

/// Walk sysfs upwards from an sdX disk to find its USB speed in Mbps.
fn usb_speed_mbps(name: &str) -> Option<u32> {
  let device = fs::canonicalize(format!("/sys/block/{name}/device")).ok()?;
  device
    .ancestors()
    .find_map(|dir| read_sysfs_text(dir.join("speed")).parse().ok())
}

/// Return a rough nominal USB link rate for a disk.
fn usb_link(name: &str) -> String {
  match usb_speed_mbps(name) {
    None => "~5 Gb/s",
    Some(0..=12) => "USB 1.x",
    Some(13..=480) => "USB 2 (480 Mb/s)",
    Some(481..=5000) => "~5 Gb/s",
    Some(5001..=10000) => "~10 Gb/s",
    Some(10001..=20000) => "~20 Gb/s",
    Some(_) => "~40+ Gb/s",
  }
  .to_string()
}

/// Return the nominal link speed text for a disk transport.
fn link_speed(transport: &str, name: &str) -> String {
  match transport {
    "nvme" => nvme_link(name).unwrap_or_else(|| "NVMe".to_string()),
    "sata" => sata_link(name),
    "usb" => usb_link(name),
    "sas" => "12 Gb/s".to_string(),
    _ => "-".to_string(),
  }
}

/// Return the display name of a disk transport.
fn bus_name(transport: &str) -> String {
  match transport {
    "nvme" => "NVMe".to_string(),
    "sata" => "SATA".to_string(),
    "usb" => "USB".to_string(),
    "sas" => "SAS".to_string(),
    "iscsi" => "iSCSI".to_string(),
    "fc" => "FC".to_string(),
    "" => "-".to_string(),
    other => other.to_uppercase(),
  }
}

/// Return the bus type of a disk from its device path in sysfs.
fn transport(name: &str, device_path: &str) -> &'static str {
  let through = |part: &str| device_path.contains(part);
  if name.starts_with("nvme") {
    "nvme"
  } else if through("/usb") {
    "usb"
  } else if through("/ata") {
    "sata"
  } else if through("/end_device-") || through("/expander-") {
    "sas"
  } else if through("/rport-") {
    "fc"
  } else if through("/session") {
    "iscsi"
  } else {
    ""
  }
}

/// One line of the mount table.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Mount {
  device: String,
  root: String,
  point: String,
  fstype: String,
  source: String,
}

/// Decode the octal escapes that the kernel uses for spaces and similar characters in paths.
fn unescape_octal(text: &str) -> String {
  let bytes = text.as_bytes();
  let mut decoded = Vec::with_capacity(bytes.len());
  let mut index = 0;
  while index < bytes.len() {
    let octal = bytes.get(index + 1..index + 4).filter(|digits| {
      bytes[index] == b'\\' && digits.iter().all(|digit| (b'0'..=b'7').contains(digit))
    });
    if let Some(digits) = octal {
      decoded.push(digits.iter().fold(0_u8, |value, digit| {
        value.wrapping_mul(8).wrapping_add(digit - b'0')
      }));
      index += 4;
    } else {
      decoded.push(bytes[index]);
      index += 1;
    }
  }
  String::from_utf8_lossy(&decoded).into_owned()
}

/// Parse the text of /proc/self/mountinfo.
fn parse_mountinfo(text: &str) -> Vec<Mount> {
  text
    .lines()
    .filter_map(|line| {
      let fields: Vec<&str> = line.split(' ').collect();
      let separator = fields.iter().position(|&field| field == "-")?;
      Some(Mount {
        device: (*fields.get(2)?).to_string(),
        root: unescape_octal(fields.get(3)?),
        point: unescape_octal(fields.get(4)?),
        fstype: (*fields.get(separator + 1)?).to_string(),
        source: unescape_octal(fields.get(separator + 2)?),
      })
    })
    .collect()
}

/// Parse /proc/swaps into block device names.
fn parse_swaps(text: &str) -> Vec<String> {
  text
    .lines()
    .skip(1)
    .filter_map(|line| line.split_whitespace().next())
    .map(unescape_octal)
    .filter_map(|path| {
      let real = fs::canonicalize(&path).unwrap_or_else(|_| PathBuf::from(&path));
      Some(real.file_name()?.to_string_lossy().into_owned())
    })
    .collect()
}

/// Return (total, used) bytes of a mounted filesystem, or None if unavailable.
fn read_usage(mountpoint: &str) -> Option<(u64, u64)> {
  let status = rustix::fs::statvfs(mountpoint).ok()?;
  let total = status.f_blocks * status.f_frsize;
  (total > 0).then_some((total, (status.f_blocks - status.f_bfree) * status.f_frsize))
}

/// The kernel views that describe the block devices.
struct System<'a> {
  block: PathBuf,
  udev: PathBuf,
  mounts: Vec<Mount>,
  swaps: Vec<String>,
  usage: &'a dyn Fn(&str) -> Option<(u64, u64)>,
}

impl System<'_> {
  /// Return the best mount of one block device; the mount of its whole filesystem wins.
  fn device_mount(&self, name: &str, dir: &Path) -> Option<&Mount> {
    let number = read_sysfs_text(dir.join("dev"));
    let direct = format!("/dev/{name}");
    let mapped = format!("/dev/mapper/{}", read_sysfs_text(dir.join("dm/name")));
    self
      .mounts
      .iter()
      .filter(|mount| mount.device == number || mount.source == direct || mount.source == mapped)
      .min_by_key(|mount| (mount.root != "/", mount.point.len()))
  }

  /// Return where a device is used, looking through LVM, dm-crypt and RAID devices on top of it.
  fn placement(&self, name: &str, dir: &Path) -> Option<(String, String)> {
    if self.swaps.iter().any(|swap| swap == name) {
      return Some((SWAP_MOUNT.to_string(), "swap".to_string()));
    }
    if let Some(mount) = self.device_mount(name, dir) {
      return Some((mount.point.clone(), mount.fstype.clone()));
    }
    let mut holders: Vec<String> = fs::read_dir(dir.join("holders"))
      .ok()?
      .flatten()
      .map(|entry| entry.file_name().to_string_lossy().into_owned())
      .collect();
    holders.sort();
    holders
      .iter()
      .find_map(|holder| self.placement(holder, &self.block.join(holder)))
  }

  /// Return one property of a device from the udev database.
  fn udev_value(&self, dir: &Path, key: &str) -> Option<String> {
    let record = self
      .udev
      .join(format!("b{}", read_sysfs_text(dir.join("dev"))));
    let prefix = format!("E:{key}=");
    fs::read_to_string(record)
      .ok()?
      .lines()
      .find_map(|line| line.strip_prefix(prefix.as_str()).map(str::to_string))
      .filter(|value| !value.is_empty())
  }

  /// Return the size of a device in bytes.
  fn device_size(dir: &Path) -> u64 {
    read_sysfs_text(dir.join("size"))
      .parse::<u64>()
      .unwrap_or(0)
      * SECTOR_BYTES
  }

  /// Create a region from a partition or a whole disk.
  fn build_region(&self, name: &str, dir: &Path) -> Region {
    let placement = self.placement(name, dir);
    let mountpoint = placement.as_ref().map(|(point, _)| point.clone());
    let measured = mountpoint
      .as_deref()
      .filter(|&mount| mount != SWAP_MOUNT)
      .and_then(self.usage);
    Region {
      name: name.to_string(),
      size: Self::device_size(dir),
      mountpoint,
      fstype: self
        .udev_value(dir, "ID_FS_TYPE")
        .or_else(|| placement.map(|(_, fstype)| fstype)),
      total: measured.map(|(total, _)| total),
      used: measured.map(|(_, used)| used),
    }
  }

  /// Return the partitions of a disk as (number, name, directory), in partition order.
  fn partitions(dir: &Path) -> Vec<(u64, String, PathBuf)> {
    let mut found: Vec<(u64, String, PathBuf)> = fs::read_dir(dir)
      .into_iter()
      .flatten()
      .flatten()
      .filter_map(|entry| {
        let number = read_sysfs_text(entry.path().join("partition"))
          .parse()
          .ok()?;
        Some((
          number,
          entry.file_name().to_string_lossy().into_owned(),
          entry.path(),
        ))
      })
      .collect();
    found.sort();
    found
  }

  /// Create a disk, adding a box for notable free space.
  fn build_disk(&self, name: &str) -> Disk {
    let dir = self.block.join(name);
    let size = Self::device_size(&dir);
    let mut regions: Vec<Region> = Self::partitions(&dir)
      .iter()
      .map(|(_, part, part_dir)| self.build_region(part, part_dir))
      .collect();
    if regions.is_empty() {
      regions.push(self.build_region(name, &dir));
    }
    let free = size.saturating_sub(regions.iter().map(|region| region.size).sum());
    if free.saturating_mul(100) >= size {
      regions.push(Region {
        name: UNALLOCATED.to_string(),
        size: free,
        ..Region::default()
      });
    }
    let device_path = fs::canonicalize(&dir)
      .map(|path| path.to_string_lossy().into_owned())
      .unwrap_or_default();
    let bus = transport(name, &device_path);
    // sysfs cuts SCSI and SATA model names to 16 characters; udev has the whole name
    let full_model = self
      .udev_value(&dir, "ID_MODEL")
      .filter(|_| name.starts_with("sd"))
      .map(|model| model.replace('_', " "));
    let model = [
      read_sysfs_text(dir.join("device/vendor")),
      full_model.unwrap_or_else(|| read_sysfs_text(dir.join("device/model"))),
    ]
    .join(" ")
    .split_whitespace()
    .collect::<Vec<_>>()
    .join(" ");
    Disk {
      name: name.to_string(),
      size,
      model: if model.is_empty() {
        "-".to_string()
      } else {
        model
      },
      bus: bus_name(bus),
      link: link_speed(bus, name),
      regions,
      health: Health::Reading,
    }
  }

  /// Return every physical disk that has a capacity, largest first.
  fn disks(&self) -> Result<Vec<Disk>, String> {
    let listing =
      fs::read_dir(&self.block).map_err(|error| format!("{}: {error}", self.block.display()))?;
    let mut disks: Vec<Disk> = listing
      .flatten()
      .map(|entry| entry.file_name().to_string_lossy().into_owned())
      .filter(|name| {
        !IGNORED_DISK_PREFIXES
          .iter()
          .any(|prefix| name.starts_with(prefix))
      })
      .filter(|name| {
        let dir = self.block.join(name);
        dir.join("device").exists() && Self::device_size(&dir) > 0
      })
      .map(|name| self.build_disk(&name))
      .collect();
    disks.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.name.cmp(&b.name)));
    Ok(disks)
  }
}

/// Return every mount point the kernel knows about right now.
pub fn current_mount_points() -> Vec<String> {
  fs::read_to_string("/proc/self/mountinfo")
    .map(|text| {
      parse_mountinfo(&text)
        .into_iter()
        .map(|mount| mount.point)
        .collect()
    })
    .unwrap_or_default()
}

/// Read how full each mounted filesystem is again, after files have changed.
pub fn refresh_usage(disks: &mut [Disk]) {
  for region in disks.iter_mut().flat_map(|disk| &mut disk.regions) {
    if let Some((total, used)) = region.browsable_mount().and_then(read_usage) {
      region.total = Some(total);
      region.used = Some(used);
    }
  }
}

/// Return every physical disk of this machine that has a capacity, largest first.
pub fn read_disks() -> Result<Vec<Disk>, String> {
  let mountinfo = fs::read_to_string("/proc/self/mountinfo")
    .map_err(|error| format!("/proc/self/mountinfo: {error}"))?;
  System {
    block: PathBuf::from("/sys/block"),
    udev: PathBuf::from("/run/udev/data"),
    mounts: parse_mountinfo(&mountinfo),
    swaps: parse_swaps(&fs::read_to_string("/proc/swaps").unwrap_or_default()),
    usage: &read_usage,
  }
  .disks()
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::os::unix::fs as unix_fs;

  const GIB: u64 = 1 << 30;
  const SECTORS_PER_GIB: u64 = GIB / SECTOR_BYTES;

  /// Write a file, creating its parent folders.
  fn write(path: PathBuf, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
  }

  /// Create a fake block device directory with a size and a device number.
  fn device(dir: &Path, number: &str, gib: u64) {
    write(dir.join("size"), &format!("{}\n", gib * SECTORS_PER_GIB));
    write(dir.join("dev"), &format!("{number}\n"));
    fs::create_dir_all(dir.join("holders")).unwrap();
  }

  /// Return an empty folder that is unique to one test.
  fn test_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("sa-disks-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
  }

  #[test]
  fn reads_partitions_nested_mounts_swap_and_free_space() {
    let root = test_dir("full");
    let block = root.join("block");
    let real = root.join("devices/pci0/ata3/host2/block/sdz");
    device(&real, "8:0", 100);
    write(real.join("device/vendor"), "ATA     \n");
    write(real.join("device/model"), "Example  Disk\n");
    fs::create_dir_all(&block).unwrap();
    unix_fs::symlink(&real, block.join("sdz")).unwrap();
    for (index, number) in ["8:1", "8:2", "8:3", "8:4"].iter().enumerate() {
      let part = real.join(format!("sdz{}", index + 1));
      device(&part, number, 10);
      write(part.join("partition"), &format!("{}\n", index + 1));
    }
    let mapper = block.join("dm-0");
    device(&mapper, "253:0", 10);
    fs::create_dir_all(real.join("sdz4/holders/dm-0")).unwrap();
    device(&block.join("loop0"), "7:0", 1);
    let virtual_disk = block.join("zz-no-device");
    device(&virtual_disk, "9:0", 500);
    let udev = root.join("udev");
    write(udev.join("b8:3"), "E:ID_FS_TYPE=ntfs\n");
    write(udev.join("b8:0"), "E:ID_MODEL=Example_Disk_Long_Name\n");
    let mounts = parse_mountinfo(concat!(
      "21 1 8:1 / /data rw - ext4 /dev/sdz1 rw\n",
      "22 1 8:1 / / rw - ext4 /dev/sdz1 rw\n",
      "23 1 8:1 /sub /a/bind rw - ext4 /dev/sdz1 rw\n",
      "24 1 253:0 / /secret\\040place rw shared:1 - xfs /dev/mapper/vault rw\n",
    ));
    let usage = |_: &str| Some((10 * GIB, 5 * GIB));
    let system = System {
      block,
      udev,
      mounts,
      swaps: vec!["sdz2".to_string()],
      usage: &usage,
    };
    let disks = system.disks().unwrap();
    assert_eq!(disks.len(), 1);
    assert_eq!(disks[0].model, "ATA Example Disk Long Name");
    assert_eq!(disks[0].bus, "SATA");
    let regions = &disks[0].regions;
    let mounts: Vec<Option<&str>> = regions
      .iter()
      .map(|region| region.mountpoint.as_deref())
      .collect();
    assert_eq!(
      mounts,
      [Some("/"), Some("[SWAP]"), None, Some("/secret place"), None]
    );
    assert_eq!(regions[0].percent(), Some(50.0));
    assert_eq!(regions[1].percent(), None);
    assert_eq!(regions[1].browsable_mount(), None);
    assert_eq!(regions[2].fstype.as_deref(), Some("ntfs"));
    assert_eq!(regions[3].fstype.as_deref(), Some("xfs"));
    assert_eq!(regions[4].name, UNALLOCATED);
    assert_eq!(regions[4].size, 60 * GIB);
    fs::remove_dir_all(&root).unwrap();
  }

  #[test]
  fn btrfs_is_found_by_its_source_device() {
    let root = test_dir("btrfs");
    let block = root.join("block");
    let disk = block.join("nvme9n1");
    device(&disk, "259:0", 10);
    fs::create_dir_all(disk.join("device")).unwrap();
    let mounts = parse_mountinfo("30 1 0:33 /@ /home rw - btrfs /dev/nvme9n1 rw\n");
    let usage = |_: &str| None;
    let system = System {
      block,
      udev: root.join("udev"),
      mounts,
      swaps: Vec::new(),
      usage: &usage,
    };
    let disks = system.disks().unwrap();
    assert_eq!(disks[0].regions[0].mountpoint.as_deref(), Some("/home"));
    assert_eq!(disks[0].bus, "NVMe");
    fs::remove_dir_all(&root).unwrap();
  }

  #[test]
  fn transports_come_from_the_device_path() {
    assert_eq!(
      transport("sda", "/sys/devices/pci0/usb2/2-1/host5/block/sda"),
      "usb"
    );
    assert_eq!(
      transport("sda", "/sys/devices/pci0/ata1/host0/block/sda"),
      "sata"
    );
    assert_eq!(
      transport("sda", "/sys/devices/pci0/host0/end_device-0:0/block/sda"),
      "sas"
    );
    assert_eq!(transport("vda", "/sys/devices/pci0/virtio1/block/vda"), "");
  }

  #[test]
  fn pcie_generations_follow_the_transfer_rate() {
    assert_eq!(pcie_generation("8.0 GT/s PCIe"), Some(3));
    assert_eq!(pcie_generation("16.0 GT/s PCIe"), Some(4));
    assert_eq!(pcie_generation("32.0 GT/s PCIe"), Some(5));
    assert_eq!(pcie_generation("unknown"), None);
    assert_eq!(pcie_throughput_label(4, 4), "~7.9 GB/s");
  }
}
