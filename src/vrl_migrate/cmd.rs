use std::fs;
use std::path::PathBuf;

use clap::Parser;

use super::{diff, migrate};

#[derive(Parser, Debug, Clone)]
#[command(rename_all = "kebab-case")]
pub struct Opts {
    /// VRL file(s) to migrate.
    #[arg(required_unless_present = "config")]
    pub files: Vec<PathBuf>,

    /// Rewrite file(s) in-place.
    #[arg(long)]
    pub in_place: bool,

    /// Show a unified diff without modifying files.
    #[arg(long)]
    pub diff: bool,

    /// Migrate all inline VRL in a Vector config file (TOML/YAML/JSON).
    #[arg(long)]
    pub config: Option<PathBuf>,
}

pub fn cmd(opts: &Opts) -> exitcode::ExitCode {
    if let Some(config_path) = &opts.config {
        return migrate_config(config_path, opts.diff, opts.in_place);
    }

    let mut any_changed = false;

    for path in &opts.files {
        let source = match fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("Error reading {}: {e}", path.display());
                return exitcode::IOERR;
            }
        };

        if opts.diff {
            let d = diff(&source, Some(path));
            if !d.is_empty() {
                print!("{d}");
                any_changed = true;
            }
        } else if opts.in_place {
            let output = migrate(&source);
            if output.text != source {
                if let Err(e) = fs::write(path, &output.text) {
                    eprintln!("Error writing {}: {e}", path.display());
                    return exitcode::IOERR;
                }
                any_changed = true;
                eprintln!(
                    "{}: {} rules applied, {} items need review",
                    path.display(),
                    output.applied.len(),
                    output.reviews.len()
                );
            } else {
                eprintln!("{}: no changes", path.display());
            }
        } else {
            let output = migrate(&source);
            print!("{output}");
            if output.text != source {
                any_changed = true;
            }
        }
    }

    if opts.diff && !any_changed {
        eprintln!("No changes needed.");
    }

    exitcode::OK
}

fn migrate_config(path: &PathBuf, show_diff: bool, in_place: bool) -> exitcode::ExitCode {
    let source = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Error reading {}: {e}", path.display());
            return exitcode::IOERR;
        }
    };

    let mut output = source.clone();
    let mut total_applied = 0;
    let mut total_reviews = 0;

    // Extract inline VRL from `source` fields in TOML/YAML config.
    // Strategy: find `source = """..."""` or `source: |` blocks and migrate them.
    // For a first version, we use a regex to find triple-quoted TOML strings
    // and YAML literal blocks that contain VRL.
    let re_toml_vrl = regex::Regex::new(
        r#"(?m)(source\s*=\s*"""\n)([\s\S]*?)(""")"#
    ).unwrap();

    let result = re_toml_vrl.replace_all(&output, |caps: &regex::Captures| {
        let prefix = &caps[1];
        let vrl_body = &caps[2];
        let suffix = &caps[3];

        let migrated = super::migrate(vrl_body);
        total_applied += migrated.applied.len();
        total_reviews += migrated.reviews.len();

        format!("{prefix}{}{suffix}", migrated.text)
    });
    output = result.into_owned();

    // Also handle single-line source = "..." patterns
    let re_toml_inline = regex::Regex::new(
        r#"(?m)(source\s*=\s*")((?:[^"\\]|\\.)*)(")"#
    ).unwrap();

    let result = re_toml_inline.replace_all(&output, |caps: &regex::Captures| {
        let prefix = &caps[1];
        let vrl_body = &caps[2];
        let suffix = &caps[3];

        let migrated = super::migrate(vrl_body);
        total_applied += migrated.applied.len();
        total_reviews += migrated.reviews.len();

        format!("{prefix}{}{suffix}", migrated.text)
    });
    output = result.into_owned();

    if show_diff {
        let d = super::unified_diff(&source, &output, path.to_str().unwrap_or("config"));
        if d.is_empty() {
            eprintln!("No changes needed.");
        } else {
            print!("{d}");
        }
    } else if in_place {
        if output != source {
            if let Err(e) = fs::write(path, &output) {
                eprintln!("Error writing {}: {e}", path.display());
                return exitcode::IOERR;
            }
            eprintln!(
                "{}: {total_applied} rules applied, {total_reviews} items need review",
                path.display()
            );
        } else {
            eprintln!("{}: no changes", path.display());
        }
    } else {
        print!("{output}");
    }

    exitcode::OK
}
