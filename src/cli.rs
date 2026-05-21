use crate::zip::{self, Options};
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Cursor, Seek, SeekFrom, Write};

struct CliArgs {
    zip_path: String,
    new_file: Option<String>,
    options: Options,
}

enum Command {
    Help,
    Process(CliArgs),
}

pub fn run_from_env() -> Result<(), String> {
    let args: Vec<String> = std::env::args().collect();
    run(&args)
}

fn run(args: &[String]) -> Result<(), String> {
    match parse_command(args)? {
        Command::Help => {
            print_help();
            Ok(())
        }
        Command::Process(cli) => execute(cli),
    }
}

fn parse_command(args: &[String]) -> Result<Command, String> {
    if args.len() < 2 {
        return Ok(Command::Help);
    }

    let mut dry_run = false;
    let mut fast = false;
    let mut not_utf8 = false;
    let mut no_default_exclude = false;
    let mut extra_excludes: Vec<String> = Vec::new();
    let mut new_file: Option<String> = None;
    let mut zip_path: Option<String> = None;

    let mut i = 1usize;
    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" => return Ok(Command::Help),
            "--dry-run" => {
                dry_run = true;
                i += 1;
            }
            "--fast" => {
                fast = true;
                i += 1;
            }
            "--not-utf-8" => {
                not_utf8 = true;
                i += 1;
            }
            "--no-default-exclude" => {
                no_default_exclude = true;
                i += 1;
            }
            "--exclude" => {
                extra_excludes.push(read_option_value(args, &mut i, "--exclude")?.to_string());
                i += 1;
            }
            "--new" => {
                new_file = Some(read_option_value(args, &mut i, "--new")?.to_string());
                i += 1;
            }
            flag if flag.starts_with('-') => {
                return Err(format!("unknown option: '{}'", flag));
            }
            path => {
                if zip_path.is_some() {
                    return Err("multiple ZIP file paths given".into());
                }
                zip_path = Some(path.to_string());
                i += 1;
            }
        }
    }

    let zip_path = zip_path.ok_or_else(|| "no ZIP file specified".to_string())?;
    if fast && new_file.is_some() {
        return Err("--fast cannot be used with --new".into());
    }

    Ok(Command::Process(CliArgs {
        zip_path,
        new_file,
        options: Options {
            dry_run,
            fast,
            not_utf8,
            no_default_exclude,
            extra_excludes,
        },
    }))
}

fn execute(cli: CliArgs) -> Result<(), String> {
    let mut stdout = std::io::stdout();
    match cli.new_file.as_deref() {
        Some(out_path) => {
            process_to_new_file(&cli.zip_path, out_path, &cli.options, &mut stdout)?;
            if !cli.options.dry_run {
                eprintln!("Written to '{}'", out_path);
            }
        }
        None => {
            zip::process_file(&cli.zip_path, &cli.options, &mut stdout)
                .map_err(|e| e.to_string())?;
            if !cli.options.dry_run {
                eprintln!("'{}' updated in place", cli.zip_path);
            }
        }
    }

    Ok(())
}

fn process_to_new_file(
    zip_path: &str,
    out_path: &str,
    opts: &Options,
    stdout: &mut impl std::io::Write,
) -> Result<(), String> {
    let mut input =
        File::open(zip_path).map_err(|e| format!("cannot open '{}': {}", zip_path, e))?;
    let file_len = input
        .seek(SeekFrom::End(0))
        .map_err(|e| format!("seek error: {}", e))?;

    if opts.dry_run {
        let mut output = Cursor::new(Vec::new());
        return zip::process_new(&mut input, file_len, &mut output, opts, stdout)
            .map_err(|e| e.to_string());
    }

    let output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(out_path)
        .map_err(|e| format!("cannot create '{}': {}", out_path, e))?;
    let mut output = BufWriter::new(output);
    zip::process_new(&mut input, file_len, &mut output, opts, stdout).map_err(|e| e.to_string())?;
    output
        .flush()
        .map_err(|e| format!("write error for '{}': {}", out_path, e))
}

fn read_option_value<'a>(
    args: &'a [String],
    index: &mut usize,
    flag: &str,
) -> Result<&'a str, String> {
    *index += 1;
    args.get(*index)
        .map(|value| value.as_str())
        .ok_or_else(|| format!("{flag} requires an argument"))
}

fn print_help() {
    println!(
        "zipkirei v{} — Clean up ZIP archives: NFC normalization, UTF-8 flag, junk removal",
        env!("CARGO_PKG_VERSION")
    );
    println!();
    println!("USAGE:");
    println!("  zipkirei [OPTIONS] <file.zip>");
    println!();
    println!("OPTIONS:");
    println!("  --dry-run             Show changes without modifying the file");
    println!("  --fast                Fast in-place mode: rewrite only the Central Directory");
    println!("  --new <outfile>       Write output to a new file instead of in-place");
    println!("  --not-utf-8           Skip UTF-8 filename fixes; only remove excluded files");
    println!("  --no-default-exclude  Do not exclude .DS_Store, __MACOSX, Thumbs.db, desktop.ini");
    println!("  --exclude <name>      Exclude entries whose basename matches <name> (repeatable)");
    println!("  -h, --help            Show this help");
    println!();
    println!("DEFAULT BEHAVIOUR (without --not-utf-8):");
    println!("  • Set bit 11 (UTF-8 flag) on non-ASCII filenames");
    println!("  • Normalize filenames to NFC (reduces byte count for NFD-encoded names)");
    println!("  • Leave ASCII-only filenames unchanged");
    println!("  • Remove .DS_Store, __MACOSX/*, Thumbs.db, and desktop.ini entries from the Central Directory");
    println!();
    println!("In-place mode patches the file with minimal I/O and truncates at the end.");
    println!("Use --dry-run to preview all changes first.");
}

#[cfg(test)]
mod tests {
    use super::{parse_command, run, Command};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_temp_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("zipkirei-main-{nanos}-{name}"))
    }

    fn manifest_archive(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("files")
            .join(name)
    }

    #[test]
    fn dry_run_with_new_does_not_create_output_file() {
        let src = manifest_archive("test.zip");
        let dst = unique_temp_path("dry-run-out.zip");
        let args = vec![
            "zipkirei".to_string(),
            "--dry-run".to_string(),
            "--new".to_string(),
            dst.display().to_string(),
            src.display().to_string(),
        ];

        let result = run(&args);
        assert!(result.is_ok(), "dry-run should succeed: {result:?}");
        assert!(!dst.exists(), "dry-run should not create {}", dst.display());

        let _ = fs::remove_file(dst);
    }

    #[test]
    fn new_output_refuses_to_overwrite_existing_file() {
        let src = manifest_archive("test.zip");
        let dst = unique_temp_path("existing-out.zip");
        let original = b"do not overwrite";
        fs::write(&dst, original).unwrap();

        let args = vec![
            "zipkirei".to_string(),
            "--new".to_string(),
            dst.display().to_string(),
            src.display().to_string(),
        ];

        let err = run(&args).unwrap_err();
        assert!(err.contains("cannot create"));
        assert_eq!(fs::read(&dst).unwrap(), original);

        let _ = fs::remove_file(dst);
    }

    #[test]
    fn parse_command_collects_repeatable_excludes_and_new_output() {
        let args = vec![
            "zipkirei".to_string(),
            "--dry-run".to_string(),
            "--exclude".to_string(),
            ".gitkeep".to_string(),
            "--exclude".to_string(),
            "Thumbs.db".to_string(),
            "--new".to_string(),
            "clean.zip".to_string(),
            "input.zip".to_string(),
        ];

        let command = parse_command(&args).unwrap();
        match command {
            Command::Process(cli) => {
                assert_eq!(cli.zip_path, "input.zip");
                assert_eq!(cli.new_file.as_deref(), Some("clean.zip"));
                assert!(cli.options.dry_run);
                assert!(!cli.options.fast);
                assert_eq!(
                    cli.options.extra_excludes,
                    vec![".gitkeep".to_string(), "Thumbs.db".to_string()]
                );
            }
            Command::Help => panic!("expected process command"),
        }
    }

    #[test]
    fn parse_command_accepts_fast_mode() {
        let args = vec![
            "zipkirei".to_string(),
            "--fast".to_string(),
            "input.zip".to_string(),
        ];

        let command = parse_command(&args).unwrap();
        match command {
            Command::Process(cli) => assert!(cli.options.fast),
            Command::Help => panic!("expected process command"),
        }
    }

    #[test]
    fn parse_command_rejects_fast_with_new_output() {
        let args = vec![
            "zipkirei".to_string(),
            "--fast".to_string(),
            "--new".to_string(),
            "out.zip".to_string(),
            "input.zip".to_string(),
        ];

        let err = match parse_command(&args) {
            Ok(_) => panic!("expected --fast with --new error"),
            Err(err) => err,
        };
        assert_eq!(err, "--fast cannot be used with --new");
    }

    #[test]
    fn parse_command_rejects_unknown_option() {
        let args = vec![
            "zipkirei".to_string(),
            "--wat".to_string(),
            "input.zip".to_string(),
        ];

        let err = match parse_command(&args) {
            Ok(Command::Help) => panic!("expected error, got help"),
            Ok(Command::Process(_)) => panic!("expected error, got process command"),
            Err(err) => err,
        };
        assert!(err.contains("unknown option"));
        assert!(err.contains("--wat"));
    }

    #[test]
    fn parse_command_requires_option_values() {
        let exclude_args = vec!["zipkirei".to_string(), "--exclude".to_string()];
        let exclude_err = match parse_command(&exclude_args) {
            Ok(_) => panic!("expected exclude argument error"),
            Err(err) => err,
        };
        assert_eq!(exclude_err, "--exclude requires an argument");

        let new_args = vec!["zipkirei".to_string(), "--new".to_string()];
        let new_err = match parse_command(&new_args) {
            Ok(_) => panic!("expected new argument error"),
            Err(err) => err,
        };
        assert_eq!(new_err, "--new requires an argument");
    }

    #[test]
    fn parse_command_rejects_multiple_zip_paths() {
        let args = vec![
            "zipkirei".to_string(),
            "first.zip".to_string(),
            "second.zip".to_string(),
        ];

        let err = match parse_command(&args) {
            Ok(_) => panic!("expected multiple path error"),
            Err(err) => err,
        };
        assert_eq!(err, "multiple ZIP file paths given");
    }

    #[test]
    fn run_fails_on_non_existent_file() {
        let path = unique_temp_path("not-there.zip");
        let args = vec!["zipkirei".to_string(), path.display().to_string()];

        let err = run(&args).unwrap_err();
        assert!(err.contains("cannot open"));
        assert!(err.contains("not-there.zip"));
    }
}
