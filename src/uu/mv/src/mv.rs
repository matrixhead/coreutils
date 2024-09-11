// This file is part of the uutils coreutils package.
//
// For the full copyright and license information, please view the LICENSE
// file that was distributed with this source code.

// spell-checker:ignore (ToDO) sourcepath targetpath nushell canonicalized

mod error;

use clap::builder::ValueParser;
use clap::{crate_version, error::ErrorKind, Arg, ArgAction, ArgMatches, Command};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use std::collections::HashSet;
use std::env;
use std::ffi::OsString;
use std::fs;
use std::io;
#[cfg(unix)]
use std::os::unix;
#[cfg(windows)]
use std::os::windows;
use std::path::{Path, PathBuf};
#[cfg(unix)]
use unix::fs::FileTypeExt;
use uucore::backup_control::{self, source_is_target_backup};
use uucore::display::Quotable;
use uucore::error::{set_exit_code, FromIo, UResult, USimpleError, UUsageError};
use uucore::fs::{
    are_hardlinks_or_one_way_symlink_to_same_file, are_hardlinks_to_same_file,
    path_ends_with_terminator,
};
#[cfg(all(unix, not(any(target_os = "macos", target_os = "redox"))))]
use uucore::fsxattr;
use uucore::update_control;
use walkdir::WalkDir;

// These are exposed for projects (e.g. nushell) that want to create an `Options` value, which
// requires these enums
pub use uucore::{backup_control::BackupMode, update_control::UpdateMode};
use uucore::{format_usage, help_about, help_section, help_usage, prompt_yes, show};

use fs_extra::{
    dir::{create_all, get_size as dir_get_size, remove},
    error::Result as FsXResult,
    file::{self, CopyOptions},
};

use crate::error::MvError;

/// Options contains all the possible behaviors and flags for mv.
///
/// All options are public so that the options can be programmatically
/// constructed by other crates, such as nushell. That means that this struct is
/// part of our public API. It should therefore not be changed without good reason.
///
/// The fields are documented with the arguments that determine their value.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Options {
    /// specifies overwrite behavior
    /// '-n' '--no-clobber'
    /// '-i' '--interactive'
    /// '-f' '--force'
    pub overwrite: OverwriteMode,

    /// `--backup[=CONTROL]`, `-b`
    pub backup: BackupMode,

    /// '-S' --suffix' backup suffix
    pub suffix: String,

    /// Available update mode "--update-mode=all|none|older"
    pub update: UpdateMode,

    /// Specifies target directory
    /// '-t, --target-directory=DIRECTORY'
    pub target_dir: Option<OsString>,

    /// Treat destination as a normal file
    /// '-T, --no-target-directory
    pub no_target_dir: bool,

    /// '-v, --verbose'
    pub verbose: bool,

    /// '--strip-trailing-slashes'
    pub strip_slashes: bool,

    /// '-g, --progress'
    pub progress_bar: bool,
}

/// specifies behavior of the overwrite flag
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OverwriteMode {
    /// '-n' '--no-clobber'   do not overwrite
    NoClobber,
    /// '-i' '--interactive'  prompt before overwrite
    Interactive,
    ///'-f' '--force'         overwrite without prompt
    Force,
}

const ABOUT: &str = help_about!("mv.md");
const USAGE: &str = help_usage!("mv.md");
const AFTER_HELP: &str = help_section!("after help", "mv.md");

static OPT_FORCE: &str = "force";
static OPT_INTERACTIVE: &str = "interactive";
static OPT_NO_CLOBBER: &str = "no-clobber";
static OPT_STRIP_TRAILING_SLASHES: &str = "strip-trailing-slashes";
static OPT_TARGET_DIRECTORY: &str = "target-directory";
static OPT_NO_TARGET_DIRECTORY: &str = "no-target-directory";
static OPT_VERBOSE: &str = "verbose";
static OPT_PROGRESS: &str = "progress";
static ARG_FILES: &str = "files";

#[uucore::main]
pub fn uumain(args: impl uucore::Args) -> UResult<()> {
    let mut app = uu_app();
    let matches = app.try_get_matches_from_mut(args)?;

    let files: Vec<OsString> = matches
        .get_many::<OsString>(ARG_FILES)
        .unwrap_or_default()
        .cloned()
        .collect();

    if files.len() == 1 && !matches.contains_id(OPT_TARGET_DIRECTORY) {
        app.error(
            ErrorKind::TooFewValues,
            format!(
                "The argument '<{ARG_FILES}>...' requires at least 2 values, but only 1 was provided"
            ),
        )
        .exit();
    }

    let overwrite_mode = determine_overwrite_mode(&matches);
    let backup_mode = backup_control::determine_backup_mode(&matches)?;
    let update_mode = update_control::determine_update_mode(&matches);

    if overwrite_mode == OverwriteMode::NoClobber && backup_mode != BackupMode::NoBackup {
        return Err(UUsageError::new(
            1,
            "options --backup and --no-clobber are mutually exclusive",
        ));
    }

    let backup_suffix = backup_control::determine_backup_suffix(&matches);

    let target_dir = matches
        .get_one::<OsString>(OPT_TARGET_DIRECTORY)
        .map(OsString::from);

    if let Some(ref maybe_dir) = target_dir {
        if !Path::new(&maybe_dir).is_dir() {
            return Err(MvError::TargetNotADirectory(maybe_dir.quote().to_string()).into());
        }
    }

    let opts = Options {
        overwrite: overwrite_mode,
        backup: backup_mode,
        suffix: backup_suffix,
        update: update_mode,
        target_dir,
        no_target_dir: matches.get_flag(OPT_NO_TARGET_DIRECTORY),
        verbose: matches.get_flag(OPT_VERBOSE),
        strip_slashes: matches.get_flag(OPT_STRIP_TRAILING_SLASHES),
        progress_bar: matches.get_flag(OPT_PROGRESS),
    };

    mv(&files[..], &opts)
}

pub fn uu_app() -> Command {
    Command::new(uucore::util_name())
        .version(crate_version!())
        .about(ABOUT)
        .override_usage(format_usage(USAGE))
        .after_help(format!(
            "{AFTER_HELP}\n\n{}",
            backup_control::BACKUP_CONTROL_LONG_HELP
        ))
        .infer_long_args(true)
        .arg(
            Arg::new(OPT_FORCE)
                .short('f')
                .long(OPT_FORCE)
                .help("do not prompt before overwriting")
                .overrides_with_all([OPT_INTERACTIVE, OPT_NO_CLOBBER])
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new(OPT_INTERACTIVE)
                .short('i')
                .long(OPT_INTERACTIVE)
                .help("prompt before override")
                .overrides_with_all([OPT_FORCE, OPT_NO_CLOBBER])
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new(OPT_NO_CLOBBER)
                .short('n')
                .long(OPT_NO_CLOBBER)
                .help("do not overwrite an existing file")
                .overrides_with_all([OPT_FORCE, OPT_INTERACTIVE])
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new(OPT_STRIP_TRAILING_SLASHES)
                .long(OPT_STRIP_TRAILING_SLASHES)
                .help("remove any trailing slashes from each SOURCE argument")
                .action(ArgAction::SetTrue),
        )
        .arg(backup_control::arguments::backup())
        .arg(backup_control::arguments::backup_no_args())
        .arg(backup_control::arguments::suffix())
        .arg(update_control::arguments::update())
        .arg(update_control::arguments::update_no_args())
        .arg(
            Arg::new(OPT_TARGET_DIRECTORY)
                .short('t')
                .long(OPT_TARGET_DIRECTORY)
                .help("move all SOURCE arguments into DIRECTORY")
                .value_name("DIRECTORY")
                .value_hint(clap::ValueHint::DirPath)
                .conflicts_with(OPT_NO_TARGET_DIRECTORY)
                .value_parser(ValueParser::os_string()),
        )
        .arg(
            Arg::new(OPT_NO_TARGET_DIRECTORY)
                .short('T')
                .long(OPT_NO_TARGET_DIRECTORY)
                .help("treat DEST as a normal file")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new(OPT_VERBOSE)
                .short('v')
                .long(OPT_VERBOSE)
                .help("explain what is being done")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new(OPT_PROGRESS)
                .short('g')
                .long(OPT_PROGRESS)
                .help(
                    "Display a progress bar. \n\
                Note: this feature is not supported by GNU coreutils.",
                )
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new(ARG_FILES)
                .action(ArgAction::Append)
                .num_args(1..)
                .required(true)
                .value_parser(ValueParser::os_string())
                .value_hint(clap::ValueHint::AnyPath),
        )
}

fn determine_overwrite_mode(matches: &ArgMatches) -> OverwriteMode {
    // This does not exactly match the GNU implementation:
    // The GNU mv defaults to Force, but if more than one of the
    // overwrite options are supplied, only the last takes effect.
    // To default to no-clobber in that situation seems safer:
    //
    if matches.get_flag(OPT_NO_CLOBBER) {
        OverwriteMode::NoClobber
    } else if matches.get_flag(OPT_INTERACTIVE) {
        OverwriteMode::Interactive
    } else {
        OverwriteMode::Force
    }
}

fn parse_paths(files: &[OsString], opts: &Options) -> Vec<PathBuf> {
    let paths = files.iter().map(Path::new);

    if opts.strip_slashes {
        paths
            .map(|p| p.components().as_path().to_owned())
            .collect::<Vec<PathBuf>>()
    } else {
        paths.map(|p| p.to_owned()).collect::<Vec<PathBuf>>()
    }
}

fn handle_two_paths(source: &Path, target: &Path, opts: &Options) -> UResult<()> {
    if opts.backup == BackupMode::SimpleBackup
        && source_is_target_backup(source, target, &opts.suffix)
    {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "backing up {} might destroy source;  {} not moved",
                target.quote(),
                source.quote()
            ),
        )
        .into());
    }
    if source.symlink_metadata().is_err() {
        return Err(if path_ends_with_terminator(source) {
            MvError::CannotStatNotADirectory(source.quote().to_string()).into()
        } else {
            MvError::NoSuchFile(source.quote().to_string()).into()
        });
    }

    if (source.eq(target)
        || are_hardlinks_to_same_file(source, target)
        || are_hardlinks_or_one_way_symlink_to_same_file(source, target))
        && opts.backup == BackupMode::NoBackup
    {
        if source.eq(Path::new(".")) || source.ends_with("/.") || source.is_file() {
            return Err(
                MvError::SameFile(source.quote().to_string(), target.quote().to_string()).into(),
            );
        } else {
            return Err(MvError::SelfSubdirectory(source.display().to_string()).into());
        }
    }

    let target_is_dir = target.is_dir();
    let source_is_dir = source.is_dir();

    if path_ends_with_terminator(target)
        && (!target_is_dir && !source_is_dir)
        && !opts.no_target_dir
        && opts.update != UpdateMode::ReplaceIfOlder
    {
        return Err(MvError::FailedToAccessNotADirectory(target.quote().to_string()).into());
    }

    if target_is_dir {
        if opts.no_target_dir {
            if source.is_dir() {
                rename(source, target, opts, None).map_err_context(|| {
                    format!("cannot move {} to {}", source.quote(), target.quote())
                })
            } else {
                Err(MvError::DirectoryToNonDirectory(target.quote().to_string()).into())
            }
        // Check that source & target do not contain same subdir/dir when both exist
        // mkdir dir1/dir2; mv dir1 dir1/dir2
        } else if target.starts_with(source) {
            Err(MvError::SelfTargetSubdirectory(
                source.display().to_string(),
                target.display().to_string(),
            )
            .into())
        } else {
            move_files_into_dir(&[source.to_path_buf()], target, opts)
        }
    } else if target.exists() && source.is_dir() {
        match opts.overwrite {
            OverwriteMode::NoClobber => return Ok(()),
            OverwriteMode::Interactive => {
                if !prompt_yes!("overwrite {}? ", target.quote()) {
                    return Err(io::Error::new(io::ErrorKind::Other, "").into());
                }
            }
            OverwriteMode::Force => {}
        };
        Err(MvError::NonDirectoryToDirectory(
            source.quote().to_string(),
            target.quote().to_string(),
        )
        .into())
    } else {
        rename(source, target, opts, None).map_err(|e| USimpleError::new(1, format!("{e}")))
    }
}

fn handle_multiple_paths(paths: &[PathBuf], opts: &Options) -> UResult<()> {
    if opts.no_target_dir {
        return Err(UUsageError::new(
            1,
            format!("mv: extra operand {}", paths[2].quote()),
        ));
    }
    let target_dir = paths.last().unwrap();
    let sources = &paths[..paths.len() - 1];

    move_files_into_dir(sources, target_dir, opts)
}

/// Execute the mv command. This moves 'source' to 'target', where
/// 'target' is a directory. If 'target' does not exist, and source is a single
/// file or directory, then 'source' will be renamed to 'target'.
pub fn mv(files: &[OsString], opts: &Options) -> UResult<()> {
    let paths = parse_paths(files, opts);

    if let Some(ref name) = opts.target_dir {
        return move_files_into_dir(&paths, &PathBuf::from(name), opts);
    }

    match paths.len() {
        2 => handle_two_paths(&paths[0], &paths[1], opts),
        _ => handle_multiple_paths(&paths, opts),
    }
}

#[allow(clippy::cognitive_complexity)]
fn move_files_into_dir(files: &[PathBuf], target_dir: &Path, options: &Options) -> UResult<()> {
    // remember the moved destinations for further usage
    let mut moved_destinations: HashSet<PathBuf> = HashSet::with_capacity(files.len());

    if !target_dir.is_dir() {
        return Err(MvError::NotADirectory(target_dir.quote().to_string()).into());
    }

    let canonicalized_target_dir = target_dir
        .canonicalize()
        .unwrap_or_else(|_| target_dir.to_path_buf());

    let multi_progress = options.progress_bar.then(MultiProgress::new);

    let count_progress = if let Some(ref multi_progress) = multi_progress {
        if files.len() > 1 {
            Some(multi_progress.add(
                ProgressBar::new(files.len().try_into().unwrap()).with_style(
                    ProgressStyle::with_template("moving {msg} {wide_bar} {pos}/{len}").unwrap(),
                ),
            ))
        } else {
            None
        }
    } else {
        None
    };

    for sourcepath in files {
        if let Some(ref pb) = count_progress {
            pb.set_message(sourcepath.to_string_lossy().to_string());
        }

        let targetpath = match sourcepath.file_name() {
            Some(name) => target_dir.join(name),
            None => {
                show!(MvError::NoSuchFile(sourcepath.quote().to_string()));
                continue;
            }
        };

        if moved_destinations.contains(&targetpath) && options.backup != BackupMode::NumberedBackup
        {
            // If the target file was already created in this mv call, do not overwrite
            show!(USimpleError::new(
                1,
                format!(
                    "will not overwrite just-created '{}' with '{}'",
                    targetpath.display(),
                    sourcepath.display()
                ),
            ));
            continue;
        }

        // Check if we have mv dir1 dir2 dir2
        // And generate an error if this is the case
        if let Ok(canonicalized_source) = sourcepath.canonicalize() {
            if canonicalized_source == canonicalized_target_dir {
                // User tried to move directory to itself, warning is shown
                // and process of moving files is continued.
                show!(USimpleError::new(
                    1,
                    format!(
                        "cannot move '{}' to a subdirectory of itself, '{}/{}'",
                        sourcepath.display(),
                        target_dir.display(),
                        canonicalized_target_dir.components().last().map_or_else(
                            || target_dir.display().to_string(),
                            |dir| { PathBuf::from(dir.as_os_str()).display().to_string() }
                        )
                    )
                ));
                continue;
            }
        }

        match rename(sourcepath, &targetpath, options, multi_progress.as_ref()) {
            Err(e) if e.to_string().is_empty() => set_exit_code(1),
            Err(e) => {
                let e = e.map_err_context(|| {
                    format!(
                        "cannot move {} to {}",
                        sourcepath.quote(),
                        targetpath.quote()
                    )
                });
                match multi_progress {
                    Some(ref pb) => pb.suspend(|| show!(e)),
                    None => show!(e),
                };
            }
            Ok(()) => (),
        }
        if let Some(ref pb) = count_progress {
            pb.inc(1);
        }
        moved_destinations.insert(targetpath.clone());
    }
    Ok(())
}

fn rename(
    from: &Path,
    to: &Path,
    opts: &Options,
    multi_progress: Option<&MultiProgress>,
) -> io::Result<()> {
    let mut backup_path = None;

    if to.exists() {
        if opts.update == UpdateMode::ReplaceIfOlder && opts.overwrite == OverwriteMode::Interactive
        {
            // `mv -i --update old new` when `new` exists doesn't move anything
            // and exit with 0
            return Ok(());
        }

        if opts.update == UpdateMode::ReplaceNone {
            return Ok(());
        }

        if (opts.update == UpdateMode::ReplaceIfOlder)
            && fs::metadata(from)?.modified()? <= fs::metadata(to)?.modified()?
        {
            return Ok(());
        }

        match opts.overwrite {
            OverwriteMode::NoClobber => {
                let err_msg = format!("not replacing {}", to.quote());
                return Err(io::Error::new(io::ErrorKind::Other, err_msg));
            }
            OverwriteMode::Interactive => {
                if !prompt_yes!("overwrite {}?", to.quote()) {
                    return Err(io::Error::new(io::ErrorKind::Other, ""));
                }
            }
            OverwriteMode::Force => {}
        };

        backup_path = backup_control::get_backup_path(opts.backup, to, &opts.suffix);
        if let Some(ref backup_path) = backup_path {
            rename_with_fallback(to, backup_path, multi_progress)?;
        }
    }

    // "to" may no longer exist if it was backed up
    if to.exists() && to.is_dir() {
        // normalize behavior between *nix and windows
        if from.is_dir() {
            if is_empty_dir(to) {
                fs::remove_dir(to)?;
            } else {
                return Err(io::Error::new(io::ErrorKind::Other, "Directory not empty"));
            }
        }
    }

    rename_with_fallback(from, to, multi_progress)?;

    if opts.verbose {
        let message = match backup_path {
            Some(path) => format!(
                "renamed {} -> {} (backup: {})",
                from.quote(),
                to.quote(),
                path.quote()
            ),
            None => format!("renamed {} -> {}", from.quote(), to.quote()),
        };

        match multi_progress {
            Some(pb) => pb.suspend(|| {
                println!("{message}");
            }),
            None => println!("{message}"),
        };
    }
    Ok(())
}

/// A wrapper around `fs::rename`, so that if it fails, we try falling back on
/// copying and removing.
fn rename_with_fallback(
    from: &Path,
    to: &Path,
    multi_progress: Option<&MultiProgress>,
) -> io::Result<()> {
    if fs::rename(from, to).is_err() {
        // Get metadata without following symlinks
        let metadata = from.symlink_metadata()?;
        let file_type = metadata.file_type();

        if file_type.is_symlink() {
            rename_symlink_fallback(from, to)?;
        } else if file_type.is_dir() {
            // We remove the destination directory if it exists to match the
            // behavior of `fs::rename`. As far as I can tell, `fs_extra`'s
            // `move_dir` would otherwise behave differently.
            if to.exists() {
                fs::remove_dir_all(to)?;
            }

            // Calculate total size of directory
            // Silently degrades:
            //    If finding the total size fails for whatever reason,
            //    the progress bar wont be shown for this file / dir.
            //    (Move will probably fail due to permission error later?)
            let total_size = dir_get_size(from).ok();

            let progress_bar =
                if let (Some(multi_progress), Some(total_size)) = (multi_progress, total_size) {
                    let bar = ProgressBar::new(total_size).with_style(
                        ProgressStyle::with_template(
                            "{msg}: [{elapsed_precise}] {wide_bar} {bytes:>7}/{total_bytes:7}",
                        )
                        .unwrap(),
                    );

                    Some(multi_progress.add(bar))
                } else {
                    None
                };

            #[cfg(all(unix, not(any(target_os = "macos", target_os = "redox"))))]
            let xattrs =
                fsxattr::retrieve_xattrs(from).unwrap_or_else(|_| std::collections::HashMap::new());

            let result = move_dir(from, to, progress_bar.as_ref());

            #[cfg(all(unix, not(any(target_os = "macos", target_os = "redox"))))]
            fsxattr::apply_xattrs(to, xattrs).unwrap();

            if let Err(err) = result {
                return match err.kind {
                    fs_extra::error::ErrorKind::PermissionDenied => Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "Permission denied",
                    )),
                    _ => Err(io::Error::new(io::ErrorKind::Other, format!("{err:?}"))),
                };
            }
        } else {
            if to.is_symlink() {
                fs::remove_file(to).map_err(|err| {
                    let to = to.to_string_lossy();
                    let from = from.to_string_lossy();
                    io::Error::new(
                        err.kind(),
                        format!(
                            "inter-device move failed: '{from}' to '{to}'\
                            ; unable to remove target: {err}"
                        ),
                    )
                })?;
            }
            #[cfg(all(unix, not(any(target_os = "macos", target_os = "redox"))))]
            fs::copy(from, to)
                .and_then(|_| fsxattr::copy_xattrs(&from, &to))
                .and_then(|_| fs::remove_file(from))?;
            #[cfg(any(target_os = "macos", target_os = "redox", not(unix)))]
            fs::copy(from, to).and_then(|_| fs::remove_file(from))?;
        }
    }
    Ok(())
}

/// Move the given symlink to the given destination. On Windows, dangling
/// symlinks return an error.
#[inline]
fn rename_symlink_fallback(from: &Path, to: &Path) -> io::Result<()> {
    let path_symlink_points_to = fs::read_link(from)?;
    #[cfg(unix)]
    {
        unix::fs::symlink(path_symlink_points_to, to).and_then(|_| fs::remove_file(from))?;
    }
    #[cfg(windows)]
    {
        if path_symlink_points_to.exists() {
            if path_symlink_points_to.is_dir() {
                windows::fs::symlink_dir(&path_symlink_points_to, to)?;
            } else {
                windows::fs::symlink_file(&path_symlink_points_to, to)?;
            }
            fs::remove_file(from)?;
        } else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "can't determine symlink type, since it is dangling",
            ));
        }
    }
    #[cfg(not(any(windows, unix)))]
    {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            "your operating system does not support symlinks",
        ));
    }
    Ok(())
}

fn is_empty_dir(path: &Path) -> bool {
    match fs::read_dir(path) {
        Ok(contents) => contents.peekable().peek().is_none(),
        Err(_e) => false,
    }
}

/// Moves a directory from one location to another with progress tracking.
/// This function assumes that `from` is a directory and `to` does not exist.

/// Returns:
/// - `Result<u64>`: The total number of bytes moved if successful.
fn move_dir(from: &Path, to: &Path, progress_bar: Option<&ProgressBar>) -> FsXResult<u64> {
    // The return value that represents the number of bytes copied.
    let mut result: u64 = 0;
    let mut error_occured = false;
    for dir_entry_result in WalkDir::new(from) {
        match dir_entry_result {
            Ok(dir_entry) => {
                if dir_entry.file_type().is_dir() {
                    let path = dir_entry.into_path();
                    let tmp_to = path.strip_prefix(from)?;
                    let dir = to.join(tmp_to);
                    if !dir.exists() {
                        create_all(&dir, false)?;
                    }
                } else {
                    let file = dir_entry.path();
                    let tp = file.strip_prefix(from)?;
                    let to_file = to.join(tp);
                    let result_file_copy = copy_file(file, &to_file, progress_bar, result)?;
                    result += result_file_copy;
                }
            }
            Err(_) => {
                error_occured = true;
            }
        }
    }
    if !error_occured {
        remove(from)?;
    }
    Ok(result)
}

/// Copies a file from one path to another, updating the progress bar if provided.
fn copy_file(
    from: &Path,
    to: &Path,
    progress_bar: Option<&ProgressBar>,
    progress_bar_start_val: u64,
) -> FsXResult<u64> {
    let copy_options: CopyOptions = CopyOptions {
        // We are overwriting here based on the assumption that the update and
        // override options are handled by a parent function call.
        overwrite: true,
        ..Default::default()
    };
    let progress_handler = if let Some(progress_bar) = progress_bar {
        let display_file_name = from
            .file_name()
            .and_then(|file_name| file_name.to_str())
            .map(|file_name| file_name.to_string())
            .unwrap_or_default();
        let _progress_handler = |info: file::TransitProcess| {
            let copied_bytes = progress_bar_start_val + info.copied_bytes;
            progress_bar.set_position(copied_bytes);
        };
        progress_bar.set_message(display_file_name);
        Some(_progress_handler)
    } else {
        None
    };
    let result_file_copy = {
        let md = from.metadata()?;
        if cfg!(unix) && FileTypeExt::is_fifo(&md.file_type()) {
            let file_size = md.len();
            uucore::fs::copy_fifo(to)?;
            if let Some(progress_bar) = progress_bar {
                progress_bar.set_position(file_size + progress_bar_start_val);
            }
            Ok(file_size)
        } else {
            if let Some(progress_handler) = progress_handler {
                file::copy_with_progress(from, to, &copy_options, progress_handler)
            } else {
                file::copy(from, to, &copy_options)
            }
        }
    };
    result_file_copy
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    extern crate fs_extra;
    use super::{copy_file, move_dir};
    use fs_extra::dir::*;
    use indicatif::{ProgressBar, ProgressStyle};
    use tempfile::tempdir;

    // These tests are copied from the `fs_extra`'s repository
    #[test]
    fn it_move_work() {
        for with_progress_bar in [false, true] {
            let temp_dir = tempdir().unwrap();
            let mut path_from = PathBuf::from(temp_dir.path());
            let test_name = "sub";
            path_from.push("it_move_work");
            let mut path_to = path_from.clone();
            path_to.push("out");
            path_from.push(test_name);

            create_all(&path_from, true).unwrap();
            assert!(path_from.exists());
            create_all(&path_to, true).unwrap();
            assert!(path_to.exists());

            let mut file1_path = path_from.clone();
            file1_path.push("test1.txt");
            let content1 = "content1";
            fs_extra::file::write_all(&file1_path, content1).unwrap();
            assert!(file1_path.exists());

            let mut sub_dir_path = path_from.clone();
            sub_dir_path.push("sub");
            create(&sub_dir_path, true).unwrap();
            let mut file2_path = sub_dir_path.clone();
            file2_path.push("test2.txt");
            let content2 = "content2";
            fs_extra::file::write_all(&file2_path, content2).unwrap();
            assert!(file2_path.exists());

            let pb = if with_progress_bar {
                Some(
                    ProgressBar::new(16).with_style(
                        ProgressStyle::with_template(
                            "{msg}: [{elapsed_precise}] {wide_bar} {bytes:>7}/{total_bytes:7}",
                        )
                        .unwrap(),
                    ),
                )
            } else {
                None
            };

            let result = move_dir(&path_from, &path_to, pb.as_ref()).unwrap();

            assert_eq!(16, result);
            assert!(path_to.exists());
            assert!(!path_from.exists());
            if let Some(pb) = pb {
                assert_eq!(pb.position(), 16);
            }
        }
    }

    #[test]
    fn it_move_exist_overwrite() {
        for with_progress_bar in [false, true] {
            let temp_dir = tempdir().unwrap();
            let mut path_from = PathBuf::from(temp_dir.path());
            let test_name = "sub";
            path_from.push("it_move_exist_overwrite");
            let mut path_to = path_from.clone();
            path_to.push("out");
            path_from.push(test_name);
            let same_file = "test.txt";

            create_all(&path_from, true).unwrap();
            assert!(path_from.exists());
            create_all(&path_to, true).unwrap();
            assert!(path_to.exists());

            let mut file1_path = path_from.clone();
            file1_path.push(same_file);
            let content1 = "content1";
            fs_extra::file::write_all(&file1_path, content1).unwrap();
            assert!(file1_path.exists());

            let mut sub_dir_path = path_from.clone();
            sub_dir_path.push("sub");
            create(&sub_dir_path, true).unwrap();
            let mut file2_path = sub_dir_path.clone();
            file2_path.push("test2.txt");
            let content2 = "content2";
            fs_extra::file::write_all(&file2_path, content2).unwrap();
            assert!(file2_path.exists());

            let mut exist_path = path_to.clone();
            exist_path.push(test_name);
            create(&exist_path, true).unwrap();
            assert!(exist_path.exists());
            exist_path.push(same_file);
            let exist_content = "exist content";
            assert_ne!(exist_content, content1);
            fs_extra::file::write_all(&exist_path, exist_content).unwrap();
            assert!(exist_path.exists());

            let dir_size = get_size(&path_from).expect("failed to get dir size");
            let pb = if with_progress_bar {
                Some(
                    ProgressBar::new(dir_size).with_style(
                        ProgressStyle::with_template(
                            "{msg}: [{elapsed_precise}] {wide_bar} {bytes:>7}/{total_bytes:7}",
                        )
                        .unwrap(),
                    ),
                )
            } else {
                None
            };
            move_dir(&path_from, &path_to, pb.as_ref()).unwrap();
            assert!(exist_path.exists());
            assert!(path_to.exists());
            assert!(!path_from.exists());
            if let Some(pb) = pb {
                assert_eq!(pb.position(), dir_size);
            }
        }
    }

    #[test]
    fn it_move_inside_work_target_dir_not_exist() {
        for with_progress_bar in [false, true] {
            let temp_dir = tempdir().unwrap();
            let path_root = PathBuf::from(temp_dir.path());
            let root = path_root.join("it_move_inside_work_target_dir_not_exist");
            let root_dir1 = root.join("dir1");
            let root_dir1_sub = root_dir1.join("sub");
            let root_dir2 = root.join("dir2");
            let file1 = root_dir1.join("file1.txt");
            let file2 = root_dir1_sub.join("file2.txt");

            create_all(&root_dir1_sub, true).unwrap();
            fs_extra::file::write_all(&file1, "content1").unwrap();
            fs_extra::file::write_all(&file2, "content2").unwrap();

            if root_dir2.exists() {
                remove(&root_dir2).unwrap();
            }

            assert!(root_dir1.exists());
            assert!(root_dir1_sub.exists());
            assert!(!root_dir2.exists());
            assert!(file1.exists());
            assert!(file2.exists());
            let dir_size = get_size(&root_dir1).expect("failed to get dir size");
            let pb = if with_progress_bar {
                Some(
                    ProgressBar::new(dir_size).with_style(
                        ProgressStyle::with_template(
                            "{msg}: [{elapsed_precise}] {wide_bar} {bytes:>7}/{total_bytes:7}",
                        )
                        .unwrap(),
                    ),
                )
            } else {
                None
            };

            let result = move_dir(&root_dir1, &root_dir2, pb.as_ref()).unwrap();

            assert_eq!(16, result);
            assert!(!root_dir1.exists());
            let root_dir2_sub = root_dir2.join("sub");
            let root_dir2_file1 = root_dir2.join("file1.txt");
            let root_dir2_sub_file2 = root_dir2_sub.join("file2.txt");
            assert!(root_dir2.exists());
            assert!(root_dir2_sub.exists());
            assert!(root_dir2_file1.exists());
            assert!(root_dir2_sub_file2.exists());
            if let Some(pb) = pb {
                assert_eq!(pb.position(), dir_size);
            }
        }
    }

    #[test]
    fn copy_file_test() {
        for with_progress_bar in [false, true] {
            let temp_dir = tempdir().unwrap();
            let temp_dir_path = temp_dir.path();

            let file1_path = temp_dir_path.join("file");
            let content = "content";
            fs_extra::file::write_all(&file1_path, content).unwrap();
            assert!(file1_path.exists());
            let path_to = temp_dir_path.join("file_out");
            let pb = if with_progress_bar {
                Some(
                    ProgressBar::new(7).with_style(
                        ProgressStyle::with_template(
                            "{msg}: [{elapsed_precise}] {wide_bar} {bytes:>7}/{total_bytes:7}",
                        )
                        .unwrap(),
                    ),
                )
            } else {
                None
            };

            let result = copy_file(&file1_path, &path_to, pb.as_ref(), 0).expect("move failed");

            assert_eq!(7, result);
            assert!(path_to.exists());
            if let Some(pb) = pb {
                assert_eq!(pb.position(), 7);
            }
        }
    }
}
