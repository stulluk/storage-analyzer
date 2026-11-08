//! Physical disks and partitions, read from lsblk, statvfs and sysfs.

use serde_json::Value;
use std::fs;
use std::path::Path;
use std::process::Command;

pub const SWAP_MOUNT: &str = "[SWAP]";
pub const UNALLOCATED: &str = "unallocated";
const IGNORED_DISK_PREFIXES: [&str; 3] = ["zram", "ram", "loop"];
const PCIE_LANE_GBPS: [f64; 5] = [0.250, 0.500, 0.985, 1.969, 3.938];

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

/// A physical disk with its regions and connection details.
#[derive(Clone, Debug)]
pub struct Disk {
  pub name: String,
  pub size: u64,
  pub model: String,
  pub bus: String,
  pub link: String,
  pub regions: Vec<Region>,
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

/// Return a numeric lsblk field, which is a number or a string depending on the version.
fn number_field(node: &Value, key: &str) -> u64 {
  match &node[key] {
    Value::Number(number) => number.as_u64().unwrap_or(0),
    Value::String(text) => text.parse().unwrap_or(0),
    _ => 0,
  }
}

/// Return a text lsblk field, with runs of whitespace collapsed.
fn text_field(node: &Value, key: &str) -> Option<String> {
  let text = node[key]
    .as_str()?
    .split_whitespace()
    .collect::<Vec<_>>()
    .join(" ");
  (!text.is_empty()).then_some(text)
}

/// Return the child nodes of an lsblk node.
fn children(node: &Value) -> &[Value] {
  node["children"].as_array().map_or(&[], Vec::as_slice)
}

/// Return the shortest mountpoint of a device, else of its first mounted child.
fn find_mountpoint(node: &Value) -> Option<String> {
  let mounts = node["mountpoints"]
    .as_array()
    .map_or(&[][..], Vec::as_slice);
  let own = mounts
    .iter()
    .filter_map(Value::as_str)
    .min_by_key(|mount| mount.len());
  own
    .map(str::to_string)
    .or_else(|| children(node).iter().find_map(find_mountpoint))
}

/// Return (total, used) bytes of a mounted filesystem, or None if unavailable.
fn read_usage(mountpoint: &str) -> Option<(u64, u64)> {
  let status = rustix::fs::statvfs(mountpoint).ok()?;
  let total = status.f_blocks * status.f_frsize;
  (total > 0).then_some((total, (status.f_blocks - status.f_bfree) * status.f_frsize))
}

/// Create a region from an lsblk partition or whole-disk node.
fn build_region(node: &Value, usage: &impl Fn(&str) -> Option<(u64, u64)>) -> Region {
  let mountpoint = find_mountpoint(node);
  let measured = mountpoint
    .as_deref()
    .filter(|&mount| mount != SWAP_MOUNT)
    .and_then(usage);
  Region {
    name: text_field(node, "name").unwrap_or_default(),
    size: number_field(node, "size"),
    mountpoint,
    fstype: text_field(node, "fstype"),
    total: measured.map(|(total, _)| total),
    used: measured.map(|(_, used)| used),
  }
}

/// Create a disk from an lsblk node, adding a box for notable free space.
fn build_disk(node: &Value, usage: &impl Fn(&str) -> Option<(u64, u64)>) -> Disk {
  let name = text_field(node, "name").unwrap_or_default();
  let size = number_field(node, "size");
  let mut regions: Vec<Region> = children(node)
    .iter()
    .map(|child| build_region(child, usage))
    .collect();
  if regions.is_empty() {
    regions.push(build_region(node, usage));
  }
  let free = size.saturating_sub(regions.iter().map(|region| region.size).sum());
  if free.saturating_mul(100) >= size {
    regions.push(Region {
      name: UNALLOCATED.to_string(),
      size: free,
      ..Region::default()
    });
  }
  let transport = text_field(node, "tran").unwrap_or_default().to_lowercase();
  let model = [text_field(node, "vendor"), text_field(node, "model")]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" ");
  Disk {
    size,
    model: if model.is_empty() {
      "-".to_string()
    } else {
      model
    },
    bus: bus_name(&transport),
    link: link_speed(&transport, &name),
    name,
    regions,
  }
}

/// Convert lsblk JSON to physical disks, largest first.
fn parse_disks(
  json: &str,
  usage: &impl Fn(&str) -> Option<(u64, u64)>,
) -> Result<Vec<Disk>, String> {
  let root: Value = serde_json::from_str(json).map_err(|error| error.to_string())?;
  let nodes = root["blockdevices"]
    .as_array()
    .ok_or("lsblk output has no blockdevices")?;
  let mut disks: Vec<Disk> = nodes
    .iter()
    .filter(|node| node["type"].as_str() == Some("disk") && number_field(node, "size") > 0)
    .filter(|node| {
      let name = node["name"].as_str().unwrap_or_default();
      !IGNORED_DISK_PREFIXES
        .iter()
        .any(|prefix| name.starts_with(prefix))
    })
    .map(|node| build_disk(node, usage))
    .collect();
  disks.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.name.cmp(&b.name)));
  Ok(disks)
}

/// Return every physical disk that has a capacity, largest first.
pub fn read_disks() -> Result<Vec<Disk>, String> {
  let output = Command::new("lsblk")
    .args([
      "-J",
      "-b",
      "-o",
      "NAME,TYPE,SIZE,VENDOR,MODEL,TRAN,FSTYPE,MOUNTPOINTS",
    ])
    .output()
    .map_err(|error| format!("cannot run lsblk: {error}"))?;
  if !output.status.success() {
    return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
  }
  parse_disks(&String::from_utf8_lossy(&output.stdout), &read_usage)
}

#[cfg(test)]
mod tests {
  use super::*;

  const GIB: u64 = 1 << 30;

  #[test]
  fn parses_partitions_nested_mounts_swap_and_free_space() {
    let json = format!(
      r#"{{"blockdevices": [
        {{"name": "loop0", "type": "loop", "size": {gib}}},
        {{"name": "sdz", "type": "disk", "size": {disk}, "vendor": "ATA     ", "model": "Example",
          "tran": "scsi", "children": [
            {{"name": "sdz1", "size": {ten}, "fstype": "ext4", "mountpoints": ["/data", "/"]}},
            {{"name": "sdz2", "size": {ten}, "fstype": "swap", "mountpoints": ["[SWAP]"]}},
            {{"name": "sdz3", "size": {ten}, "fstype": "ntfs", "mountpoints": [null]}},
            {{"name": "sdz4", "size": {ten}, "mountpoints": [null], "children": [
              {{"name": "dm-0", "size": {ten}, "mountpoints": ["/secret"]}}]}}
          ]}}
      ]}}"#,
      gib = GIB,
      disk = 100 * GIB,
      ten = 10 * GIB,
    );
    let disks = parse_disks(&json, &|_| Some((10 * GIB, 5 * GIB))).unwrap();
    assert_eq!(disks.len(), 1);
    assert_eq!(disks[0].model, "ATA Example");
    assert_eq!(disks[0].bus, "SCSI");
    let mounts: Vec<Option<&str>> = disks[0]
      .regions
      .iter()
      .map(|region| region.mountpoint.as_deref())
      .collect();
    assert_eq!(
      mounts,
      [Some("/"), Some("[SWAP]"), None, Some("/secret"), None]
    );
    assert_eq!(disks[0].regions[0].percent(), Some(50.0));
    assert_eq!(disks[0].regions[1].percent(), None);
    assert_eq!(disks[0].regions[1].browsable_mount(), None);
    assert_eq!(disks[0].regions[4].name, UNALLOCATED);
    assert_eq!(disks[0].regions[4].size, 60 * GIB);
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
