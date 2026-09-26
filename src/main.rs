#![allow(clippy::upper_case_acronyms)]

use binrw::{BinRead, BinWrite};
use glob::glob;
use std::env;
use std::error::Error;
use std::io::{BufWriter, Cursor, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::net::TcpListener;

#[cfg(feature = "ziso")]
use zarchive::reader::ZArchiveReader;
#[cfg(feature = "ziso")]
use zarchive::pack;

#[cfg(feature = "chd")]
use chd::Chd;

#[cfg(feature = "chd-create")]
use libchdman_rs::dvd;

const NETISO_SRV_PORT: u16 = 4323;
const SECTOR_SIZE: u16 = 0x800; // 2048
const XGD_MAGIC: &[u8; 20] = b"MICROSOFT*XBOX*MEDIA";

/// MAME codec tag for Zstandard: ASCII "zstd" packed big-endian.
/// Using the raw value here avoids depending on whichever constant or
/// parser helper a particular version of `libchdman-rs` happens to export.
#[cfg(feature = "chd-create")]
const CHD_CODEC_ZSTD_TAG: u32 = 0x7a73_7464;

// ============================================================================
// Data types
// ============================================================================

#[derive(Debug)]
enum IsoFile {
    Regular(File),
    #[cfg(feature = "ziso")]
    Ziso {
        reader: Arc<ZArchiveReader>,
        inner_path: String,
    },
    #[cfg(feature = "chd")]
    Chd {
        state: Arc<Mutex<ChdState>>,
    },
}

#[derive(Debug)]
struct ActiveIso {
    file: IsoFile,
    metadata: IsoEntry,
}

#[derive(Clone, Debug)]
struct IsoEntry {
    path: PathBuf,
    filename: String,
    data_start: u64,
    sector_count: u64,
    has_type1_file: u32,
}

#[derive(BinRead, BinWrite, Debug)]
#[brw(repr = u16)]
enum Cmd {
    Ping = 0,
    GetIsoSize = 1,
    HasType1File = 2,
    ReadData = 3,
    GetIsoName = 4,
    MountIso = 5,
}

#[derive(BinRead, BinWrite, Debug)]
#[brw(big, magic = b"ISVR")]
struct Message {
    cmd_type: Cmd,
    iso_index: u16,
    offset: u64,
    length: u32,
}

#[derive(Default, Debug)]
struct Server {
    files: Vec<IsoEntry>,
    active_file: Option<ActiveIso>,
    verbose: bool,
    read_buffer: Vec<u8>,
}

// ============================================================================
// CHD streaming state
// ============================================================================

#[cfg(feature = "chd")]
#[derive(Debug)]
struct ChdState {
    path: PathBuf,
    hunk_size: u64,
    logical_size: u64,
    cache_start_hunk: u64,
    cache_hunk_count: u64,
    cache_data: Vec<u8>,
}

/// How many hunks to pull in on a cache miss.
///
/// Tuned for 2048-byte hunks (MAME's DVD default for XGD images):
///   2048 hunks × 2048 B = 4 MiB
///   4096 hunks × 2048 B = 8 MiB
///
/// 8 MiB covers essentially any contiguous burst the Xbox 360 issues in
/// a single cache miss, at the cost of ~8 MiB RAM per active stream.
#[cfg(feature = "chd")]
const CHD_READ_AHEAD_HUNKS: u64 = 4096;

#[cfg(feature = "chd")]
impl ChdState {
    fn open(path: &Path) -> Result<Self, String> {
        let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
        let mut br = std::io::BufReader::with_capacity(1 << 16, file);
        let chd = Chd::open(&mut br, None).map_err(|e| format!("{e:?}"))?;
        let header = chd.header();
        let hunk_size = header.hunk_size() as u64;
        let hunk_count = header.hunk_count() as u64;
        Ok(Self {
            path: path.to_path_buf(),
            hunk_size,
            logical_size: hunk_size * hunk_count,
            cache_start_hunk: 0,
            cache_hunk_count: 0,
            cache_data: Vec::new(),
        })
    }

    /// Load up to `CHD_READ_AHEAD_HUNKS` hunks starting at `start_hunk`.
    fn load_range(&mut self, start_hunk: u64) -> Result<(), String> {
        let total_hunks = self.logical_size / self.hunk_size;
        let end_hunk = (start_hunk + CHD_READ_AHEAD_HUNKS).min(total_hunks);
        let count = end_hunk - start_hunk;

        let file = std::fs::File::open(&self.path).map_err(|e| e.to_string())?;
        let mut br = std::io::BufReader::with_capacity(1 << 18, file);
        let mut chd = Chd::open(&mut br, None).map_err(|e| format!("{e:?}"))?;

        let hunk_size = self.hunk_size as usize;
        let mut buf = vec![0u8; count as usize * hunk_size];
        let mut cmp_buf: Vec<u8> = Vec::new();
        let mut tmp = vec![0u8; hunk_size];

        for i in 0..count {
            let h = start_hunk + i;
            let h_u32 = u32::try_from(h).map_err(|_| format!("hunk {h} exceeds u32"))?;
            let mut hunk = chd
                .hunk(h_u32)
                .map_err(|e| format!("hunk({h_u32}) failed: {e:?}"))?;
            hunk.read_hunk_in(&mut cmp_buf, &mut tmp)
                .map_err(|e| format!("read_hunk_in failed: {e:?}"))?;
            buf[i as usize * hunk_size..(i as usize + 1) * hunk_size].copy_from_slice(&tmp);
        }

        self.cache_start_hunk = start_hunk;
        self.cache_hunk_count = count;
        self.cache_data = buf;
        Ok(())
    }
}

/// Read `length` bytes from a CHD at `offset`, using a large prefetch cache.
/// Synchronous — call from `spawn_blocking`.
#[cfg(feature = "chd")]
fn read_from_chd_cached(
    state: &mut ChdState,
    offset: u64,
    length: usize,
) -> Result<Vec<u8>, String> {
    if offset >= state.logical_size {
        return Err(format!(
            "CHD read past end: offset {} >= logical size {}",
            offset, state.logical_size
        ));
    }

    let end = (offset + length as u64).min(state.logical_size);
    let to_read = (end - offset) as usize;
    let mut out = vec![0u8; to_read];

    let mut pos = offset;
    let mut written = 0usize;

    while written < to_read {
        let hunk_num = pos / state.hunk_size;

        let in_cache = hunk_num >= state.cache_start_hunk
            && hunk_num < state.cache_start_hunk + state.cache_hunk_count;

        if !in_cache {
            state.load_range(hunk_num)?;
        }

        let cache_offset = (pos - state.cache_start_hunk * state.hunk_size) as usize;
        let available = state.cache_data.len() - cache_offset;
        let to_copy = available.min(to_read - written);
        out[written..written + to_copy]
            .copy_from_slice(&state.cache_data[cache_offset..cache_offset + to_copy]);

        written += to_copy;
        pos += to_copy as u64;
    }

    Ok(out)
}

// ============================================================================
// ISO helpers
// ============================================================================

async fn get_data_start(file: &mut File) -> Result<u64, Box<dyn std::error::Error>> {
    const OFFSETS: [(u64, u64); 3] = [
        (0xfda0000, 0xfd90000), // XGD2 / GDF
        (0x2090000, 0x2080000), // XGD3
        (0x10000, 0x0),         // XSF
    ];

    let len = file.metadata().await?.len();
    let mut buf = [0u8; 20];

    for &(offset, data_start) in &OFFSETS {
        if len >= offset + buf.len() as u64 {
            file.seek(SeekFrom::Start(offset)).await?;
            file.read_exact(&mut buf).await?;
            if &buf == XGD_MAGIC {
                return Ok(data_start);
            }
        }
    }
    Ok(0)
}

// ============================================================================
// File discovery
// ============================================================================

async fn get_iso_files(
    old_entries: &Vec<IsoEntry>,
    directory: &Path,
    recursive: bool,
    verbose: bool,
) -> Result<Vec<IsoEntry>, Box<dyn std::error::Error>> {
    let mut ret = old_entries.clone();
    ret.retain(|x| x.path.exists());

    let glob_pattern = if recursive {
        format!(
            "{}{s}**{s}*",
            directory.display(),
            s = std::path::MAIN_SEPARATOR_STR
        )
    } else {
        format!("{}{}*", directory.display(), std::path::MAIN_SEPARATOR_STR)
    };

    let all_files: Vec<PathBuf> = glob(&glob_pattern)?
        .filter_map(|x| x.ok())
        .filter(|x| x.is_file())
        .filter(|x| !ret.iter().any(|y| y.path == *x))
        .filter(|x| {
            let ext = x.extension().and_then(|s| s.to_str()).unwrap_or("");
            if ext.eq_ignore_ascii_case("iso") {
                return true;
            }
            #[cfg(feature = "ziso")]
            if ext.eq_ignore_ascii_case("ziso") {
                return true;
            }
            #[cfg(feature = "chd")]
            if ext.eq_ignore_ascii_case("chd") {
                return true;
            }
            let _ = ext;
            false
        })
        .collect();

    for filepath in all_files {
        let filesize = filepath.metadata()?.len();
        let filename = filepath
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();

        let ext_of = |p: &Path| -> String {
            p.extension()
                .and_then(|s| s.to_str())
                .map(|s| s.to_lowercase())
                .unwrap_or_default()
        };
        let file_ext = ext_of(&filepath);

        let (data_start, actual_filesize) = if file_ext == "ziso" {
            #[cfg(feature = "ziso")]
            {
                match ZArchiveReader::open(&filepath) {
                    Ok(reader) => match reader.get_files() {
                        Ok(files) => {
                            let iso_file =
                                files.iter().find(|f| f.to_lowercase().ends_with(".iso"));
                            if let Some(iso_path) = iso_file {
                                if let Some(size) = reader.file_size(iso_path) {
                                    if verbose {
                                        println!(
                                            "Found ISO '{}' in ZISO archive '{}'",
                                            iso_path, filename
                                        );
                                    }
                                    (0, size as u64)
                                } else {
                                    eprintln!("Could not get size of ISO in ZISO: {filepath:?}");
                                    continue;
                                }
                            } else {
                                eprintln!("No ISO file found in ZISO archive: {filepath:?}");
                                continue;
                            }
                        }
                        Err(err) => {
                            eprintln!("Failed to list files in ZISO: {filepath:?}, err: {err:?}");
                            continue;
                        }
                    },
                    Err(err) => {
                        eprintln!("Invalid ZISO file: {filepath:?}, err: {err:?}");
                        continue;
                    }
                }
            }
            #[cfg(not(feature = "ziso"))]
            {
                eprintln!("ZISO file found but support not compiled: {filepath:?}");
                continue;
            }
        } else if file_ext == "chd" {
            #[cfg(feature = "chd")]
            {
                match probe_chd_sync(&filepath) {
                    Ok((logical_size, hunk_size)) => {
                        if verbose {
                            println!(
                                "Found CHD '{}' (hunk_size={}, logical_size={})",
                                filename, hunk_size, logical_size
                            );
                        }
                        (0, logical_size)
                    }
                    Err(err) => {
                        eprintln!("Invalid CHD file: {filepath:?}, err: {err}");
                        continue;
                    }
                }
            }
            #[cfg(not(feature = "chd"))]
            {
                eprintln!("CHD file found but support not compiled: {filepath:?}");
                continue;
            }
        } else {
            // Regular ISO
            let mut handle = File::open(&filepath).await?;
            let data_start = match get_data_start(&mut handle).await {
                Ok(ds) => ds,
                Err(err) => {
                    eprintln!("Invalid iso file: {filepath:?}, err: {err:?}");
                    continue;
                }
            };
            (data_start, filesize)
        };

        ret.push(IsoEntry {
            path: filepath.clone(),
            filename,
            data_start,
            sector_count: actual_filesize / SECTOR_SIZE as u64,
            has_type1_file: 0,
        });
    }

    Ok(ret)
}

async fn scan_iso_files_initial(
    directory: &Path,
    recursive: bool,
    verbose: bool,
) -> Result<Vec<IsoEntry>, Box<dyn std::error::Error>> {
    get_iso_files(&Vec::new(), directory, recursive, verbose).await
}

// ============================================================================
// CHD probe helper (synchronous)
// ============================================================================

#[cfg(feature = "chd")]
fn probe_chd_sync(path: &Path) -> Result<(u64, u64), String> {
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut reader = std::io::BufReader::with_capacity(1 << 16, file);
    let chd = Chd::open(&mut reader, None).map_err(|e| format!("{e:?}"))?;
    let header = chd.header();
    let hunk_size = header.hunk_size() as u64;
    let hunk_count = header.hunk_count() as u64;
    Ok((hunk_size * hunk_count, hunk_size))
}

// ============================================================================
// Server
// ============================================================================

impl Server {
    fn disable_current_iso(&mut self) {
        self.active_file = None;
    }

    async fn handler(
        &mut self,
        mut socket: tokio::net::TcpStream,
    ) -> Result<(), Box<dyn std::error::Error>> {
        loop {
            let mut buffer = [0u8; 20];
            match socket.read(&mut buffer).await {
                Ok(size) => {
                    if size == 0 {
                        eprintln!("EOF - Client '{:?}' disconnected", socket.peer_addr());
                        self.disable_current_iso();
                        break;
                    }

                    let mut cur = Cursor::new(&buffer);
                    let msg = Message::read(&mut cur)?;

                    if self.verbose {
                        println!("< {msg:?}");
                    }

                    match msg.cmd_type {
                        Cmd::Ping => {
                            socket.try_write("ISVRokOK".as_bytes())?;
                        }
                        Cmd::GetIsoSize => {
                            let sector_count = self
                                .active_file
                                .as_ref()
                                .map(|iso| iso.metadata.sector_count as u32)
                                .unwrap_or(0);

                            let mut resp = [0u8; 8];
                            resp[0..4].copy_from_slice(&sector_count.to_be_bytes());
                            resp[4..8].copy_from_slice(&(SECTOR_SIZE as u32).to_be_bytes());
                            socket.try_write(&resp)?;
                        }
                        Cmd::HasType1File => {
                            let v = self
                                .files
                                .get(msg.iso_index as usize)
                                .map(|iso| iso.has_type1_file)
                                .unwrap_or(0);
                            socket.try_write(&v.to_be_bytes())?;
                        }
                        Cmd::ReadData => {
                            if let Some(active) = self.active_file.as_mut() {
                                let length = msg.length as usize;

                                if self.read_buffer.len() != length {
                                    self.read_buffer.resize(length, 0);
                                }

                                match &mut active.file {
                                    IsoFile::Regular(file) => {
                                        file.seek(SeekFrom::Start(msg.offset)).await?;
                                        file.read_exact(&mut self.read_buffer).await?;
                                    }
                                    #[cfg(feature = "ziso")]
                                    IsoFile::Ziso {
                                        reader,
                                        inner_path,
                                    } => {
                                        let offset_in_iso =
                                            msg.offset.saturating_sub(active.metadata.data_start);
                                        match reader.read_from_file(
                                            inner_path,
                                            offset_in_iso as usize,
                                            length,
                                        ) {
                                            Some(data) => {
                                                self.read_buffer[..length]
                                                    .copy_from_slice(&data[..length]);
                                            }
                                            None => {
                                                eprintln!(
                                                    "Failed to read from ZISO at offset {}",
                                                    msg.offset
                                                );
                                                self.read_buffer.fill(0);
                                            }
                                        }
                                    }
                                    #[cfg(feature = "chd")]
                                    IsoFile::Chd { state } => {
                                        let state = state.clone();
                                        let offset = msg.offset;
                                        let data = tokio::task::spawn_blocking(move || {
                                            let mut guard = state.lock().map_err(|_| {
                                                "CHD state mutex poisoned".to_string()
                                            })?;
                                            read_from_chd_cached(&mut guard, offset, length)
                                        })
                                        .await
                                        .map_err(|e| format!("CHD worker panicked: {e}"))?
                                        .map_err(|e| -> Box<dyn Error> { e.into() })?;

                                        let n = data.len().min(self.read_buffer.len());
                                        self.read_buffer[..n].copy_from_slice(&data[..n]);
                                        if n < self.read_buffer.len() {
                                            self.read_buffer[n..].fill(0);
                                        }
                                    }
                                }

                                socket.try_write(&self.read_buffer)?;
                            }
                        }
                        Cmd::GetIsoName => {
                            let filename = self
                                .files
                                .get(msg.iso_index as usize)
                                .map(|iso| iso.filename.as_str())
                                .unwrap_or("");

                            let mut response = vec![0u8; msg.length as usize];
                            let bytes = filename.as_bytes();
                            let copy_len = bytes.len().min(response.len());
                            response[..copy_len].copy_from_slice(&bytes[..copy_len]);
                            socket.try_write(&response)?;
                        }
                        Cmd::MountIso => {
                            let mut iso_name = vec![0; msg.length as usize];
                            assert_eq!(socket.read(&mut iso_name).await?, msg.length as usize);

                            let iso_name_human = String::from_utf8(iso_name)?;
                            let normalized = iso_name_human
                                .replace("\\Mount", "")
                                .replace("\\", "")
                                .replace("\x00", "");

                            println!("Normalized ISO Name: {iso_name_human} -> {normalized}");

                            if normalized == "[Disable Current ISO]" {
                                println!("Unmounting current iso...");
                                self.disable_current_iso();
                                socket.try_write(&0u32.to_be_bytes())?;
                            } else {
                                let normalized_lc = normalized.to_lowercase();
                                let found = self.files.iter().find(|x| {
                                    x.filename.to_lowercase().ends_with(&normalized_lc)
                                });

                                let code: u32 = match found {
                                    Some(iso) => {
                                        println!("Mounting: {:?}", iso.path);
                                        let ext = iso
                                            .path
                                            .extension()
                                            .and_then(|s| s.to_str())
                                            .map(|s| s.to_lowercase())
                                            .unwrap_or_default();

                                        match ext.as_str() {
                                            #[cfg(feature = "ziso")]
                                            "ziso" => match ZArchiveReader::open(&iso.path) {
                                                Ok(reader) => match reader.get_files() {
                                                    Ok(files) => {
                                                        let inner = files.iter().find(|f| {
                                                            f.to_lowercase().ends_with(".iso")
                                                        });
                                                        if let Some(inner_path) = inner {
                                                            let file = IsoFile::Ziso {
                                                                reader: Arc::new(reader),
                                                                inner_path: inner_path.clone(),
                                                            };
                                                            self.active_file =
                                                                Some(ActiveIso {
                                                                    file,
                                                                    metadata: iso.to_owned(),
                                                                });
                                                            1
                                                        } else {
                                                            eprintln!(
                                                                "MountIso: No ISO found in ZISO '{}'",
                                                                iso.filename
                                                            );
                                                            0
                                                        }
                                                    }
                                                    Err(err) => {
                                                        eprintln!(
                                                            "MountIso: Failed to read ZISO '{}': {:?}",
                                                            iso.filename, err
                                                        );
                                                        0
                                                    }
                                                },
                                                Err(err) => {
                                                    eprintln!(
                                                        "MountIso: Failed to open ZISO '{}': {:?}",
                                                        iso.filename, err
                                                    );
                                                    0
                                                }
                                            },
                                            #[cfg(feature = "chd")]
                                            "chd" => match ChdState::open(&iso.path) {
                                                Ok(state) => {
                                                    let file = IsoFile::Chd {
                                                        state: Arc::new(Mutex::new(state)),
                                                    };
                                                    self.active_file = Some(ActiveIso {
                                                        file,
                                                        metadata: iso.to_owned(),
                                                    });
                                                    1
                                                }
                                                Err(err) => {
                                                    eprintln!(
                                                        "MountIso: Failed to open CHD '{}': {:?}",
                                                        iso.filename, err
                                                    );
                                                    0
                                                }
                                            },
                                            _ => match File::open(&iso.path).await {
                                                Ok(file) => {
                                                    self.active_file = Some(ActiveIso {
                                                        file: IsoFile::Regular(file),
                                                        metadata: iso.to_owned(),
                                                    });
                                                    1
                                                }
                                                Err(err) => {
                                                    eprintln!(
                                                        "MountIso: Failed to open ISO '{}': {:?}",
                                                        iso.filename, err
                                                    );
                                                    0
                                                }
                                            },
                                        }
                                    }
                                    None => {
                                        eprintln!("MountIso: Failed to find ISO '{normalized}' !");
                                        0
                                    }
                                };

                                socket.try_write(&code.to_be_bytes())?;
                            }
                        }
                    }
                }
                Err(err) => {
                    eprintln!("Failed reading from socket, err: {err}");
                }
            }
        }

        Ok(())
    }

    async fn handle_connection(&mut self, socket: tokio::net::TcpStream) {
        if let Err(err) = self.handler(socket).await {
            eprintln!("Connection handler exited with error: {err}");
        }
    }
}

// ============================================================================
// Case helpers
// ============================================================================

fn case_matched_extension(input_ext: &str, target_lower: &str) -> String {
    let has_lower = input_ext.chars().any(|c| c.is_lowercase());
    let has_upper = input_ext.chars().any(|c| c.is_uppercase());

    match (has_lower, has_upper) {
        (false, true) => target_lower.to_uppercase(),
        (true, false) => target_lower.to_string(),
        (true, true) => {
            let mut chars = target_lower.chars();
            match chars.next() {
                Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        }
        (false, false) => target_lower.to_string(),
    }
}

fn output_path_with_matched_ext(
    input: &Path,
    target_lower: &str,
) -> Result<PathBuf, Box<dyn Error>> {
    let input_ext = input.extension().and_then(|s| s.to_str()).unwrap_or("");
    let new_ext = case_matched_extension(input_ext, target_lower);

    let mut out = input.to_path_buf();
    out.set_extension(&new_ext);

    if out == input {
        return Err(format!(
            "Output path would equal input path: {}",
            input.display()
        )
        .into());
    }
    Ok(out)
}

// ============================================================================
// ISO <-> ZISO conversion
// ============================================================================

#[cfg(feature = "ziso")]
fn convert_iso_to_ziso(iso_path: &Path) -> Result<(), Box<dyn Error>> {
    if !iso_path.exists() {
        return Err(format!("Input file does not exist: {}", iso_path.display()).into());
    }
    if !iso_path.is_file() {
        return Err(format!("Input path is not a file: {}", iso_path.display()).into());
    }
    let extension = iso_path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    if !extension.eq_ignore_ascii_case("iso") {
        return Err("Input file must have .iso extension".into());
    }

    let output_path = output_path_with_matched_ext(iso_path, "ziso")?;
    if output_path.exists() {
        return Err(format!("Output file already exists: {}", output_path.display()).into());
    }

    println!("Converting {} to ZISO format...", iso_path.display());
    println!("Output: {}", output_path.display());

    let parent = iso_path.parent().unwrap_or_else(|| Path::new("."));
    let temp_dir = parent.join(format!(".netiso_convert_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&temp_dir);
    std::fs::create_dir_all(&temp_dir)?;

    let iso_filename = iso_path.file_name().ok_or("Input path has no filename")?;
    let temp_iso = temp_dir.join(iso_filename);

    println!("Preparing files...");
    if let Err(link_err) = std::fs::hard_link(iso_path, &temp_iso) {
        println!("Hard link failed ({link_err}); falling back to copy...");
        if let Err(copy_err) = std::fs::copy(iso_path, &temp_iso) {
            let _ = std::fs::remove_dir_all(&temp_dir);
            return Err(format!("Failed to stage ISO in temp dir: {copy_err}").into());
        }
    }

    println!("Compressing (this may take a while)...");
    let pack_result = pack(&temp_dir, &output_path);
    let _ = std::fs::remove_dir_all(&temp_dir);

    match pack_result {
        Ok(_) => {
            println!("✓ Successfully created: {}", output_path.display());
            if let Ok(reader) = ZArchiveReader::open(&output_path) {
                if let Ok(files) = reader.get_files() {
                    println!("✓ Archive contains {} file(s)", files.len());
                }
            }
        }
        Err(e) => {
            let _ = std::fs::remove_file(&output_path);
            return Err(format!("Failed to create ZISO: {e}").into());
        }
    }
    Ok(())
}

#[cfg(not(feature = "ziso"))]
fn convert_iso_to_ziso(_iso_path: &Path) -> Result<(), Box<dyn Error>> {
    Err("ZISO support not compiled in. Rebuild with --features ziso".into())
}

#[cfg(feature = "ziso")]
fn convert_ziso_to_iso(ziso_path: &Path) -> Result<(), Box<dyn Error>> {
    if !ziso_path.exists() {
        return Err("Input file does not exist".into());
    }
    if !ziso_path.is_file() {
        return Err("Input is not a file".into());
    }
    let ext = ziso_path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    if !ext.eq_ignore_ascii_case("ziso") {
        return Err("Input file must have .ziso extension".into());
    }

    let reader = ZArchiveReader::open(ziso_path)?;
    let files = reader.get_files()?;
    let inner_iso = files
        .iter()
        .find(|f| f.to_lowercase().ends_with(".iso"))
        .ok_or("No .iso file found inside ZISO archive")?;

    let size = reader
        .file_size(inner_iso)
        .ok_or("Could not determine inner ISO size")? as usize;

    let output_path = output_path_with_matched_ext(ziso_path, "iso")?;
    if output_path.exists() {
        return Err(format!("Output file already exists: {}", output_path.display()).into());
    }

    let mut out = BufWriter::new(std::fs::File::create(&output_path)?);

    const CHUNK_SIZE: usize = 1024 * 1024;
    let mut offset = 0;

    while offset < size {
        let len = std::cmp::min(CHUNK_SIZE, size - offset);
        let data = reader
            .read_from_file(inner_iso, offset, len)
            .ok_or_else(|| format!("Failed to read at offset {offset}"))?;
        out.write_all(&data)?;
        offset += len;
    }

    out.flush()?;
    println!("✓ Extracted: {}", output_path.display());
    Ok(())
}

#[cfg(not(feature = "ziso"))]
fn convert_ziso_to_iso(_ziso_path: &Path) -> Result<(), Box<dyn Error>> {
    Err("ZISO support not compiled in. Rebuild with --features ziso".into())
}

// ============================================================================
// ISO <-> CHD conversion
// ============================================================================

#[cfg(feature = "chd-create")]
fn convert_iso_to_chd(iso_path: &Path) -> Result<(), Box<dyn Error>> {
    if !iso_path.exists() {
        return Err(format!("Input file does not exist: {}", iso_path.display()).into());
    }
    if !iso_path.is_file() {
        return Err(format!("Input path is not a file: {}", iso_path.display()).into());
    }
    let extension = iso_path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    if !extension.eq_ignore_ascii_case("iso") {
        return Err("Input file must have .iso extension".into());
    }

    let output_path = output_path_with_matched_ext(iso_path, "chd")?;
    if output_path.exists() {
        return Err(format!("Output file already exists: {}", output_path.display()).into());
    }

    println!("Converting {} to CHD format...", iso_path.display());
    println!("Output: {}", output_path.display());
    println!("Compressing (this may take a while)...");

    // MAME CHD codec tags are 4-byte ASCII identifiers packed big-endian.
    // For zstd that's 0x7a737464 ("zstd"). The codec field is a fixed-size
    // [u32; 4]; the parser stops at the first zero entry, so one codec is
    // enough. zstd is a good streaming choice: ~10% bigger than LZMA but
    // decompresses 5-10x faster, which matters because the Xbox 360's boot
    // chain has strict read timeouts.
    let mut opts = dvd::DvdCreateOptions::default();
    opts.codecs = [CHD_CODEC_ZSTD_TAG, 0, 0, 0];
    opts.hunk_size = 2048; // 1 sector per hunk — smaller decompression unit

    let mut progress = |_p| {}; // noop progress reporter
    let cancel = || false; // never cancel

    dvd::create_from_iso(iso_path, &output_path, opts, &mut progress, &cancel)
        .map_err(|e| format!("Failed to create CHD: {e:?}"))?;

    println!("✓ Successfully created: {}", output_path.display());
    Ok(())
}

#[cfg(not(feature = "chd-create"))]
fn convert_iso_to_chd(_iso_path: &Path) -> Result<(), Box<dyn Error>> {
    Err("CHD creation not compiled in. Rebuild with --features chd-create".into())
}

#[cfg(feature = "chd")]
fn convert_chd_to_iso(chd_path: &Path) -> Result<(), Box<dyn Error>> {
    if !chd_path.exists() {
        return Err("Input file does not exist".into());
    }
    if !chd_path.is_file() {
        return Err("Input is not a file".into());
    }
    let ext = chd_path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    if !ext.eq_ignore_ascii_case("chd") {
        return Err("Input file must have .chd extension".into());
    }

    let mut state = ChdState::open(chd_path)?;
    let logical_size = state.logical_size;

    let output_path = output_path_with_matched_ext(chd_path, "iso")?;
    if output_path.exists() {
        return Err(format!("Output file already exists: {}", output_path.display()).into());
    }

    println!("Extracting {} to ISO...", chd_path.display());
    println!("Output: {}", output_path.display());

    let mut out = BufWriter::new(std::fs::File::create(&output_path)?);

    const CHUNK_SIZE: u64 = 4 * 1024 * 1024; // 4 MiB chunks
    let mut offset = 0u64;

    while offset < logical_size {
        let len = std::cmp::min(CHUNK_SIZE, logical_size - offset) as usize;
        let chunk = read_from_chd_cached(&mut state, offset, len)?;
        out.write_all(&chunk)?;
        offset += len as u64;
    }

    out.flush()?;
    println!("✓ Extracted: {}", output_path.display());
    Ok(())
}

#[cfg(not(feature = "chd"))]
fn convert_chd_to_iso(_chd_path: &Path) -> Result<(), Box<dyn Error>> {
    Err("CHD support not compiled in. Rebuild with --features chd".into())
}

// ============================================================================
// CLI
// ============================================================================

fn print_usage(bin_name: &str) {
    println!(
        "Usage: {bin_name} [-rvh] [-i <iso_file>] [-d <ziso_file>] [-c <iso_file>] [-x <chd_file>] [iso directory path]"
    );
    println!("\nArgs:");
    println!("\t-r - Recursive ISO / ZISO / CHD scanning");
    println!("\t-v - Verbose output");
    println!("\t-h - Print help / usage");
    println!("\t-i <file.iso>   - Convert ISO to ZISO (case preserved)");
    println!("\t-d <file.ziso>  - Convert ZISO to ISO (case preserved)");
    println!("\t-c <file.iso>   - Convert ISO to CHD  (requires --features chd-create)");
    println!("\t-x <file.chd>   - Convert CHD to ISO  (requires --features chd)");
}

fn check_arg(args: &mut Vec<String>, arg_name: &str) -> bool {
    match args.iter().position(|x| arg_name == x) {
        Some(i) => {
            args.remove(i);
            true
        }
        None => false,
    }
}

fn get_arg_value(args: &mut Vec<String>, arg_name: &str) -> Option<String> {
    match args.iter().position(|x| arg_name == x) {
        Some(i) => {
            if i + 1 < args.len() {
                args.remove(i);
                Some(args.remove(i))
            } else {
                args.remove(i);
                None
            }
        }
        None => None,
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let mut args: Vec<String> = env::args().collect();

    let print_help = check_arg(&mut args, "-h");
    let recursive_scan = check_arg(&mut args, "-r");
    let verbose = check_arg(&mut args, "-v");
    let convert_ziso = get_arg_value(&mut args, "-i"); // ISO  -> ZISO
    let extract_ziso = get_arg_value(&mut args, "-d"); // ZISO -> ISO
    let convert_chd = get_arg_value(&mut args, "-c"); // ISO  -> CHD
    let extract_chd = get_arg_value(&mut args, "-x"); // CHD  -> ISO

    let mut mode_count = 0usize;
    for opt in [&convert_ziso, &extract_ziso, &convert_chd, &extract_chd] {
        if opt.is_some() {
            mode_count += 1;
        }
    }
    if mode_count > 1 {
        println!("ERROR: conversion modes are mutually exclusive\n");
        print_usage(&args[0]);
        return Ok(());
    }
    if mode_count == 1 && recursive_scan {
        println!("ERROR: conversion mode and -r (server mode) are mutually exclusive\n");
        print_usage(&args[0]);
        return Ok(());
    }

    if let Some(f) = convert_ziso {
        return convert_iso_to_ziso(Path::new(&f));
    }
    if let Some(f) = extract_ziso {
        return convert_ziso_to_iso(Path::new(&f));
    }
    if let Some(f) = convert_chd {
        return convert_iso_to_chd(Path::new(&f));
    }
    if let Some(f) = extract_chd {
        return convert_chd_to_iso(Path::new(&f));
    }

    if print_help || args.len() < 2 {
        if !print_help && args.len() < 2 {
            println!("ERROR: Invalid number of arguments!\n");
        }
        print_usage(&args[0]);
        return Ok(());
    }

    let filepath = Path::new(&args[1]);

    println!("Enumerating ISOs in {filepath:?}...");
    let mut files = scan_iso_files_initial(filepath, recursive_scan, verbose).await?;

    if files.is_empty() {
        return Err("No iso files enumerated".into());
    }

    println!("Found the following ISOs");
    for (index, file) in files.iter().enumerate() {
        println!("{index}: {}", &file.filename);
    }

    let listener = TcpListener::bind(("0.0.0.0", NETISO_SRV_PORT)).await?;
    println!("Start listening for incoming connections...");

    loop {
        let (socket, _) = listener.accept().await?;
        println!("Got connection from: {:?}", &socket.peer_addr());

        files = get_iso_files(&files, filepath, recursive_scan, verbose).await?;

        let files_clone = files.clone();
        tokio::spawn(async move {
            let mut srv = Server {
                files: files_clone,
                verbose,
                ..Default::default()
            };
            srv.handle_connection(socket).await
        });
    }
}
