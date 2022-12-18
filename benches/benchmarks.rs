use std::{time::{Instant, Duration}, path::{Path, PathBuf}, io::Write};

use ascii_table::AsciiTable;
use clap::Parser;
use fs_extra::dir::CopyOptions;

#[path = "../tests/test_utils.rs"]
mod test_utils;

#[derive(Debug, Clone)]
enum Target {
    Local(PathBuf),
    Remote {
        is_windows: bool,
        user_and_host: String,
        folder: String,
    }
}

#[derive(clap::Parser)]
struct CliArgs {
    /// This is passed to us by "cargo bench", so we need to declare it, but we simply ignore it.
    #[arg(long)]
    bench: bool,

    /// Skips the setup of the files that will be copied in the tests (i.e. cloning stuff from GitHub)
    /// if the file already exist. This speeds up running the benchmark if the files are up to date, but
    /// if they're out of date, this might give misleading results.
    #[arg(long)]
    skip_setup: bool,
    /// Only runs tests for local filesystem destinations, skipping the remote ones.
    #[arg(long)]
    only_local: bool,
    /// Only runs tests for remote filesystem destinations, skipping the local ones.
    #[arg(long)]
    only_remote: bool,
    /// Only runs tests for the given programs (comma-separated list).
    #[arg(long, value_delimiter=',', default_value="rjrssync,rsync,scp,cp,xcopy,robocopy,apis")]
    programs: Vec<String>,
    /// Number of times to repeat each test, to get more accurate results in the presence of noise.
    #[arg(long, short, default_value_t=1)]
    num_samples: u32,
}

fn set_up_src_folders(args: &CliArgs) {
    if Path::new("src").exists() && args.skip_setup {
        println!("Skipping setup. Beware this may be stale!");
        return;
    }

    // Delete any old stuff, so we start from a clean state each time
    if Path::new("src").exists() {
        std::fs::remove_dir_all("src").expect("Failed to delete old src folder");
    }
    std::fs::create_dir_all("src").expect("Failed to create src dir");

    // Representative example of a directory structure with varied depth, varied file size etc.
    // PowerToys, specific version (so doesn't change in future runs)
    let result = std::process::Command::new("git").arg("clone")
        .arg("--depth").arg("1")
        .arg("--branch").arg("v0.64.0")
        .arg("https://github.com/microsoft/PowerToys.git")
        .arg("src/example-repo")
        .status().expect("Failed to launch git");
    assert!(result.success());

    // Copy the repo then check out a slightly different version, so that only some files have changed
    std::fs::create_dir("src/example-repo-slight-change").expect("Failed to create folder");
    fs_extra::dir::copy("src/example-repo", "src/example-repo-slight-change", &CopyOptions { content_only: true, ..Default::default() })
        .expect("Failed to copy dir");
    assert!(std::process::Command::new("git").arg("remote").arg("set-branches").arg("origin").arg("*")
        .current_dir("src/example-repo-slight-change")
        .status().expect("Failed to launch git").success());
    assert!(std::process::Command::new("git").arg("fetch").arg("--depth").arg("1").arg("origin").arg("v0.64.1")
        .current_dir("src/example-repo-slight-change")
        .status().expect("Failed to launch git").success());
    assert!(std::process::Command::new("git").arg("checkout").arg("FETCH_HEAD")
        .current_dir("src/example-repo-slight-change")
        .status().expect("Failed to launch git").success());

    // Delete the .git folders so these aren't synced too.
    std::fs::remove_dir_all("src/example-repo/.git").expect("Failed to delete .git");
    std::fs::remove_dir_all("src/example-repo-slight-change/.git").expect("Failed to delete .git");

    // Delete some particularly deeply-nested folders, which cause scp.exe on windows to crash with a
    // stack overflow.
    std::fs::remove_dir_all("src/example-repo/src/modules/previewpane/MonacoPreviewHandler/monacoSRC/min/vs").expect("Failed to delete nested folders");
    std::fs::remove_dir_all("src/example-repo/src/settings-ui/Settings.UI.UnitTests/BackwardsCompatibility/TestFiles/").expect("Failed to delete nested folders");
  
    std::fs::remove_dir_all("src/example-repo-slight-change/src/modules/previewpane/MonacoPreviewHandler/monacoSRC/min/vs").expect("Failed to delete nested folders");
    std::fs::remove_dir_all("src/example-repo-slight-change/src/settings-ui/Settings.UI.UnitTests/BackwardsCompatibility/TestFiles/").expect("Failed to delete nested folders");

    // Single large file
    std::fs::create_dir_all("src/large-file").expect("Failed to create dir");
    let mut f = std::fs::File::create("src/large-file/large.bin").expect("Failed to create file");
    for i in 0..1000_000 as i32 {
        let buf = [(i % 256) as u8; 1024];
        f.write_all(&buf).expect("Failed to write to file");
    }
}

fn main () {
    let args = CliArgs::parse();

    // Change working directory to a temporary folder which we will run all our benchmarks in
    let temp_dir = std::env::temp_dir().join("rjrssync-benchmarks");
    std::fs::create_dir_all(&temp_dir).expect("Failed to create temp dir");
    std::env::set_current_dir(&temp_dir).expect("Failed to set working directory");

    set_up_src_folders(&args);

    
    let mut results = vec![];
    
    let local_name = if cfg!(windows) {
        "Windows"
    } else {
        "Linux"
    };
    
    if !args.only_remote {
        results.push((format!("{local_name} -> {local_name}"), run_benchmarks_for_target(&args, Target::Local(temp_dir.join("dest")))));
        
        #[cfg(windows)]
        results.push((format!(r"{local_name} -> \\wsl$\..."), run_benchmarks_for_target(&args, Target::Local(PathBuf::from(r"\\wsl$\\Ubuntu\\tmp\\rjrssync-benchmark-dest\\")))));

        #[cfg(unix)]
        results.push((format!("{local_name} -> /mnt/..."), run_benchmarks_for_target(&args, Target::Local(PathBuf::from("/mnt/t/Temp/rjrssync-benchmarks/dest")))));
    }
    
    if !args.only_local {
        results.push((format!("{local_name} -> Remote Windows"), run_benchmarks_for_target(&args, 
            Target::Remote { is_windows: true, user_and_host: test_utils::REMOTE_WINDOWS_CONFIG.0.clone(), folder: test_utils::REMOTE_WINDOWS_CONFIG.1.clone() + "\\benchmark-dest" })));
        
        results.push((format!("{local_name} -> Remote Linux"), run_benchmarks_for_target(&args, 
            Target::Remote { is_windows: false, user_and_host: test_utils::REMOTE_LINUX_CONFIG.0.clone(), folder: test_utils::REMOTE_LINUX_CONFIG.1.clone() + "/benchmark-dest" })));
    }

    let mut ascii_table = AsciiTable::default();
    ascii_table.set_max_width(200);
    ascii_table.column(0).set_header("Method");
    ascii_table.column(1).set_header("Everything copied");
    ascii_table.column(2).set_header("Nothing copied");
    ascii_table.column(3).set_header("Some copied");
    ascii_table.column(4).set_header("Single large file");

    for (table_name, table_data) in results {
        println!();
        println!("{}", table_name);
        ascii_table.print(table_data);    
    }
}

fn run_benchmarks_for_target(args: &CliArgs, target: Target) -> Vec<Vec<String>> {
    println!("Target: {:?}", target);
    let mut result_table = vec![];

    if args.programs.contains(&String::from("rjrssync")) {
        let rjrssync_path = env!("CARGO_BIN_EXE_rjrssync");
        run_benchmarks_using_program(args, rjrssync_path, &["$SRC", "$DEST"], target.clone(), &mut result_table);
    }
   
    if args.programs.contains(&String::from("rsync")) && !matches!(target, Target::Remote{ is_windows, .. } if is_windows) { // rsync is Linux -> Linux only
        #[cfg(unix)]
        // Note trailing slash on the src is important for rsync!
        run_benchmarks_using_program(args, "rsync", &["--archive", "--delete", "$SRC/", "$DEST"], target.clone(), &mut result_table);
    }

    if args.programs.contains(&String::from("scp")) {
        run_benchmarks_using_program(args, "scp", &["-r", "-q", "$SRC", "$DEST"], target.clone(), &mut result_table);
    }
   
    if args.programs.contains(&String::from("cp")) && matches!(target, Target::Local(..)) { // cp is local only
        #[cfg(unix)]
        run_benchmarks_using_program(args, "cp", &["-r", "$SRC", "$DEST"], target.clone(), &mut result_table);
    }

    if args.programs.contains(&String::from("xcopy")) && matches!(target, Target::Local(..)) { // xcopy is local only
        #[cfg(windows)]
        run_benchmarks_using_program(args, "xcopy", &["/i", "/s", "/q", "/y", "$SRC", "$DEST"], target.clone(), &mut result_table);
    }
   
    if args.programs.contains(&String::from("robocopy")) && matches!(target, Target::Local(..)) { // robocopy is local only
        #[cfg(windows)]
        run_benchmarks_using_program(args, "robocopy", &["/MIR", "/nfl", "/NJH", "/NJS", "/nc", "/ns", "/np", "/ndl", "$SRC", "$DEST"], target.clone(), &mut result_table);
    }

    if args.programs.contains(&String::from("apis")) && matches!(target, Target::Local(..)) { // APIs are local only
            run_benchmarks(args, "APIs", |src, dest| {
            if !Path::new(&dest).exists() {
                std::fs::create_dir_all(&dest).expect("Failed to create dest folder");
            }
            fs_extra::dir::copy(src, dest, &CopyOptions { content_only: true, overwrite: true, ..Default::default() })
                .expect("Copy failed");
        }, target.clone(), &mut result_table);
    }

    result_table
}

fn run_benchmarks_using_program(cli_args: &CliArgs, program: &str, program_args: &[&str], target: Target, result_table: &mut Vec<Vec<String>>) {
    let id = Path::new(program).file_name().unwrap().to_string_lossy().to_string();
    let f = |src: String, dest: String| {
        let substitute = |p: &str| PathBuf::from(p.replace("$SRC", &src).replace("$DEST", &dest));
        let mut cmd = std::process::Command::new(program);
        let result = cmd
            .args(program_args.iter().map(|a| substitute(a)));
        let hide_stdout = program == "scp"; // scp spams its stdout, and we can't turn this off, so we hide it.
        let result = test_utils::run_process_with_live_output_impl(result, hide_stdout, false);
        if program == "robocopy" {
            // robocopy has different exit codes (0 isn't what we want)
            let code = result.exit_status.code().unwrap();
            // println!("code = {code}");
            assert!(code == 0 || code == 1 || code == 3);
        } else {
            assert!(result.exit_status.success());
        }
    };
    run_benchmarks(cli_args, &id, f, target, result_table);
}

fn run_benchmarks<F>(cli_args: &CliArgs, id: &str, sync_fn: F, target: Target, result_table: &mut Vec<Vec<String>>) where F : Fn(String, String) {
    println!("  Subject: {id}");
    let mut samples : Vec<Vec<Option<Duration>>> = vec![];
    for sample_idx in 0..cli_args.num_samples {
        println!("    Sample {sample_idx}");

        // Delete any old dest folder from other subjects
        let dest_prefix = match &target {
            Target::Local(d) => {
                if Path::new(&d).exists() {
                    std::fs::remove_dir_all(&d).expect("Failed to delete old dest folder");
                }
            std::fs::create_dir(&d).expect("Failed to create dest dir");
                d.to_string_lossy().to_string() + &std::path::MAIN_SEPARATOR.to_string()
            }
            Target::Remote { is_windows, user_and_host, folder } => {
                if *is_windows {
                    // Use run_process_with_live_output to avoid messing up terminal line endings
                    let _ = test_utils::run_process_with_live_output(std::process::Command::new("ssh").arg(&user_and_host).arg(format!("rmdir /Q /S {folder}")));
                    // This one can fail, if the folder doesn't exist

                    let result = test_utils::run_process_with_live_output(std::process::Command::new("ssh").arg(&user_and_host).arg(format!("mkdir {folder}")));
                    assert!(result.exit_status.success());
                } else {
                    let result = test_utils::run_process_with_live_output(std::process::Command::new("ssh").arg(&user_and_host).arg(format!("rm -rf '{folder}' && mkdir -p '{folder}'")));
                    assert!(result.exit_status.success());
                }
                let remote_sep = if *is_windows { "\\" } else { "/" };
                user_and_host.clone() + ":" + &folder + remote_sep
            }
        };

        let run = |src, dest| {
            let start = Instant::now();
            sync_fn(src, dest);
            let elapsed = start.elapsed();
            elapsed    
        };

        let mut sample = vec![];

        // Sync example-repo to an empty folder, so this means everything is copied
        println!("      {id} example-repo everything copied...");
        let elapsed = run(Path::new("src").join("example-repo").to_string_lossy().to_string(), dest_prefix.clone() + "example-repo");
        println!("      {id} example-repo everything copied: {:?}", elapsed);
        sample.push(Some(elapsed));

        // Sync again - this should be a no-op, but still needs to check that everything is up-to-date
        if id.contains("rjrssync") || id.contains("robocopy") || id.contains("rsync") {
            println!("      {id} example-repo nothing copied...");
            let elapsed = run(Path::new("src").join("example-repo").to_string_lossy().to_string(), dest_prefix.clone() + "example-repo");
            println!("      {id} example-repo nothing copied: {:?}", elapsed);
            sample.push(Some(elapsed));
        } else {
            sample.push(None); // Programs like scp will always copy everything, so there's no point running this part of the test
        }

        // Make some small changes, e.g. check out a new version
        if id.contains("rjrssync") || id.contains("robocopy") || id.contains("rsync") {
            println!("      {id} example-repo some copied...");
            let elapsed = run(Path::new("src").join("example-repo-slight-change").to_string_lossy().to_string(), dest_prefix.clone() + "example-repo");
            println!("      {id} example-repo some copied: {:?}", elapsed);
            sample.push(Some(elapsed));
        } else {
            sample.push(None); // Programs like scp will always copy everything, so there's no point running this part of the test
        }

        // Sync a single large file
        println!("    {id} example-repo single large file...");
        let elapsed = run(Path::new("src").join("large-file").to_string_lossy().to_string(), dest_prefix.clone() + "large-file");
        println!("    {id} example-repo single large file: {:?}", elapsed);
        sample.push(Some(elapsed));

        samples.push(sample);
    }

    // Make statistics and add to results table
    let mut results = vec![format!("{id} (x{})", samples.len())];
    for c in 0..samples[0].len() {
        let min = samples.iter().filter_map(|s| s[c]).min();
        let max = samples.iter().filter_map(|s| s[c]).max();
        if let (Some(min), Some(max)) = (min, max) {
            let percent = 100.0 * (max - min).as_secs_f32() / min.as_secs_f32();
            results.push(format!("{} (+{:.0}%)", format_duration(min), percent));
        } else {
            results.push(format!("Skipped")); 
        }
    }
    result_table.push(results);
}

fn format_duration(d: Duration) -> String {
    if d.as_secs_f32() < 1.0 {
        format!("{}ms", d.as_millis())
    } else {
        format!("{:.2}s", d.as_secs_f32())
    }
}