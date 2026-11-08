//! Disk health read straight from the drives with kernel ioctls, without external programs.

use std::fs::{File, OpenOptions};
use std::io;
use std::mem::size_of;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::sync::mpsc::{self, Receiver};
use std::thread;

const NVME_IOCTL_ADMIN_CMD: u64 = 0xC048_4E41;
const NVME_GET_LOG_PAGE: u8 = 0x02;
const NVME_ALL_NAMESPACES: u32 = 0xFFFF_FFFF;
const NVME_HEALTH_LOG: u32 = 0x02;
const NVME_DATA_UNIT_BYTES: u128 = 512_000;

const SG_IO: u64 = 0x2285;
const SG_INTERFACE: i32 = 'S' as i32;
const SG_DXFER_NONE: i32 = -1;
const SG_DXFER_FROM_DEV: i32 = -3;
const SG_TIMEOUT_MS: u32 = 5000;

const ATA_PASS_THROUGH_16: u8 = 0x85;
const ATA_PROTOCOL_NON_DATA: u8 = 3;
const ATA_PROTOCOL_PIO_IN: u8 = 4;
const ATA_CHECK_POWER_MODE: u8 = 0xE5;
const ATA_SMART: u8 = 0xB0;
const SMART_READ_DATA: u8 = 0xD0;
const SMART_RETURN_STATUS: u8 = 0xDA;
const SMART_LBA_MID: u8 = 0x4F;
const SMART_LBA_HIGH: u8 = 0xC2;
const SMART_FAILING_LBA_MID: u8 = 0xF4;
const SMART_FAILING_LBA_HIGH: u8 = 0x2C;
const ATA_STATUS_ERROR: u8 = 0x01;
const SENSE_DESCRIPTOR_FORMAT: u8 = 0x72;
const SENSE_ATA_DESCRIPTOR: u8 = 0x09;
const SENSE_DESCRIPTOR_OFFSET: usize = 8;

const ATTRIBUTE_TABLE_OFFSET: usize = 2;
const ATTRIBUTE_SIZE: usize = 12;
const ATTRIBUTE_COUNT: usize = 30;
const ATA_SECTOR_BYTES: u64 = 512;

const ATTRIBUTE_REALLOCATED: u8 = 5;
const ATTRIBUTE_POWER_ON_HOURS: u8 = 9;
const ATTRIBUTE_SAMSUNG_WEAR: u8 = 177;
const ATTRIBUTE_AIRFLOW_TEMPERATURE: u8 = 190;
const ATTRIBUTE_TEMPERATURE: u8 = 194;
const ATTRIBUTE_PENDING: u8 = 197;
const ATTRIBUTE_UNCORRECTABLE: u8 = 198;
const ATTRIBUTE_CRC_ERRORS: u8 = 199;
const ATTRIBUTE_CRUCIAL_LIFE: u8 = 202;
const ATTRIBUTE_SAMSUNG_WRITTEN: u8 = 241;
const ATTRIBUTE_CRUCIAL_WRITTEN: u8 = 246;

const WEAR_WARNING_PERCENT: u64 = 80;
const WEAR_ALARM_PERCENT: u64 = 100;

/// The kernel's nvme_passthru_cmd structure.
#[repr(C)]
#[derive(Default)]
struct NvmeAdminCommand {
  opcode: u8,
  flags: u8,
  reserved: u16,
  nsid: u32,
  cdw2: u32,
  cdw3: u32,
  metadata: u64,
  addr: u64,
  metadata_len: u32,
  data_len: u32,
  cdw10: u32,
  cdw11: u32,
  cdw12: u32,
  cdw13: u32,
  cdw14: u32,
  cdw15: u32,
  timeout_ms: u32,
  result: u32,
}

/// The kernel's sg_io_hdr structure.
#[repr(C)]
struct SgIoHeader {
  interface_id: i32,
  dxfer_direction: i32,
  cmd_len: u8,
  mx_sb_len: u8,
  iovec_count: u16,
  dxfer_len: u32,
  dxferp: *mut u8,
  cmdp: *const u8,
  sbp: *mut u8,
  timeout: u32,
  flags: u32,
  pack_id: i32,
  usr_ptr: *mut u8,
  status: u8,
  masked_status: u8,
  msg_status: u8,
  sb_len_wr: u8,
  host_status: u16,
  driver_status: u16,
  resid: i32,
  duration: u32,
  info: u32,
}

const _: () = assert!(size_of::<NvmeAdminCommand>() == 72);
const _: () = assert!(size_of::<SgIoHeader>() == 88);

/// The health values a drive reported; a value it does not offer stays None.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
  pub critical: bool,
  pub wear_percent: Option<u64>,
  pub written_bytes: Option<u64>,
  pub power_on_hours: Option<u64>,
  pub spare_percent: Option<u64>,
  pub spare_low: bool,
  pub media_errors: Option<u64>,
  pub reallocated: Option<u64>,
  pub pending: Option<u64>,
  pub uncorrectable: Option<u64>,
  pub crc_errors: Option<u64>,
  pub temperature: Option<i64>,
}

/// The state of a disk's health information.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Health {
  Reading,
  NeedsRoot,
  Sleeping,
  Unavailable,
  Report(Report),
}

/// How much attention one health value needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
  Muted,
  Normal,
  Warning,
  Alarm,
}

/// One value of the health row; a higher rank is dropped first when space is short.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Field {
  pub text: String,
  pub level: Level,
  rank: u8,
}

/// Return the level of a counter that should stay at zero.
fn counter_level(value: u64, when_not_zero: Level) -> Level {
  if value == 0 {
    Level::Normal
  } else {
    when_not_zero
  }
}

/// Format written bytes in decimal terabytes, the unit drive ratings use.
fn format_terabytes(bytes: u64) -> String {
  let terabytes = bytes as f64 / 1e12;
  if terabytes >= 100.0 {
    format!("{terabytes:.0}T")
  } else {
    format!("{terabytes:.1}T")
  }
}

/// Return the values of a report in display order.
fn report_fields(report: &Report) -> Vec<Field> {
  let field = |text: String, level, rank| Field { text, level, rank };
  let mut fields = Vec::new();
  if report.critical {
    fields.push(field("CRITICAL".to_string(), Level::Alarm, 0));
  }
  if let Some(wear) = report.wear_percent {
    let level = match wear {
      WEAR_ALARM_PERCENT.. => Level::Alarm,
      WEAR_WARNING_PERCENT.. => Level::Warning,
      _ => Level::Normal,
    };
    fields.push(field(format!("Wear {wear}%"), level, 4));
  }
  if let Some(written) = report.written_bytes {
    fields.push(field(
      format!("TBW {}", format_terabytes(written)),
      Level::Normal,
      6,
    ));
  }
  if let Some(hours) = report.power_on_hours {
    fields.push(field(format!("{hours}h"), Level::Normal, 5));
  }
  if let Some(spare) = report.spare_percent {
    let level = if report.spare_low {
      Level::Alarm
    } else {
      Level::Normal
    };
    fields.push(field(format!("Spare {spare}%"), level, 8));
  }
  let counters = [
    ("Errors", report.media_errors, Level::Alarm, 1),
    ("Realloc", report.reallocated, Level::Alarm, 1),
    ("Pending", report.pending, Level::Alarm, 2),
    ("Uncorr", report.uncorrectable, Level::Alarm, 3),
    ("CRC", report.crc_errors, Level::Warning, 7),
  ];
  for (name, value, level, rank) in counters {
    if let Some(value) = value {
      fields.push(field(
        format!("{name} {value}"),
        counter_level(value, level),
        rank,
      ));
    }
  }
  if let Some(temperature) = report.temperature {
    fields.push(field(format!("{temperature}°C"), Level::Normal, 9));
  }
  fields
}

/// Return what the health row shows for a disk.
pub fn health_fields(health: &Health) -> Vec<Field> {
  let status = |text: &str| {
    vec![Field {
      text: text.to_string(),
      level: Level::Muted,
      rank: 0,
    }]
  };
  match health {
    Health::Reading => status("SMART: reading"),
    Health::NeedsRoot => status("SMART: needs root"),
    Health::Sleeping => status("SMART: disk asleep"),
    Health::Unavailable => status("SMART: n/a"),
    Health::Report(report) => report_fields(report),
  }
}

/// Drop the least important fields until the rest fit, giving up values without a problem first.
pub fn fit_fields(mut fields: Vec<Field>, width: usize) -> Vec<Field> {
  let needed = |fields: &[Field]| {
    fields
      .iter()
      .map(|field| field.text.chars().count())
      .sum::<usize>()
      + 2 * fields.len().saturating_sub(1)
  };
  while fields.len() > 1 && needed(&fields) > width {
    let least = (0..fields.len())
      .max_by_key(|&index| (fields[index].level == Level::Normal, fields[index].rank))
      .unwrap_or(0);
    fields.remove(least);
  }
  fields
}

/// Return a little-endian number from part of a log page.
fn little_endian(bytes: &[u8]) -> u128 {
  bytes
    .iter()
    .rev()
    .fold(0, |value, &byte| (value << 8) | byte as u128)
}

/// Convert an NVMe SMART / Health log page to a report.
fn parse_nvme_log(log: &[u8; 512]) -> Report {
  let kelvin = little_endian(&log[1..3]) as i64;
  let written = little_endian(&log[48..64]).saturating_mul(NVME_DATA_UNIT_BYTES);
  Report {
    critical: log[0] != 0,
    wear_percent: Some(log[5] as u64),
    written_bytes: Some(written.min(u64::MAX as u128) as u64),
    power_on_hours: Some(little_endian(&log[128..144]).min(u64::MAX as u128) as u64),
    spare_percent: Some(log[3] as u64),
    spare_low: log[3] < log[4],
    media_errors: Some(little_endian(&log[160..176]).min(u64::MAX as u128) as u64),
    temperature: (kelvin > 0).then_some(kelvin - 273),
    ..Report::default()
  }
}

/// Return the normalized value and the raw value of a SMART attribute.
fn attribute(page: &[u8; 512], id: u8) -> Option<(u8, u64)> {
  (0..ATTRIBUTE_COUNT)
    .map(|index| &page[ATTRIBUTE_TABLE_OFFSET + index * ATTRIBUTE_SIZE..][..ATTRIBUTE_SIZE])
    .find(|entry| entry[0] == id)
    .map(|entry| (entry[3], little_endian(&entry[5..11]) as u64))
}

/// Return the raw value of a counter attribute; some vendors put other data above bit 31.
fn counter(page: &[u8; 512], id: u8) -> Option<u64> {
  attribute(page, id).map(|(_, raw)| raw & 0xFFFF_FFFF)
}

/// Return wear and written bytes for SSD families whose attribute meaning is known.
fn known_ssd_counters(model: &str, page: &[u8; 512]) -> (Option<u64>, Option<u64>) {
  let model = model.to_lowercase();
  let (wear_id, written_id) = if model.contains("samsung ssd") {
    (ATTRIBUTE_SAMSUNG_WEAR, ATTRIBUTE_SAMSUNG_WRITTEN)
  } else if model.contains("crucial") || model.contains("micron") || model.contains(" ct") {
    (ATTRIBUTE_CRUCIAL_LIFE, ATTRIBUTE_CRUCIAL_WRITTEN)
  } else {
    return (None, None);
  };
  let wear =
    attribute(page, wear_id).map(|(life_left, _)| 100_u64.saturating_sub(life_left as u64));
  let written = attribute(page, written_id).map(|(_, sectors)| sectors * ATA_SECTOR_BYTES);
  (wear, written)
}

/// Convert an ATA SMART data page to a report, refusing a page with a bad checksum.
fn parse_ata_page(page: &[u8; 512], model: &str) -> Option<Report> {
  if page.iter().fold(0_u8, |sum, &byte| sum.wrapping_add(byte)) != 0 {
    return None;
  }
  let (wear_percent, written_bytes) = known_ssd_counters(model, page);
  let temperature = attribute(page, ATTRIBUTE_TEMPERATURE)
    .or_else(|| attribute(page, ATTRIBUTE_AIRFLOW_TEMPERATURE))
    .map(|(_, raw)| (raw & 0xFF) as i64);
  Some(Report {
    wear_percent,
    written_bytes,
    power_on_hours: counter(page, ATTRIBUTE_POWER_ON_HOURS),
    reallocated: counter(page, ATTRIBUTE_REALLOCATED),
    pending: counter(page, ATTRIBUTE_PENDING),
    uncorrectable: counter(page, ATTRIBUTE_UNCORRECTABLE),
    crc_errors: counter(page, ATTRIBUTE_CRC_ERRORS),
    temperature,
    ..Report::default()
  })
}

/// Return the health state that a failed system call means.
fn health_of_error(error: &io::Error) -> Health {
  if error.kind() == io::ErrorKind::PermissionDenied {
    Health::NeedsRoot
  } else {
    Health::Unavailable
  }
}

/// Open a block device for read-only commands.
fn open_device(name: &str) -> io::Result<File> {
  OpenOptions::new()
    .read(true)
    .custom_flags(libc::O_NONBLOCK)
    .open(format!("/dev/{name}"))
}

/// Read the SMART / Health log of an NVMe drive.
fn read_nvme(name: &str) -> Health {
  let file = match open_device(name) {
    Ok(file) => file,
    Err(error) => return health_of_error(&error),
  };
  let mut log = [0_u8; 512];
  let mut command = NvmeAdminCommand {
    opcode: NVME_GET_LOG_PAGE,
    nsid: NVME_ALL_NAMESPACES,
    addr: log.as_mut_ptr() as u64,
    data_len: log.len() as u32,
    cdw10: ((log.len() as u32 / 4 - 1) << 16) | NVME_HEALTH_LOG,
    ..NvmeAdminCommand::default()
  };
  // SAFETY: command has the layout the ioctl expects and log outlives the call.
  let result = unsafe { libc::ioctl(file.as_raw_fd(), NVME_IOCTL_ADMIN_CMD as _, &mut command) };
  match result {
    0 => Health::Report(parse_nvme_log(&log)),
    1.. => Health::Unavailable,
    _ => health_of_error(&io::Error::last_os_error()),
  }
}

/// Build an ATA PASS-THROUGH (16) command; a non-data command returns the ATA registers.
fn ata_command(protocol: u8, feature: u8, lba_mid: u8, lba_high: u8, command: u8) -> [u8; 16] {
  let mut cdb = [0_u8; 16];
  cdb[0] = ATA_PASS_THROUGH_16;
  cdb[1] = protocol << 1;
  if protocol == ATA_PROTOCOL_PIO_IN {
    cdb[2] = 0x0E;
    cdb[6] = 1;
  } else {
    cdb[2] = 0x20;
  }
  cdb[4] = feature;
  cdb[10] = lba_mid;
  cdb[12] = lba_high;
  cdb[14] = command;
  cdb
}

/// Send one SCSI command and return its SCSI status and sense data.
fn send_scsi(
  fd: RawFd,
  cdb: &[u8; 16],
  data: Option<&mut [u8; 512]>,
) -> io::Result<(u8, [u8; 32])> {
  let mut sense = [0_u8; 32];
  let (direction, pointer, length) = match data {
    Some(buffer) => (SG_DXFER_FROM_DEV, buffer.as_mut_ptr(), buffer.len() as u32),
    None => (SG_DXFER_NONE, std::ptr::null_mut(), 0),
  };
  let mut header = SgIoHeader {
    interface_id: SG_INTERFACE,
    dxfer_direction: direction,
    cmd_len: cdb.len() as u8,
    mx_sb_len: sense.len() as u8,
    iovec_count: 0,
    dxfer_len: length,
    dxferp: pointer,
    cmdp: cdb.as_ptr(),
    sbp: sense.as_mut_ptr(),
    timeout: SG_TIMEOUT_MS,
    flags: 0,
    pack_id: 0,
    usr_ptr: std::ptr::null_mut(),
    status: 0,
    masked_status: 0,
    msg_status: 0,
    sb_len_wr: 0,
    host_status: 0,
    driver_status: 0,
    resid: 0,
    duration: 0,
    info: 0,
  };
  // SAFETY: header has the layout the ioctl expects and every buffer outlives the call.
  if unsafe { libc::ioctl(fd, SG_IO as _, &mut header) } < 0 {
    return Err(io::Error::last_os_error());
  }
  Ok((header.status, sense))
}

/// The ATA registers returned by a non-data command.
struct AtaRegisters {
  count: u8,
  lba_mid: u8,
  lba_high: u8,
}

/// Extract the ATA registers from descriptor format sense data, unless the command failed.
fn ata_registers(sense: &[u8; 32]) -> Option<AtaRegisters> {
  let descriptor = &sense[SENSE_DESCRIPTOR_OFFSET..];
  let valid = sense[0] & 0x7F == SENSE_DESCRIPTOR_FORMAT && descriptor[0] == SENSE_ATA_DESCRIPTOR;
  (valid && descriptor[13] & ATA_STATUS_ERROR == 0).then(|| AtaRegisters {
    count: descriptor[5],
    lba_mid: descriptor[9],
    lba_high: descriptor[11],
  })
}

/// Read the SMART data of an ATA drive, leaving it asleep when it reports being in standby.
fn read_ata(name: &str, model: &str) -> io::Result<Health> {
  let file = open_device(name)?;
  let fd = file.as_raw_fd();
  let power = ata_command(ATA_PROTOCOL_NON_DATA, 0, 0, 0, ATA_CHECK_POWER_MODE);
  // many USB bridges return no ATA registers, yet they still pass SMART commands through
  if ata_registers(&send_scsi(fd, &power, None)?.1).is_some_and(|power| power.count <= 1) {
    return Ok(Health::Sleeping);
  }
  let mut page = [0_u8; 512];
  let read = ata_command(
    ATA_PROTOCOL_PIO_IN,
    SMART_READ_DATA,
    SMART_LBA_MID,
    SMART_LBA_HIGH,
    ATA_SMART,
  );
  if send_scsi(fd, &read, Some(&mut page))?.0 != 0 {
    return Ok(Health::Unavailable);
  }
  let Some(mut report) = parse_ata_page(&page, model) else {
    return Ok(Health::Unavailable);
  };
  let status = ata_command(
    ATA_PROTOCOL_NON_DATA,
    SMART_RETURN_STATUS,
    SMART_LBA_MID,
    SMART_LBA_HIGH,
    ATA_SMART,
  );
  report.critical = ata_registers(&send_scsi(fd, &status, None)?.1)
    .is_some_and(|r| r.lba_mid == SMART_FAILING_LBA_MID && r.lba_high == SMART_FAILING_LBA_HIGH);
  Ok(Health::Report(report))
}

/// Read the health of one disk.
pub fn read_health(name: &str, model: &str) -> Health {
  if name.starts_with("nvme") {
    read_nvme(name)
  } else if name.starts_with("sd") {
    read_ata(name, model).unwrap_or_else(|error| health_of_error(&error))
  } else {
    Health::Unavailable
  }
}

/// Read the health of every (name, model) disk in the background, so that drawing never waits.
pub fn read_in_background(disks: Vec<(String, String)>) -> Receiver<(String, Health)> {
  let (sender, receiver) = mpsc::channel();
  for (name, model) in disks {
    let sender = sender.clone();
    thread::spawn(move || {
      let health = read_health(&name, &model);
      let _ = sender.send((name, health));
    });
  }
  receiver
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Put one attribute in a page.
  fn set_attribute(page: &mut [u8; 512], slot: usize, id: u8, current: u8, raw: u64) {
    let entry = &mut page[ATTRIBUTE_TABLE_OFFSET + slot * ATTRIBUTE_SIZE..][..ATTRIBUTE_SIZE];
    entry[0] = id;
    entry[3] = current;
    entry[5..11].copy_from_slice(&raw.to_le_bytes()[..6]);
  }

  /// Make the page checksum valid.
  fn seal(page: &mut [u8; 512]) {
    page[511] = 0;
    page[511] = 0_u8.wrapping_sub(page.iter().fold(0_u8, |sum, &byte| sum.wrapping_add(byte)));
  }

  #[test]
  fn nvme_log_follows_the_standard_layout() {
    let mut log = [0_u8; 512];
    log[1..3].copy_from_slice(&314_u16.to_le_bytes());
    log[3] = 100;
    log[4] = 10;
    log[5] = 12;
    log[48..56].copy_from_slice(&1_000_000_u64.to_le_bytes());
    log[128..136].copy_from_slice(&22_310_u64.to_le_bytes());
    log[160..168].copy_from_slice(&3_u64.to_le_bytes());
    let report = parse_nvme_log(&log);
    assert_eq!(report.temperature, Some(41));
    assert_eq!(report.spare_percent, Some(100));
    assert!(!report.spare_low);
    assert_eq!(report.wear_percent, Some(12));
    assert_eq!(report.written_bytes, Some(512_000_000_000));
    assert_eq!(report.power_on_hours, Some(22_310));
    assert_eq!(report.media_errors, Some(3));
    assert!(!report.critical);
  }

  #[test]
  fn nvme_warnings_are_noticed() {
    let mut log = [0_u8; 512];
    log[0] = 1;
    log[3] = 5;
    log[4] = 10;
    let report = parse_nvme_log(&log);
    assert!(report.critical);
    assert!(report.spare_low);
    assert_eq!(report.temperature, None);
  }

  #[test]
  fn ata_page_reads_the_common_attributes() {
    let mut page = [0_u8; 512];
    set_attribute(&mut page, 0, ATTRIBUTE_REALLOCATED, 100, 7);
    set_attribute(&mut page, 1, ATTRIBUTE_POWER_ON_HOURS, 60, 38_201);
    set_attribute(&mut page, 2, ATTRIBUTE_TEMPERATURE, 62, 0x0014_0026);
    set_attribute(&mut page, 3, ATTRIBUTE_PENDING, 100, 1);
    set_attribute(&mut page, 4, ATTRIBUTE_UNCORRECTABLE, 100, 2);
    set_attribute(&mut page, 5, ATTRIBUTE_CRC_ERRORS, 100, 0xABCD_0000_0009);
    seal(&mut page);
    let report = parse_ata_page(&page, "ATA WDC WD10JPVT").unwrap();
    assert_eq!(report.reallocated, Some(7));
    assert_eq!(report.power_on_hours, Some(38_201));
    assert_eq!(report.temperature, Some(38));
    assert_eq!(report.pending, Some(1));
    assert_eq!(report.uncorrectable, Some(2));
    assert_eq!(report.crc_errors, Some(9));
    assert_eq!(report.wear_percent, None);
    assert_eq!(report.written_bytes, None);
  }

  #[test]
  fn ssd_wear_is_shown_only_for_known_families() {
    let mut page = [0_u8; 512];
    set_attribute(&mut page, 0, ATTRIBUTE_SAMSUNG_WEAR, 92, 55);
    set_attribute(&mut page, 1, ATTRIBUTE_SAMSUNG_WRITTEN, 99, 1000);
    seal(&mut page);
    let known = parse_ata_page(&page, "ATA Samsung SSD 870 EVO").unwrap();
    assert_eq!(known.wear_percent, Some(8));
    assert_eq!(known.written_bytes, Some(512_000));
    let unknown = parse_ata_page(&page, "ATA Other SSD").unwrap();
    assert_eq!(unknown.wear_percent, None);
    assert_eq!(unknown.written_bytes, None);
  }

  #[test]
  fn ata_page_with_a_bad_checksum_is_refused() {
    let mut page = [0_u8; 512];
    page[100] = 1;
    assert_eq!(parse_ata_page(&page, "ATA X"), None);
  }

  #[test]
  fn ata_commands_match_the_sat_layout() {
    let read = ata_command(
      ATA_PROTOCOL_PIO_IN,
      SMART_READ_DATA,
      SMART_LBA_MID,
      SMART_LBA_HIGH,
      ATA_SMART,
    );
    assert_eq!(
      read,
      [
        0x85, 0x08, 0x0E, 0, 0xD0, 0, 1, 0, 0, 0, 0x4F, 0, 0xC2, 0, 0xB0, 0
      ]
    );
    let power = ata_command(ATA_PROTOCOL_NON_DATA, 0, 0, 0, ATA_CHECK_POWER_MODE);
    assert_eq!(
      power,
      [0x85, 0x06, 0x20, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xE5, 0]
    );
  }

  #[test]
  fn ata_registers_come_from_descriptor_sense() {
    let mut sense = [0_u8; 32];
    sense[0] = 0x72;
    sense[8] = 0x09;
    sense[8 + 5] = 0xFF;
    sense[8 + 9] = 0xF4;
    sense[8 + 11] = 0x2C;
    let registers = ata_registers(&sense).unwrap();
    assert_eq!(
      (registers.count, registers.lba_mid, registers.lba_high),
      (0xFF, 0xF4, 0x2C)
    );
    sense[8 + 13] = ATA_STATUS_ERROR;
    assert!(ata_registers(&sense).is_none());
    assert!(ata_registers(&[0; 32]).is_none());
  }

  /// Return the text of fields.
  fn texts(fields: &[Field]) -> Vec<&str> {
    fields.iter().map(|field| field.text.as_str()).collect()
  }

  #[test]
  fn narrow_rows_drop_temperature_first_and_errors_last() {
    let report = Report {
      wear_percent: Some(12),
      written_bytes: Some(143_000_000_000_000),
      power_on_hours: Some(22_310),
      spare_percent: Some(100),
      media_errors: Some(0),
      temperature: Some(41),
      ..Report::default()
    };
    let all = report_fields(&report);
    assert_eq!(
      texts(&all),
      [
        "Wear 12%",
        "TBW 143T",
        "22310h",
        "Spare 100%",
        "Errors 0",
        "41°C"
      ]
    );
    assert_eq!(
      texts(&fit_fields(all.clone(), 48)),
      ["Wear 12%", "TBW 143T", "22310h", "Spare 100%", "Errors 0"]
    );
    assert_eq!(
      texts(&fit_fields(all.clone(), 20)),
      ["Wear 12%", "Errors 0"]
    );
    assert_eq!(texts(&fit_fields(all, 3)), ["Errors 0"]);
  }

  #[test]
  fn a_value_with_a_problem_outlasts_healthy_ones() {
    let report = Report {
      power_on_hours: Some(9323),
      reallocated: Some(0),
      crc_errors: Some(2),
      ..Report::default()
    };
    let kept = fit_fields(report_fields(&report), 12);
    assert_eq!(texts(&kept), ["CRC 2"]);
  }

  #[test]
  fn problems_raise_the_level() {
    let level = |report: Report| report_fields(&report)[0].level;
    let zero = Report {
      reallocated: Some(0),
      ..Report::default()
    };
    assert_eq!(level(zero), Level::Normal);
    let bad = Report {
      reallocated: Some(1),
      ..Report::default()
    };
    assert_eq!(level(bad), Level::Alarm);
    let cable = Report {
      crc_errors: Some(4),
      ..Report::default()
    };
    assert_eq!(level(cable), Level::Warning);
    let worn = Report {
      wear_percent: Some(85),
      ..Report::default()
    };
    assert_eq!(level(worn), Level::Warning);
    let dead = Report {
      wear_percent: Some(100),
      ..Report::default()
    };
    assert_eq!(level(dead), Level::Alarm);
  }

  #[test]
  fn unknown_devices_have_no_health() {
    assert_eq!(read_health("vda", "x"), Health::Unavailable);
  }
}
