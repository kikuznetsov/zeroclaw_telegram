use anyhow::{Context, Result};
use regex::Regex;
use std::process::Stdio;
use tokio::process::Command;
use tokio::time::{timeout, Duration};

pub(crate) fn interactive_command_hint(command: &str) -> Option<&'static str> {
    let trimmed = command.trim();
    let first = trimmed.split_whitespace().next().unwrap_or("");

    match first {
        "top" => Some("Interactive command detected: `top`. Use `top -b -n 1 | head -n 20` instead."),
        "htop" => Some("Interactive command detected: `htop`. Use `ps aux --sort=-%cpu | head -n 15` or `top -b -n 1 | head -n 20` instead."),
        "less" => Some("Interactive command detected: `less`. Use `tail -n 50 <file>` or `head -n 50 <file>` instead."),
        "more" => Some("Interactive command detected: `more`. Use `tail -n 50 <file>` or `head -n 50 <file>` instead."),
        "nano" => Some("Interactive command detected: `nano`. Edit files over SSH or use non-interactive file commands instead."),
        "vim" => Some("Interactive command detected: `vim`. Edit files over SSH or use non-interactive file commands instead."),
        "vi" => Some("Interactive command detected: `vi`. Edit files over SSH or use non-interactive file commands instead."),
        "watch" => Some("Interactive command detected: `watch`. Run the target command directly once instead."),
        _ => {
            if trimmed.contains("tail -f") {
                Some("Interactive/follow mode detected: `tail -f`. Use `tail -n 50 <file>` instead.")
            } else {
                None
            }
        }
    }
}

pub(crate) async fn run_shell(command: &str, timeout_sec: u64) -> Result<String> {
    let mut cmd = Command::new("bash");
    cmd.arg("-lc")
        .arg(command)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("CLICOLOR", "0")
        .env("NO_COLOR", "1");

    let fut = cmd.output();

    let output = timeout(Duration::from_secs(timeout_sec), fut)
        .await
        .context("Shell command timed out")?
        .context("Failed to spawn shell")?;

    let stdout = strip_ansi(&String::from_utf8_lossy(&output.stdout))
        .trim()
        .to_string();
    let stderr = strip_ansi(&String::from_utf8_lossy(&output.stderr))
        .trim()
        .to_string();

    let mut combined = String::new();

    if !stdout.is_empty() {
        combined.push_str(&stdout);
    }

    if !stderr.is_empty() {
        if !combined.is_empty() {
            combined.push_str("\n\n");
        }
        combined.push_str("stderr:\n");
        combined.push_str(&stderr);
    }

    if !output.status.success() {
        if !combined.is_empty() {
            combined.push_str("\n\n");
        }
        combined.push_str(&format!("[exit: {}]", output.status));
    }

    if combined.trim().is_empty() {
        Ok("(no output)".to_string())
    } else {
        Ok(combined.trim().to_string())
    }
}

pub(crate) fn is_exact_command(text: &str, command: &str) -> bool {
    let text = text.trim();
    if text == format!("/{command}") {
        return true;
    }

    text.strip_prefix(&format!("/{command}@"))
        .map(|rest| !rest.chars().any(char::is_whitespace))
        .unwrap_or(false)
}

pub(crate) fn build_ls_command(path: &str) -> String {
    format!("ls -la -- {}", shell_quote(path))
}

pub(crate) fn build_cat_command(path: &str) -> String {
    format!("cat -- {}", shell_quote(path))
}

fn shell_quote(arg: &str) -> String {
    format!("'{}'", arg.replace('\'', r#"'"'"'"#))
}

fn strip_ansi(s: &str) -> String {
    let re = Regex::new(r"\x1B\[[0-9;]*[A-Za-z]").unwrap();
    re.replace_all(s, "").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_shell_commands_with_safe_quoting() {
        assert_eq!(build_ls_command("some dir"), "ls -la -- 'some dir'");
        assert_eq!(build_cat_command("a'b.txt"), "cat -- 'a'\"'\"'b.txt'");
    }
}
