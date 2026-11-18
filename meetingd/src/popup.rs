//! Open a Terminal window with a meeting's notes as soon as its summary is ready.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::config::Config;

/// Write a one-time `.command` script and let Terminal run it. The script deletes itself first.
pub fn show_meeting(config: &Config, meeting_id: &str) -> Result<()> {
    let exe = std::env::current_exe().context("finding own binary")?;
    let dir = config.db_path.with_file_name("popups");
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;

    let script = dir.join(format!("{meeting_id}.command"));
    let body = format!(
        "#!/bin/zsh\nrm -f -- \"$0\"\nexec {} --config {} popup {}\n",
        quote_path(&exe),
        quote_path(&config.config_path),
        quote(meeting_id),
    );
    fs::write(&script, body).with_context(|| format!("writing {}", script.display()))?;
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755))?;

    let status = Command::new("/usr/bin/open").args(["-a", "Terminal"]).arg(&script).status()?;
    if !status.success() {
        bail!("open -a Terminal failed ({status})");
    }

    Ok(())
}

fn quote_path(path: &Path) -> String {
    quote(&path.to_string_lossy())
}

/// Wrap text in single quotes for the shell, so spaces like "Application Support" are safe.
fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::quote;

    #[test]
    fn quotes_spaces_and_single_quotes() {
        assert_eq!(quote("Application Support"), "'Application Support'");
        assert_eq!(quote("it's"), r"'it'\''s'");
    }
}
