//! Conversion pipelines: wraps chdman / extract-xiso / zarchive and turns their
//! console output into job progress.

use std::{
    collections::{HashMap, VecDeque},
    io,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
};

use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
    sync::mpsc,
};

use crate::{
    detect,
    jobs::{AppState, Job, Platform, Stage},
};

/// Lines of tool output kept for error messages.
const TAIL_LINES: usize = 6;

pub enum ConvertError {
    Cancelled,
    Failed(String),
}

impl From<io::Error> for ConvertError {
    fn from(e: io::Error) -> Self {
        Self::Failed(e.to_string())
    }
}

type Result<T> = std::result::Result<T, ConvertError>;

fn failed<T>(msg: impl Into<String>) -> Result<T> {
    Err(ConvertError::Failed(msg.into()))
}

/// Waits for a free slot, converts, and records the outcome on the job.
pub async fn run(state: AppState, job: Arc<Job>, requested: Option<Platform>) {
    job.update(|v| v.stage = Stage::Queued);
    let permit = tokio::select! {
        permit = state.sem.clone().acquire_owned() => permit.expect("semaphore is never closed"),
        _ = job.cancel.cancelled() => return finish_cancelled(&job).await,
    };

    let result = convert(&job, requested).await;
    drop(permit);
    let _ = tokio::fs::remove_file(job.input_path()).await;

    match result {
        Ok(size) => {
            tracing::info!(id = %job.id, name = %job.name, size, "done");
            job.update(|v| {
                v.stage = Stage::Done;
                v.percent = 100.0;
                v.message = None;
                v.output_size = Some(size);
            });
            job.touch();
        }
        Err(ConvertError::Cancelled) => finish_cancelled(&job).await,
        Err(ConvertError::Failed(msg)) => {
            tracing::warn!(id = %job.id, name = %job.name, "failed: {msg}");
            let _ = tokio::fs::remove_dir_all(&job.dir).await;
            job.update(|v| {
                v.stage = Stage::Failed;
                v.message = Some(msg);
            });
            job.touch();
        }
    }
}

pub async fn finish_cancelled(job: &Job) {
    tracing::info!(id = %job.id, name = %job.name, "cancelled");
    let _ = tokio::fs::remove_dir_all(&job.dir).await;
    job.update(|v| v.stage = Stage::Cancelled);
}

/// Returns the size of the output file.
async fn convert(job: &Job, requested: Option<Platform>) -> Result<u64> {
    let input = job.input_path();
    job.update(|v| v.stage = Stage::Detecting);
    let path = input.clone();
    let detected = tokio::task::spawn_blocking(move || detect::detect_path(&path))
        .await
        .map_err(|e| ConvertError::Failed(e.to_string()))??;
    let platform = resolve_platform(requested, detected).map_err(ConvertError::Failed)?;
    job.update(|v| v.platform = Some(platform));

    let output = job.output_path(platform);
    match platform {
        Platform::Ps2 => ps2_to_chd(job, &input, &output).await?,
        Platform::X360 => x360_to_zar(job, &input, &output).await?,
    }
    Ok(tokio::fs::metadata(&output).await?.len())
}

/// A platform picked in the UI must agree with what the ISO actually is.
fn resolve_platform(requested: Option<Platform>, detected: Option<Platform>) -> std::result::Result<Platform, String> {
    let label = |p: Platform| match p {
        Platform::Ps2 => "PS2",
        Platform::X360 => "Xbox 360",
    };
    match (requested, detected) {
        (_, None) => Err(match requested {
            Some(p) => format!("This doesn't look like a {} ISO.", label(p)),
            None => "Couldn't recognise this as a PS2 or Xbox 360 ISO.".into(),
        }),
        (Some(want), Some(got)) if want != got => Err(format!(
            "You picked {}, but this ISO is for {}. Switch to Auto-detect or {} and drop it again.",
            label(want),
            label(got),
            label(got),
        )),
        (_, Some(got)) => Ok(got),
    }
}

async fn ps2_to_chd(job: &Job, input: &Path, output: &Path) -> Result<()> {
    job.update(|v| {
        v.stage = Stage::Compressing;
        v.percent = 0.0;
    });
    let mut cmd = Command::new("chdman");
    cmd.arg("createdvd").arg("-i").arg(input).arg("-o").arg(output).arg("-f");
    run_tool(job, cmd, |line| {
        if let Some((percent, ratio)) = parse_chdman_progress(line) {
            job.update(|v| {
                v.percent = percent;
                v.message = ratio.map(|r| format!("ratio {r}"));
            });
        }
    })
    .await
}

/// ISO → extracted folder (first half of the progress bar) → .zar (second half).
async fn x360_to_zar(job: &Job, input: &Path, output: &Path) -> Result<()> {
    let game_dir = job.dir.join("game");
    job.update(|v| {
        v.stage = Stage::Extracting;
        v.percent = 0.0;
        v.message = Some("reading ISO".into());
    });

    let mut total = None;
    let mut cmd = Command::new("extract-xiso");
    cmd.arg("-l").arg(input);
    let listed = run_tool(job, cmd, |line| {
        if let Some(t) = parse_xiso_total(line) {
            total = Some(t);
        }
    })
    .await;
    let total = match (listed, total) {
        (Err(ConvertError::Cancelled), _) => return Err(ConvertError::Cancelled),
        (Ok(()), Some(t)) if t > 0 => t,
        _ => return failed("This isn't a readable Xbox 360 ISO."),
    };

    let prefix = format!("{}/", game_dir.display());
    let mut done = 0u64;
    let mut current: Option<(String, u64)> = None;
    let mut cmd = Command::new("extract-xiso");
    // -s: skip $SystemUpdate, emulators don't need it
    cmd.arg("-s").arg("-x").arg(input).arg("-d").arg(&game_dir);
    run_tool(job, cmd, |line| {
        let Some(p) = parse_xiso_extract(line) else { return };
        if current.as_ref().is_some_and(|(path, _)| path != p.path) {
            done += current.take().map_or(0, |(_, size)| size);
        }
        let in_file = p.size * u64::from(p.percent) / 100;
        let frac = (done + in_file) as f32 / total as f32;
        job.update(|v| {
            v.percent = 50.0 * frac.min(1.0);
            v.message = Some(p.path.strip_prefix(&prefix).unwrap_or(p.path).to_owned());
        });
        current = Some((p.path.to_owned(), p.size));
    })
    .await?;

    let root = root_file_names(&game_dir).await?;
    if !root.iter().any(|n| n.eq_ignore_ascii_case("default.xex")) {
        if root.iter().any(|n| n.eq_ignore_ascii_case("default.xbe")) {
            return failed("This is an original Xbox game (default.xbe); Xenia only runs Xbox 360 games.");
        }
        return failed("No default.xex after extracting, this doesn't look like an Xbox 360 game.");
    }

    job.update(|v| {
        v.stage = Stage::Compressing;
        v.percent = 50.0;
        v.message = None;
    });
    let sizes = {
        let dir = game_dir.clone();
        tokio::task::spawn_blocking(move || file_sizes(&dir))
            .await
            .map_err(|e| ConvertError::Failed(e.to_string()))??
    };
    let total = sizes.values().sum::<u64>().max(1);
    let mut done = 0u64;
    let mut previous = None;
    let mut cmd = Command::new("zarchive");
    cmd.arg(&game_dir).arg(output);
    // zarchive prints "Adding <path>" as it starts each file
    run_tool(job, cmd, |line| {
        let Some(rel) = line.strip_prefix("Adding ") else { return };
        done += previous.take().unwrap_or(0);
        previous = sizes.get(rel).copied();
        let frac = done as f32 / total as f32;
        job.update(|v| {
            v.percent = 50.0 + 50.0 * frac.min(1.0);
            v.message = Some(rel.to_owned());
        });
    })
    .await?;

    tokio::fs::remove_dir_all(&game_dir).await?;
    if !output.exists() {
        return failed("zarchive finished without writing an archive");
    }
    Ok(())
}

async fn root_file_names(dir: &Path) -> io::Result<Vec<String>> {
    let mut names = Vec::new();
    let mut entries = tokio::fs::read_dir(dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        names.push(entry.file_name().to_string_lossy().into_owned());
    }
    Ok(names)
}

/// Sizes of every file under `root`, keyed by `/`-separated relative path.
fn file_sizes(root: &Path) -> io::Result<HashMap<String, u64>> {
    let mut sizes = HashMap::new();
    let mut stack: Vec<PathBuf> = vec![root.to_owned()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let meta = entry.metadata()?;
            if meta.is_dir() {
                stack.push(entry.path());
            } else if let Ok(rel) = entry.path().strip_prefix(root) {
                sizes.insert(rel.to_string_lossy().into_owned(), meta.len());
            }
        }
    }
    Ok(sizes)
}

/// Runs a tool to completion, feeding each line of stdout/stderr (split on `\r`
/// too, since progress output rewrites the same line) to `on_line`.
async fn run_tool(job: &Job, mut cmd: Command, mut on_line: impl FnMut(&str)) -> Result<()> {
    let program = cmd.as_std().get_program().to_string_lossy().into_owned();
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd
        .spawn()
        .map_err(|e| ConvertError::Failed(format!("couldn't start {program}: {e}")))?;

    let (tx, mut rx) = mpsc::unbounded_channel();
    tokio::spawn(read_lines(child.stdout.take().unwrap(), tx.clone()));
    tokio::spawn(read_lines(child.stderr.take().unwrap(), tx));

    let mut tail = VecDeque::with_capacity(TAIL_LINES);
    loop {
        tokio::select! {
            line = rx.recv() => match line {
                Some(line) => {
                    on_line(&line);
                    if tail.len() == TAIL_LINES {
                        tail.pop_front();
                    }
                    tail.push_back(line);
                }
                None => break,
            },
            _ = job.cancel.cancelled() => {
                let _ = child.kill().await;
                return Err(ConvertError::Cancelled);
            }
        }
    }
    let status = tokio::select! {
        status = child.wait() => status?,
        _ = job.cancel.cancelled() => {
            let _ = child.kill().await;
            return Err(ConvertError::Cancelled);
        }
    };
    if status.success() {
        Ok(())
    } else {
        let tail: Vec<_> = tail.into_iter().collect();
        failed(format!("{program} failed ({status}): {}", tail.join(" | ")))
    }
}

async fn read_lines(mut r: impl AsyncRead + Unpin, tx: mpsc::UnboundedSender<String>) {
    let mut buf = [0u8; 8192];
    let mut line = Vec::new();
    loop {
        let n = match r.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        for &b in &buf[..n] {
            if b == b'\n' || b == b'\r' {
                emit_line(&mut line, &tx);
            } else {
                line.push(b);
            }
        }
    }
    emit_line(&mut line, &tx);
}

fn emit_line(line: &mut Vec<u8>, tx: &mpsc::UnboundedSender<String>) {
    let s = String::from_utf8_lossy(line).trim().to_owned();
    line.clear();
    if !s.is_empty() {
        let _ = tx.send(s);
    }
}

/// `Compressing, 42.1% complete... (ratio=99.0%)` → (42.1, Some("99.0%"))
fn parse_chdman_progress(line: &str) -> Option<(f32, Option<&str>)> {
    let rest = line.strip_prefix("Compressing, ")?;
    let (percent, rest) = rest.split_once('%')?;
    let ratio = rest
        .split_once("ratio=")
        .and_then(|(_, r)| r.split_once(')'))
        .map(|(r, _)| r);
    Some((percent.trim().parse().ok()?, ratio))
}

/// `2 files in game.iso total 50000003 bytes` → 50000003
fn parse_xiso_total(line: &str) -> Option<u64> {
    let (_, rest) = line.rsplit_once(" total ")?;
    rest.strip_suffix(" bytes")?.trim().parse().ok()
}

struct XisoProgress<'a> {
    path: &'a str,
    size: u64,
    percent: u8,
}

/// `extracting out/default.xex (50000000 bytes) [4%]`
fn parse_xiso_extract(line: &str) -> Option<XisoProgress<'_>> {
    let rest = line.strip_prefix("extracting ")?;
    let (path, rest) = rest.rsplit_once(" (")?;
    let (size, rest) = rest.split_once(" bytes)")?;
    let percent = match rest.trim().strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
        Some(p) => p.strip_suffix('%').and_then(|p| p.parse().ok()).unwrap_or(100),
        None => 0,
    };
    Some(XisoProgress {
        path,
        size: size.parse().ok()?,
        percent,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picked_platform_must_match() {
        use Platform::*;
        assert_eq!(resolve_platform(None, Some(Ps2)), Ok(Ps2));
        assert_eq!(resolve_platform(Some(X360), Some(X360)), Ok(X360));
        assert!(resolve_platform(Some(Ps2), Some(X360)).unwrap_err().contains("this ISO is for Xbox 360"));
        assert!(resolve_platform(Some(X360), Some(Ps2)).is_err());
        assert!(resolve_platform(Some(Ps2), None).is_err());
        assert!(resolve_platform(None, None).is_err());
    }

    #[test]
    fn chdman_progress() {
        assert_eq!(
            parse_chdman_progress("Compressing, 42.1% complete... (ratio=99.0%)"),
            Some((42.1, Some("99.0%")))
        );
        assert_eq!(parse_chdman_progress("Logical size: 209,715,200"), None);
    }

    #[test]
    fn xiso_total() {
        assert_eq!(
            parse_xiso_total("2 files in x.iso total 50000003 bytes"),
            Some(50000003)
        );
        assert_eq!(parse_xiso_total("/sub/a.txt (3 bytes)"), None);
    }

    #[test]
    fn xiso_extract() {
        let p = parse_xiso_extract("extracting xout/my (game)/default.xex (50000000 bytes) [4%]").unwrap();
        assert_eq!((p.path, p.size, p.percent), ("xout/my (game)/default.xex", 50000000, 4));
        let p = parse_xiso_extract("extracting xout/a.txt (3 bytes) [OK]").unwrap();
        assert_eq!(p.percent, 100);
        assert!(parse_xiso_extract("extracting x.iso:").is_none());
    }
}
