use std::env;
use std::fs::File;
use std::io::Read;
use std::ptr::read;
use std::str;
use std::{fs, io};
use std::{path::Path};
use std::time::{SystemTime, UNIX_EPOCH};
use rustop::opts;
use filesize::PathExt;
use memmap::Mmap;
use chrono::DateTime;
use chrono::offset::Utc;
use chrono::prelude::*;
use flexi_logger::*;
use file_format::FileFormat;
use sysinfo::CpuExt;
use sysinfo::PidExt;
use sysinfo::{ProcessExt, System, SystemExt, DiskExt};
use arrayvec::ArrayVec;
use walkdir::WalkDir;
use csv::Error as csvError;
use csv::ReaderBuilder;
use human_bytes::human_bytes;
use sha2::{Sha256, Digest};
use md5::*;
use sha1::*;
use memmap::MmapOptions;
use yara::*;

// Specific TODOs
// - skipping non-local file systems like network mounts or cloudfs drives

// General TODOs
// - better error handling
// - putting all modules in an array and looping over that list instead of a fixed sequence
// - restructuring project to multiple files

const VERSION: &str = "2.0.0-alpha";

const SIGNATURE_SOURCE: &str = "./signatures";
const REL_EXTS: &'static [&'static str] = &[".exe", ".dll", ".bat", ".ps1", ".asp", ".aspx", ".jsp", ".jspx", 
    ".php", ".plist", ".sh", ".vbs", ".js", ".dmp"];
const MODULES: &'static [&'static str] = &["FileScan", "ProcessCheck"];
const FILE_TYPES: &'static [&'static str] = &[
    "Debian Binary Package",
    "Executable and Linkable Format",
    "Google Chrome Extension",
    "ISO 9660",
    // "Java Class", // buggy .. many other types get detected as Java Class
    "Microsoft Compiled HTML Help",
    "PCAP Dump",
    "PCAP Next Generation Dump",
    "Windows Executable",
    "Windows Shortcut",
    "ZIP",
];  // see https://docs.rs/file-format/latest/file_format/index.html

#[derive(Debug)]
struct GenMatch {
    message: String,
    score: u16,
}

struct YaraMatch {
    rulename: String,
    score: u16,
}

struct ScanConfig {
    max_file_size: usize,
    show_access_errors: bool,
    scan_all_types: bool,
}

#[derive(Debug)]
struct SampleInfo {
    MD5: String,
    SHA1: String,
    SHA256: String,
    atime: String,
    mtime: String,
    ctime: String,
}

#[derive(Debug)]
struct ExtVars {
    filename: String,
    filepath: String,
    filetype: String,
    extension: String,
    owner: String,
}

#[derive(Debug)]
struct HashIOC {
    hash_type: HashType,
    hash_value: String,
    description: String,
    score: u16,
}

#[derive(Debug)]
enum HashType {
    Md5,
    Sha1,
    Sha256,
    Unknown
}

// TODO: under construction - the data structure to hold the IOCs is still limited to 100.000 elements. 
//       I have to find a data structure that allows to store an unknown number of entries.
// Initialize the IOCs
fn initialize_hash_iocs() -> Vec<HashIOC> {
    // Compose the location of the hash IOC file
    let hash_ioc_file = format!("{}/iocs/hash-iocs.txt", SIGNATURE_SOURCE);
    // Read the hash IOC file
    let hash_iocs_string = fs::read_to_string(hash_ioc_file).expect("Unable to read hash IOC file (use --debug for more information)");
    // Configure the CSV reader
    let mut reader = ReaderBuilder::new()
        .delimiter(b';')
        .flexible(true)
        .from_reader(hash_iocs_string.as_bytes());
    // Vector that holds the hashes
    let mut hash_iocs:Vec<HashIOC> = Vec::new();
    // Read the lines from the CSV file
    for result in reader.records() {
        let record_result = result;
        let record = match record_result {
            Ok(r) => r,
            Err(e) => { log::debug!("Cannot read line in hash IOCs file (which can be okay) ERROR: {:?}", e); continue;}
        };
        // If more than two elements have been found
        if record.len() > 1 {
            // if it's not a comment line
            if !record[0].starts_with("#") {
                // determining hash type
                let hash_type: HashType = get_hash_type(&record[0]);
                log::trace!("Read hash IOC from from HASH: {} DESC: {} TYPE: {:?}", &record[0], &record[1], hash_type);
                hash_iocs.push(
                    HashIOC { 
                        hash_type: hash_type,
                        hash_value: record[0].to_ascii_lowercase(), 
                        description: record[1].to_string(), 
                        score: 100,  // TODO 
                    });
            }
        }
    }
    return hash_iocs;
}

// Get the hash type
fn get_hash_type(hash_value: &str) -> HashType {
    let hash_value_length = hash_value.len();
    match hash_value_length {
        32 => HashType::Md5,
        40 => HashType::Sha1,
        64 => HashType::Sha256,
        _ => HashType::Unknown,
    }
} 

// Initialize the rule files
fn initialize_rules() -> Rules {
    // Composed YARA rule set 
    // we're concatenating all rules from all rule files to a single string and 
    // compile them all together into a single big rule set for performance purposes
    let mut all_rules = String::new();
    let mut count = 0u16;
    // Reading the signature folder
    let yara_sigs_folder = format!("{}/yara", SIGNATURE_SOURCE);
    let files = fs::read_dir(yara_sigs_folder).unwrap();
    // Filter 
    let filtered_files = files
        .filter_map(Result::ok)
        .filter(|d| if let Some(e) = d.path().extension() { e == "yar" } else { false })
        .into_iter();
    // Test compile each rule
    for file in filtered_files {
        log::debug!("Reading YARA rule file {} ...", file.path().to_str().unwrap());
        // Read the rule file
        let rules_string = fs::read_to_string(file.path()).expect("Unable to read YARA rule file (use --debug for more information)");
        let compiled_file_result = compile_yara_rules(&rules_string);
        match compiled_file_result {
            Ok(_) => { 
                log::debug!("Successfully compiled rule file {:?} - adding it to the big set", file.path().to_str().unwrap());
                // adding content of that file to the whole rules string
                all_rules += &rules_string;
                count += 1;
            },
            Err(e) => {
                log::error!("Cannot compile rule file {:?}. Ignoring file. ERROR: {:?}", file.path().to_str().unwrap(), e)                
            }
        };
    }
    // Compile the full set and return the compiled rules
    let compiled_all_rules = compile_yara_rules(&all_rules)
        .expect("Error parsing the composed rule set");
    log::info!("Successfully compiled {} rule files into a big set", count);
    return compiled_all_rules;
}

// Compile a rule set string and check for errors
fn compile_yara_rules(rules_string: &str) -> Result<Rules, Error> {
    let mut compiler = Compiler::new().unwrap();
    compiler.define_variable("filename", "")?;
    compiler.define_variable("filepath", "")?;
    compiler.define_variable("extension", "")?;
    compiler.define_variable("filetype", "")?;
    compiler.define_variable("owner", "")?;
    // Parse the rules
    let compiler_result = compiler
        .add_rules_str(rules_string);
    // Handle parse errors
    let compiler = match compiler_result {
        Ok(c) => c,
        Err(e) => return Err(Error::from(e)),
    };
    // Compile the rules
    let compiled_rules_result = compiler.compile_rules();
    // Handle compile errors
    let compiled_rules = match compiled_rules_result {
        Ok(r) => r,
        Err(e) => return Err(Error::from(e)),
    };
    // Return the compiled rule set
    return Ok(compiled_rules);
}

// Scan process memory of all processes
fn scan_processes(compiled_rules: &Rules, scan_config: &ScanConfig) ->() {
    // Refresh the process information
    let mut sys = System::new_all();
    sys.refresh_all();
    for (pid, process) in sys.processes() {
        // Debug output : show every file that gets scanned
        log::debug!("Scanning process PID: {} NAME: {}", pid, process.name());
        // ------------------------------------------------------------
        // Matches (all types)
        let mut proc_matches = ArrayVec::<GenMatch, 100>::new();
        // ------------------------------------------------------------
        // YARA scanning
        let yara_matches = 
            compiled_rules.scan_process(pid.as_u32(), 30);
        log::debug!("Scan result: {:?}", yara_matches);
        match &yara_matches {
            Ok(_) => {},
            Err(e) => {
                if scan_config.show_access_errors { log::error!("Error while scanning process memory PROCESS: {} ERROR: {:?}", process.name(), e); }
                else { log::debug!("Error while scanning process memory PROCESS: {} ERROR: {:?}", process.name(), e); }
            }
        }
        // TODO: better scan error handling (debug messages)
        for ymatch in yara_matches.unwrap_or_default().iter() {
            if !proc_matches.is_full() {
                let match_message: String = format!("YARA match with rule {:?}", ymatch.identifier);
                //println!("{}", match_message);
                proc_matches.insert(
                    proc_matches.len(), 
                    // TODO: get score from meta data in a safe way
                    GenMatch{message: match_message, score: 75}
                );
            }
        }

        // Show matches on process
        if proc_matches.len() > 0 {
            log::warn!("Process with matches found PID: {} PROCESS: {} REASONS: {:?}", 
            pid, process.name(), proc_matches);
        }
    }
}

// Scan a given file system path
fn scan_path (target_folder: String, compiled_rules: &Rules, scan_config: &ScanConfig, hash_iocs: &Vec<HashIOC>) -> () {
    // Walk the file system
    for entry in WalkDir::new(target_folder).into_iter().filter_map(|e| e.ok()) {
        
        // Skip certain elements
        // Skip all elements that aren't files
        if !entry.path().is_file() { 
            log::trace!("Skipped element that isn't a file ELEMENT: {} TYPE: {:?}", entry.path().display(), entry.path().symlink_metadata());
            continue;
        };
        // Skip big files
        let metadata = entry.path().symlink_metadata().unwrap();
        let realsize = entry.path().size_on_disk_fast(&metadata).unwrap();
        if realsize > scan_config.max_file_size as u64 { 
            log::trace!("Skipping file due to size FILE: {} SIZE: {} MAX_FILE_SIZE: {}", 
            entry.path().display(), realsize, scan_config.max_file_size);
            continue; 
        }
        // Skip certain file types
        let extension = entry.path().extension().unwrap_or_default().to_str().unwrap();
        let file_format = FileFormat::from_file(entry.path()).unwrap_or_default();
        let file_format_desc = file_format.to_owned().to_string();
        let file_format_extension = file_format.name();

        if !FILE_TYPES.contains(&file_format_desc.as_str()) &&  // Include certain file types
            !REL_EXTS.contains(&extension) &&  // Include extensions that are in the relevant extensions list 
            !scan_config.scan_all_types  // Scan all types if user enforced it via command line flag
            { 
                log::trace!("Skipping file due to extension or type FILE: {} EXT: {:?} TYPE: {:?}", 
                entry.path().display(), extension, file_format_desc);
                continue; 
            };

        // Debug output : show every file that gets scanned
        log::debug!("Scanning file {} TYPE: {:?}", entry.path().display(), file_format_desc);
        
        // ------------------------------------------------------------
        // VARS
        // Matches (all types)
        let mut sample_matches = ArrayVec::<GenMatch, 100>::new();
        let mut sample_info: SampleInfo;

        // TIME STAMPS
        let metadata = fs::metadata(entry.path()).unwrap();
        let msecs = &metadata.modified().unwrap().duration_since(UNIX_EPOCH).unwrap().as_secs();
        let asecs = &metadata.accessed().unwrap().duration_since(UNIX_EPOCH).unwrap().as_secs();
        let csecs = &metadata.created().unwrap().duration_since(UNIX_EPOCH).unwrap().as_secs();
        let mtime = Utc.timestamp(*msecs as i64, 0);
        let atime = Utc.timestamp(*asecs as i64, 0);
        let ctime = Utc.timestamp(*csecs as i64, 0);

        // ------------------------------------------------------------
        // READ FILE
        // Read file to data blob
        let result = fs::File::open(&entry.path());
        let file_handle = match &result {
            Ok(data) => data,
            Err(e) => { 
                if scan_config.show_access_errors { log::error!("Cannot access file FILE: {:?} ERROR: {:?}", entry.path(), e); }
                else { log::debug!("Cannot access file FILE: {:?} ERROR: {:?}", entry.path(), e); }
                continue; // skip the rest of the analysis 
            }
        };
        let mmap = unsafe { MmapOptions::new().map(&file_handle).unwrap() };

        // ------------------------------------------------------------
        // IOC Matching

        // Hash Matching
        // Generate hashes
        let md5_value = format!("{:x}", md5::compute(&mmap));
        let sha1_hash_array = Sha1::new()
            .chain_update(&mmap)
            .finalize();
        let sha256_hash_array = Sha256::new()
            .chain_update(&mmap)
            .finalize();
        let sha1_value = hex::encode(&sha1_hash_array);
        let sha256_value = hex::encode(&sha256_hash_array);
        //let md5_hash = hex::encode(&md5_hash_array);
        log::trace!("Hashes of FILE: {:?} SHA256: {} SHA1: {} MD5: {}", entry.path(), sha256_value, sha1_value, md5_value);
        // Compare hashes with hash IOCs
        let mut hash_match: bool = false;
        for hash_ioc in hash_iocs.iter() {
            if !sample_matches.is_full() {
                match hash_ioc.hash_type {
                    HashType::Md5 => { if hash_ioc.hash_value == md5_value { hash_match = true; }}, 
                    HashType::Sha1 => { if hash_ioc.hash_value == sha1_value { hash_match = true; }}, 
                    HashType::Sha256 => { if hash_ioc.hash_value == sha256_value { hash_match = true; }}, 
                    _ => {},
                }
            }
            // Hash Match
            if hash_match {
                let match_message: String = format!("HASH match with IOC HASH: {} DESC: {}", hash_ioc.hash_value, hash_ioc.description);
                sample_matches.insert(
                    sample_matches.len(), 
                    // TODO: get meta data in a safe way from Vec structure
                    GenMatch{message: match_message, score: hash_ioc.score}
                );
            }
        }
        
        // ------------------------------------------------------------
        // SAMPLE INFO 
        let sample_info = SampleInfo {
            MD5: md5_value,
            SHA1: sha1_value,
            SHA256: sha256_value,
            atime: atime.to_rfc3339(),
            mtime: mtime.to_rfc3339(),
            ctime: ctime.to_rfc3339(),
        };

        // ------------------------------------------------------------
        // YARA scanning
        // Preparing the external variables
        let ext_vars = ExtVars{
            filename: entry.path().file_name().unwrap().to_string_lossy().to_string(),
            filepath: entry.path().parent().unwrap().to_string_lossy().to_string(),
            extension: extension.to_string(),
            filetype: file_format_extension.to_ascii_uppercase(),
            owner: "".to_string(),  // TODO
        };
        log::trace!("Passing external variables to the scan EXT_VARS: {:?}", ext_vars);
        // Actual scanning and result analysis
        let yara_matches = 
            scan_file(&compiled_rules, &file_handle, scan_config, &ext_vars);
        for ymatch in yara_matches.iter() {
            if !sample_matches.is_full() {
                let match_message: String = format!("YARA match with rule {}", ymatch.rulename);
                sample_matches.insert(
                    sample_matches.len(), 
                    // TODO: get meta data in a safe way from Vec structure
                    GenMatch{message: match_message, score: ymatch.score}
                );
            }
        }
        // Scan Results
        if sample_matches.len() > 0 {
            // Calculate a total score
            let mut total_score: u16 = 0; 
            for sm in sample_matches.iter() {
                total_score += sm.score;
            }
            // Print line
            // TODO: print all matches in a nested form
            log::warn!("File match found FILE: {} {:?} SCORE: {} REASONS: {:?}", 
                entry.path().display(), 
                sample_info, 
                total_score, 
                sample_matches);
        }
    }
}

// scan a file
fn scan_file(rules: &Rules, file_handle: &File, scan_config: &ScanConfig, ext_vars: &ExtVars) -> ArrayVec<YaraMatch, 100> {
    // Preparing the external variables
    // Preparing the scanner
    let mut scanner = rules.scanner().unwrap();
    scanner.set_timeout(10);
    scanner.define_variable("filename", ext_vars.filename.as_str()).unwrap();
    scanner.define_variable("filepath", ext_vars.filepath.as_str()).unwrap();
    scanner.define_variable("extension", ext_vars.extension.as_str()).unwrap();
    scanner.define_variable("filetype", ext_vars.filetype.as_str()).unwrap();
    scanner.define_variable("owner", ext_vars.owner.as_str()).unwrap();
    // Scan file
    let results = scanner.scan_fd(file_handle);
    match &results {
        Ok(_) => {},
        Err(e) => { 
            if scan_config.show_access_errors { log::error!("Cannot access file descriptor ERROR: {:?}", e); }
        }
    }
    //println!("{:?}", results);
    let mut yara_matches = ArrayVec::<YaraMatch, 100>::new();
    for _match in results.iter() {
        if _match.len() > 0 {
            log::debug!("MATCH FOUND: {:?} LEN: {}", _match, _match.len());
            if !yara_matches.is_full() {
                yara_matches.insert(
                    yara_matches.len(), 
                    YaraMatch{rulename: _match[0].identifier.to_string(), score: 60}
                );
            }
        }
    }
    return yara_matches;
}

// Evaluate platform & environment information
fn evaluate_env() {
    let mut sys = System::new_all();
    sys.refresh_all();
    // Command line arguments 
    let args: Vec<String> = env::args().collect();
    log::info!("Command line flags FLAGS: {:?}", args);
    // OS
    log::info!("Operating system information OS: {} ARCH: {}", env::consts::OS, env::consts::ARCH);
    // System Names
    log::info!("System information NAME: {:?} KERNEL: {:?} OS_VER: {:?} HOSTNAME: {:?}",
    sys.name().unwrap(), sys.kernel_version().unwrap(), sys.os_version().unwrap(), sys.host_name().unwrap());
    // CPU
    log::info!("CPU information NUM_CORES: {} FREQUENCY: {:?} VENDOR: {:?}", 
    sys.cpus().len(), sys.cpus()[0].frequency(), sys.cpus()[0].vendor_id());
    // Memory
    log::info!("Memory information TOTAL: {:?} USED: {:?}", 
    human_bytes(sys.total_memory() as f64), human_bytes(sys.used_memory() as f64));
    // Hard disks
    for disk in sys.disks() {
        log::info!(
            "Hard disk NAME: {:?} FS_TYPE: {:?} MOUNT_POINT: {:?} AVAIL: {:?} TOTAL: {:?} REMOVABLE: {:?}", 
            disk.name(), 
            str::from_utf8(disk.file_system()).unwrap(), 
            disk.mount_point(), 
            human_bytes(disk.available_space() as f64),
            human_bytes(disk.total_space() as f64),
            disk.is_removable(),
        );
    }
}

// Log file format for files
fn log_file_format(
    write: &mut dyn std::io::Write,
    now: &mut flexi_logger::DeferredNow,
    record: &log::Record,
 ) -> std::io::Result<()> {
    write!(
        write,
        "[{}] {} {}",
        now.format("%Y-%m-%dT%H:%M:%SZ"),
        record.level(),
        &record.args()
    )
}

// Log file format for command line
fn log_cmdline_format(
    w: &mut dyn std::io::Write,
    _now: &mut DeferredNow,
    record: &Record,
) -> Result<(), std::io::Error> {
    let level = record.level();
    write!(
        w,
        "[{}] {}",
        style(level).paint(level.to_string()),
        record.args().to_string()
    )
}

// Welcome message
fn welcome_message() {
    println!("------------------------------------------------------------------------");
    println!("     __   ____  __ ______  ____                                        ");
    println!("    / /  / __ \\/ //_/  _/ / __/______ ____  ___  ___ ____              ");
    println!("   / /__/ /_/ / ,< _/ /  _\\ \\/ __/ _ `/ _ \\/ _ \\/ -_) __/           ");
    println!("  /____/\\____/_/|_/___/ /___/\\__/\\_,_/_//_/_//_/\\__/_/              ");
    println!("  Simple IOC and YARA Scanner                                           ");
    println!(" ");
    println!("  Version {} (Rust)                                            ", VERSION);
    println!("  Florian Roth 2022                                                     ");
    println!(" ");
    println!("------------------------------------------------------------------------");                      
}

fn main() {

    // Show welcome message
    welcome_message();

    // Parsing command line flags
    let (args, _rest) = opts! {
        synopsis "LOKI YARA and IOC Scanner";
        opt max_file_size:usize=10_000_000, desc:"Maximum file size to scan";
        opt show_access_errors:bool, desc:"Show all file and process access errors";
        opt scan_all_files:bool, desc:"Scan all files regardless of their file type / extension";
        opt debug:bool, desc:"Show debugging information";
        opt trace:bool, desc:"Show very verbose trace output";
        opt noprocs:bool, desc:"Don't scan processes";
        opt nofs:bool, desc:"Don't scan the file system";
        opt folder:Option<String>, desc:"Folder to scan"; // an optional (positional) parameter
    }.parse_or_exit();
    // Create a config
    let scan_config = ScanConfig {
        max_file_size: args.max_file_size,
        show_access_errors: args.show_access_errors,
        scan_all_types: args.scan_all_files,
    };

    // Logger
    let mut log_level: String = "info".to_string(); let mut std_out = Duplicate::Info; // default
    if args.debug { log_level = "debug".to_string(); std_out = Duplicate::Debug; }  // set to debug level
    if args.trace { log_level = "trace".to_string(); std_out = Duplicate::Trace; }  // set to trace level
    let mut sys = System::new_all();
    sys.refresh_all();
    let log_file_name = format!("loki_{}", sys.host_name().unwrap());
    Logger::try_with_str(log_level).unwrap()
        .log_to_file(
            FileSpec::default()
                .basename(log_file_name)
        )
        .use_utc()
        .format(log_cmdline_format)
        .format_for_files(log_file_format)
        .duplicate_to_stdout(std_out)
        .append()
        .start()
        .unwrap();
    log::info!("LOKI scan started VERSION: {}", VERSION);

    // Print platform & environment information
    evaluate_env();

    // Evaluate active modules
    let mut active_modules: ArrayVec<String, 20> = ArrayVec::<String, 20>::new();
    for module in MODULES {
        if args.noprocs && module.to_string() == "ProcessCheck" { continue; }
        if args.nofs && module.to_string() == "FileScan" { continue; }
        active_modules.insert(active_modules.len(), module.to_string());
    }
    log::info!("Active modules MODULES: {:?}", active_modules);

    // Set some default values
    // default target folder
    let mut target_folder: String = '/'.to_string(); 
    if env::consts::OS.to_string() == "windows" { target_folder = "C:\\".to_string(); }
    // if target folder has ben set via command line flag
    if let Some(args_target_folder) = args.folder {
        target_folder = args_target_folder;
    }
    
    // Initialize IOCs 
    // TODO: not ready yet
    log::info!("Initialize hash IOCs ...");
    let hash_iocs = initialize_hash_iocs();

    // Initialize the rules
    log::info!("Initializing YARA rules ...");
    let compiled_rules = initialize_rules();

    // Process scan
    if active_modules.contains(&"ProcessCheck".to_owned()) {
        log::info!("Scanning running processes ... ");
        scan_processes(&compiled_rules, &scan_config);
    }

    // File system scan
    if active_modules.contains(&"FileScan".to_owned()) {
        log::info!("Scanning local file system ... ");
        scan_path(target_folder, &compiled_rules, &scan_config, &hash_iocs);
    }

    // Finished scan
    log::info!("LOKI scan finished");
}