[![Build](https://github.com/xeroxxx/netiso-srv/actions/workflows/build.yml/badge.svg)](https://github.com/xeroxxx/netiso-srv/actions/workflows/build.yml)
[![Docker image tags](https://ghcr-badge.egpl.dev/xeroxxx/netiso-srv-rs/tags?color=%2344cc11&ignore=latest&n=3&label=image+tags&trim=)](https://github.com/xeroxxx/netiso-srv/pkgs/container/netiso-srv-rs)
[![GitHub Release](https://img.shields.io/github/v/release/xeroxxx/netiso-srv)](https://github.com/xeroxxx/netiso-srv/releases/latest)

# NetISO server daemon

NetISO server for modded Xbox 360 that supports both uncompressed ISO files and compressed ZISO (ZArchive-compressed ISO) format with real-time decompression.

## Features

- Serves Xbox 360 ISOs over network to modded consoles
- **NEW**: Support for compressed ZISO (ZArchive-compressed ISO) files
- Real-time decompression on server-side (transparent to Xbox 360)
- Automatic detection of both .iso and .ziso files
- Recursive directory scanning
- Docker support

## Supported Formats

- **Uncompressed ISO files** (.iso) - Direct streaming
- **Compressed ZISO files** (.ziso) - Decompressed on-the-fly using zstd
  - ZArchive-compressed ISO format (single .iso file inside archive)
  - Uses 64KiB block compression for efficient random access
  - Significantly reduces storage requirements
  - **Note**: Different from Xenia's .zar format (which contains extracted files)

## Usage

Options:

    `-r` - Recursive scanning for ISO files
    `-v` - Verbose output
    `-h` - Print usage
    `-i <file.iso>` - Convert ISO to ZISO format (creates file.ziso and exits)

**Server mode:**

```
netiso-srv [-r] [-v] [directory with *.iso files]
```

**Convert mode:**

```
netiso-srv -i <file.iso>
```

## Creating ZISO Archives

### Method 1: Using netiso-srv (Recommended)

The built-in converter works on both Windows and Linux:

```bash
# Convert a single ISO file to ZISO
netiso-srv -i /path/to/game.iso
# Creates: /path/to/game.ziso
```
Windows Release: netiso-srv-x86_64-pc-windows-msvc
Windows MinGW: netiso-srv-x86_64-pc-windows-gnu
Linux Release: choose according to your architecture

This method:

- ✅ Works natively on Windows and Linux
- ✅ Creates properly formatted ZISO files compatible with the server
- ✅ Automatic compression with optimal settings
- ✅ No additional tools required

### Method 2: Using Xenia Canary (Alternative)

Alternatively, you can use Xenia emulator's ZArchive tool:

1. Download [Xenia Canary](https://github.com/xenia-canary/xenia-canary/releases)
2. Launch Xenia Canary
3. Go to File → Create ZArchive
4. Select your directory containing single ISO file
5. Choose output location
6. **Important**: Rename the resulting `.zar` files to `.ziso`
   - Example: `Game.zar` → `Game.ziso`

**Why rename?** Xenia's native .zar format contains extracted game files, while our .ziso format contains a compressed ISO file. The renaming makes this distinction clear.

The resulting .ziso files can be placed in your NetISO directory alongside regular .iso files.

### Benefits of ZISO Format

- **Storage savings**: 10-30% smaller than uncompressed ISOs (varies by game)
- **Fast random access**: Block-based compression allows quick seeking
- **Transparent**: Xbox 360 doesn't know the file is compressed

## Docker

Spawn container standalone

```
docker run -p 4323:4323 -v /path/to/isos:/mnt ghcr.io/xeroxxx/netiso-srv-rs:latest
```

or

Spawn via docker compose

```
docker compose up
```

## Development

### Build

```
cargo build [--release]
```

### Build docker image locally

Make sure to have the `netiso-srv` built and available in current directory.

Build the image:

```
docker build -t netiso:localdev .
```

The resulting docker image is now ready-to-use from `netiso:localdev`, see `Docker`-steps above for regular docker-usage.
