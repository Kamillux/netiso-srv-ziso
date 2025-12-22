#![allow(clippy::upper_case_acronyms)]

use binrw::{BinRead, BinWrite};
use glob::glob;
use tokio::fs::File;
use std::env;
use std::error::Error;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::net::TcpListener;

#[cfg(feature = "ziso")]
use zarchive::reader::ZArchiveReader;
#[cfg(feature = "ziso")]
use zarchive::pack;

const NETISO_SRV_PORT: u16 = 4323;
const SECTOR_SIZE: u16 = 0x800; // 2048
const XGD_MAGIC: &[u8; 20] = b"MICROSOFT*XBOX*MEDIA";

#[derive(Debug)]
enum IsoFile {
    Regular(File),
    #[cfg(feature = "ziso")]
    Ziso { reader: Arc<ZArchiveReader>, inner_path: String },
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

// Message structure definition
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
    /// Reusable buffer to avoid allocations on every read operation.
    /// This significantly reduces memory pressure during continuous ISO streaming
    /// by reusing the same buffer instead of allocating a new Vec for each read.
    read_buffer: Vec<u8>,
}

async fn get_data_start(file: &mut File) -> Result<u64, Box<dyn std::error::Error>> {
    const OFFSETS: [(u64, u64); 3] = [
        (0xfda0000, 0xfd90000), // XGD2 / GDF
        (0x2090000, 0x2080000), // XGD3
        (0x10000, 0x0),         // XSF
    ];

    let len = file.metadata().await?.len();
    let mut buf = [0u8; 20]; // XGD_MAGIC.len() = 20
    
    for &(offset, data_start) in &OFFSETS {
        if len >= offset + buf.len() as u64 {
            file.seek(std::io::SeekFrom::Start(offset)).await?;
            file.read_exact(&mut buf).await?;
            
            if &buf == XGD_MAGIC {
                return Ok(data_start);
            }
        }
    }
    Ok(0) // No XGD Magic found, assume data starts @ 0x0
}

async fn get_iso_files(old_entries: &Vec<IsoEntry>, directory: &Path, recursive: bool, verbose: bool) -> Result<Vec<IsoEntry>, Box<dyn std::error::Error>> {
    let mut ret = old_entries.clone();

    // First, throw out obsolete entries
    ret.retain(|x| x.path.exists());

    // Assemble glob patterns for both .iso and .ziso files (case-insensitive)
    #[cfg(feature = "ziso")]
    let patterns = vec!["*.iso", "*.ISO", "*.ziso", "*.ZISO"];
    #[cfg(not(feature = "ziso"))]
    let patterns = vec!["*.iso", "*.ISO"];
    
    let mut all_files = Vec::new();
    
    for pattern in patterns {
        let isofiles_glob_pattern = if recursive {
            format!("{}{s}**{s}{}", directory.display(), pattern, s = std::path::MAIN_SEPARATOR_STR)
        } else {
            format!("{}{}{}", directory.display(), std::path::MAIN_SEPARATOR_STR, pattern)
        };

        let files: Vec<PathBuf> = glob(&isofiles_glob_pattern)?
            .filter_map(|x| x.ok())
            .filter(|x| x.is_file() && !ret.iter().any(|y| y.path == *x))
            .collect();
        
        all_files.extend(files);
    }

    for filepath in all_files {
        let filesize = filepath.metadata()?.len();
        let filename = filepath.file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        
        // Check if it's a ZISO file (case-insensitive)
        #[cfg(feature = "ziso")]
        let is_ziso = filepath.extension()
            .and_then(|s| s.to_str())
            .map(|s| s.eq_ignore_ascii_case("ziso"))
            .unwrap_or(false);
        #[cfg(not(feature = "ziso"))]
        let is_ziso = false;
        
        let (data_start, actual_filesize) = if is_ziso {
            // For ZISO files, we need to open the archive and find the ISO inside
            #[cfg(feature = "ziso")]
            match ZArchiveReader::open(&filepath) {
                Ok(reader) => {
                    // Look for an ISO file inside the archive
                    // ZISO archives contain a single compressed ISO file
                    let files_result = reader.get_files();
                    match files_result {
                        Ok(files) => {
                            // Find the first .iso file in the archive (case-insensitive)
                            let iso_file = files.iter()
                                .find(|f| f.to_lowercase().ends_with(".iso"));
                            
                            if let Some(iso_path) = iso_file {
                                if let Some(size) = reader.file_size(iso_path) {
                                    // For ZISO files, data_start is 0 as we read directly from the decompressed stream
                                    if verbose {
                                        println!("Found ISO '{}' in ZISO archive '{}'", iso_path, filename);
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
                        },
                        Err(err) => {
                            eprintln!("Failed to list files in ZISO: {filepath:?}, err: {err:?}");
                            continue;
                        }
                    }
                },
                Err(err) => {
                    eprintln!("Invalid ZISO file: {filepath:?}, err: {err:?}");
                    continue;
                }
            }
            #[cfg(not(feature = "ziso"))]
            {
                // ZISO support not compiled in
                eprintln!("ZISO file found but support not compiled: {filepath:?}");
                continue;
            }
        } else {
            // Regular ISO file
            let mut handle = File::open(&filepath).await?;
            let data_start = match get_data_start(&mut handle).await {
                Ok(data_start) => data_start,
                Err(err) => {
                    eprintln!("Invalid iso file: {filepath:?}, err: {err:?}");
                    continue;
                }
            };
            (data_start, filesize)
        };

        let entry = IsoEntry {
            path: filepath.clone(),
            filename,
            data_start,
            sector_count: actual_filesize / SECTOR_SIZE as u64,
            has_type1_file: 0,
        };

        ret.push(entry);
    }

    Ok(ret)
}

async fn scan_iso_files_initial(directory: &Path, recursive: bool, verbose: bool) -> Result<Vec<IsoEntry>, Box<dyn std::error::Error>> {
    get_iso_files(&Vec::new(), directory, recursive, verbose).await
}

impl Server {
    fn disable_current_iso(&mut self) {
        self.active_file = None;
    }

    async fn handler(&mut self, mut socket: tokio::net::TcpStream) -> Result<(), Box<dyn std::error::Error>> {
        loop {
            let mut buffer = [0; 20];
            match socket.read(&mut buffer).await {
                Ok(size) => {
                    if size == 0 {
                        eprintln!("EOF - Client '{:?}' disconnected", socket.peer_addr());
                        self.disable_current_iso();
                        break
                    }

                    let mut cur = Cursor::new(&buffer);
                    let msg = Message::read(&mut cur)?;

                    if self.verbose {
                        println!("< {msg:?}");
                    }

                    match msg.cmd_type {
                        Cmd::Ping => {
                            let reply = "ISVRokOK".as_bytes();
                            socket.try_write(reply)?;
                        },
                        Cmd::GetIsoSize => {
                            let sector_count = self.active_file.as_ref()
                                .map(|iso| iso.metadata.sector_count as u32)
                                .unwrap_or(0);

                            let mut resp = [0u8; 8];
                            resp[0..4].copy_from_slice(&sector_count.to_be_bytes());
                            resp[4..8].copy_from_slice(&(SECTOR_SIZE as u32).to_be_bytes());

                            socket.try_write(&resp)?;
                        },
                        Cmd::HasType1File => {
                            let has_type1_file = self.files.get(msg.iso_index as usize)
                                .map(|iso| iso.has_type1_file)
                                .unwrap_or(0);

                            socket.try_write(&has_type1_file.to_be_bytes())?;
                        },
                        Cmd::ReadData => {
                            if let Some(active) = self.active_file.as_mut() {
                                let length = msg.length as usize;
                                
                                // Reuse the buffer, only resize if needed
                                if self.read_buffer.len() != length {
                                    self.read_buffer.resize(length, 0);
                                }

                                match &mut active.file {
                                    IsoFile::Regular(file) => {
                                        // Regular ISO file - use async file I/O
                                        file.seek(std::io::SeekFrom::Start(msg.offset)).await?;
                                        file.read_exact(&mut self.read_buffer).await?;
                                    },
                                    #[cfg(feature = "ziso")]
                                    IsoFile::Ziso { reader, inner_path } => {
                                        // ZISO compressed file - decompress on-the-fly
                                        // The zarchive library handles decompression transparently
                                        let offset_in_iso = msg.offset - active.metadata.data_start;
                                        
                                        if let Some(data) = reader.read_from_file(inner_path, offset_in_iso as usize, length) {
                                            // Copy data to our reusable buffer
                                            self.read_buffer[..length].copy_from_slice(&data[..length]);
                                        } else {
                                            eprintln!("Failed to read from ZISO file at offset {}", msg.offset);
                                            // Fill with zeros on error
                                            self.read_buffer.fill(0);
                                        }
                                    }
                                }

                                socket.try_write(&self.read_buffer)?;
                            }
                        },
                        Cmd::GetIsoName => {
                            let filename = self.files.get(msg.iso_index as usize)
                                .map(|iso| iso.filename.as_str())
                                .unwrap_or("");
                            
                            let mut response = vec![0u8; msg.length as usize];
                            let bytes = filename.as_bytes();
                            let copy_len = bytes.len().min(response.len());
                            response[..copy_len].copy_from_slice(&bytes[..copy_len]);

                            socket.try_write(&response)?;
                        },
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
                                let found = self.files.iter().find(|x| x.filename.ends_with(&normalized));
    
                                let code: u32 = match found {
                                    Some(iso) => {
                                        println!("Mounting: {:?}", iso.path);
                                        
                                        // Check if it's a ZISO file (case-insensitive)
                                        #[cfg(feature = "ziso")]
                                        let is_ziso = iso.path.extension()
                                            .and_then(|s| s.to_str())
                                            .map(|s| s.eq_ignore_ascii_case("ziso"))
                                            .unwrap_or(false);
                                        #[cfg(not(feature = "ziso"))]
                                        let is_ziso = false;
                                        
                                        if is_ziso {
                                            // Open ZISO archive
                                            #[cfg(feature = "ziso")]
                                            match ZArchiveReader::open(&iso.path) {
                                                Ok(reader) => {
                                                    // Find the ISO file inside
                                                    match reader.get_files() {
                                                        Ok(files) => {
                                                            // Find the first .iso file (case-insensitive)
                                                            let iso_file = files.iter()
                                                                .find(|f| f.to_lowercase().ends_with(".iso"));
                                                            
                                                            if let Some(inner_path) = iso_file {
                                                                println!("Found ISO in ZISO: {}", inner_path);
                                                                let file = IsoFile::Ziso { 
                                                                    reader: Arc::new(reader), 
                                                                    inner_path: inner_path.clone() 
                                                                };
                                                                self.active_file = Some(ActiveIso { file, metadata: iso.to_owned() });
                                                                1 // success
                                                            } else {
                                                                eprintln!("MountIso: No ISO found in ZISO archive '{}'!", iso.filename);
                                                                0 // error
                                                            }
                                                        },
                                                        Err(err) => {
                                                            eprintln!("MountIso: Failed to read ZISO archive '{}': {:?}", iso.filename, err);
                                                            0 // error
                                                        }
                                                    }
                                                },
                                                Err(err) => {
                                                    eprintln!("MountIso: Failed to open ZISO archive '{}': {:?}", iso.filename, err);
                                                    0 // error
                                                }
                                            }
                                            #[cfg(not(feature = "ziso"))]
                                            {
                                                eprintln!("MountIso: ZISO support not compiled in");
                                                0 // error
                                            }
                                        } else {
                                            // Regular ISO file
                                            match File::open(&iso.path).await {
                                                Ok(file) => {
                                                    self.active_file = Some(ActiveIso { file: IsoFile::Regular(file), metadata: iso.to_owned() });
                                                    1 // success
                                                },
                                                Err(err) => {
                                                    eprintln!("MountIso: Failed to open ISO '{}': {:?}", iso.filename, err);
                                                    0 // error
                                                }
                                            }
                                        }
                                    },
                                    None => {
                                        eprintln!("MountIso: Failed to find ISO '{normalized}' !");
                                        0 // error
                                    }
                                };
    
                                socket.try_write(&code.to_be_bytes())?;
                            }
                        }
                    }
                },
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

fn print_usage(bin_name: &str) {
    println!("Usage: {} [-rvh] [-i <iso_file>] [iso directory path]", bin_name);
    println!("\nArgs:");
    println!("\t-r - Recursive ISO scanning");
    println!("\t-v - Verbose output");
    println!("\t-h - Print help / usage");
    println!("\t-i <file.iso> - Convert ISO to ZISO format (creates file.ziso)");
}

fn check_arg(args: &mut Vec<String>, arg_name: &str) -> bool {
    match args.iter().position(|x| arg_name == x) {
        Some(removal_index) => {
            args.remove(removal_index);
            true
        },
        None => false
    }
}

fn get_arg_value(args: &mut Vec<String>, arg_name: &str) -> Option<String> {
    match args.iter().position(|x| arg_name == x) {
        Some(index) => {
            if index + 1 < args.len() {
                args.remove(index); // Remove the flag
                Some(args.remove(index)) // Remove and return the value
            } else {
                args.remove(index);
                None
            }
        },
        None => None
    }
}

#[cfg(feature = "ziso")]
fn convert_iso_to_ziso(iso_path: &Path) -> Result<(), Box<dyn Error>> {
    // Validate input file
    if !iso_path.exists() {
        return Err(format!("Input file does not exist: {}", iso_path.display()).into());
    }
    
    if !iso_path.is_file() {
        return Err(format!("Input path is not a file: {}", iso_path.display()).into());
    }
    
    // Check if it's an ISO file
    let extension = iso_path.extension()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    if !extension.eq_ignore_ascii_case("iso") {
        return Err("Input file must have .iso extension".into());
    }
    
    // Create output path (same directory, .ziso extension)
    let mut output_path = iso_path.to_path_buf();
    output_path.set_extension("ziso");
    
    if output_path.exists() {
        return Err(format!("Output file already exists: {}", output_path.display()).into());
    }
    
    println!("Converting {} to ZISO format...", iso_path.display());
    println!("Output: {}", output_path.display());
    
    // Create a temporary directory containing the ISO
    let temp_dir = std::env::temp_dir().join(format!("netiso_convert_{}", std::process::id()));
    std::fs::create_dir_all(&temp_dir)?;
    
    // Copy ISO to temp directory
    let iso_filename = iso_path.file_name().unwrap();
    let temp_iso = temp_dir.join(iso_filename);
    
    println!("Preparing files...");
    std::fs::copy(iso_path, &temp_iso)?;
    
    // Pack the temp directory into ZISO
    println!("Compressing (this may take a while)...");
    match pack(&temp_dir, &output_path) {
        Ok(_) => {
            println!("✓ Successfully created: {}", output_path.display());
            
            // Verify the archive
            if let Ok(reader) = ZArchiveReader::open(&output_path) {
                if let Ok(files) = reader.get_files() {
                    println!("✓ Archive contains {} file(s)", files.len());
                }
            }
        },
        Err(e) => {
            // Clean up on error
            let _ = std::fs::remove_file(&output_path);
            return Err(format!("Failed to create ZISO: {}", e).into());
        }
    }
    
    // Clean up temp directory
    let _ = std::fs::remove_dir_all(&temp_dir);
    
    Ok(())
}

#[cfg(not(feature = "ziso"))]
fn convert_iso_to_ziso(_iso_path: &Path) -> Result<(), Box<dyn Error>> {
    Err("ZISO support not compiled in. Rebuild with --features ziso".into())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let mut args: Vec<String> = env::args().collect();

    let print_help = check_arg(&mut args, "-h"); // Help
    let recursive_scan = check_arg(&mut args, "-r"); // Recursive iso scanning
    let verbose = check_arg(&mut args, "-v"); // Verbose / Debug
    let convert_input = get_arg_value(&mut args, "-i"); // Convert ISO to ZISO

    // Check for mutually exclusive flags
    if convert_input.is_some() && recursive_scan {
        println!("ERROR: -i (convert mode) and -r (server mode) are mutually exclusive\n");
        print_usage(&args[0]);
        return Ok(());
    }

    // Handle conversion mode
    if let Some(iso_file) = convert_input {
        let iso_path = Path::new(&iso_file);
        return convert_iso_to_ziso(iso_path);
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

        // Update list of isos
        files = get_iso_files(&files, filepath, recursive_scan, verbose).await?;

        let files_clone = files.clone();
        tokio::spawn(async move {
            let mut srv = Server {
                files: files_clone,
                verbose: verbose,
                ..Default::default()
            };
            srv.handle_connection(socket).await
        });
    }
}
