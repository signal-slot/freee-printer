//! `~/.config/freee/credentials`: the freee app, its tokens and the company,
//! in `KEY=value` lines so that shell scripts on the same machine (such as a
//! file manager action that uploads to the file box) can `source` it. Every
//! value this program keeps goes into the same file.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::ThreadId;

use anyhow::{Context, Result};
use freee_printer_core::net::Store;

pub struct CredentialsFile {
    path: PathBuf,
    held: Arc<Held>,
}

/// Who holds the file lock. The lock is re-entrant within a thread, so that
/// a token refresh can write while it keeps other processes out.
#[derive(Default)]
struct Held {
    state: Mutex<Option<(ThreadId, usize, File)>>,
    released: Condvar,
}

/// Releases the file lock when the outermost guard drops.
struct Guard(Arc<Held>);

impl Drop for Guard {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().unwrap();
        if let Some((_, depth, _)) = state.as_mut() {
            *depth -= 1;
            if *depth == 0 {
                // Dropping the file releases the OS lock.
                *state = None;
                self.0.released.notify_all();
            }
        }
    }
}

impl CredentialsFile {
    pub fn open(path: &Path) -> Result<Self> {
        let dir = path.parent().context("invalid credentials path")?;
        fs::create_dir_all(dir).with_context(|| format!("{} を作成できません", dir.display()))?;
        if path.exists() {
            // Fail early on a file we could not parse rather than at the first write.
            read_lines(path).with_context(|| format!("{} を読めません", path.display()))?;
        }
        Ok(CredentialsFile {
            path: path.to_path_buf(),
            held: Arc::default(),
        })
    }

    fn lines(&self) -> Vec<String> {
        read_lines(&self.path).unwrap_or_default()
    }

    fn take_lock(&self) -> io::Result<Guard> {
        let me = std::thread::current().id();
        let mut state = self.held.state.lock().unwrap();
        loop {
            match state.as_mut() {
                None => {
                    let file = File::create(self.path.with_extension("lock"))?;
                    file.lock()?;
                    *state = Some((me, 1, file));
                    return Ok(Guard(self.held.clone()));
                }
                Some((holder, depth, _)) if *holder == me => {
                    *depth += 1;
                    return Ok(Guard(self.held.clone()));
                }
                Some(_) => state = self.held.released.wait(state).unwrap(),
            }
        }
    }

    /// Rewrites the file atomically, keeping lines that are not ours (comments
    /// and variables of other tools) in place. Only the owner may read it.
    fn write(&self, lines: &[String]) -> io::Result<()> {
        let tmp = self.path.with_extension("tmp");
        let mut options = fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp)?;
        for line in lines {
            writeln!(file, "{line}")?;
        }
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, &self.path)
    }

    fn update(&self, key: &str, value: Option<&str>) -> io::Result<()> {
        let _lock = self.take_lock()?;
        let mut lines = self.lines();
        let prefix = format!("{key}=");
        let new_line = value.map(|value| format!("{key}={}", quote(value)));
        // Replace the line in place so the file keeps its order.
        match lines.iter().position(|line| line.starts_with(&prefix)) {
            Some(index) => {
                lines.retain(|line| !line.starts_with(&prefix));
                if let Some(new_line) = new_line {
                    lines.insert(index, new_line);
                }
            }
            None => lines.extend(new_line),
        }
        self.write(&lines)
    }
}

fn read_lines(path: &Path) -> io::Result<Vec<String>> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(text.lines().map(str::to_string).collect()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

/// Values the shell would read back unchanged are written bare; anything
/// else is single-quoted.
fn quote(value: &str) -> String {
    let plain = !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-./:@+=,".contains(&b));
    if plain {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

fn unquote(raw: &str) -> String {
    let raw = raw.trim();
    if raw.len() >= 2 && raw.starts_with('\'') && raw.ends_with('\'') {
        raw[1..raw.len() - 1].replace("'\\''", "'")
    } else if raw.len() >= 2 && raw.starts_with('"') && raw.ends_with('"') {
        raw[1..raw.len() - 1].to_string()
    } else {
        raw.to_string()
    }
}

impl Store for CredentialsFile {
    fn get(&self, key: &str) -> Option<String> {
        // Read every time: another process may have refreshed the tokens.
        let prefix = format!("{key}=");
        self.lines()
            .iter()
            .rev()
            .find_map(|line| line.strip_prefix(&prefix).map(unquote))
    }

    fn set(&self, key: &str, value: &str) -> io::Result<()> {
        self.update(key, Some(value))
    }

    fn remove(&self, key: &str) -> io::Result<()> {
        self.update(key, None)
    }

    fn lock(&self) -> io::Result<Box<dyn std::any::Any + Send>> {
        Ok(Box::new(self.take_lock()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_foreign_lines_and_quotes_values() {
        let dir = std::env::temp_dir().join(format!("freee-printer-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("credentials");
        fs::write(&path, "# mine\nCLIENT_ID=abc\nOTHER=\"x y\"\n").unwrap();
        let store = CredentialsFile::open(&path).unwrap();
        assert_eq!(store.get("CLIENT_ID").as_deref(), Some("abc"));
        assert_eq!(store.get("OTHER").as_deref(), Some("x y"));
        store.set("pr_name", "freee ファイル's").unwrap();
        store.set("CLIENT_ID", "def").unwrap();
        store.remove("OTHER").unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(
            text,
            "# mine\nCLIENT_ID=def\npr_name='freee ファイル'\\''s'\n"
        );
        assert_eq!(store.get("pr_name").as_deref(), Some("freee ファイル's"));
        assert_eq!(store.get("OTHER"), None);
        // A refresh writes while holding the lock; that must not deadlock.
        let guard = store.lock().unwrap();
        store.set("ACCESS_TOKEN", "t").unwrap();
        drop(guard);
        assert_eq!(store.get("ACCESS_TOKEN").as_deref(), Some("t"));
        fs::remove_dir_all(&dir).unwrap();
    }
}
