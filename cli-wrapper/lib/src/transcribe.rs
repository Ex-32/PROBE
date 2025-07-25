use rayon::iter::{ParallelBridge, ParallelIterator};
use std::{
    collections::HashMap,
    ffi::{OsStr, OsString},
    fs::{self, DirEntry, File},
    io::Write,
    mem::size_of,
    path::{Path, PathBuf},
    time::SystemTime,
};

use crate::{
    error::{option_err, ConvertErr, ProbeError, Result, WrapErr},
    ops::{self, C_Op, FfiFrom},
};

/// Recursively parse a whole probe record directory and write it to a probe log directory.
///
/// This function calls [`parse_pid()`] on each sub-directory in `in_dir` **in parallel**.
///
/// on success, returns the number of Ops processed in the top-level directory
//  OPTIMIZE: consider improved parallelism heuristic.
pub fn parse_top_level<P1: AsRef<Path>, P2: AsRef<Path> + Sync>(
    in_dir: P1,
    out_dir: P2,
) -> Result<usize> {
    log::info!(
        "Processing arena dir {} into output dir {}",
        in_dir.as_ref().to_string_lossy(),
        out_dir.as_ref().to_string_lossy()
    );

    let start = SystemTime::now();

    let ptc = probe_headers::object_from_bytes::<probe_headers::ProcessTreeContext>(
        fs::read(
            in_dir
                .as_ref()
                .join(probe_headers::PROCESS_TREE_CONTEXT_FILE),
        )
        .map_err(|_| ProbeError::UnreachableCode)?,
    );

    fs::write(
        out_dir.as_ref().join("options.json"),
        serde_json::to_vec(&ptc).map_err(|_| ProbeError::UnreachableCode)?,
    )
    .map_err(|_| ProbeError::UnreachableCode)?;

    let pids_out_dir = out_dir.as_ref().join(probe_headers::PIDS_SUBDIR);
    fs::create_dir(&pids_out_dir).wrap_err("Failed to create pids directory")?;

    let ops_count = fs::read_dir(in_dir.as_ref().join(probe_headers::PIDS_SUBDIR))
        .wrap_err("Error opening record directory")?
        .par_bridge()
        .map(|x| {
            parse_pid(
                x.wrap_err("Error reading DirEntry from record directory")?
                    .path(),
                &pids_out_dir,
            )
        })
        .try_fold(|| 0usize, |acc, x| x.map(|x| acc + x))
        .try_reduce(|| 0usize, |id, x| Ok(id + x))?;

    let inodes_in_dir = in_dir.as_ref().join(probe_headers::INODES_SUBDIR);
    let inodes_out_dir = out_dir.as_ref().join(probe_headers::INODES_SUBDIR);
    fs::create_dir(&inodes_out_dir).wrap_err("Failed to create inodes directory")?;
    let inode_count = fs::read_dir(inodes_in_dir.clone())
        .wrap_err("Error opening inodes directory")?
        .par_bridge()
        .map(|inode_contents| {
            let name = inode_contents
                .wrap_err("Error reading from inodes directory")?
                .file_name();
            fs::hard_link(inodes_in_dir.join(name.clone()), inodes_out_dir.join(name))
                .wrap_err("Error hardlinking inode")?;
            Ok(1usize)
        })
        .try_fold(|| 0usize, |acc, x: Result<usize>| x.map(|x| acc + x))
        .try_reduce(|| 0usize, |id, x| Ok(id + x))?;

    match SystemTime::now().duration_since(start) {
        Ok(x) => log::info!(
            "Processed {} Ops, {} inodes in {:.3} seconds",
            ops_count,
            inode_count,
            x.as_secs_f32()
        ),
        Err(_) => log::error!("Processing arena dir took negative time"),
    };

    Ok(ops_count)
}

/// Recursively parse a probe record PID directory and write it as a probe log PID directory.
///
/// This function calls [`parse_exec_epoch()`] on each sub-directory in `in_dir`.
///
/// On success, returns the number of Ops processed in the PID directory.
pub fn parse_pid<P1: AsRef<Path>, P2: AsRef<Path>>(in_dir: P1, out_dir: P2) -> Result<usize> {
    let pid = filename_numeric(&in_dir).wrap_err("Error parsing numeric pid")?;

    let dir = {
        let mut path = out_dir.as_ref().to_owned();
        path.push(pid.to_string());
        path
    };

    fs::create_dir(&dir).wrap_err("Failed to create PID directory")?;

    fs::read_dir(in_dir)
        .wrap_err("Error opening PID directory")?
        // .par_bridge()
        .map(|entry| {
            parse_exec_epoch(
                entry
                    .wrap_err("Error reading DirEntry from PID directory")?
                    .path(),
                &dir,
            )
        })
        .try_fold(0usize, |acc, x| x.map(|x| acc + x))
}

/// Recursively parse a probe record exec epoch directory and write it as a probe log exec epoch
/// directory.
///
/// This function calls [`parse_tid()`] on each sub-directory in `in_dir`.
///
/// On success, returns the number of Ops processed in the ExecEpoch directory.
pub fn parse_exec_epoch<P1: AsRef<Path>, P2: AsRef<Path>>(
    in_dir: P1,
    out_dir: P2,
) -> Result<usize> {
    let epoch = filename_numeric(&in_dir).wrap_err("Error parsing numeric epoch")?;

    let dir = {
        let mut path = out_dir.as_ref().to_owned();
        path.push(epoch.to_string());
        path
    };

    fs::create_dir(&dir).wrap_err("Failed to create ExecEpoch output directory")?;

    fs::read_dir(in_dir)
        .wrap_err("Error opening ExecEpoch directory")?
        // .par_bridge()
        .map(|entry| {
            parse_tid(
                entry
                    .wrap_err("Error reading DirEntry from ExecEpoch directory")?
                    .path(),
                &dir,
            )
        })
        .try_fold(0usize, |acc, x| x.map(|x| acc + x))
}

/// Recursively parse a probe record TID directory and write it as a probe log TID directory.
///
/// This function parses a TID directory in 6 steps:
///
/// 1. Output file is created.
/// 2. Paths of sub-directory are parsed into a [`HashMap`].
/// 3. `data` directory is is read and parsed into [`DataArena`]s which are then parsed into an
///    [`ArenaContext`].
/// 4. `ops` directory is read and parsed into [`OpsArena`]s.
/// 5. [`OpsArena`]s are parsed into which are then parsed into [`ops::Op`]s using the
///    [`ArenaContext`].
/// 6. [`ops::Op`]s are serialized into json and written line-by-line into the output directory.
///
/// (steps 5 & 6 are done lazily with iterators to reduce unnecessary memory allocations)
///
/// On success, returns the number of Ops processed in the TID directory.
pub fn parse_tid<P1: AsRef<Path>, P2: AsRef<Path>>(in_dir: P1, out_dir: P2) -> Result<usize> {
    fn try_files_from_dir<P: AsRef<Path>>(dir: P) -> Result<Vec<PathBuf>> {
        match fs::read_dir(&dir) {
            Ok(entry_iter) => entry_iter
                .map(|entry_result| {
                    entry_result
                        .map(|entry| entry.path())
                        .wrap_err("Error reading DirEntry from record TID subdirectory")
                })
                .collect::<Result<Vec<_>>>(),
            Err(e) => Err(e.convert("Error opening record TID directory")),
        }
    }

    // STEP 1
    let tid = filename_numeric(&in_dir).wrap_err("Error parsing numeric tid")?;
    let mut outfile = {
        let mut path = out_dir.as_ref().to_owned();
        path.push(tid.to_string());
        File::create_new(path).wrap_err("Failed to create TID output file")?
    };

    // STEP 2
    let paths = fs::read_dir(&in_dir)
        .wrap_err("Error reading record TID directory")?
        .filter_map(|entry_result| match entry_result {
            Ok(entry) => Some((entry.file_name(), entry)),
            Err(e) => {
                log::warn!("Error reading DirEntry in TID directory: {}", e);
                None
            }
        })
        .collect::<HashMap<OsString, DirEntry>>();

    // STEP 3
    let ctx = ArenaContext(
        try_files_from_dir(
            paths
                .get(OsStr::new(probe_headers::DATA_SUBDIR))
                .ok_or_else(|| option_err("Missing data directory from TID directory"))?
                .path(),
        )?
        .into_iter()
        .map(|data_dat_file| {
            DataArena::from_bytes(
                std::fs::read(&data_dat_file)
                    .wrap_err("Failed to read file from data directory")?,
                filename_numeric(&data_dat_file).wrap_err("Error parsing numeric data/$n.dat")?,
            )
        })
        .collect::<Result<Vec<_>>>()?,
    );

    // STEP 4
    let mut count: usize = 0;
    try_files_from_dir(
        paths
            .get(OsStr::new(probe_headers::OPS_SUBDIR))
            .ok_or_else(|| option_err("Missing ops directory from TID directory"))?
            .path(),
    )?
    // STEP 5
    .into_iter()
    .map(|ops_dat_file| {
        std::fs::read(&ops_dat_file)
            .wrap_err("Failed to read file from ops directory")
            .and_then(|file_contents| {
                OpsArena::from_bytes(
                    file_contents,
                    filename_numeric(&ops_dat_file).wrap_err("Error parsing numeric ops/$n.dat")?,
                )
                .wrap_err("Error constructing OpsArena")?
                .decode(&ctx)
                .wrap_err("Error decoding OpsArena")
            })
    })
    // STEP 6
    .try_for_each(|arena_file_ops| {
        for op in arena_file_ops? {
            outfile
                .write_all(
                    serde_json::to_string(&op)
                        .wrap_err("Unable to serialize Op")?
                        .as_bytes(),
                )
                .wrap_err("Failed to write serialized Op to tempfile")?;
            outfile
                .write_all("\n".as_bytes())
                .wrap_err("Failed to write newline deliminator to tempfile")?;
            count += 1;
        }

        Ok::<(), ProbeError>(())
    })?;

    Ok(count)
}

/// Gets the [`file stem`](Path::file_stem()) from a path and returns it parsed as an integer.
///
/// Errors if the path has no file stem (see [`Path::file_stem()`] for details), the file stem
/// isn't valid UTF-8, or the filename can't be parsed as an integer.
// TODO: cleanup errors, better context
fn filename_numeric<P: AsRef<Path>>(dir: P) -> Result<usize> {
    dir.as_ref()
        .file_stem()
        .ok_or_else(|| option_err("File has no stem"))?
        .to_str()
        .ok_or_else(|| option_err("Unable to parse as Unicode"))
        .wrap_err("Failed to parse filename to integer")?
        .parse::<usize>()
        .map_err(ProbeError::ParseIntError)
}

/// this struct represents a `<TID>/data` probe record directory.
#[derive(Debug)]
pub struct ArenaContext(pub Vec<DataArena>);

impl ArenaContext {
    pub fn try_get_slice(&self, ptr: usize) -> Option<&[u8]> {
        for vec in self.0.iter() {
            if let Some(x) = vec.try_get_slice(ptr) {
                return Some(x);
            }
        }
        None
    }
}

/// This struct represents a single `data/*.dat` file from a probe record directory.
#[derive(Debug)]
pub struct DataArena {
    header: ArenaHeader,
    raw: Vec<u8>,
}

impl DataArena {
    pub fn from_bytes(bytes: Vec<u8>, instantiation: usize) -> Result<Self> {
        let header = ArenaHeader::from_bytes(&bytes, instantiation)
            .wrap_err("Failed to create ArenaHeader for DataArena")?;

        Ok(Self { header, raw: bytes })
    }

    pub fn try_get_slice(&self, ptr: usize) -> Option<&[u8]> {
        let end = self.header.base_address + self.header.used;
        match ptr >= self.header.base_address && ptr <= end {
            false => None,
            true => Some(unsafe {
                let new_ptr = self.raw.as_ptr().add(ptr - self.header.base_address);
                let len = end - ptr;

                core::slice::from_raw_parts(new_ptr, len)
            }),
        }
    }
}

/// This struct represents a single `ops/*.dat` file from a probe record directory.
pub struct OpsArena<'a> {
    // raw is needed even though it's unused since ops is a reference to it;
    // the compiler doesn't know this since it's constructed using unsafe code.
    #[allow(dead_code)]
    /// raw byte buffer of Ops arena allocator.
    raw: Vec<u8>,
    /// slice over Ops of the raw buffer.
    ops: &'a [C_Op],
}

impl OpsArena<'_> {
    pub fn from_bytes(bytes: Vec<u8>, instantiation: usize) -> Result<Self> {
        let header = ArenaHeader::from_bytes(&bytes, instantiation)
            .wrap_err("Failed to create ArenaHeader for OpsArena")?;

        if ((header.used - size_of::<ArenaHeader>()) % size_of::<C_Op>()) != 0 {
            return Err(ArenaError::Misaligned {
                size: header.used,
                header_size: size_of::<ArenaHeader>(),
                op_size: size_of::<C_Op>(),
            }
            .into());
        }

        let count = (header.used - size_of::<ArenaHeader>()) / size_of::<C_Op>();

        log::debug!("[unsafe] converting Vec<u8> to &[C_Op] of size {}", count);
        let ops = unsafe {
            let ptr = bytes.as_ptr().add(size_of::<ArenaHeader>()) as *const C_Op;
            std::slice::from_raw_parts(ptr, count)
        };

        Ok(Self { raw: bytes, ops })
    }

    pub fn decode(self, ctx: &ArenaContext) -> Result<Vec<ops::Op>> {
        self.ops
            .iter()
            .map(|x| ops::Op::ffi_from(x, ctx))
            .collect::<Result<Vec<_>>>()
            .wrap_err("Failed to decode arena ops")
    }
}

/// Arena allocator metadata placed at the beginning of arena files by libprobe.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArenaHeader {
    instantiation: libc::size_t,
    base_address: libc::uintptr_t,
    capacity: libc::uintptr_t,
    used: libc::uintptr_t,
}

impl ArenaHeader {
    /// Parse the front of a raw byte buffer into a libprobe arena header
    fn from_bytes(bytes: &[u8], instantiation: usize) -> Result<Self> {
        let ptr = bytes as *const [u8] as *const Self;

        if bytes.len() < size_of::<Self>() {
            return Err(ArenaError::BufferTooSmall {
                got: bytes.len(),
                needed: size_of::<Self>(),
            }
            .into());
        }

        log::debug!("[unsafe] converting byte buffer into ArenaHeader");
        let header = unsafe {
            Self {
                instantiation: (*ptr).instantiation,
                base_address: (*ptr).base_address,
                capacity: (*ptr).capacity,
                used: (*ptr).used,
            }
        };
        log::debug!(
            "[unsafe] created ArenaHeader [ inst={}, base_addr={:#x}, capacity: {}, used={} ]",
            header.instantiation,
            header.base_address,
            header.capacity,
            header.used
        );

        if header.capacity != bytes.len() {
            return Err(ArenaError::InvalidCapacity {
                expected: header.capacity,
                actual: bytes.len(),
            }
            .into());
        }
        if header.used > header.capacity {
            return Err(ArenaError::InvalidSize {
                size: header.used,
                capacity: header.capacity,
            }
            .into());
        }

        if header.instantiation != instantiation {
            return Err(ArenaError::InstantiationMismatch {
                header: header.instantiation,
                passed: instantiation,
            }
            .into());
        }

        Ok(header)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ArenaError {
    /// Returned if an [`ArenaHeader`] was construction was attempted with a byte buffer smaller
    /// than an [`ArenaHeader`].
    #[error("Arena buffer too small, got {got}, minimum size {needed}")]
    BufferTooSmall { got: usize, needed: usize },

    /// Returned if the [`ArenaHeader`]'s capacity value doesn't match the size of the byte buffer.
    #[error("Invalid arena capacity, expected {expected}, got {actual}")]
    InvalidCapacity { expected: usize, actual: usize },

    /// Returned if the [`ArenaHeader`]'s size value is larger than the capacity value. This
    #[error("Arena size {size} is greater than capacity {capacity}")]
    InvalidSize { size: usize, capacity: usize },

    /// Returned if an [`OpsArena`]'s size isn't isn't `HEADER_SIZE + (N * OP_SIZE)` when `N` is
    /// some integer.
    #[error("Arena alignment error: arena size ({size}) minus header ({header_size}) isn't a multiple of op size ({op_size})")]
    Misaligned {
        size: usize,
        header_size: usize,
        op_size: usize,
    },

    /// Returned if the instantiation in a [`ArenaHeader`] doesn't match the indicated one
    #[error("Header contained Instantiation ID {header}, but {passed} was indicated")]
    InstantiationMismatch { header: usize, passed: usize },
}
