use crate::zip::{self, Options};
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Cursor, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

struct CliArgs {
    zip_path: PathBuf,
    new_file: Option<PathBuf>,
    options: Options,
}

enum Command {
    Help,
    Process(CliArgs),
}

pub fn run_from_env() -> Result<(), String> {
    let args: Vec<String> = std::env::args().collect();
    run(&args).map_err(|e| e.to_string())
}

fn run(args: &[String]) -> Result<(), zip::Error> {
    match parse_command(args)? {
        Command::Help => {
            print_help();
            Ok(())
        }
        Command::Process(cli) => execute(cli),
    }
}

fn parse_command(args: &[String]) -> Result<Command, zip::Error> {
    if args.len() < 2 {
        return Ok(Command::Help);
    }

    let mut dry_run = false;
    let mut fast = false;
    let mut not_utf8 = false;
    let mut keep_backslashes = false;
    let mut no_default_exclude = false;
    let mut extra_excludes: Vec<String> = Vec::new();
    let mut new_file: Option<PathBuf> = None;
    let mut zip_path: Option<PathBuf> = None;

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
            "--keep-backslashes" => {
                keep_backslashes = true;
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
                new_file = Some(PathBuf::from(read_option_value(args, &mut i, "--new")?));
                i += 1;
            }
            flag if flag.starts_with('-') => {
                return Err(zip::Error::from(format!("unknown option: '{}'", flag)));
            }
            path => {
                if zip_path.is_some() {
                    return Err(zip::Error::from("multiple ZIP file paths given"));
                }
                zip_path = Some(PathBuf::from(path));
                i += 1;
            }
        }
    }

    let zip_path = zip_path.ok_or_else(|| zip::Error::from("no ZIP file specified"))?;
    if fast && new_file.is_some() {
        return Err(zip::Error::from("--fast cannot be used with --new"));
    }

    Ok(Command::Process(CliArgs {
        zip_path,
        new_file,
        options: Options {
            dry_run,
            fast,
            not_utf8,
            keep_backslashes,
            no_default_exclude,
            extra_excludes,
        },
    }))
}

fn execute(cli: CliArgs) -> Result<(), zip::Error> {
    let mut stdout = std::io::stdout();
    match cli.new_file.as_deref() {
        Some(out_path) => {
            process_to_new_file(&cli.zip_path, out_path, &cli.options, &mut stdout)?;
            if !cli.options.dry_run {
                eprintln!("Written to '{}'", out_path.display());
            }
        }
        None => {
            zip::process_file(&cli.zip_path, &cli.options, &mut stdout)?;
            if !cli.options.dry_run {
                eprintln!("'{}' updated in place", cli.zip_path.display());
            }
        }
    }

    Ok(())
}

fn process_to_new_file(
    zip_path: &Path,
    out_path: &Path,
    opts: &Options,
    stdout: &mut impl std::io::Write,
) -> Result<(), zip::Error> {
    let mut input = File::open(zip_path)
        .map_err(|e| zip::Error::io_context(format!("cannot open '{}'", zip_path.display()), e))?;
    let file_len = input.seek(SeekFrom::End(0))?;

    if opts.dry_run {
        let mut output = Cursor::new(Vec::new());
        return zip::process_new(&mut input, file_len, &mut output, opts, stdout);
    }

    let output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(out_path)
        .map_err(|e| {
            zip::Error::io_context(format!("cannot create '{}'", out_path.display()), e)
        })?;
    let mut output = BufWriter::new(output);
    zip::process_new(&mut input, file_len, &mut output, opts, stdout)?;
    output
        .flush()
        .map_err(|e| zip::Error::io_context(format!("write error for '{}'", out_path.display()), e))
}

fn read_option_value<'a>(
    args: &'a [String],
    index: &mut usize,
    flag: &str,
) -> Result<&'a str, zip::Error> {
    *index += 1;
    args.get(*index)
        .map(|value| value.as_str())
        .ok_or_else(|| zip::Error::from(format!("{flag} requires an argument")))
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
    println!("  --not-utf-8           Skip UTF-8 filename fixes");
    println!("  --keep-backslashes    Do not normalize backslashes in entry paths");
    println!("  --no-default-exclude  Do not exclude .DS_Store, __MACOSX, Thumbs.db, desktop.ini");
    println!("  --exclude <name>      Exclude entries whose basename matches <name> (repeatable)");
    println!("  -h, --help            Show this help");
    println!();
    println!("DEFAULT BEHAVIOUR:");
    println!("  • Set bit 11 (UTF-8 flag) on non-ASCII filenames");
    println!("  • Normalize filenames to NFC (reduces byte count for NFD-encoded names)");
    println!("  • Normalize backslashes in entry paths to slashes");
    println!("  • Leave other ASCII-only filenames unchanged");
    println!("  • Remove .DS_Store, __MACOSX, Thumbs.db, and desktop.ini entries from the Central Directory");
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
        assert!(err.to_string().contains("cannot create"));
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
                assert_eq!(cli.zip_path, Path::new("input.zip"));
                assert_eq!(cli.new_file.as_deref(), Some(Path::new("clean.zip")));
                assert!(cli.options.dry_run);
                assert!(!cli.options.fast);
                assert!(!cli.options.keep_backslashes);
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
    fn parse_command_accepts_keep_backslashes() {
        let args = vec![
            "zipkirei".to_string(),
            "--keep-backslashes".to_string(),
            "input.zip".to_string(),
        ];

        let command = parse_command(&args).unwrap();
        match command {
            Command::Process(cli) => assert!(cli.options.keep_backslashes),
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
        assert_eq!(err.to_string(), "--fast cannot be used with --new");
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
        assert!(err.to_string().contains("unknown option"));
        assert!(err.to_string().contains("--wat"));
    }

    #[test]
    fn parse_command_requires_option_values() {
        let exclude_args = vec!["zipkirei".to_string(), "--exclude".to_string()];
        let exclude_err = match parse_command(&exclude_args) {
            Ok(_) => panic!("expected exclude argument error"),
            Err(err) => err,
        };
        assert_eq!(exclude_err.to_string(), "--exclude requires an argument");

        let new_args = vec!["zipkirei".to_string(), "--new".to_string()];
        let new_err = match parse_command(&new_args) {
            Ok(_) => panic!("expected new argument error"),
            Err(err) => err,
        };
        assert_eq!(new_err.to_string(), "--new requires an argument");
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
        assert_eq!(err.to_string(), "multiple ZIP file paths given");
    }

    #[test]
    fn run_fails_on_non_existent_file() {
        let path = unique_temp_path("not-there.zip");
        let args = vec!["zipkirei".to_string(), path.display().to_string()];

        let err = run(&args).unwrap_err();
        assert!(err.to_string().contains("cannot open"));
        assert!(err.to_string().contains("not-there.zip"));
    }
}
