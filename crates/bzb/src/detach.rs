use anyhow::Result;
use bzb_core::{client, enqueue, group};

pub async fn run(cmd: Vec<String>, name: Option<String>) -> Result<()> {
    let mut client = client::connect_or_spawn().await?;
    group::ensure_busybee_group(&mut client).await?;
    let spec = enqueue::TaskSpec::from_current_env(shell_escape_join(&cmd), name)?;
    let id = enqueue::enqueue(&mut client, spec).await?;
    println!("busybee: enqueued task {id}");
    Ok(())
}

/// Join argv-style command parts into a single shell-safe string for pueue's
/// `sh -c` runner. Used by both detach and blocking modes.
pub fn shell_escape_join(parts: &[String]) -> String {
    parts
        .iter()
        .map(|p| shell_escape(p))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Best-effort POSIX shell quoting for a single argv element.
fn shell_escape(s: &str) -> String {
    if s.is_empty() {
        return "''".into();
    }
    if s.chars().all(|c| c.is_ascii_alphanumeric() || "-_./".contains(c)) {
        return s.into();
    }
    let escaped = s.replace('\'', r#"'\''"#);
    format!("'{escaped}'")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_simple_words_is_passthrough() {
        assert_eq!(
            shell_escape_join(&["echo".into(), "hi".into()]),
            "echo hi"
        );
    }

    #[test]
    fn join_quotes_args_with_spaces() {
        assert_eq!(
            shell_escape_join(&["echo".into(), "hello world".into()]),
            "echo 'hello world'"
        );
    }

    #[test]
    fn join_escapes_single_quotes() {
        assert_eq!(
            shell_escape_join(&["echo".into(), "it's".into()]),
            r#"echo 'it'\''s'"#
        );
    }
}
