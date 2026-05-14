use std::ffi::OsStr;
use std::path::Path;
use std::process::Command;

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub fn run_output<I, S>(argv: I, cwd: Option<&Path>) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut iterator = argv.into_iter();
    let Some(program) = iterator.next() else {
        return String::new();
    };
    let mut command = Command::new(program);
    command.args(iterator);
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    command
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).to_string())
        .unwrap_or_default()
}

pub fn compact(value: impl AsRef<str>, limit: usize) -> String {
    let text = value
        .as_ref()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if limit == 0 || UnicodeWidthStr::width(text.as_str()) <= limit {
        return text;
    }
    if limit <= 3 {
        return text.chars().take(limit).collect();
    }
    let mut output = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + width > limit - 3 {
            break;
        }
        output.push(ch);
        used += width;
    }
    format!("{}...", output.trim_end())
}

pub fn shell_join(argv: &[String]) -> String {
    argv.iter()
        .map(|part| shell_quote(part))
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_quote(value: &str) -> String {
    if value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.' | '/' | ':'))
    {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_trims_by_display_width() {
        assert_eq!("ab...", compact("abcdef", 5));
    }

    #[test]
    fn shell_join_quotes_spaces() {
        assert_eq!(
            "git commit -m 'hello world'",
            shell_join(&[
                "git".to_string(),
                "commit".to_string(),
                "-m".to_string(),
                "hello world".to_string()
            ])
        );
    }
}
