[![Build](https://github.com/tuxuser/netiso-srv/actions/workflows/build.yml/badge.svg)](https://github.com/tuxuser/netiso-srv/actions/workflows/build.yml)
[![Docker image tags](https://ghcr-badge.egpl.dev/tuxuser/netiso-srv-rs/tags?color=%2344cc11&ignore=latest&n=3&label=image+tags&trim=)](https://github.com/tuxuser/netiso-srv/pkgs/container/netiso-srv-rs)
[![GitHub Release](https://img.shields.io/github/v/release/tuxuser/netiso-srv)](https://github.com/tuxuser/netiso-srv/releases/latest)

# NetISO server daemon

NetISO server for modded Xbox 360 that supports both uncompressed ISO files and compressed ZAR (ZArchive) format with real-time decompression.

## Features

- Serves Xbox 360 ISOs over network to modded consoles
- **NEW**: Support for compressed ZAR (ZArchive) ISO files
- Real-time decompression on server-side (transparent to Xbox 360)
- Automatic detection of both .iso and .zar files
- Recursive directory scanning
- Docker support

## Supported Formats

- **Uncompressed ISO files** (.iso) - Direct streaming
- **Compressed ZAR files** (.zar) - Decompressed on-the-fly using zstd
  - Based on Xenia emulator's ZArchive format
  - Uses 64KiB block compression for efficient random access
  - Significantly reduces storage requirements

## Usage

Options:

    `-r` - Recursive scanning for ISO files
    `-v` - Verbose output
    `-h` - Print usage

Run: `netiso-srv [-r] [-v] [-h] [directory with *.iso files]`

## Creating ZAR Archives

To compress your Xbox 360 ISOs to ZAR format, you can use the Xenia emulator or ZArchive tools:

### Using Xenia Canary

1. Download [Xenia Canary](https://github.com/xenia-canary/xenia-canary/releases)
2. Launch Xenia Canary
3. Go to File → Create ZArchive
4. Select your ISO file(s)
5. Choose output location for .zar file(s)

The resulting .zar files can be placed in your NetISO directory alongside regular .iso files.

### Benefits of ZAR Format

- **Storage savings**: 30-60% smaller than uncompressed ISOs (varies by game)
- **Fast random access**: Block-based compression allows quick seeking
- **Transparent**: Xbox 360 doesn't know the file is compressed
- **Network efficient**: Less data transferred over network

## Docker

Spawn container standalone

```
docker run -p 4323:4323 -v /path/to/isos:/mnt ghcr.io/tuxuser/netiso-srv-rs:latest
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
