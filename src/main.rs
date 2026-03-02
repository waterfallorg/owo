use anyhow::{Context, Result};
use clap::{Arg, ArgAction, Command};
use ignore::{WalkBuilder, overrides::OverrideBuilder};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::fs;
use tokio::sync::Semaphore;

#[derive(Debug)]
struct FileEntry {
    path: PathBuf,
    content: String,
    size: u64,
    line_count: usize,
}

#[derive(Default)]
struct Stats {
    file_count: usize,
    total_lines: usize,
    total_bytes: u64,
    skipped_size: usize,
    skipped_binary: usize,
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut cmd = Command::new("owo")
        .version("0.2.0")
        .author("Waterfall.Org")
        .about("Like tree but outputs file contents to a single markdown file")
        .override_help(
            r#"USAGE:
    owo [OPTIONS] [PATH]

EXAMPLES:
    owo -o context.md
    owo -I "node_modules|dist" -n "rs|toml" -o context.md
    owo -I "obj|bin" -w -s 100 --stats -o context.md
    owo --help

FLAGS:
    -w, --with-dotfiles    Include hidden files and directories
        --stats            Print summary stats after completion
    -h, --help             Print help information
    -V, --version          Print version information

OPTIONS:
    -I, --ignore <PATTERNS>    Ignore files/dirs matching these patterns (pipe-separated)
                               [default: obj|bin|build|dist|.git|.env|.env.*]
    -n, --include <EXT>        Only include files with these extensions (pipe-separated)
                               e.g. "rs|toml|md"  [default: all files]
    -s, --max-size <KB>        Skip files larger than this size in kilobytes [default: 512]
    -o, --output <FILE>        Output file (required)

ARGS:
    <PATH>                     Directory to traverse [default: current directory]"#,
        )
        .arg(
            Arg::new("ignore")
                .short('I')
                .long("ignore")
                .value_name("PATTERNS")
                .help("Ignore files/directories matching these patterns (pipe-separated)")
                .default_value("obj|bin|build|dist|.git|.env|.env.*"),
        )
        .arg(
            Arg::new("include")
                .short('n')
                .long("include")
                .value_name("EXT")
                .help("Only include files with these extensions (pipe-separated), e.g. rs|toml|md"),
        )
        .arg(
            Arg::new("max_size")
                .short('s')
                .long("max-size")
                .value_name("KB")
                .help("Skip files larger than this size in kilobytes")
                .default_value("512"),
        )
        .arg(
            Arg::new("output")
                .short('o')
                .long("output")
                .value_name("FILE")
                .help("Output file")
                .required(true),
        )
        .arg(
            Arg::new("with_dotfiles")
                .short('w')
                .long("with-dotfiles")
                .action(ArgAction::SetTrue)
                .help("Include dotfiles (hidden files and directories)"),
        )
        .arg(
            Arg::new("stats")
                .long("stats")
                .action(ArgAction::SetTrue)
                .help("Print a summary of files processed after completion"),
        )
        .arg(
            Arg::new("directory")
                .help("Directory to traverse [default: current directory]")
                .default_value("."),
        )
        .disable_help_flag(true)
        .disable_version_flag(true);

    let args: Vec<String> = std::env::args().collect();
    if args.len() == 1 || args.contains(&"-h".to_string()) || args.contains(&"--help".to_string()) {
        println!("{}", cmd.render_help());
        return Ok(());
    }

    let matches = cmd.clone().try_get_matches().map_err(|e| {
        if e.kind() == clap::error::ErrorKind::DisplayHelp {
            println!("{}", cmd.render_help());
            std::process::exit(0);
        }
        e
    })?;

    let ignore_patterns = matches.get_one::<String>("ignore").unwrap();
    let output_file = matches.get_one::<String>("output").unwrap();
    let with_dotfiles = matches.get_flag("with_dotfiles");
    let show_stats = matches.get_flag("stats");
    let directory = matches.get_one::<String>("directory").unwrap();

    let max_size_kb: u64 = matches
        .get_one::<String>("max_size")
        .unwrap()
        .parse()
        .context("--max-size must be a positive integer (kilobytes)")?;
    let max_size_bytes = max_size_kb * 1024;

    // Parse optional include-extensions into a set
    let include_exts: Option<Vec<String>> = matches
        .get_one::<String>("include")
        .map(|s| s.split('|').map(|e| e.trim().to_lowercase()).collect());

    let mut override_builder = OverrideBuilder::new(directory);
    for pattern in ignore_patterns.split('|') {
        let pattern = pattern.trim();
        if !pattern.is_empty() {
            override_builder.add(&format!("!{}", pattern))?;
        }
    }
    let overrides = override_builder.build()?;

    let start = Instant::now();

    // Collect all matching file paths first (deterministic order via BTreeMap later)
    let walker = WalkBuilder::new(directory)
        .hidden(false)
        .git_ignore(true)
        .require_git(false)
        .overrides(overrides)
        .sort_by_file_name(|a, b| a.cmp(b))
        .build()
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let path = entry.path();

            if path.is_dir() {
                return Some(entry);
            }

            // Dotfile filter
            if !with_dotfiles {
                for component in path.components() {
                    if let std::path::Component::Normal(name) = component {
                        let name_str = name.to_string_lossy();
                        if name_str.starts_with('.') && name_str != ".git" {
                            return None;
                        }
                    }
                }
            }

            // Extension filter
            if let Some(ref exts) = include_exts {
                let file_ext = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(|e| e.to_lowercase())
                    .unwrap_or_default();
                if !exts.iter().any(|e| e == &file_ext) {
                    return None;
                }
            }

            Some(entry)
        })
        .collect::<Vec<_>>();

    // Build tree structure from all entries (dirs + files)
    let tree_str = build_tree(&walker, directory);

    // Filter to files only and process
    let file_paths: Vec<PathBuf> = walker
        .iter()
        .filter(|e| e.path().is_file())
        .map(|e| e.path().to_path_buf())
        .collect();

    let total_files = file_paths.len();
    eprintln!("📂 Found {} files to process...", total_files);

    let results: Arc<Mutex<BTreeMap<PathBuf, FileEntry>>> = Arc::new(Mutex::new(BTreeMap::new()));
    let stats = Arc::new(Mutex::new(Stats::default()));
    let semaphore = Arc::new(Semaphore::new(num_cpus::get() * 2));
    let mut handles = Vec::new();

    for path in file_paths {
        let results = Arc::clone(&results);
        let stats = Arc::clone(&stats);
        let semaphore = Arc::clone(&semaphore);

        let permit = semaphore.clone().acquire_owned().await?;
        handles.push(tokio::spawn(async move {
            let _permit = permit;

            // Check file size before reading
            let file_size = match fs::metadata(&path).await {
                Ok(m) => m.len(),
                Err(_) => return,
            };

            if file_size > max_size_bytes {
                let mut s = stats.lock().unwrap();
                s.skipped_size += 1;
                eprintln!(
                    "  ⏭  Skipping {} ({}KB > {}KB limit)",
                    path.display(),
                    file_size / 1024,
                    max_size_kb
                );
                return;
            }

            match read_file_with_fallback(&path).await {
                Ok(content) => {
                    let line_count = content.lines().count();
                    let trimmed = content.trim_end().to_string();
                    let mut s = stats.lock().unwrap();
                    s.file_count += 1;
                    s.total_lines += line_count;
                    s.total_bytes += file_size;
                    drop(s);

                    results.lock().unwrap().insert(
                        path.clone(),
                        FileEntry {
                            path,
                            content: trimmed,
                            size: file_size,
                            line_count,
                        },
                    );
                }
                Err(_) => {
                    let mut s = stats.lock().unwrap();
                    s.skipped_binary += 1;
                }
            }
        }));
    }

    for handle in handles {
        let _ = handle.await;
    }

    let elapsed = start.elapsed();
    let final_results = results.lock().unwrap();
    let final_stats = stats.lock().unwrap();

    // Build output markdown
    let mut output = String::new();

    // Header block
    output.push_str(&format!(
        "# 🦉 owo — codebase snapshot\n\n\
         > Generated by [owo](https://github.com/waterfallorg/owo)  \n\
         > Path: `{}`  \n\
         > Files: **{}** | Lines: **{}** | Size: **{}**  \n\n\
         ---\n\n",
        std::fs::canonicalize(directory)
            .unwrap_or_else(|_| PathBuf::from(directory))
            .display(),
        final_stats.file_count,
        final_stats.total_lines,
        human_size(final_stats.total_bytes),
    ));

    // Tree section
    output.push_str("## 📁 Directory Structure\n\n```\n");
    output.push_str(&tree_str);
    output.push_str("```\n\n---\n\n");

    // File contents
    output.push_str("## 📄 File Contents\n\n");
    for entry in final_results.values() {
        output.push_str(&format!("\n### `{}`\n", entry.path.display()));
        output.push_str(&format!(
            "_{}  ·  {} lines_\n\n",
            human_size(entry.size),
            entry.line_count
        ));
        output.push_str("```");
        if let Some(ext) = entry.path.extension().and_then(|e| e.to_str()) {
            output.push_str(ext);
        }
        output.push('\n');
        output.push_str(&entry.content);
        output.push_str("\n```\n");
    }

    drop(final_results);

    fs::write(output_file, output)
        .await
        .context("Failed to write output file")?;

    eprintln!(
        "✅ Done in {:.2}s — wrote {} to {}",
        elapsed.as_secs_f64(),
        human_size(std::fs::metadata(output_file).map(|m| m.len()).unwrap_or(0)),
        output_file
    );

    if show_stats {
        println!("\n┌─ Stats ──────────────────────────────");
        println!("│  Files processed : {}", final_stats.file_count);
        println!("│  Total lines     : {}", final_stats.total_lines);
        println!(
            "│  Total size      : {}",
            human_size(final_stats.total_bytes)
        );
        println!("│  Skipped (size)  : {}", final_stats.skipped_size);
        println!("│  Skipped (binary): {}", final_stats.skipped_binary);
        println!("│  Elapsed         : {:.2}s", elapsed.as_secs_f64());
        println!("└──────────────────────────────────────");
    } else {
        println!(
            "Successfully wrote {} files ({} lines) to {}",
            final_stats.file_count, final_stats.total_lines, output_file
        );
    }

    Ok(())
}

/// Build a pretty tree string from walker entries
fn build_tree(entries: &[ignore::DirEntry], root: &str) -> String {
    // Group children by parent path
    let mut tree: BTreeMap<PathBuf, Vec<(PathBuf, bool)>> = BTreeMap::new();
    let root_path = PathBuf::from(root);

    for entry in entries {
        let path = entry.path().to_path_buf();
        if path == root_path {
            continue;
        }
        let parent = path.parent().unwrap_or(&root_path).to_path_buf();
        let is_dir = path.is_dir();
        tree.entry(parent).or_default().push((path, is_dir));
    }

    let mut out = String::new();
    out.push_str(&format!(
        "{}/\n",
        root_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(root)
    ));
    render_tree(&root_path, &tree, "", &mut out);
    out
}

fn render_tree(
    dir: &Path,
    tree: &BTreeMap<PathBuf, Vec<(PathBuf, bool)>>,
    prefix: &str,
    out: &mut String,
) {
    let children = match tree.get(dir) {
        Some(c) => c,
        None => return,
    };

    for (i, (path, is_dir)) in children.iter().enumerate() {
        let is_last = i == children.len() - 1;
        let connector = if is_last { "└── " } else { "├── " };
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("?");

        if *is_dir {
            out.push_str(&format!("{}{}{}/\n", prefix, connector, name));
            let new_prefix = format!("{}{}", prefix, if is_last { "    " } else { "│   " });
            render_tree(path, tree, &new_prefix, out);
        } else {
            out.push_str(&format!("{}{}{}\n", prefix, connector, name));
        }
    }
}

async fn read_file_with_fallback(path: &Path) -> Result<String> {
    match fs::read_to_string(path).await {
        Ok(content) => Ok(content),
        Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
            Err(anyhow::anyhow!("binary file"))
        }
        Err(e) => Err(e.into()),
    }
}

fn human_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}
