# storage-analyzer

`storage-analyzer` shows where your disk space went, as boxes you can walk through in the terminal.
It starts on a map of your physical disks and partitions, sized by capacity and filled with `#` by
usage. Open a partition and its folders become boxes sized by what they occupy; open a folder to go
deeper. It is a single static binary and it only reads: nothing can be deleted or moved from it.

```text
/home/user/storage-analyzer  347M          347M  8835 files  scanned in 0.0s
┌────────────────────────────────────────────────────────┬──────────────────────────┬──────────┐
│ target/  212M  61.2%                                   │ .docker-cargo/           │ (10 more)│
│##################################......................│ 134M  38.5%              │ 0.1%     │
│##################################......................│##########................│..........│
│##################################......................│##########................│..........│
│##################################......................│##########................│..........│
│##################################......................│##########................│..........│
│##################################......................│##########................│..........│
│##################################......................│##########................│..........│
│##################################......................│##########................│..........│
│##################################......................│##########................│..........│
│##################################......................│##########................├──────────┤
│##################################......................│##########................│ out/     │
└────────────────────────────────────────────────────────┴──────────────────────────┴──────────┘
target/  212M  61.2%                        arrows/hjkl move  Enter open  Backspace back  q quit
```

## How it reads

- Disk map: every disk is a box sized by its capacity and shows its model, bus and maximum link
  speed. Partitions are boxes inside it, and `#` is used space.
- Folder view: every box is a child of the folder on screen, and `#` is its share of that folder.
  Folders are written with a trailing `/`.
- Boxes that would be too small to read are merged into one `(N more)` box.
- The map fills in while the scan runs; you can walk through it before the scan ends. For a
  partition opened from the disk map, the top right shows how much of its used space was found.
- A scan that finished is kept while the program runs, so going back and forth is instant. Opening
  another partition stops a scan that has not finished.

## What is counted

- The space really occupied on disk (`st_blocks`), like `du`, not the apparent file size.
- Hard links count once. Which of the paths receives the size depends on which is seen first.
- A scan stays on one filesystem: other filesystems mounted below it are skipped.
- A folder that cannot be read counts as `unreadable` at the top right. Run as root to see them.
- Each folder keeps the 128 largest files by name. The others stay in the totals as one
  merged entry, which keeps memory low on very large trees.

## Scan speed

A scan uses 80% of the CPU cores on a solid state disk. On a rotating disk it uses one thread,
to avoid constant head movement; `--threads` overrides this.

Measured on one machine (32 threads, NVMe, ext4, files already in the page cache) against
gdu 5.36.1, on a tree of 21.3 million files and 4.8 million folders:

| Tool and mode | Time | Peak memory |
| --- | --- | --- |
| `storage-analyzer --summary` | 33.8 s | 1.7 GB |
| `gdu -npcsx` (total only, keeps no tree) | 28.9 s | 0.6 GB |
| `gdu -npcx -o /dev/null` (keeps the tree) | 61.2 s | 34 GB |

## Requirements

- Linux with sysfs.
- `lsblk` from util-linux 2.37 or newer, for the disk map.

## Installation

Build the binary as described below and link it into a directory on your `PATH`:

```bash
ln -s "$PWD/out/storage-analyzer" ~/bin/storage-analyzer
```

## Usage

```bash
storage-analyzer
storage-analyzer /some/folder
storage-analyzer --summary /some/folder --threads 8
```

| Key | Action |
| --- | --- |
| Arrow keys, `h` `j` `k` `l` | Move between boxes |
| `Enter` | Open the selected partition or folder |
| `Backspace`, `Esc` | Go one level up |
| `q`, `Ctrl-C` | Quit |

`--summary` prints the totals without starting the interface.

## Contributing

The toolchain stays inside Docker. Build the image once, then format, test, lint and compile:

```bash
./dockerbuild.sh
./indockerbuild.sh 25 fmt
./indockerbuild.sh
```

The static binary is written to `out/storage-analyzer`.
