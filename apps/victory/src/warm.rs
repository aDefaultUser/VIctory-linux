//! `victory warm`: fills the caches a run reads, before the run.
//!
//! 1. Translations (DXIL): each container is translated into the shader
//!    cache under the key a run looks it up by. The key holds the host
//!    facts the device hands the translator (widest wave, view heap limit),
//!    so the warm-up makes the host device as a run does; without one it
//!    falls back to `VICTORY_HOST_WAVE` and `VICTORY_VIEW_LIMIT`.
//! 2. With `--pipelines`, compute containers also get their host pipelines,
//!    made as a run's first dispatch makes them: into the pipeline library
//!    and the driver's own caches (vkd3d-proton's and Mesa's under Wine).
//!    Graphics pipelines need the title's graphics state and are not made
//!    here; a run starts those it drew with before at their creation.
//!
//! The containers come from the cache's `shader-sources` folder (a run with
//! `VICTORY_SHADER_SOURCES=1` keeps every container it translates there),
//! or from the files and folders given: `.cdxil` files (those and
//! `VICTORY_SHADER_SAVE`'s) are read whole as one container, other files
//! are searched for containers.
//!
//! Bounded: `--seconds` stops taking new work, and a pipeline the driver
//! does not return within `--pipeline-seconds` is left behind. Each
//! pipeline is noted before it is asked for and after it is made, per host
//! driver build, so a pipeline that hung or took the process down is not
//! asked for again by the next warm-up with the same driver.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use victory_hle::modules::d3d12x;

use crate::shader::{MAX_FILE, sources};

pub struct Options {
    /// Files and folders to take containers from; none: the kept sources.
    pub sources: Vec<PathBuf>,
    /// Only the containers with these hashes (a file naming them).
    pub only: Option<PathBuf>,
    pub jobs: Option<usize>,
    pub seconds: Option<u64>,
    pub pipelines: bool,
    pub pipeline_seconds: u64,
}

pub fn run(o: Options) -> Result<()> {
    let began = Instant::now();
    let deadline = o.seconds.map(|s| began + Duration::from_secs(s));
    let late = || deadline.is_some_and(|d| Instant::now() >= d);
    let only = match &o.only {
        Some(file) => {
            let text = std::fs::read_to_string(file)
                .with_context(|| format!("reading {}", file.display()))?;
            let wanted = parse_hashes(&text);
            anyhow::ensure!(
                !wanted.is_empty(),
                "no 16-digit shader hash in {}",
                file.display()
            );
            Some(wanted)
        }
        None => None,
    };
    let cfg = victory_core::Config::load()?;
    let shaders = cfg.cache_subdir("shaders")?;
    d3d12x::use_shader_cache(shaders.clone());
    let roots = if o.sources.is_empty() {
        let kept = d3d12x::shader_sources_dir(&shaders);
        if !kept.is_dir() {
            println!(
                "warm-up      nothing to warm: no kept shader sources in {} (a run with VICTORY_SHADER_SOURCES=1 keeps them)",
                kept.display()
            );
            return Ok(());
        }
        vec![kept]
    } else {
        o.sources
    };
    let device = d3d12x::warm_host();
    if !device {
        println!(
            "warm-up      no host device: translations are keyed by VICTORY_HOST_WAVE and VICTORY_VIEW_LIMIT as set, which a run's device may not match; no pipelines"
        );
    }

    let threads = o.jobs.unwrap_or(4).max(1);
    let done = Mutex::new(HashSet::new());
    let compute: Mutex<Vec<(String, Vec<u8>)>> = Mutex::new(Vec::new());
    let counts = [const { AtomicUsize::new(0) }; 3];
    let keep_compute = o.pipelines && device;
    let all_found = || only.as_ref().is_some_and(|w| lock(&done).len() >= w.len());
    'walk: for root in &roots {
        for entry in walkdir::WalkDir::new(root)
            .into_iter()
            .filter_map(Result::ok)
        {
            if late() || all_found() {
                break 'walk;
            }
            if !entry.file_type().is_file() || entry.metadata().map_or(0, |m| m.len()) > MAX_FILE {
                continue;
            }
            let Ok(data) = std::fs::read(entry.path()) else {
                continue;
            };
            // A kept container is the title's bytes exactly: they are the key.
            let whole = entry.path().extension().is_some_and(|e| e == "cdxil");
            let found = if whole { Vec::new() } else { sources(&data) };
            let count = if whole { 1 } else { found.len() };
            let next = AtomicUsize::new(0);
            std::thread::scope(|s| {
                for _ in 0..threads.min(count) {
                    s.spawn(|| {
                        loop {
                            let i = next.fetch_add(1, Ordering::Relaxed);
                            if i >= count || late() {
                                break;
                            }
                            let bytes = if whole {
                                std::borrow::Cow::Borrowed(&data[..])
                            } else {
                                match found[i].bytes() {
                                    Some(b) => trimmed(b),
                                    None => continue,
                                }
                            };
                            let bytes = &bytes[..];
                            let hash = blake3::hash(bytes).to_hex()[..16].to_owned();
                            if only.as_ref().is_some_and(|w| !w.contains(&hash))
                                || !lock(&done).insert(hash.clone())
                            {
                                continue;
                            }
                            let outcome = d3d12x::warm_shader(bytes);
                            let which = match outcome {
                                Ok(true) => 0,
                                Ok(false) => 1,
                                Err(_) => 2,
                            };
                            let n = counts[which].fetch_add(1, Ordering::Relaxed);
                            if n > 0 && n.is_multiple_of(500) {
                                println!(
                                    "warm-up      {} shaders so far, {:.0} s",
                                    lock(&done).len(),
                                    began.elapsed().as_secs_f64()
                                );
                            }
                            if keep_compute && outcome.is_ok() && is_compute(bytes) {
                                lock(&compute).push((hash, bytes.to_vec()));
                            }
                        }
                    });
                }
            });
        }
    }
    let [translated, there, failed] = counts.map(|c| c.into_inner());
    let asked = only
        .as_ref()
        .map_or("all".to_owned(), |w| w.len().to_string());
    println!(
        "warm-up      shaders: {asked} asked, {} found: {translated} translated now, {there} in the cache already, {failed} do not translate; {:.1} s on {threads} threads{}",
        translated + there + failed,
        began.elapsed().as_secs_f64(),
        if late() {
            " (stopped at the time limit)"
        } else {
            ""
        }
    );

    if o.pipelines && device {
        let compute = compute.into_inner().unwrap_or_else(|e| e.into_inner());
        let driver = d3d12x::host_driver_id().unwrap_or_else(|| "unknown".into());
        let notes_path = shaders
            .with_file_name("pipelines")
            .join(format!("warm-pipelines-{driver}.txt"));
        let notes = Notes::parse(&std::fs::read_to_string(&notes_path).unwrap_or_default());
        let (todo, skipped): (Vec<_>, Vec<_>) =
            compute.into_iter().partition(|(hash, _)| notes.retry(hash));
        let pipeline_threads = (threads / 2).max(1);
        let stall = Duration::from_secs(o.pipeline_seconds.max(1));
        let started = Instant::now();
        let p = make_pipelines(todo, &notes_path, pipeline_threads, stall, deadline);
        let saved = save_with_limit(Duration::from_secs(60));
        println!(
            "warm-up      compute pipelines (driver {driver}): {} made or loaded, {} refused, {} not returned within {} s, {} left out (refused or not back in an earlier warm-up), {} not started (time limit); {:.1} s on {pipeline_threads} threads; library {}",
            p.made,
            p.refused,
            p.stalled,
            stall.as_secs(),
            skipped.len(),
            p.not_started,
            started.elapsed().as_secs_f64(),
            if saved {
                "saved"
            } else {
                "not saved within 60 s"
            }
        );
        if p.stalled > 0 || !skipped.is_empty() {
            println!(
                "warm-up      notes on those pipelines: {}",
                notes_path.display()
            );
        }
    }
    let _ = std::io::stdout().flush();
    Ok(())
}

/// A container found in a file, cut to the size its header gives (a raw
/// one runs on to the end of the file): no more than the title passes, as
/// the cache's key is the bytes.
fn trimmed(bytes: std::borrow::Cow<'_, [u8]>) -> std::borrow::Cow<'_, [u8]> {
    let size = bytes
        .get(0x18..0x1c)
        .and_then(|b| b.try_into().ok())
        .map(u32::from_le_bytes);
    match (size, bytes) {
        (Some(n), std::borrow::Cow::Borrowed(b)) => {
            std::borrow::Cow::Borrowed(&b[..(n as usize).min(b.len())])
        }
        (Some(n), std::borrow::Cow::Owned(mut b)) => {
            b.truncate(n as usize);
            std::borrow::Cow::Owned(b)
        }
        (None, bytes) => bytes,
    }
}

/// Whether `container` holds a compute program.
fn is_compute(container: &[u8]) -> bool {
    use victory_shader::{Container, FourCC, HwStage, native_programs};
    Container::parse(container)
        .ok()
        .and_then(|c| {
            c.part(FourCC::DXIL)
                .and_then(|p| native_programs(p.data).ok())
        })
        .is_some_and(|programs| programs.iter().any(|p| p.stage == HwStage::Cs))
}

/// The 16-hex-digit shader hashes anywhere in `text` (a run's log, a list),
/// lower case.
pub(crate) fn parse_hashes(text: &str) -> HashSet<String> {
    text.lines()
        .flat_map(|line| line.split(|c: char| !c.is_ascii_hexdigit()))
        .filter(|word| word.len() == 16)
        .map(str::to_ascii_lowercase)
        .collect()
}

/// What earlier warm-ups noted of each compute pipeline, with one host
/// driver build: one line per event, `<event> <hash>`, the last one wins.
#[derive(Debug, Default, PartialEq)]
struct Notes(HashMap<String, Note>);

#[derive(Debug, Clone, Copy, PartialEq)]
enum Note {
    /// Asked for; nothing came back: not within the limit, or the process
    /// ended first.
    Begun,
    Made,
    Refused,
}

impl Note {
    fn word(self) -> &'static str {
        match self {
            Note::Begun => "begun",
            Note::Made => "made",
            Note::Refused => "refused",
        }
    }
}

impl Notes {
    fn parse(text: &str) -> Notes {
        let mut notes = HashMap::new();
        for line in text.lines() {
            let Some((word, hash)) = line.trim().split_once(' ') else {
                continue;
            };
            let note = match word {
                "begun" => Note::Begun,
                "made" => Note::Made,
                "refused" => Note::Refused,
                _ => continue,
            };
            notes.insert(hash.trim().to_owned(), note);
        }
        Notes(notes)
    }

    /// Whether the pipeline is asked for again: unless an earlier warm-up
    /// with this driver saw it refused or never come back.
    fn retry(&self, hash: &str) -> bool {
        matches!(self.0.get(hash), None | Some(Note::Made))
    }
}

/// Appends one note as one write, so a process that ends next keeps it.
fn note(file: &Mutex<Option<std::fs::File>>, n: Note, hash: &str) {
    if let Some(f) = lock(file).as_mut() {
        let _ = f.write_all(format!("{} {hash}\n", n.word()).as_bytes());
    }
}

#[derive(Debug, Default, PartialEq)]
struct PipelineCounts {
    made: usize,
    refused: usize,
    stalled: usize,
    not_started: usize,
}

/// Makes the compute pipelines of `todo` on `threads` workers, as
/// [`d3d12x::warm_compute`] does, noting each in the file at `notes`. A
/// pipeline not back within `stall` is noted and left to its worker, and
/// another worker takes its place; nothing new starts after `deadline`.
fn make_pipelines(
    todo: Vec<(String, Vec<u8>)>,
    notes: &Path,
    threads: usize,
    stall: Duration,
    deadline: Option<Instant>,
) -> PipelineCounts {
    if let Some(dir) = notes.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(notes);
    if let Err(e) = &file {
        eprintln!("warm-up      notes not kept in {}: {e}", notes.display());
    }
    let file = Arc::new(Mutex::new(file.ok()));
    run_workers(todo, threads, stall, deadline, move |hash, bytes| {
        note(&file, Note::Begun, hash);
        let made = d3d12x::warm_compute(bytes);
        note(
            &file,
            if made == Some(true) {
                Note::Made
            } else {
                Note::Refused
            },
            hash,
        );
        made == Some(true)
    })
}

/// The worker pool of [`make_pipelines`], with the work as `make`.
fn run_workers(
    todo: Vec<(String, Vec<u8>)>,
    threads: usize,
    stall: Duration,
    deadline: Option<Instant>,
    make: impl Fn(&str, &[u8]) -> bool + Send + Sync + 'static,
) -> PipelineCounts {
    enum Event {
        Began(usize),
        Ended(usize, bool),
    }
    let total = todo.len();
    let todo = Arc::new(todo);
    let next = Arc::new(AtomicUsize::new(0));
    let make = Arc::new(make);
    let (tx, rx) = mpsc::channel();
    let spawn = || {
        let (todo, next, make, tx) = (todo.clone(), next.clone(), make.clone(), tx.clone());
        // Driver compiles of real compute programs overflowed 2 MiB stacks
        // under Wine; as the run's pipeline workers (`pipeline/make.rs`).
        std::thread::Builder::new()
            .stack_size(8 << 20)
            .spawn(move || {
                loop {
                    if deadline.is_some_and(|d| Instant::now() >= d) {
                        return;
                    }
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some((hash, bytes)) = todo.get(i) else {
                        return;
                    };
                    if tx.send(Event::Began(i)).is_err() {
                        return;
                    }
                    let made = make(hash, bytes);
                    if tx.send(Event::Ended(i, made)).is_err() {
                        return;
                    }
                }
            })
    };
    let mut workers = 0;
    for _ in 0..threads.min(total) {
        workers += usize::from(spawn().is_ok());
    }
    let mut counts = PipelineCounts::default();
    let mut running: HashMap<usize, Instant> = HashMap::new();
    let mut ended = 0;
    while workers > 0 && ended < total {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(Event::Began(i)) => {
                running.insert(i, Instant::now());
            }
            Ok(Event::Ended(i, made)) => {
                // One that stalled and came back late was counted already.
                if running.remove(&i).is_some() {
                    ended += 1;
                    if made {
                        counts.made += 1;
                    } else {
                        counts.refused += 1;
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        let now = Instant::now();
        let stalled: Vec<usize> = running
            .iter()
            .filter(|(_, t)| now - **t >= stall)
            .map(|(i, _)| *i)
            .collect();
        for i in stalled {
            running.remove(&i);
            ended += 1;
            counts.stalled += 1;
            println!(
                "warm-up      compute pipeline {} not back within {} s; left to its worker",
                todo[i].0,
                stall.as_secs()
            );
            // That worker may never return: another takes its place.
            if spawn().is_err() {
                workers -= 1;
            }
        }
        // Past the deadline, the workers stop taking work; wait only for
        // what is running.
        if deadline.is_some_and(|d| now >= d) && running.is_empty() {
            break;
        }
    }
    counts.not_started = total - ended - running.len();
    counts.stalled += running.len();
    counts
}

/// Saves the pipeline library on another thread; whether it was done
/// within `limit` (a driver thread that never came back may hold a lock
/// the save needs: the library's file on disk is then left as it was).
fn save_with_limit(limit: Duration) -> bool {
    let (tx, rx) = mpsc::channel();
    let spawned = std::thread::Builder::new().spawn(move || {
        d3d12x::save_pipeline_cache();
        let _ = tx.send(());
    });
    spawned.is_ok() && rx.recv_timeout(limit).is_ok()
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_are_found_anywhere_in_a_line() {
        let text = "INFO shader translated stage=\"compute\" hash=00167D58CC5A78D7 user_data=0\n\
                    002f8eb4802deb36.compute.cdxil\n\
                    too short 0123456789abcde, too long 0123456789abcdef0\n";
        let hashes = parse_hashes(text);
        assert_eq!(
            hashes,
            HashSet::from(["00167d58cc5a78d7".to_owned(), "002f8eb4802deb36".to_owned()])
        );
    }

    #[test]
    fn found_containers_are_cut_to_their_size() {
        let mut file = b"DXBC".to_vec();
        file.extend([0; 20]);
        file.extend(40u32.to_le_bytes());
        file.resize(40, 7);
        file.extend(b"the rest of the file");
        let cut = trimmed(std::borrow::Cow::Borrowed(&file[..]));
        assert_eq!(cut.len(), 40);
        assert_eq!(trimmed(std::borrow::Cow::Owned(file.clone())).len(), 40);
        // A size past the end, or no header, leaves the bytes as they are.
        assert_eq!(trimmed(std::borrow::Cow::Borrowed(&file[..30])).len(), 30);
        assert_eq!(trimmed(std::borrow::Cow::Borrowed(&b"DXBC"[..])).len(), 4);
    }

    #[test]
    fn notes_keep_back_what_did_not_finish() {
        let notes = Notes::parse(
            "begun aaaaaaaaaaaaaaaa\n\
             begun bbbbbbbbbbbbbbbb\nmade bbbbbbbbbbbbbbbb\n\
             begun cccccccccccccccc\nrefused cccccccccccccccc\n\
             begun eeeeeeeeeeeeeeee\nmade eeeeeeeeeeeeeeee\n\
             stalled dddddddddddddddd\ngarbage line\n\nbegun\n",
        );
        assert!(
            !notes.retry("aaaaaaaaaaaaaaaa"),
            "never came back: the process ended"
        );
        assert!(notes.retry("bbbbbbbbbbbbbbbb"));
        assert!(!notes.retry("cccccccccccccccc"));
        assert!(
            notes.retry("dddddddddddddddd"),
            "an unknown word is no note"
        );
        assert!(
            notes.retry("eeeeeeeeeeeeeeee"),
            "came back late: the last note wins"
        );
        assert!(notes.retry("ffffffffffffffff"), "never tried");
        assert_eq!(notes.0.len(), 4);
    }

    #[test]
    fn notes_written_are_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.txt");
        let file = Mutex::new(Some(
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .unwrap(),
        ));
        note(&file, Note::Begun, "aaaaaaaaaaaaaaaa");
        note(&file, Note::Begun, "bbbbbbbbbbbbbbbb");
        note(&file, Note::Made, "bbbbbbbbbbbbbbbb");
        let notes = Notes::parse(&std::fs::read_to_string(&path).unwrap());
        assert_eq!(notes.0.get("aaaaaaaaaaaaaaaa"), Some(&Note::Begun));
        assert_eq!(notes.0.get("bbbbbbbbbbbbbbbb"), Some(&Note::Made));
    }

    fn jobs(n: usize) -> Vec<(String, Vec<u8>)> {
        (0..n)
            .map(|i| (format!("{i:016x}"), vec![i as u8]))
            .collect()
    }

    #[test]
    fn workers_make_everything_and_count_refusals() {
        let counts = run_workers(jobs(20), 3, Duration::from_secs(30), None, |_, bytes| {
            bytes[0] % 4 != 0
        });
        assert_eq!(
            counts,
            PipelineCounts {
                made: 15,
                refused: 5,
                stalled: 0,
                not_started: 0
            }
        );
    }

    #[test]
    fn a_pipeline_that_does_not_come_back_is_left_behind() {
        // Job 1 never returns: its worker is replaced and the rest are made.
        let counts = run_workers(jobs(6), 2, Duration::from_millis(300), None, |_, bytes| {
            if bytes[0] == 1 {
                std::thread::sleep(Duration::from_secs(3600));
            }
            true
        });
        assert_eq!(
            counts,
            PipelineCounts {
                made: 5,
                refused: 0,
                stalled: 1,
                not_started: 0
            }
        );
    }

    #[test]
    fn nothing_starts_after_the_deadline() {
        let deadline = Instant::now() + Duration::from_millis(300);
        let began = Instant::now();
        let counts = run_workers(
            jobs(100),
            2,
            Duration::from_secs(30),
            Some(deadline),
            |_, _| {
                std::thread::sleep(Duration::from_millis(100));
                true
            },
        );
        assert!(began.elapsed() < Duration::from_secs(5));
        assert!(counts.made >= 2 && counts.made < 20, "{counts:?}");
        assert_eq!(counts.made + counts.not_started, 100, "{counts:?}");
        assert_eq!(counts.stalled, 0);
    }

    #[test]
    fn no_work_returns_at_once() {
        let counts = run_workers(Vec::new(), 4, Duration::from_secs(30), None, |_, _| true);
        assert_eq!(counts, PipelineCounts::default());
    }
}
