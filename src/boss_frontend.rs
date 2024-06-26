use std::path::Path;
use std::process::ExitCode;
use std::io::Write;
use std::sync::Mutex;

use clap::{Parser, ValueEnum, CommandFactory};
use env_logger::{Env, fmt::Color};
use indicatif::{ProgressBar, HumanBytes, ProgressStyle};
use log::info;
use log::{debug, error};
use regex::Regex;
use yaml_rust::{YamlLoader, Yaml};
use lazy_static::{lazy_static};

use crate::profiling::{dump_all_profiling, start_timer, stop_timer, self};
use crate::logger_and_progress::LoggerAndProgress;
use crate::{boss_launch::*, profile_this, function_name, boss_deploy};
use crate::boss_sync::*;

/// Fast rsync-like tool for incrementally copying files.
///
/// Runs natively on both Windows and Linux
/// and uses network for communication, to maximise speed when syncing between Windows and WSL
/// filesystems.
///
/// Also see README.md on GitHub for more documentation: https://github.com/Robert-Hughes/rjrssync/blob/main/README.md
#[derive(clap::Parser)]
#[command(version)]
pub struct BossCliArgs {
    /// The source path. Must be an existing file, folder or symlink, local or remote. Format: [[username@]hostname:]path
    ///
    /// If a file or symlink is provided, only that single item will be copied (symlinks are not followed).
    /// If a folder is provided, all its contents will be copied as well, recursively. Symlinks inside the folder are never followed.
    #[arg(required_unless_present_any=["spec", "generate_auto_complete_script", "list_embedded_binaries"], conflicts_with="spec")]
    src: Option<RemotePathDesc>,
    /// The destination path. Can be existent or non-existent, local or remote. Format: [[username@]hostname:]path
    ///
    /// The sync will make the destination path equivalent to the source path, replacing whatever
    /// is there already.
    ///
    /// One exception to this is that if the source is a file/symlink and the destination path ends
    /// with a trailing slash, the file will be placed inside a destination folder (created if necessary).
    ///
    /// Some examples:
    ///
    ///   * Syncing a folder to a folder will update the contents of the destination folder to match the source folder.
    ///
    ///   * Syncing a folder to a file will delete the destination file and copy the source folder in its place
    ///
    ///   * Syncing a file to a destination path like "dest/" will copy the file inside the "dest/" folder
    ///
    ///   * Syncing a file to a symlink will delete the destination symlink and copy the source file its place
    ///
    #[arg(required_unless_present_any=["spec", "generate_auto_complete_script", "list_embedded_binaries"], conflicts_with="spec")]
    dest: Option<RemotePathDesc>,

    /// Instead of providing SRC and DEST, a YAML file can be used to define the sync.
    ///
    /// The file has the following structure:
    ///
    ///     # Omit src_hostname/dest_hostname for local targets
    ///     src_hostname: source.domain.com
    ///     src_username: root
    ///     dest_hostname: dest.domain.com
    ///     dest_username: myuser
    ///     syncs:
    ///       - src: /root/source
    ///         dest: /home/myuser/dest
    ///         # See description of the --filter parameter
    ///         filters: [ "+.*\.txt", "-garbage\.txt" ]
    ///         dest_file_newer_behaviour: error
    ///         dest_file_older_behaviour: skip
    ///         dest_entry_needs_deleting_behaviour: prompt
    ///         dest_root_needs_deleting_behaviour: delete
    ///       # Multiple paths can be synced
    ///       - src: /root/source2
    ///         dest: /home/myuser/dest2
    ///
    /// If the same argument is given in both the spec file and on the command-line,
    /// the command-line value will take precedence.
    #[arg(long, verbatim_doc_comment)]
    spec: Option<String>,

    #[arg(long)]
    ssh_identity_file: Option<String>,

    /// Ignore or include matching entries inside a folder being synced
    ///
    /// Can be specified multiple times to define a list of filters.
    /// Each filter is a '+' or '-' character followed by a regular expression (https://docs.rs/regex/latest/regex/#syntax).
    /// The '+'/'-' indicates if this filter includes (+) or excludes (-) matching entries.
    ///
    /// If the first filter is an include (+), then only those entries matching this filter will be synced.
    /// If the first filter is an exclude (-), then entries matching this filter will *not* be synced.
    /// Further filters can then override this decision.
    ///
    /// The regexes are matched against a 'normalized' path relative to the root path of the source/dest:
    ///
    ///    * Forward slashes are always used as directory separators, even on Windows platforms
    ///
    ///    * There are never any trailing slashes
    ///
    ///    * Matches are done against the entire normalized path - a substring match is not sufficient
    ///
    /// If a folder is excluded, then the contents of the folder will not be inspected,
    /// even if they would otherwise be included by the filters.
    ///
    /// For example:
    ///
    ///     * --filter '+.*\.txt' --filter '-subfolder'  Syncs all files with the extension .txt, but not inside `subfolder`
    ///
    #[arg(name="filter", long, allow_hyphen_values(true))]
    filter: Vec<String>,

    /// Show which files/folders will be copied or deleted, without making any real changes.
    #[arg(long)]
    dry_run: bool,

    /// Hide the progress bar.
    ///
    /// In some cases this can increase performance, especially on systems with a lower number of CPU cores.
    #[arg(long)]
    no_progress: bool,

    /// Show additional statistics about the files and folders copied.
    //
    // This is a separate flag to --verbose, because that is more for debugging, but this is useful for normal users
    #[arg(long)]
    stats: bool,

    /// Hide all output except warnings, errors and prompts.
    #[arg(short, long, group="verbosity")]
    quiet: bool,
    /// Show additional output, useful for debugging.
    #[arg(short, long, group="verbosity")]
    verbose: bool,

    /// Override the TCP port for the remote rjrssync to listen on.
    ///
    /// For remote targets, rjrssync connects to a remote copy of itself using a TCP conenction.
    /// If not specified, a free TCP port is chosen automatically.
    #[arg(long)]
    remote_port: Option<u16>,

    /// Behaviour for deploying rjrssync to remote targets.
    ///
    /// If a remote target doesn't have rjrssync, or the version it has is incompatible with this version,
    /// then a new version will need to be deployed.
    /// The default is 'prompt'.
    // This uploads a binary to a folder on the remote target, so we check with the user first.
    // (the default isn't defined here, because it's defined in SyncSpec::default() and if we duplicate it
    //  here then we'll have no way of knowing if the user provided it on the cmd prompt as an override or not)
    #[arg(long)]
    deploy: Option<DeployBehaviour>,

    /// Behaviour when a file exists on both source and destination sides, but the destination file has a newer modified timestamp.
    ///
    /// This might indicate that data is about to be unintentionally lost.
    /// The default is 'prompt'.
    // (the default isn't defined here, because it's defined in SyncSpec::default() and if we duplicate it
    //  here then we'll have no way of knowing if the user provided it on the cmd prompt as an override or not)
    #[arg(long)]
    dest_file_newer: Option<DestFileUpdateBehaviour>,

    /// Behaviour when a file exists on both source and destination sides, and the destination file has a older modified timestamp.
    ///
    /// This might indicate that data is about to be unintentionally lost.
    /// The default is 'overwrite'.
    // (the default isn't defined here, because it's defined in SyncSpec::default() and if we duplicate it
    //  here then we'll have no way of knowing if the user provided it on the cmd prompt as an override or not)
    // Although this option might not be very useful most of the time, it completes the set of options needed
    // for --all-destructive-behaviour to be available (i.e. to prevent anything destructive)
    #[arg(long)]
    dest_file_older: Option<DestFileUpdateBehaviour>,

    /// Behaviour when a file/folder/symlink on the destination side needs to be deleted.
    ///
    /// This might indicate that data is about to be unintentionally lost.
    /// The default is 'delete'.
    // (the default isn't defined here, because it's defined in SyncSpec::default() and if we duplicate it
    //  here then we'll have no way of knowing if the user provided it on the cmd prompt as an override or not)
    #[arg(long)]
    dest_entry_needs_deleting: Option<DestEntryNeedsDeletingBehaviour>,

    /// Behaviour when the entire root path on the destination needs to be deleted.
    ///
    /// This might indicate that data is about to be unintentionally lost.
    /// The default is 'prompt'.
    // This is separate to --dest-entry-needs-deleting, because there is some potentially
    // surprising behaviour with regards to replacing the destination root that warrants
    // special warning.
    // (the default isn't defined here, because it's defined in SyncSpec::default() and if we duplicate it
    //  here then we'll have no way of knowing if the user provided it on the cmd prompt as an override or not)
    #[arg(long)]
    dest_root_needs_deleting: Option<DestRootNeedsDeletingBehaviour>,

    /// Behaviour when a file exists on both source and destination sides,
    /// and both files have the same modified timestamp.
    ///
    /// This can be useful to force copying, even if the destination appears to be up-to-date.
    /// The default is 'skip'.
    // (the default isn't defined here, because it's defined in SyncSpec::default() and if we duplicate it
    //  here then we'll have no way of knowing if the user provided it on the cmd prompt as an override or not)
    #[arg(long)]
    files_same_time: Option<DestFileUpdateBehaviour>,

    /// Behaviour when any destructive action is required.
    ///
    /// This might indicate that data is about to be unintentionally lost.
    /// This is a convenience for setting the following flags, if their default value or
    /// value set in the spec file could lead to data being lost.
    ///
    ///   --dest-file-newer
    ///
    ///   --dest-file-older
    ///
    ///   --dest-entry-needs-deleting
    ///
    ///   --dest-root-needs-deleting
    ///
    ///   --files-same-time
    ///
    /// If any of these arguments are also set individually, their value will take precedence.
    /// This can be useful for running rjrssync in a "safe" mode (set this to 'prompt' or 'error'),
    /// or in an unattended mode (set this to 'proceed').
    #[arg(long)]
    all_destructive_behaviour: Option<AllDestructiveBehaviour>,

    /// List the binaries embedded inside this program ready for deployment to remote targets, instead of performing a sync.
    #[arg(long)]
    list_embedded_binaries: bool,

    /// Output an auto-complete script for the provided shell, instead of performing a sync.
    ///
    /// For example, to configure auto-complete for bash:
    ///
    ///     rjrssync --generate-auto-complete-script=bash > /usr/share/bash-completion/completions/rjrssync.bash
    ///
    /// And for PowerShell:
    ///
    /// Add the following line to the file "C:\Users\<USER>\Documents\WindowsPowerShell\Microsoft.PowerShell_profile.ps1" (create the file if it doesn't exist):
    ///
    ///     rjrssync --generate-auto-complete-script=powershell | Out-String | Invoke-Expression
    ///
    #[arg(long, verbatim_doc_comment)]
    generate_auto_complete_script: Option<clap_complete::Shell>,

    /// [Internal] Launches as a doer process, rather than a boss process.
    /// This shouldn't be needed for regular operation.
    #[arg(long, hide(true))]
    doer: bool,
}

/// Describes a local or remote path, parsed from the `src` or `dest` command-line arguments.
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct RemotePathDesc {
    pub username: String,
    pub hostname: String,
    // Note this shouldn't be a PathBuf, because the syntax of this path will be for the remote system,
    // which might be different to the local system.
    pub path: String,
}
impl std::str::FromStr for RemotePathDesc {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // There's some quirks here with windows paths containing colons for drive letters

        let mut r = RemotePathDesc::default();

        // The first colon splits path from the rest, apart from special case for drive letters
        match s.split_once(':') {
            None => {
                r.path = s.to_string();
            }
            Some((a, b)) if a.len() == 1 && (b.is_empty() || b.starts_with('\\')) => {
                r.path = s.to_string();
            }
            Some((user_and_host, path)) => {
                r.path = path.to_string();

                // The first @ splits the user and hostname
                match user_and_host.split_once('@') {
                    None => {
                        r.hostname = user_and_host.to_string();
                    }
                    Some((user, host)) => {
                        r.username = user.to_string();
                        if r.username.is_empty() {
                            return Err("Missing username".to_string());
                        }
                        r.hostname = host.to_string();
                    }
                };
                if r.hostname.is_empty() {
                    return Err("Missing hostname".to_string());
                }
            }
        };

        if r.path.is_empty() {
            return Err("Path must be specified".to_string());
        }

        Ok(r)
    }
}

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, ValueEnum, Debug)]
pub enum DeployBehaviour {
    /// The user will be asked what to do if a deploy is needed.
    /// (In a non-interactive environment, this is equivalent to 'error')
    Prompt,
    /// Deploying is not allowed and will instead raise an error if it is required, and the sync will not happen.
    Error,
    /// rjrssync will be deployed as necessary.
    Ok,
    /// rjrssync will be deployed regardless if the remote target already has an up-to-date version.
    Force,
}

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, ValueEnum, Debug)]
pub enum DestFileUpdateBehaviour {
    /// The user will be asked what to do. (In a non-interactive environment, this is equivalent to 'error')
    Prompt,
    /// An error will be raised, the sync will stop and the destination file will not be overwritten.
    Error,
    /// The destination file will not be modified and the rest of the sync will continue.
    Skip,
    /// The destination file will be overwritten and the rest of the sync will continue.
    Overwrite,
}

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, ValueEnum, Debug)]
pub enum DestEntryNeedsDeletingBehaviour {
    /// The user will be asked what to do. (In a non-interactive environment, this is equivalent to 'error')
    Prompt,
    /// An error will be raised, the sync will stop and the destination file will not be deleted.
    Error,
    /// The destination file will not be deleted and the rest of the sync will continue.
    /// Note that this choice may lead to later errors, as the entry that needed deleting might be preventing
    /// something else from being copied there.
    Skip,
    /// The destination entry will be deleted and the rest of the sync will continue.
    Delete,
}

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, ValueEnum, Debug)]
pub enum DestRootNeedsDeletingBehaviour {
    /// The user will be asked what to do. (In a non-interactive environment, this is equivalent to 'error')
    Prompt,
    /// An error will be raised, the sync will stop and the destination will not be changed.
    Error,
    /// The destination root will not be deleted and the sync will stop, but no error will be raised.
    /// The only difference between this and 'error' is that rjrssync will still report success, even though
    /// nothing has been synced.
    Skip,
    /// The destination root will be deleted and the rest of the sync will continue.
    Delete,
}

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, ValueEnum, Debug)]
pub enum AllDestructiveBehaviour {
    /// The user will be asked what to do. (In a non-interactive environment, this is equivalent to 'error')
    Prompt,
    /// An error will be raised, the sync will stop and the destructive action will not take place.
    Error,
    /// The destructive action will not take place and the rest of the sync will continue, if possible.
    Skip,
    /// The destructive action will take place and the rest of the sync will continue.
    Proceed,
}

/// The hostname/usernames are fixed for the whole program (you can't set them differently for each
/// sync like you can with the filters etc.), because this doesn't bring much benefit over just
/// running rjrssync multiple times with different arguments. We do allow syncing multiple folders
/// between the same two hosts though because this saves the connecting/setup time.
#[derive(Debug, PartialEq)]
struct Spec {
    src_hostname: String,
    src_username: String,
    dest_hostname: String,
    dest_username: String,
    deploy_behaviour: DeployBehaviour,
    syncs: Vec<SyncSpec>,
}
impl Default for Spec {
    fn default() -> Self {
        Self {
            src_hostname: String::from(""),
            src_username: String::from(""),
            dest_hostname: String::from(""),
            dest_username: String::from(""),
            deploy_behaviour: DeployBehaviour::Prompt,
            syncs: vec![],
        }
    }
}

#[derive(Debug, PartialEq)]
pub struct SyncSpec {
    pub src: String,
    pub dest: String,
    pub filters: Vec<String>,
    pub dest_file_newer_behaviour: DestFileUpdateBehaviour,
    pub dest_file_older_behaviour: DestFileUpdateBehaviour,
    pub files_same_time_behaviour: DestFileUpdateBehaviour,
    pub dest_entry_needs_deleting_behaviour: DestEntryNeedsDeletingBehaviour,
    pub dest_root_needs_deleting_behaviour: DestRootNeedsDeletingBehaviour,
}
impl Default for SyncSpec {
    fn default() -> Self {
        Self {
            src: String::new(),
            dest: String::new(),
            filters: vec![],
            dest_file_newer_behaviour: DestFileUpdateBehaviour::Prompt,
            dest_file_older_behaviour: DestFileUpdateBehaviour::Overwrite,
            files_same_time_behaviour: DestFileUpdateBehaviour::Skip,
            dest_entry_needs_deleting_behaviour: DestEntryNeedsDeletingBehaviour::Delete,
            dest_root_needs_deleting_behaviour: DestRootNeedsDeletingBehaviour::Prompt,
        }
    }
}

fn parse_string(yaml: &Yaml, key_name: &str) -> Result<String, String> {
    match yaml {
        Yaml::String(x) => Ok(x.to_string()),
        x => Err(format!("Unexpected value for '{}'. Expected a string, but got {:?}", key_name, x)),
    }
}

fn parse_sync_spec(yaml: &Yaml) -> Result<SyncSpec, String> {
    let mut result = SyncSpec::default();
    for (root_key, root_value) in yaml.as_hash().ok_or("Sync value must be a dictionary")? {
        match root_key {
            Yaml::String(x) if x == "src" => result.src = parse_string(root_value, "src")?,
            Yaml::String(x) if x == "dest" => result.dest = parse_string(root_value, "dest")?,
            Yaml::String(x) if x == "filters" => {
                match root_value {
                    Yaml::Array(array_yaml) => {
                        for element_yaml in array_yaml {
                            match element_yaml {
                                Yaml::String(x) => result.filters.push(x.to_string()),
                                x => return Err(format!("Unexpected value in 'filters' array. Expected string, but got {:?}", x)),
                            }
                        }
                    }
                    x => return Err(format!("Unexpected value for 'filters'. Expected an array, but got {:?}", x)),
                }
            },
            Yaml::String(x) if x == "dest_file_newer_behaviour" =>
                result.dest_file_newer_behaviour = DestFileUpdateBehaviour::from_str(&parse_string(root_value, "dest_file_newer_behaviour")?, true)?,
            Yaml::String(x) if x == "dest_file_older_behaviour" =>
                result.dest_file_older_behaviour = DestFileUpdateBehaviour::from_str(&parse_string(root_value, "dest_file_older_behaviour")?, true)?,
            Yaml::String(x) if x == "files_same_time_behaviour" =>
                result.files_same_time_behaviour = DestFileUpdateBehaviour::from_str(&parse_string(root_value, "files_same_time_behaviour")?, true)?,
            Yaml::String(x) if x == "dest_entry_needs_deleting_behaviour" =>
                result.dest_entry_needs_deleting_behaviour = DestEntryNeedsDeletingBehaviour::from_str(&parse_string(root_value, "dest_entry_needs_deleting_behaviour")?, true)?,
            Yaml::String(x) if x == "dest_root_needs_deleting_behaviour" =>
                result.dest_root_needs_deleting_behaviour = DestRootNeedsDeletingBehaviour::from_str(&parse_string(root_value, "dest_root_needs_deleting_behaviour")?, true)?,
            x => return Err(format!("Unexpected key in 'syncs' entry: {:?}", x)),
        }
    }

    if result.src.is_empty() {
        return Err("src must be provided and non-empty".to_string());
    }
    if result.dest.is_empty() {
        return Err("dest must be provided and non-empty".to_string());
    }

    Ok(result)
}

fn parse_spec_file(path: &Path) -> Result<Spec, String> {
    profile_this!();
    let mut result = Spec::default();

    let contents = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let docs = YamlLoader::load_from_str(&contents).map_err(|e| e.to_string())?;
    if docs.len() < 1 {
        // We allow >1 doc, but just ignore the rest, this might be useful for users, to use like a comments or versions
        return Err("Expected at least one YAML document".to_string());
    }
    let doc = &docs[0];

    for (root_key, root_value) in doc.as_hash().ok_or("Document root must be a dictionary")? {
        match root_key {
            Yaml::String(x) if x == "src_hostname" => result.src_hostname = parse_string(root_value, "src_hostname")?,
            Yaml::String(x) if x == "src_username" => result.src_username = parse_string(root_value, "src_username")?,
            Yaml::String(x) if x == "dest_hostname" => result.dest_hostname = parse_string(root_value, "dest_hostname")?,
            Yaml::String(x) if x == "dest_username" => result.dest_username = parse_string(root_value, "dest_username")?,
            Yaml::String(x) if x == "deploy_behaviour" => result.deploy_behaviour = DeployBehaviour::from_str(&parse_string(root_value, "deploy_behaviour")?, true)?,
            Yaml::String(x) if x == "syncs" => {
                match root_value {
                    Yaml::Array(syncs_yaml) => {
                        for sync_yaml in syncs_yaml {
                            result.syncs.push(parse_sync_spec(sync_yaml)?);
                        }
                    }
                    x => return Err(format!("Unexpected value for 'syncs'. Expected an array, but got {:?}", x)),
                }
            },
            x => return Err(format!("Unexpected key in root dictionary: {:?}", x)),
        }
    }

    Ok(result)
}

pub fn boss_main() -> ExitCode {
    let timer = start_timer(function_name!());

    let args = {
        profile_this!("Parsing cmd line");
        BossCliArgs::parse()
    };

    // Configure logging, based on the user's --quiet/--verbose flag.
    // If the RUST_LOG env var is set though then this overrides everything, as this is useful for developers
    let logging_timer = profiling::start_timer("Configuring logging");
    let args_level = match (args.quiet, args.verbose) {
        (true, false) => "warn",
        (false, true) => "debug",
        (false, false) => "info",
        (true, true) => panic!("Shouldn't be allowed by cmd args parser"),
    };
    let mut builder = env_logger::Builder::from_env(Env::default().default_filter_or(args_level));
    builder.format(|buf, record| {
        // Strip "rjrssync::" prefix, as this doesn't add anything
        let target = record.target().replace("rjrssync::", "");
        let target_style = if target.contains("boss") {
            buf.style().set_color(Color::Rgb(255, 64, 255)).clone()
        } else if target.contains("remote") {
            buf.style().set_color(Color::Yellow).clone()
        } else if target.contains("doer") {
            buf.style().set_color(Color::Cyan).clone()
        } else {
            buf.style()
        };

        let level_style = buf.default_level_style(record.level());

        match record.level() {
            log::Level::Info => {
                // Info messages are intended for the average user, so format them plainly
                writeln!(
                    buf,
                    "{}",
                    record.args()
                )
            }
            log::Level::Warn | log::Level::Error => {
                // Warn/error messages are also for a regular user, but deserve a prefix indicating
                // that they are an error/warning
                writeln!(
                    buf,
                    "{}: {}",
                    level_style.value(record.level()),
                    record.args()
                )
            }
            log::Level::Debug | log::Level::Trace => {
                // Debug/trace messages are for developers or power-users, so have more detail
                writeln!(
                    buf,
                    "{} {:5} | {}: {}",
                    buf.timestamp_nanos(),
                    level_style.value(record.level()),
                    target_style.value(target),
                    record.args()
                )
            }
        }
    });
    let logger = builder.build();

    log::set_max_level(logger.filter());

    // Wrap the env_logger::Logger in our own wrapper, which handles ProgressBar hiding.
    // Use Box::leak to turn it into a 'static reference, which is required for the log API
    let log_wrapper = Box::new(LoggerAndProgress::new(logger, !args.quiet));
    let log_wrapper = Box::leak(log_wrapper);
    log::set_logger(log_wrapper).expect("Failed to init logging");

    profiling::stop_timer(logging_timer);
    profiling::stop_timer(timer); // Have to stop this before calling boss_main_impl, as that will dump all the profiling

    let result = boss_main_impl(args, log_wrapper.get_progress_bar());

    // The LoggerAndProgress never gets dropped (due to Box::leak), so we manually clean it up here.
    log_wrapper.shutdown();

    result
}

fn boss_main_impl(args: BossCliArgs, progress_bar: &ProgressBar) -> ExitCode {
    let timer = start_timer(function_name!());
    debug!("Running as boss");

    if let Some(shell) = args.generate_auto_complete_script {
        let mut cmd = BossCliArgs::command();
        let name = cmd.get_name().to_string();
        clap_complete::generate(shell, &mut cmd, name, &mut std::io::stdout());
        return ExitCode::SUCCESS;
    }

    if args.list_embedded_binaries {
        match boss_deploy::get_embedded_binaries() {
            Ok(eb) => {
                for b in eb.0.binaries {
                    println!("{} ({}, {})", b.target_triple, HumanBytes(b.data.len() as u64),
                        if eb.0.is_compressed { "compressed" } else { "uncompressed" });
                }
                return ExitCode::SUCCESS;
            }
            Err(e) => {
                error!("Error getting embedded binaries: {e}");
                return ExitCode::from(19);
            }
        }
    }

    // Decide what to sync - defined either on the command line or in a spec file if provided
    let spec = match resolve_spec(&args) {
        Ok(s) => s,
        Err(e) => {
            error!("{}", e);
            return ExitCode::from(18);
        }
    };

    let exit_code = execute_spec(spec, &args, progress_bar);

    stop_timer(timer);

    dump_all_profiling();

    // Dump memory usage figures when used for benchmarking. There isn't a good way of determining this from the benchmarking app
    // (especially for remote processes), so we instrument it instead.
    if std::env::var("RJRSSYNC_TEST_DUMP_MEMORY_USAGE").is_ok() {
        println!("Boss peak memory usage: {}", profiling::get_peak_memory_usage());
    }

    exit_code
}

/// Figures out the Spec that we should execute, from a combination of the command-line args
/// and a --spec file (if provided)
fn resolve_spec(args: &BossCliArgs) -> Result<Spec, String> {
    let mut spec = Spec::default();
    match &args.spec {
        Some(s) => {
            // If --spec was provided, use that as the starting point
            spec = match parse_spec_file(Path::new(&s)) {
                Ok(s) => s,
                Err(e) => return Err(format!("Failed to parse spec file at '{}': {}", s, e)),
            }
            // Some things in the spec file are overridable by command line equivalents (behaviours, filters etc.)
            // which is done below
        },
        None => {
            // No spec - the command-line must have the src and dest specified
            let src = args.src.as_ref().unwrap(); // Command-line parsing rules means these must be valid, if spec is not provided
            let dest = args.dest.as_ref().unwrap();
            spec.src_hostname = src.hostname.clone();
            spec.src_username = src.username.clone();
            spec.dest_hostname = dest.hostname.clone();
            spec.dest_username = dest.username.clone();
            spec.syncs.push(SyncSpec {
                src: src.path.clone(),
                dest: dest.path.clone(),
                ..Default::default()
            });
            // The rest of the command-line arguments are applied below (as they are also relevant
            // when a spec file is used).
        }
    }

    // Apply additional command-line args, which may override/augment what's in the spec file.
    if let Some(b) = args.deploy {
        spec.deploy_behaviour = b;
    }
    for mut sync in &mut spec.syncs {
        if !args.filter.is_empty() {
            sync.filters = args.filter.clone();
        }

        if let Some(b) = args.all_destructive_behaviour {
            // We don't want --all-destructive-behaviour
            // to override things set to Skip (by default or in the spec file),
            // as then you would get a bunch of errors/prompts/etc. that for things
            // that weren't going to be overwritten anyway.
            // The idea is that --all-destructive-behaviour
            // can be used to prevent any accidental data loss.
            // Therefore we only modify behaviours if they're set to something other than Skip
            if sync.dest_file_newer_behaviour != DestFileUpdateBehaviour::Skip {
                sync.dest_file_newer_behaviour = match b {
                    AllDestructiveBehaviour::Prompt => DestFileUpdateBehaviour::Prompt,
                    AllDestructiveBehaviour::Error => DestFileUpdateBehaviour::Error,
                    AllDestructiveBehaviour::Skip => DestFileUpdateBehaviour::Skip,
                    AllDestructiveBehaviour::Proceed => DestFileUpdateBehaviour::Overwrite,
                }
            }
            if sync.dest_file_older_behaviour != DestFileUpdateBehaviour::Skip {
                sync.dest_file_older_behaviour = match b {
                    AllDestructiveBehaviour::Prompt => DestFileUpdateBehaviour::Prompt,
                    AllDestructiveBehaviour::Error => DestFileUpdateBehaviour::Error,
                    AllDestructiveBehaviour::Skip => DestFileUpdateBehaviour::Skip,
                    AllDestructiveBehaviour::Proceed => DestFileUpdateBehaviour::Overwrite,
                }
            }
            if sync.files_same_time_behaviour != DestFileUpdateBehaviour::Skip {
                sync.files_same_time_behaviour = match b {
                    AllDestructiveBehaviour::Prompt => DestFileUpdateBehaviour::Prompt,
                    AllDestructiveBehaviour::Error => DestFileUpdateBehaviour::Error,
                    AllDestructiveBehaviour::Skip => DestFileUpdateBehaviour::Skip,
                    AllDestructiveBehaviour::Proceed => DestFileUpdateBehaviour::Overwrite,
                }
            }
            if sync.dest_entry_needs_deleting_behaviour != DestEntryNeedsDeletingBehaviour::Skip {
                sync.dest_entry_needs_deleting_behaviour = match b {
                    AllDestructiveBehaviour::Prompt => DestEntryNeedsDeletingBehaviour::Prompt,
                    AllDestructiveBehaviour::Error => DestEntryNeedsDeletingBehaviour::Error,
                    AllDestructiveBehaviour::Skip => DestEntryNeedsDeletingBehaviour::Skip,
                    AllDestructiveBehaviour::Proceed => DestEntryNeedsDeletingBehaviour::Delete,
                }
            }
            if sync.dest_root_needs_deleting_behaviour != DestRootNeedsDeletingBehaviour::Skip {
                sync.dest_root_needs_deleting_behaviour = match b {
                    AllDestructiveBehaviour::Prompt => DestRootNeedsDeletingBehaviour::Prompt,
                    AllDestructiveBehaviour::Error => DestRootNeedsDeletingBehaviour::Error,
                    AllDestructiveBehaviour::Skip => DestRootNeedsDeletingBehaviour::Skip,
                    AllDestructiveBehaviour::Proceed => DestRootNeedsDeletingBehaviour::Delete,
                }
            }
        }

        // Individual behaviours specified on the command-line override everything else
        // (spec file and --all-destructive-behaviour).
        if let Some(b) = args.dest_file_newer {
            sync.dest_file_newer_behaviour = b;
        }
        if let Some(b) = args.dest_file_older {
            sync.dest_file_older_behaviour = b;
        }
        if let Some(b) = args.files_same_time {
            sync.files_same_time_behaviour = b;
        }
        if let Some(b) = args.dest_entry_needs_deleting {
            sync.dest_entry_needs_deleting_behaviour = b;
        }
        if let Some(b) = args.dest_root_needs_deleting {
            sync.dest_root_needs_deleting_behaviour = b;
        }
    }

    Ok(spec)
}

fn execute_spec(spec: Spec, args: &BossCliArgs, progress_bar: &ProgressBar) -> ExitCode {
    // The src and/or dest may be on another computer. We need to run a copy of rjrssync on the remote
    // computer(s) and set up network commmunication.
    // There are therefore up to three copies of our program involved (although some may actually be the same as each other)
    //   Boss - this copy, which received the command line from the user
    //   Source - runs on the computer specified by the `src` command-line arg, and so if this is the local computer
    //            then this may be the same copy as the Boss. If it's remote then it will be a remote doer process.
    //   Dest - the computer specified by the `dest` command-line arg, and so if this is the local computer
    //          then this may be the same copy as the Boss. If it's remote then it will be a remote doer process.
    //          If Source and Dest are the same computer, they are still separate copies for simplicity.
    //          (It might be more efficient to just have one remote copy, but remember that there could be different users specified
    //           on the Source and Dest, with separate permissions to the paths being synced, so they can't access each others' paths,
    //           in which case we couldn't share a copy. Also might need to make it multithreaded on the other end to handle
    //           doing one command at the same time for each Source and Dest, which might be more complicated.)

    // Configure the progress bar for the first phase (connecting to remote doers).
    // Functions inside setup_comms will set the message appropriately.
    // Note the use of wide_msg to prevent line wrapping issues if terminal too narrow
    progress_bar.set_style(ProgressStyle::with_template("{wide_msg}").unwrap());
    // Unfortunately we can't use enable_steady_tick to get a nice animation as we connect, because
    // this will clash with potential ssh output/prompts

    // Launch doers on remote hosts or threads on local targets and estabilish communication (check version etc.)
    let mut src_comms = match setup_comms(
        &spec.src_hostname,
        &spec.src_username,
        args.remote_port,
        args.ssh_identity_file.clone(),
        "src".to_string(),
        spec.deploy_behaviour,
        &progress_bar,
    ) {
        Ok(c) => c,
        Err(e) => {
            error!("Error connecting to {}: {}", spec.src_hostname, e);
            return ExitCode::from(10);
        }
    };
    let mut dest_comms = match setup_comms(
        &spec.dest_hostname,
        &spec.dest_username,
        args.remote_port,
        args.ssh_identity_file.clone(),
        "dest".to_string(),
        spec.deploy_behaviour,
        &progress_bar,
    ) {
        Ok(c) => c,
        Err(e) => {
            error!("Error connecting to {}: {}", spec.dest_hostname, e);
            src_comms.shutdown(); // Clean shutdown
            return ExitCode::from(11);
        }
    };

    // Perform the actual file sync(s)
    for sync_spec in &spec.syncs {
        // Indicate which sync this is, if there are many
        if spec.syncs.len() > 1 {
            info!("{} => {}:", sync_spec.src, sync_spec.dest);
        }

        // No point showing progress when doing a dry run
        let show_progress = !args.no_progress && !args.dry_run;
        let sync_result = sync(&sync_spec, args.dry_run, &progress_bar, show_progress,
            args.stats, &mut src_comms, &mut dest_comms);

        match sync_result {
            Ok(()) => (),
            Err(e) => {
                error!("Sync error: {}", e);
                 // Clean shutdown
                src_comms.shutdown();
                dest_comms.shutdown();
                return ExitCode::from(12);
            }
        }
    }

    // Shutdown the comms before dumping profiling, so that any doer threads and comms threads have cleanly exited,
    // and their profiling data is saved, and we have received profiling data from any remote doer processes.
    src_comms.shutdown();
    dest_comms.shutdown();

    ExitCode::SUCCESS
}

/// For testing purposes, this env var can be set to a list of responses to prompts
/// that we might display, which we use immediately rather than waiting for a real user
/// to respond.
const TEST_PROMPT_RESPONSE_ENV_VAR: &str = "RJRSSYNC_TEST_PROMPT_RESPONSE";

lazy_static! {
    // We're only accessing this on one thread, but the compiler doesn't know that so we need a mutex.
    // It's only used for the prompt code, so performance should not be a concern.
    static ref TEST_PROMPT_RESPONSES: Mutex<TestPromptResponses> = Mutex::new(TestPromptResponses::from_env());
}

struct TestPromptResponses {
    responses: Vec<(usize, Regex, String)>
}
impl TestPromptResponses {
    fn from_env() -> TestPromptResponses {
        let mut result = TestPromptResponses { responses: vec![] };
        if let Ok(all_responses) = std::env::var(TEST_PROMPT_RESPONSE_ENV_VAR) {
            // The env var is a comma-separated list of entries, where each entry has
            // a regex defining what prompts it matches, a maximum number of prompts that it
            // can be used to respond to and the prompt response itself.
            // The count reduces each time the response is used,
            // and once it hits zero it will no longer be used as a response.
            for max_occurences_and_regex in all_responses.split(',') {
                if max_occurences_and_regex.is_empty() {
                    continue;
                }
                let mut parts = max_occurences_and_regex.splitn(3, ':');
                let max_occurences = parts.next().expect("Invalid syntax").parse::<usize>().expect("Invalid number");
                let regex = Regex::new(parts.next().expect("Invalid syntax")).expect("Invalid regex");
                let response = parts.next().expect("Invalid syntax");
                result.responses.push((max_occurences, regex, response.to_string()));
            }
        }
        result
    }

    /// Gets the response to use for the given prompt, and reduces the max occurences count accordingly.
    fn get_response(&mut self, prompt: &str) -> Option<String> {
        for (max_occurences, regex, response) in &mut self.responses {
            if regex.is_match(prompt) && *max_occurences > 0 {
                *max_occurences -= 1;
                return Some(response.clone());
            }
        }
        None
    }
}

#[derive(Clone, Copy)]
pub struct ResolvePromptResult<B> {
    /// The decision that was made for this occurence.
    pub immediate_behaviour: B,
    /// The decision that was made to be remembered for future occurences (if any)
    pub remembered_behaviour: Option<B>,
}
impl<B: Copy> ResolvePromptResult<B> {
    fn once(b: B) -> Self {
        Self { immediate_behaviour: b, remembered_behaviour: None }
    }
    fn always(b: B) -> Self {
        Self { immediate_behaviour: b, remembered_behaviour: Some(b) }
    }
}

pub fn resolve_prompt<B: Copy>(prompt: String, progress_bar: Option<&ProgressBar>,
    options: &[(&str, B)], include_always_versions: bool, cancel_behaviour: B) -> ResolvePromptResult<B> {

    let mut items = vec![];
    for o in options {
        if include_always_versions {
            items.push((format!("{} (just this occurence)", o.0), ResolvePromptResult::once(o.1)));
            items.push((format!("{} (all occurences)", o.0), ResolvePromptResult::always(o.1)));
        } else {
            items.push((String::from(o.0), ResolvePromptResult::once(o.1)));
        }
    }
    items.push((String::from("Cancel sync"), ResolvePromptResult::once(cancel_behaviour)));

    // Allow overriding the prompt response for testing
    let mut response_idx = None;
    if let Some(auto_response) = TEST_PROMPT_RESPONSES.lock().expect("Mutex problem").get_response(&prompt) {
        // Print the prompt anyway, so the test can confirm that it was hit
        println!("{}", prompt);
        response_idx = Some(items.iter().position(|i| i.0 == auto_response).expect("Invalid response"));
    }
    let response_idx = match response_idx {
        Some(r) => r,
        None => {
            if !dialoguer::console::user_attended() {
                debug!("Unattended terminal, behaving as if prompt cancelled");
                items.len() - 1 // Last entry is always cancel
            } else {
                // The prompt message provided as input to this function may have styling applied
                // (e.g. if it comes from our PrettyPath), which won't play nicely with the styling applied
                // by the theme for the prompt. To work around this, we modify the provided prompt to append
                // any occurences of the "reset" ANSI code with the code(s) that re-apply the theme used by
                // the whole prompt.
                let theme = dialoguer::theme::ColorfulTheme::default();
                // Figure out the ANSI code(s) needed to apply the prompt theme
                let style_begin = theme.prompt_style.apply_to("").to_string().replace("\x1b[0m", "");
                // Append these to any occurences of RESET on the original prompt string.
                let prompt = prompt.replace("\x1b[0m", &format!("\x1b[0m{style_begin}"));

                let f = || {
                    let r = dialoguer::Select::with_theme(&theme)
                        .with_prompt(prompt)
                        .items(&items.iter().map(|i| &i.0).collect::<Vec<&String>>())
                        .default(0).interact_opt();
                    let response = match r {
                        Ok(Some(i)) => i,
                        _ => items.len() - 1 // Last entry is always cancel, e.g. if user presses q or Esc
                    };
                    response
                };

                match progress_bar {
                    Some(p) => p.suspend(f), // Hide the progress bar while showing this message, otherwise the background tick will redraw it over our prompt!
                    None => f(),
                }
            }
        }
    };

    items[response_idx].1
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;
    use tempfile::NamedTempFile;

    use super::*;

    #[test]
    fn parse_remote_path_desc() {
        // There's some quirks here with windows paths containing colons for drive letters

        assert_eq!(
            RemotePathDesc::from_str(""),
            Err("Path must be specified".to_string())
        );
        assert_eq!(
            RemotePathDesc::from_str("f"),
            Ok(RemotePathDesc {
                path: "f".to_string(),
                ..Default::default()
            })
        );
        assert_eq!(
            RemotePathDesc::from_str("h:f"),
            Ok(RemotePathDesc {
                path: "f".to_string(),
                hostname: "h".to_string(),
                username: "".to_string()
            })
        );
        assert_eq!(
            RemotePathDesc::from_str("hh:"),
            Err("Path must be specified".to_string())
        );
        assert_eq!(
            RemotePathDesc::from_str(":f"),
            Err("Missing hostname".to_string())
        );
        assert_eq!(
            RemotePathDesc::from_str(":"),
            Err("Missing hostname".to_string())
        );
        assert_eq!(
            RemotePathDesc::from_str("@"),
            Ok(RemotePathDesc {
                path: "@".to_string(),
                ..Default::default()
            })
        );

        assert_eq!(
            RemotePathDesc::from_str("u@h:f"),
            Ok(RemotePathDesc {
                path: "f".to_string(),
                hostname: "h".to_string(),
                username: "u".to_string()
            })
        );
        assert_eq!(
            RemotePathDesc::from_str("@h:f"),
            Err("Missing username".to_string())
        );
        assert_eq!(
            RemotePathDesc::from_str("u@h:"),
            Err("Path must be specified".to_string())
        );
        assert_eq!(
            RemotePathDesc::from_str("u@:f"),
            Err("Missing hostname".to_string())
        );
        assert_eq!(
            RemotePathDesc::from_str("@:f"),
            Err("Missing username".to_string())
        );
        assert_eq!(
            RemotePathDesc::from_str("u@:"),
            Err("Missing hostname".to_string())
        );
        assert_eq!(
            RemotePathDesc::from_str("@h:"),
            Err("Missing username".to_string())
        );

        assert_eq!(
            RemotePathDesc::from_str("u@f"),
            Ok(RemotePathDesc {
                path: "u@f".to_string(),
                ..Default::default()
            })
        );
        assert_eq!(
            RemotePathDesc::from_str("@f"),
            Ok(RemotePathDesc {
                path: "@f".to_string(),
                ..Default::default()
            })
        );
        assert_eq!(
            RemotePathDesc::from_str("u@"),
            Ok(RemotePathDesc {
                path: "u@".to_string(),
                ..Default::default()
            })
        );

        assert_eq!(
            RemotePathDesc::from_str("u:u@u:u@h:f:f:f@f"),
            Ok(RemotePathDesc {
                path: "u@u:u@h:f:f:f@f".to_string(),
                hostname: "u".to_string(),
                username: "".to_string()
            })
        );

        assert_eq!(
            RemotePathDesc::from_str(r"C:\Path\On\Windows"),
            Ok(RemotePathDesc {
                path: r"C:\Path\On\Windows".to_string(),
                ..Default::default()
            })
        );
        assert_eq!(
            RemotePathDesc::from_str(r"C:"),
            Ok(RemotePathDesc {
                path: r"C:".to_string(),
                ..Default::default()
            })
        );
        assert_eq!(
            RemotePathDesc::from_str(r"C:\"),
            Ok(RemotePathDesc {
                path: r"C:\".to_string(),
                ..Default::default()
            })
        );
        assert_eq!(
            RemotePathDesc::from_str(r"C:folder"),
            Ok(RemotePathDesc {
                path: r"folder".to_string(),
                hostname: "C".to_string(),
                ..Default::default()
            })
        );
        assert_eq!(
            RemotePathDesc::from_str(r"C:\folder"),
            Ok(RemotePathDesc {
                path: r"C:\folder".to_string(),
                ..Default::default()
            })
        );
        assert_eq!(
            RemotePathDesc::from_str(r"CC:folder"),
            Ok(RemotePathDesc {
                path: r"folder".to_string(),
                hostname: "CC".to_string(),
                ..Default::default()
            })
        );
        assert_eq!(
            RemotePathDesc::from_str(r"CC:\folder"),
            Ok(RemotePathDesc {
                path: r"\folder".to_string(),
                hostname: "CC".to_string(),
                ..Default::default()
            })
        );
        assert_eq!(
            RemotePathDesc::from_str(r"s:C:\folder"),
            Ok(RemotePathDesc {
                path: r"C:\folder".to_string(),
                hostname: "s".to_string(),
                ..Default::default()
            })
        );
        assert_eq!(
            RemotePathDesc::from_str(r"u@s:C:\folder"),
            Ok(RemotePathDesc {
                path: r"C:\folder".to_string(),
                hostname: "s".to_string(),
                username: "u".to_string()
            })
        );

        assert_eq!(
            RemotePathDesc::from_str(r"\\network\share\windows"),
            Ok(RemotePathDesc {
                path: r"\\network\share\windows".to_string(),
                ..Default::default()
            })
        );

        assert_eq!(
            RemotePathDesc::from_str("/unix/absolute"),
            Ok(RemotePathDesc {
                path: "/unix/absolute".to_string(),
                ..Default::default()
            })
        );
        assert_eq!(
            RemotePathDesc::from_str("username@server:/unix/absolute"),
            Ok(RemotePathDesc {
                path: "/unix/absolute".to_string(),
                hostname: "server".to_string(),
                username: "username".to_string()
            })
        );
    }

    #[test]
    fn test_parse_spec_file_missing() {
        let err = parse_spec_file(Path::new("does/not/exist")).unwrap_err();
        // Check for Windows and Linux error messages
        assert!(err.contains("cannot find the path") || err.contains("No such file"));
    }

    #[test]
    fn test_parse_spec_file_empty() {
        let s = NamedTempFile::new().unwrap();
        assert!(parse_spec_file(s.path()).unwrap_err().contains("Expected at least one YAML document"));
    }

    #[test]
    fn test_parse_spec_file_invalid_syntax() {
        let mut s = NamedTempFile::new().unwrap();
        writeln!(s, "!!").unwrap();
        assert!(parse_spec_file(s.path()).unwrap_err().contains("did not find expected tag"));
    }

    #[test]
    fn test_parse_spec_file_all_fields() {
        let mut s = NamedTempFile::new().unwrap();
        write!(s, r#"
            src_hostname: "computer1"
            src_username: "user1"
            dest_hostname: "computer2"
            dest_username: "user2"
            deploy_behaviour: ok
            syncs:
            - src: T:\Source1
              dest: T:\Dest1
              filters: [ "-exclude1", "-exclude2" ]
              dest_file_newer_behaviour: error
              dest_file_older_behaviour: skip
              files_same_time_behaviour: overwrite
              dest_entry_needs_deleting_behaviour: prompt
              dest_root_needs_deleting_behaviour: delete
            - src: T:\Source2
              dest: T:\Dest2
              filters: [ "-exclude3", "-exclude4" ]
              dest_file_newer_behaviour: prompt
              dest_file_older_behaviour: overwrite
              files_same_time_behaviour: error
              dest_entry_needs_deleting_behaviour: error
              dest_root_needs_deleting_behaviour: skip
        "#).unwrap();

        let expected_result = Spec {
            src_hostname: "computer1".to_string(),
            src_username: "user1".to_string(),
            dest_hostname: "computer2".to_string(),
            dest_username: "user2".to_string(),
            deploy_behaviour: DeployBehaviour::Ok,
            syncs: vec![
                SyncSpec {
                    src: "T:\\Source1".to_string(),
                    dest: "T:\\Dest1".to_string(),
                    filters: vec![ "-exclude1".to_string(), "-exclude2".to_string() ],
                    dest_file_newer_behaviour: DestFileUpdateBehaviour::Error,
                    dest_file_older_behaviour: DestFileUpdateBehaviour::Skip,
                    files_same_time_behaviour: DestFileUpdateBehaviour::Overwrite,
                    dest_entry_needs_deleting_behaviour: DestEntryNeedsDeletingBehaviour::Prompt,
                    dest_root_needs_deleting_behaviour: DestRootNeedsDeletingBehaviour::Delete,
                },
                SyncSpec {
                    src: "T:\\Source2".to_string(),
                    dest: "T:\\Dest2".to_string(),
                    filters: vec![ "-exclude3".to_string(), "-exclude4".to_string() ],
                    dest_file_newer_behaviour: DestFileUpdateBehaviour::Prompt,
                    dest_file_older_behaviour: DestFileUpdateBehaviour::Overwrite,
                    files_same_time_behaviour: DestFileUpdateBehaviour::Error,
                    dest_entry_needs_deleting_behaviour: DestEntryNeedsDeletingBehaviour::Error,
                    dest_root_needs_deleting_behaviour: DestRootNeedsDeletingBehaviour::Skip,
                }
            ]
        };

        assert_eq!(parse_spec_file(s.path()), Ok(expected_result));
    }

    /// Checks that parse_spec_file() allows some fields to be omitted, with sensible defaults.
    #[test]
    fn test_parse_spec_file_default_fields() {
        let mut s = NamedTempFile::new().unwrap();
        write!(s, r#"
            syncs:
            - src: T:\Source1
              dest: T:\Dest1
        "#).unwrap();

        let expected_result = Spec {
            src_hostname: "".to_string(), // Default - not specified in the YAML
            src_username: "".to_string(), // Default - not specified in the YAML
            dest_hostname: "".to_string(), // Default - not specified in the YAML
            dest_username: "".to_string(), // Default - not specified in the YAML
            deploy_behaviour: DeployBehaviour::Prompt, // Default - not specified in the YAML
            syncs: vec![
                SyncSpec {
                    src: "T:\\Source1".to_string(),
                    dest: "T:\\Dest1".to_string(),
                    filters: vec![], // Default - not specified in the YAML
                    ..Default::default()
                },
            ]
        };

        assert_eq!(parse_spec_file(s.path()), Ok(expected_result));
    }

    /// Checks that parse_spec_file() errors if required fields are omitted.
    #[test]
    fn test_parse_spec_file_missing_required_src() {
        let mut s = NamedTempFile::new().unwrap();
        write!(s, r#"
            syncs:
            - src: T:\Source1
        "#).unwrap();

        assert!(parse_spec_file(s.path()).unwrap_err().contains("dest must be provided and non-empty"));
    }

    /// Checks that parse_spec_file() errors if required fields are omitted.
    #[test]
    fn test_parse_spec_file_missing_required_dest() {
        let mut s = NamedTempFile::new().unwrap();
        write!(s, r#"
            syncs:
            - dest: T:\Dest1
        "#).unwrap();

        assert!(parse_spec_file(s.path()).unwrap_err().contains("src must be provided and non-empty"));
    }

    #[test]
    fn test_parse_spec_file_invalid_root() {
        let mut s = NamedTempFile::new().unwrap();
        write!(s, "123").unwrap();
        assert!(parse_spec_file(s.path()).unwrap_err().contains("Document root must be a dictionary"));
    }

    #[test]
    fn test_parse_spec_file_invalid_string_field() {
        let mut s = NamedTempFile::new().unwrap();
        write!(s, "dest_hostname: [ 341 ]").unwrap();
        assert!(parse_spec_file(s.path()).unwrap_err().contains("Unexpected value for 'dest_hostname'"));
    }

    #[test]
    fn test_parse_spec_file_invalid_field_name() {
        let mut s = NamedTempFile::new().unwrap();
        write!(s, "this-isnt-valid: 0").unwrap();
        assert!(parse_spec_file(s.path()).unwrap_err().contains("Unexpected key in root dictionary"));
    }

    #[test]
    fn test_parse_spec_file_invalid_syncs_field() {
        let mut s = NamedTempFile::new().unwrap();
        write!(s, "syncs: 0").unwrap();
        assert!(parse_spec_file(s.path()).unwrap_err().contains("Unexpected value for 'syncs'"));
    }

    #[test]
    fn test_parse_spec_file_invalid_sync_spec_type() {
        let mut s = NamedTempFile::new().unwrap();
        write!(s, r#"
            syncs:
            - not-a-dict
        "#).unwrap();
        assert!(parse_spec_file(s.path()).unwrap_err().contains("Sync value must be a dictionary"));
    }

    #[test]
    fn test_parse_spec_file_invalid_sync_spec_field() {
        let mut s = NamedTempFile::new().unwrap();
        write!(s, r#"
            syncs:
            - unexpected-field: 0
        "#).unwrap();
        assert!(parse_spec_file(s.path()).unwrap_err().contains("Unexpected key in 'syncs' entry"));
    }

    #[test]
    fn test_parse_spec_file_invalid_filters_type() {
        let mut s = NamedTempFile::new().unwrap();
        write!(s, r#"
            syncs:
            - filters: 0
        "#).unwrap();
        assert!(parse_spec_file(s.path()).unwrap_err().contains("Unexpected value for 'filters'"));
    }

    #[test]
    fn test_parse_spec_file_invalid_filters_element() {
        let mut s = NamedTempFile::new().unwrap();
        write!(s, r#"
            syncs:
            - filters: [ 9 ]
        "#).unwrap();
        assert!(parse_spec_file(s.path()).unwrap_err().contains("Unexpected value in 'filters' array"));
    }

    /// Checks that an invalid enum value for dest_file_newer_behaviour is rejected.
    /// We don't bother to test all the different behaviours in the same way, just this one.
    #[test]
    fn test_parse_spec_file_invalid_behaviour_value() {
        let mut s = NamedTempFile::new().unwrap();
        write!(s, r#"
            syncs:
            - dest_file_newer_behaviour: notallowed
        "#).unwrap();
        assert!(parse_spec_file(s.path()).unwrap_err().contains("invalid variant: notallowed"));
    }

    /// Tests that command-line args can be used to override things set in the spec file.
    #[test]
    fn resolve_spec_overrides() {
        let mut spec_file = NamedTempFile::new().unwrap();
        write!(spec_file, r#"
            deploy_behaviour: error
            syncs:
            - src: a
              dest: b
              filters: [ +hello ]
              dest_file_newer_behaviour: skip
              dest_root_needs_deleting_behaviour: error
            - src: c
              dest: d

        "#).unwrap();

        let args = BossCliArgs::try_parse_from(&["rjrssync",
            "--spec", spec_file.path().to_str().unwrap(),
            "--filter", "-meow",
            "--dest-file-newer=error",
            "--deploy=ok",
        ]).unwrap();
        let spec = resolve_spec(&args).unwrap();
        assert_eq!(spec, Spec {
            deploy_behaviour: DeployBehaviour::Ok, // Overriden by command-line args
            syncs: vec![
                SyncSpec {
                    src: "a".to_string(),
                    dest: "b".to_string(),
                    filters: vec!["-meow".into()], // Overriden by command-line args
                    dest_file_newer_behaviour: DestFileUpdateBehaviour::Error,  // Overriden by command-line args
                    dest_root_needs_deleting_behaviour: DestRootNeedsDeletingBehaviour::Error, // From the spec file, not overriden by command-line args
                    ..Default::default()
                },
                SyncSpec {
                    src: "c".to_string(),
                    dest: "d".to_string(),
                    filters: vec!["-meow".into()], // Set by command-line args
                    dest_file_newer_behaviour: DestFileUpdateBehaviour::Error,  // Set by command-line args
                    ..Default::default()
                }
            ],
            ..Default::default()
        });
    }

    /// Tests that --all-destructive-behaviour overrides things set in the spec file,
    /// but can itself be overridden by individual behaviours set on the command-line.
    #[test]
    fn all_destructive_behaviour_override() {
        let mut spec_file = NamedTempFile::new().unwrap();
        write!(spec_file, r#"
            syncs:
            - src: a
              dest: b
              dest_file_newer_behaviour: skip
              dest_root_needs_deleting_behaviour: prompt
              files_same_time_behaviour: overwrite
            - src: c
              dest: d
        "#).unwrap();

        let args = BossCliArgs::try_parse_from(&["rjrssync",
            "--spec", spec_file.path().to_str().unwrap(),
            "--all-destructive-behaviour=error",
            "--dest-file-older=overwrite"
        ]).unwrap();
        let spec = resolve_spec(&args).unwrap();
        assert_eq!(spec, Spec {
            syncs: vec![
                SyncSpec {
                    src: "a".to_string(),
                    dest: "b".to_string(),
                    // Specified as skip in the spec file, so --all-destructive-behaviour does not change it as it's not overwriting
                    dest_file_newer_behaviour: DestFileUpdateBehaviour::Skip,
                    // Specified on the command-line, which always takes priority
                    dest_file_older_behaviour: DestFileUpdateBehaviour::Overwrite,
                    // Specified as overwrite in the spec file, but --all-destructive-behaviour overrides this to Error
                    files_same_time_behaviour: DestFileUpdateBehaviour::Error,
                    // Not specified anywhere, and the default behaviour is to delete, so --all-destructive-behaviour overrides this to Error
                    dest_entry_needs_deleting_behaviour: DestEntryNeedsDeletingBehaviour::Error,
                    // Specified as prompt in the spec file, so --all-destructive-behaviour overrides this to Error
                    dest_root_needs_deleting_behaviour: DestRootNeedsDeletingBehaviour::Error,
                    ..Default::default()
                },
                SyncSpec {
                    src: "c".to_string(),
                    dest: "d".to_string(),
                    // Default is prompt, so --all-destructive-behaviour overrides this to Error
                    dest_file_newer_behaviour: DestFileUpdateBehaviour::Error,
                    // Specified on the command-line, which always takes priority
                    dest_file_older_behaviour: DestFileUpdateBehaviour::Overwrite,
                    // Default is skip, so --all-destructive-behaviour doesn't affect this
                    files_same_time_behaviour: DestFileUpdateBehaviour::Skip,
                    // Not specified anywhere, and the default behaviour is to delete, so --all-destructive-behaviour overrides this to Error
                    dest_entry_needs_deleting_behaviour: DestEntryNeedsDeletingBehaviour::Error,
                    // Default is to prompt, so --all-destructive-behaviour overrides this to Error
                    dest_root_needs_deleting_behaviour: DestRootNeedsDeletingBehaviour::Error,
                    ..Default::default()
                }
            ],
            ..Default::default()
        });
    }
}
