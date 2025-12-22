#![allow(clippy::upper_case_acronyms)]

use binrw::{BinRead, BinWrite};
use glob::glob;
use tokio::fs::File;
use std::env;
use std::error::Error;
use std::ffi::OsStr;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::net::TcpListener;

#[cfg(feature = "zar")]
use zarchive::reader::ZArchiveReader;

const NETISO_SRV_PORT: u16 = 4323;
const SECTOR_SIZE: u16 = 0x800; // 2048
const XGD_MAGIC: &[u8; 20] = b"MICROSOFT*XBOX*MEDIA";

#[derive(Debug)]
enum IsoType {
    XSF,
    XGD2,
    XGD3
}

#[derive(Debug)]
enum IsoFile {
    Regular(File),
    #[cfg(feature = "zar")]
    Zarchive { reader: Arc<ZArchiveReader>, inner_path: String },
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
    filesize: u64,
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
}

async fn get_data_start(file: &mut File) -> Result<u64, Box<dyn std::error::Error>> {
    let offsets: [(u64, u64); 3] = [
        // XGD2 / GDF
        (0xfda0000, 0xfd90000),
        // XGD3
        (0x2090000, 0x2080000),
        // XSF
        (0x10000, 0x0)
    ];

    let len = file.metadata().await?.len();
    let mut buf = vec![0u8; XGD_MAGIC.len()];
    for (offset, data_start) in offsets {
        if len >= offset + XGD_MAGIC.len() as u64 {
            file.seek(std::io::SeekFrom::Start(offset)).await?;
            file.read_exact(&mut buf).await?;
            
            if buf == XGD_MAGIC {
                return Ok(data_start);
            }
        }
    }

    // No XGD Magic found in expected offsets
    // Assume data starts @ 0x0
    Ok(0)
}

async fn get_iso_files(old_entries: &Vec<IsoEntry>, directory: &Path, recursive: bool) -> Result<Vec<IsoEntry>, Box<dyn std::error::Error>> {
    let mut ret = old_entries.clone();

    // First, throw out obsolete entries
    ret.retain(|x| x.path.exists());

    // Assemble glob patterns for both .iso and .zar files
    #[cfg(feature = "zar")]
    let patterns = vec!["*.iso", "*.zar"];
    #[cfg(not(feature = "zar"))]
    let patterns = vec!["*.iso"];
    
    let mut all_files = Vec::new();
    
    for pattern in patterns {
        let isofiles_glob_pattern = {
            let mut path_glob = directory.to_str().unwrap().to_string();
            if recursive {
                path_glob += std::path::MAIN_SEPARATOR_STR;
                path_glob += "**"
            }

            path_glob += std::path::MAIN_SEPARATOR_STR;
            path_glob += pattern;

            path_glob
        };

        // Search for new files
        let files: Vec<PathBuf> = glob(&isofiles_glob_pattern)?
            .filter_map(|x| x.ok())
            // Filter for existing files
            .filter(|x|x.is_file())
            // Filter for new files (PathBuf not identical to any previous entry)
            .filter(|x|
                ret
                    .iter()
                    .find(|y|y.path == *x)
                    .is_none()
            )
            .collect();
        
        all_files.extend(files);
    }

    for filepath in all_files {
        let filesize = filepath.metadata()?.len();
        let filename = filepath.file_name()
            .unwrap_or(OsStr::new(""))
            .to_str()
            .unwrap_or("")
            .to_string();
        
        // Check if it's a ZAR file
        #[cfg(feature = "zar")]
        let is_zar = filepath.extension().and_then(|s| s.to_str()) == Some("zar");
        #[cfg(not(feature = "zar"))]
        let is_zar = false;
        
        let (data_start, actual_filesize) = if is_zar {
            // For ZAR files, we need to open the archive and find the ISO inside
            #[cfg(feature = "zar")]
            match ZArchiveReader::open(&filepath) {
                Ok(reader) => {
                    // Look for an ISO file inside the archive
                    // Usually ZAR archives for Xbox 360 contain a single ISO file
                    let files_result = reader.get_files();
                    match files_result {
                        Ok(files) => {
                            // Find the first .iso file in the archive
                            let iso_file = files.iter()
                                .find(|f| f.ends_with(".iso") || f.ends_with(".ISO"));
                            
                            if let Some(iso_path) = iso_file {
                                if let Some(size) = reader.file_size(iso_path) {
                                    // For ZAR files, data_start is 0 as we read directly from the decompressed stream
                                    println!("Found ISO '{}' in ZAR archive '{}'", iso_path, filename);
                                    (0, size as u64)
                                } else {
                                    eprintln!("Could not get size of ISO in ZAR: {filepath:?}");
                                    continue;
                                }
                            } else {
                                eprintln!("No ISO file found in ZAR archive: {filepath:?}");
                                continue;
                            }
                        },
                        Err(err) => {
                            eprintln!("Failed to list files in ZAR: {filepath:?}, err: {err:?}");
                            continue;
                        }
                    }
                },
                Err(err) => {
                    eprintln!("Invalid ZAR file: {filepath:?}, err: {err:?}");
                    continue;
                }
            }
            #[cfg(not(feature = "zar"))]
            {
                // ZAR support not compiled in
                eprintln!("ZAR file found but support not compiled: {filepath:?}");
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
            filesize: actual_filesize,
            data_start,
            sector_count: actual_filesize / SECTOR_SIZE as u64,
            has_type1_file: 0,
        };

        ret.push(entry);
    }

    Ok(ret)
}

async fn scan_iso_files_initial(directory: &Path, recursive: bool) -> Result<Vec<IsoEntry>, Box<dyn std::error::Error>> {
    get_iso_files(&vec![], directory, recursive).await
}

impl Server {
    async fn disable_current_iso(&mut self) {
        if self.active_file.is_some() {
            self.active_file = None;
        }
    }

    async fn handler(&mut self, mut socket: tokio::net::TcpStream) -> Result<(), Box<dyn std::error::Error>> {
        loop {
            let mut buffer = [0; 20];
            match socket.read(&mut buffer).await {
                Ok(size) => {
                    if size == 0 {
                        eprintln!("EOF - Client '{:?}' disconnected", socket.peer_addr());
                        self.disable_current_iso().await;
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
                            // Get iso sector count for mounted file
                            // If no iso is mounted, reply with 0
                            let sector_count = match &self.active_file {
                                Some(iso) => {
                                    iso.metadata.sector_count
                                },
                                None => 0,
                            };

                            let mut resp = vec![];
                            resp.extend_from_slice(&(sector_count as u32).to_be_bytes());
                            resp.extend_from_slice(&(SECTOR_SIZE as u32).to_be_bytes());

                            socket.try_write(&resp)?;
                        },
                        Cmd::HasType1File => {
                            let maybe_iso = self.files.get(msg.iso_index as usize);
                            let has_type1_file = match maybe_iso {
                                Some(iso) => {
                                    iso.has_type1_file
                                },
                                None => {
                                    0
                                }
                            };

                            socket.try_write(&has_type1_file.to_be_bytes())?;
                        },
                        Cmd::ReadData => {
                            if let Some(active) = self.active_file.as_mut() {
                                let mut buf = vec![0u8; msg.length as usize];

                                match &mut active.file {
                                    IsoFile::Regular(file) => {
                                        // Regular ISO file - use async file I/O
                                        file.seek(std::io::SeekFrom::Start(msg.offset)).await?;
                                        file.read_exact(&mut buf).await?;
                                    },
                                    #[cfg(feature = "zar")]
                                    IsoFile::Zarchive { reader, inner_path } => {
                                        // ZAR compressed file - decompress on-the-fly
                                        // The zarchive library handles decompression transparently
                                        let offset_in_iso = msg.offset - active.metadata.data_start;
                                        
                                        if let Some(data) = reader.read_from_file(inner_path, offset_in_iso as usize, msg.length as usize) {
                                            buf = data;
                                        } else {
                                            eprintln!("Failed to read from ZAR file at offset {}", msg.offset);
                                            // Return zeros on error
                                        }
                                    }
                                }

                                socket.try_write(&buf)?;
                            }
                        },
                        Cmd::GetIsoName => {
                            let maybe_iso = self.files.get(msg.iso_index as usize);
                            let filename = match maybe_iso {
                                Some(iso) => {
                                    &iso.filename
                                },
                                None => {
                                    ""
                                }
                            };
                            let mut response = filename.as_bytes().to_vec();
                            // The request contains the expected bytecount, so we extend the slice here
                            response.resize(msg.length as usize, 0);

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
                                self.disable_current_iso().await;
                                let code = 0u32;
                                socket.try_write(&code.to_be_bytes())?;
                            } else {
                                let found = self.files.iter().find(|x| x.filename.ends_with(&normalized));
    
                                let code: u32 = match found {
                                    Some(iso) => {
                                        println!("Mounting: {:?}", iso.path);
                                        
                                        // Check if it's a ZAR file
                                        #[cfg(feature = "zar")]
                                        let is_zar = iso.path.extension().and_then(|s| s.to_str()) == Some("zar");
                                        #[cfg(not(feature = "zar"))]
                                        let is_zar = false;
                                        
                                        if is_zar {
                                            // Open ZAR archive
                                            #[cfg(feature = "zar")]
                                            match ZArchiveReader::open(&iso.path) {
                                                Ok(reader) => {
                                                    // Find the ISO file inside
                                                    match reader.get_files() {
                                                        Ok(files) => {
                                                            let iso_file = files.iter()
                                                                .find(|f| f.ends_with(".iso") || f.ends_with(".ISO"));
                                                            
                                                            if let Some(inner_path) = iso_file {
                                                                println!("Found ISO in ZAR: {}", inner_path);
                                                                let file = IsoFile::Zarchive { 
                                                                    reader: Arc::new(reader), 
                                                                    inner_path: inner_path.clone() 
                                                                };
                                                                self.active_file = Some(ActiveIso { file, metadata: iso.to_owned() });
                                                                1 // success
                                                            } else {
                                                                eprintln!("MountIso: No ISO found in ZAR archive '{}'!", iso.filename);
                                                                0 // error
                                                            }
                                                        },
                                                        Err(err) => {
                                                            eprintln!("MountIso: Failed to read ZAR archive '{}': {:?}", iso.filename, err);
                                                            0 // error
                                                        }
                                                    }
                                                },
                                                Err(err) => {
                                                    eprintln!("MountIso: Failed to open ZAR archive '{}': {:?}", iso.filename, err);
                                                    0 // error
                                                }
                                            }
                                            #[cfg(not(feature = "zar"))]
                                            {
                                                eprintln!("MountIso: ZAR support not compiled in");
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
    eprintln!("Usage: {} [-rbvh] [iso directory path]", bin_name);
    eprintln!("\nArgs:");
    eprintln!("\t-r - Recursive ISO scanning");
    eprintln!("\t-b - Enable workaround for big iso library (132+ games)");
    eprintln!("\t-v - Verbose output");
    eprintln!("\t-h - Print help / usage")
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

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let mut args: Vec<String> = env::args().collect();

    let print_help = check_arg(&mut args, "-h"); // Help
    let recursive_scan = check_arg(&mut args, "-r"); // Recursive iso scanning
    let verbose = check_arg(&mut args, "-v"); // Verbose / Debug

    if print_help {
        print_usage(&args[0]);
        return Ok(());
    }
    else if args.len() < 2 {
        eprintln!("ERROR: Invalid number of arguments!");
        print_usage(&args[0]);
        return Ok(());
    }

    let filepath = Path::new(&args[1]);

    println!("Enumerating ISOs in {filepath:?}...");
    let mut files = scan_iso_files_initial(filepath, recursive_scan).await?;

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
        files = get_iso_files(&files, filepath, recursive_scan).await?;

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
