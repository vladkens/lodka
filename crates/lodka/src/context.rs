// Copyright (c) vladkens | MIT License | https://github.com/vladkens/lodka
//! lodka's Docker context: the file the Docker CLI reads a context from, pointing at the VM's
//! Docker socket, so `docker context use lodka` works without DOCKER_HOST.

use std::path::{Path, PathBuf};
use std::{env, fs, io};

use crate::bad;

pub const NAME: &str = "lodka";
/// The SHA-256 of the name: the Docker CLI keeps a context in a folder named so.
const DIR: &str = "7e97b581531023244583ca8a0c7541342c2cf9d6deb417f702bdb2db9cb481b1";

/// Adds the context, or points it at `sock` again; a file already right is left alone.
pub fn add(sock: &Path) -> io::Result<()> {
  write(&docker_dir().join("contexts/meta").join(DIR), &meta(sock)?)
}

fn write(dir: &Path, meta: &str) -> io::Result<()> {
  let path = dir.join("meta.json");
  if fs::read(&path).is_ok_and(|x| x == meta.as_bytes()) {
    return Ok(());
  }

  // Through a rename, so the Docker CLI never reads half a file.
  fs::create_dir_all(dir)?;
  let tmp = dir.join(".meta.json.lodka");
  fs::write(&tmp, meta)?;
  fs::rename(&tmp, &path)
}

/// The Docker CLI's config folder: $DOCKER_CONFIG, else ~/.docker.
fn docker_dir() -> PathBuf {
  match env::var_os("DOCKER_CONFIG") {
    Some(x) if !x.is_empty() => PathBuf::from(x),
    _ => PathBuf::from(env::var_os("HOME").unwrap_or_default()).join(".docker"),
  }
}

/// The context's metadata, as `docker context create` writes it.
fn meta(sock: &Path) -> io::Result<String> {
  let sock = sock.to_str().ok_or_else(|| bad("Docker's socket: a path that isn't UTF-8"))?;
  let host = json(&format!("unix://{sock}"));
  let endpoint = format!(r#"{{"Host":{host},"SkipTLSVerify":false}}"#);
  let desc = r#"{"Description":"lodka"}"#;
  Ok(format!(r#"{{"Name":"{NAME}","Metadata":{desc},"Endpoints":{{"docker":{endpoint}}}}}"#))
}

/// A JSON string.
fn json(s: &str) -> String {
  let mut out = String::from('"');
  for c in s.chars() {
    match c {
      '"' | '\\' => out.extend(['\\', c]),
      c if c < ' ' => out += &format!("\\u{:04x}", c as u32),
      c => out.push(c),
    }
  }

  out.push('"');
  out
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn writes_the_docker_cli_metadata() {
    let meta = meta(Path::new("/Users/me/.local/state/lodka/docker.sock")).unwrap();
    assert_eq!(
      meta,
      concat!(
        r#"{"Name":"lodka","Metadata":{"Description":"lodka"},"Endpoints":{"docker":"#,
        r#"{"Host":"unix:///Users/me/.local/state/lodka/docker.sock","SkipTLSVerify":false}}}"#
      )
    );
    assert_eq!(json("a\"b\\c\nd"), r#""a\"b\\c\u000ad""#);
  }

  #[test]
  fn rewrites_only_a_changed_file() {
    let dir = env::temp_dir().join(format!("lodka-context-{}", std::process::id()));
    let path = dir.join("meta.json");
    write(&dir, "one").unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), "one");

    let before = fs::metadata(&path).unwrap().modified().unwrap();
    write(&dir, "one").unwrap();
    assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), before);
    write(&dir, "two").unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), "two");
    fs::remove_dir_all(&dir).unwrap();
  }
}
