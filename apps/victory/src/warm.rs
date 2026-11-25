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
//! does not return within `--pipeline-seconds` is left behind to its
//! worker, which takes no more work; after [`MAX_LEFT_BEHIND`] of those no
//! more pipelines are started. Each pipeline is noted before it is asked
//! for and after it is made, per host driver build and the settings that
//! choose the driver's path ([`DRIVER_SETTINGS`]), and by its name in the
//! pipeline library (a hash of the translated code and root signature the
//! driver is handed). A pipeline that hung or took the process down is not
//! asked for again by the next warm-up with the same driver; a translator,
//! switch or host change that changes what the driver gets, or another
//! driver path, asks for it again.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
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
    let stop = || late() || all_found();
    let warm = |bytes: &[u8]| {
        let hash = blake3::hash(bytes).to_hex()[..16].to_owned();
        if only.as_ref().is_some_and(|w| !w.contains(&hash)) || !lock(&done).insert(hash.clone()) {
            return;
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
    };
    // Kept containers are one per file: their paths are gathered so the
    // workers share many files, each reading its own.
    let mut loose = Vec::new();
    'walk: for root in &roots {
        for entry in walkdir::WalkDir::new(root)
            .into_iter()
            .filter_map(Result::ok)
        {
            if stop() {
                break 'walk;
            }
            if !entry.file_type().is_file() || entry.metadata().map_or(0, |m| m.len()) > MAX_FILE {
                continue;
            }
            // A kept container is the title's bytes exactly: they are the key.
            if entry.path().extension().is_some_and(|e| e == "cdxil") {
                loose.push(entry.into_path());
                if loose.len() >= LOOSE_BATCH {
                    warm_files(&loose, threads, &stop, &warm);
                    loose.clear();
                }
                continue;
            }
            let Ok(data) = std::fs::read(entry.path()) else {
                continue;
            };
            let found = sources(&data);
            on_threads(&found, threads, &stop, |source| {
                if let Some(bytes) = source.bytes() {
                    warm(&trimmed(bytes));
                }
            });
        }
    }
    warm_files(&loose, threads, &stop, &warm);
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
        let build = d3d12x::host_driver_id().unwrap_or_else(|| "unknown".into());
        let driver = driver_key(&build, |name| std::env::var_os(name));
        let notes_path = shaders
            .with_file_name("pipelines")
            .join(format!("warm-pipelines-{driver}.txt"));
        // The library beside the notes is refused the same way at
        // `use_shader_cache`; the title's launch goes on either way.
        if let Err(e) = notes_allowed(&notes_path) {
            eprintln!("warm-up      compute pipelines not made: {e:#}");
            let _ = std::io::stdout().flush();
            return Ok(());
        }
        let pipeline_threads = (threads / 2).max(1);
        let stall = Duration::from_secs(o.pipeline_seconds.max(1));
        let started = Instant::now();
        let p = make_pipelines(compute, &notes_path, pipeline_threads, stall, deadline);
        let saved = save_with_limit(Duration::from_secs(60));
        println!(
            "warm-up      compute pipelines (driver {driver}): {} made or loaded, {} refused, {} not returned within {} s, {} left out (refused or not back in an earlier warm-up, same driver and code), {} not started (time limit, or {MAX_LEFT_BEHIND} left behind); {:.1} s on {pipeline_threads} threads; library {}",
            p.made,
            p.refused,
            p.stalled,
            stall.as_secs(),
            p.skipped,
            p.not_started,
            started.elapsed().as_secs_f64(),
            if saved {
                "saved"
            } else {
                "not saved within 60 s"
            }
        );
        if p.stalled > 0 || p.skipped > 0 {
            println!(
                "warm-up      notes on those pipelines: {}",
                notes_path.display()
            );
        }
    }
    let _ = std::io::stdout().flush();
    Ok(())
}

/// Kept container files given to the workers at a time: enough to keep
/// them all busy, few enough that a walk of many files holds little.
const LOOSE_BATCH: usize = 256;

/// Reads each of `paths` on up to `threads` threads and hands its bytes,
/// whole, to `warm`; a file that does not read is passed over.
fn warm_files(
    paths: &[PathBuf],
    threads: usize,
    stop: &(impl Fn() -> bool + Sync),
    warm: &(impl Fn(&[u8]) + Sync),
) {
    on_threads(paths, threads, stop, |path| {
        if let Ok(data) = std::fs::read(path) {
            warm(&data);
        }
    });
}

/// Runs `work` on each of `items`, on up to `threads` threads taking the
/// next one in turn, until all are done or `stop`.
fn on_threads<T: Sync>(
    items: &[T],
    threads: usize,
    stop: &(impl Fn() -> bool + Sync),
    work: impl Fn(&T) + Sync,
) {
    let next = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..threads.min(items.len()) {
            s.spawn(|| {
                while let Some(item) = items.get(next.fetch_add(1, Ordering::Relaxed)) {
                    if stop() {
                        break;
                    }
                    work(item);
                }
            });
        }
    });
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

/// Settings of vkd3d-proton and Mesa that choose the driver's path through
/// a pipeline: with one of them changed, a pipeline refused or not back
/// before may be made, so the notes are kept per driver build and these.
const DRIVER_SETTINGS: [&str; 7] = [
    "VKD3D_CONFIG",
    "VKD3D_DISABLE_EXTENSIONS",
    "VKD3D_FEATURE_LEVEL",
    "VKD3D_SHADER_MODEL",
    "RADV_DEBUG",
    "RADV_PERFTEST",
    "ACO_DEBUG",
];

/// The notes' driver: the host driver `build` (see
/// [`d3d12x::host_driver_id`]) and the [`DRIVER_SETTINGS`] set, 16 hex
/// digits; the build alone when none is set.
fn driver_key(build: &str, setting: impl Fn(&str) -> Option<std::ffi::OsString>) -> String {
    let mut h = blake3::Hasher::new();
    let mut any = false;
    for name in DRIVER_SETTINGS {
        if let Some(value) = setting(name) {
            any = true;
            h.update(format!("{name}={}\0", value.to_string_lossy()).as_bytes());
        }
    }
    if !any {
        return build.to_owned();
    }
    h.update(build.as_bytes());
    h.finalize().to_hex()[..16].to_owned()
}

/// What earlier warm-ups noted of each compute pipeline, with one host
/// driver build: one line per event, `<event> <name> <container hash>`, by
/// the pipeline's name in the library; the last one wins. (Lines of older
/// builds, by container hash alone, match no name: those are asked again.)
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
        let notes = text.lines().filter_map(Notes::line);
        Notes(notes.map(|(name, note)| (name.to_owned(), note)).collect())
    }

    /// The pipeline name and note of one line, if it is a note.
    fn line(line: &str) -> Option<(&str, Note)> {
        let mut words = line.split_whitespace();
        let note = match words.next()? {
            "begun" => Note::Begun,
            "made" => Note::Made,
            "refused" => Note::Refused,
            _ => return None,
        };
        Some((words.next()?, note))
    }

    /// Whether the pipeline named `name` is asked for: unless an earlier
    /// warm-up with this driver saw it refused or never come back.
    fn retry(&self, name: &str) -> bool {
        matches!(self.0.get(name), None | Some(Note::Made))
    }
}

/// Appends one note as one write, so a process that ends next keeps it.
fn note(file: &Mutex<Option<std::fs::File>>, n: Note, name: &str, hash: &str) {
    if let Some(f) = lock(file).as_mut() {
        let _ = f.write_all(format!("{} {name} {hash}\n", n.word()).as_bytes());
    }
}

/// Pipelines left behind to workers that never came back, after which
/// no more are started: each holds a thread (and maybe a core) until the
/// process ends. The three dxil-spirv #376 programs of stock vkd3d-proton
/// 2576c0b4 are left behind with one to spare.
const MAX_LEFT_BEHIND: usize = 4;

/// What became of one pipeline asked for.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Outcome {
    Made,
    /// Refused by the host, or the container gave it nothing to make.
    Refused,
    /// Not asked for: refused or never back in an earlier warm-up.
    Skipped,
}

#[derive(Debug, Default, PartialEq)]
struct PipelineCounts {
    made: usize,
    refused: usize,
    skipped: usize,
    stalled: usize,
    not_started: usize,
}

/// Whether warm-up notes may be kept at `notes`, checked before they are
/// read. The asset policy resolves links among its folders and a note
/// file's link to something that exists. A note file that links to nothing
/// is refused too, as the policy cannot resolve it. (A folder that links to
/// nothing cannot be opened through.) Links the policy cannot see, such as
/// a Unix link under Wine, are never written through: see [`open_notes`].
fn notes_allowed(notes: &Path) -> Result<()> {
    victory_core::ensure_outside_repo(notes)?;
    let dangling = std::fs::symlink_metadata(notes).is_ok_and(|m| m.file_type().is_symlink())
        && std::fs::metadata(notes).is_err();
    anyhow::ensure!(!dangling, "{} is a link to nothing", notes.display());
    Ok(())
}

/// The earlier notes at `notes`, and the file this warm-up's notes go to.
///
/// The file at `notes` is only ever read: its notes are written to a file
/// this process makes anew beside it, which then takes its place, so what
/// was at `notes` (perhaps a link the policy cannot see, such as a Unix
/// link under Wine) is replaced, never written through. The earlier notes
/// are in the new file before any pipeline is asked for, and each note is
/// one write to it after, so a process that ends at any point keeps them.
/// Notes that cannot be read are left as they are and none are kept. Of two
/// warm-ups of one driver at once, the notes of the one replaced first are
/// lost: their pipelines are asked for again.
fn open_notes(notes: &Path) -> (Notes, std::io::Result<std::fs::File>) {
    let text = match std::fs::read(notes) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return (Notes::default(), Err(e)),
    };
    let kept: String = text
        .lines()
        .filter(|line| Notes::line(line).is_some())
        .flat_map(|line| [line, "\n"])
        .collect();
    (Notes::parse(&kept), replace_notes(notes, &kept))
}

/// Writes `kept` to a new file beside `notes`, then renames it to `notes`,
/// returning it open at its end.
fn replace_notes(notes: &Path, kept: &str) -> std::io::Result<std::fs::File> {
    let (Some(dir), Some(name)) = (notes.parent(), notes.file_name()) else {
        return Err(std::io::ErrorKind::InvalidInput.into());
    };
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(
        "{}.{}.tmp",
        name.to_string_lossy(),
        std::process::id()
    ));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)?;
    match file
        .write_all(kept.as_bytes())
        .and_then(|()| std::fs::rename(&tmp, notes))
    {
        Ok(()) => Ok(file),
        Err(e) => {
            drop(file);
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Makes the compute pipelines of `todo` on `threads` workers, as
/// [`d3d12x::warm_compute`] does, noting each in the file at `notes` by
/// its name in the pipeline library; one an earlier warm-up noted as
/// refused or not back is not asked for.
fn make_pipelines(
    todo: Vec<(String, Vec<u8>)>,
    notes: &Path,
    threads: usize,
    stall: Duration,
    deadline: Option<Instant>,
) -> PipelineCounts {
    let (earlier, file) = open_notes(notes);
    if let Err(e) = &file {
        eprintln!("warm-up      notes not kept in {}: {e}", notes.display());
    }
    let earlier = Arc::new(earlier);
    let file = Arc::new(Mutex::new(file.ok()));
    let make = move |hash: &str, bytes: &[u8]| {
        // The gate runs on this thread, before the host is asked: the
        // pipeline's name, and whether it was asked for.
        let seen = std::rc::Rc::new(std::cell::RefCell::new(None::<(String, bool)>));
        let gate = {
            let (earlier, file, seen, hash) =
                (earlier.clone(), file.clone(), seen.clone(), hash.to_owned());
            Box::new(move |name: &str| {
                let ask = earlier.retry(name);
                if ask {
                    note(&file, Note::Begun, name, &hash);
                }
                *seen.borrow_mut() = Some((name.to_owned(), ask));
                ask
            })
        };
        let made = d3d12x::warm_compute(bytes, gate) == Some(true);
        let seen = seen.borrow_mut().take();
        match seen {
            Some((_, false)) => Outcome::Skipped,
            Some((name, true)) => {
                let n = if made { Note::Made } else { Note::Refused };
                note(&file, n, &name, hash);
                if made {
                    Outcome::Made
                } else {
                    Outcome::Refused
                }
            }
            // No pipeline to ask for (no translation, no root signature).
            None => Outcome::Refused,
        }
    };
    run_workers(todo, threads, stall, deadline, MAX_LEFT_BEHIND, make)
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Job {
    Waiting,
    Running(Instant),
    /// Not back within the limit: its worker takes no more work.
    LeftBehind,
    Done(Outcome),
}

struct Pool {
    jobs: Vec<Job>,
    next: usize,
    /// No more jobs are handed out.
    closed: bool,
}

/// The worker pool of [`make_pipelines`], with the work as `make`. At most
/// `threads` workers take jobs: one whose job is not back within `stall`
/// is left with it, retired, and another takes its place, until
/// `max_left_behind` are left behind; then the pool closes. Nothing new
/// starts after `deadline`.
fn run_workers(
    todo: Vec<(String, Vec<u8>)>,
    threads: usize,
    stall: Duration,
    deadline: Option<Instant>,
    max_left_behind: usize,
    make: impl Fn(&str, &[u8]) -> Outcome + Send + Sync + 'static,
) -> PipelineCounts {
    let total = todo.len();
    let todo = Arc::new(todo);
    let make = Arc::new(make);
    let shared = Arc::new((
        Mutex::new(Pool {
            jobs: vec![Job::Waiting; total],
            next: 0,
            closed: false,
        }),
        Condvar::new(),
    ));
    let past = |deadline: Option<Instant>| deadline.is_some_and(|d| Instant::now() >= d);
    let spawn = || {
        let (todo, make, shared) = (todo.clone(), make.clone(), shared.clone());
        // Driver compiles of real compute programs overflowed 2 MiB stacks
        // under Wine; as the run's pipeline workers (`pipeline/make.rs`).
        std::thread::Builder::new()
            .stack_size(8 << 20)
            .spawn(move || {
                let (pool, ended) = &*shared;
                loop {
                    let i = {
                        let mut pool = lock(pool);
                        if pool.closed || pool.next >= total || past(deadline) {
                            return;
                        }
                        let i = pool.next;
                        pool.next += 1;
                        pool.jobs[i] = Job::Running(Instant::now());
                        i
                    };
                    let (hash, bytes) = &todo[i];
                    let outcome = make(hash, bytes);
                    let mut pool = lock(pool);
                    // Left behind meanwhile: another worker has this one's
                    // place, and this one takes no more work.
                    if !matches!(pool.jobs[i], Job::Running(_)) {
                        return;
                    }
                    pool.jobs[i] = Job::Done(outcome);
                    ended.notify_one();
                }
            })
            .is_ok()
    };
    // Workers that take jobs: spawned, less those left behind.
    let mut active = (0..threads.min(total)).filter(|_| spawn()).count();
    let mut left_behind = 0;
    let (pool, ended) = &*shared;
    let mut pool = lock(pool);
    loop {
        let now = Instant::now();
        let mut running = 0;
        for i in 0..pool.next {
            let Job::Running(began) = pool.jobs[i] else {
                continue;
            };
            if now - began < stall {
                running += 1;
                continue;
            }
            pool.jobs[i] = Job::LeftBehind;
            left_behind += 1;
            active -= 1;
            println!(
                "warm-up      compute pipeline {} not back within {} s; left to its worker",
                todo[i].0,
                stall.as_secs()
            );
            if left_behind >= max_left_behind {
                if !pool.closed {
                    println!(
                        "warm-up      {left_behind} compute pipelines left behind: no more are started"
                    );
                }
                pool.closed = true;
            } else if spawn() {
                active += 1;
            }
        }
        // Past the deadline the workers take no more jobs: wait only for
        // those running.
        if running == 0 && (pool.next >= total || pool.closed || active == 0 || past(deadline)) {
            pool.closed = true;
            break;
        }
        pool = ended
            .wait_timeout(pool, Duration::from_millis(200))
            .unwrap_or_else(|e| e.into_inner())
            .0;
    }
    let mut counts = PipelineCounts::default();
    for job in &pool.jobs {
        match job {
            Job::Done(Outcome::Made) => counts.made += 1,
            Job::Done(Outcome::Refused) => counts.refused += 1,
            Job::Done(Outcome::Skipped) => counts.skipped += 1,
            Job::LeftBehind => counts.stalled += 1,
            Job::Waiting => counts.not_started += 1,
            Job::Running(_) => unreachable!("the pool ends with nothing running"),
        }
    }
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

    // Pipeline names as the library makes them (32 hex digits).
    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const C: &str = "cccccccccccccccccccccccccccccccc";
    const D: &str = "dddddddddddddddddddddddddddddddd";
    const E: &str = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
    const F: &str = "ffffffffffffffffffffffffffffffff";

    #[test]
    fn notes_keep_back_what_did_not_finish() {
        let notes = Notes::parse(&format!(
            "begun {A} 0000000000000001\n\
             begun {B} 0000000000000002\nmade {B} 0000000000000002\n\
             begun {C} 0000000000000003\nrefused {C} 0000000000000003\n\
             begun {E} 0000000000000005\nmade {E} 0000000000000005\n\
             stalled {D} 0000000000000004\ngarbage line\n\nbegun\n"
        ));
        assert!(!notes.retry(A), "never came back: the process ended");
        assert!(notes.retry(B));
        assert!(!notes.retry(C));
        assert!(notes.retry(D), "an unknown word is no note");
        assert!(notes.retry(E), "came back late: the last note wins");
        assert!(notes.retry(F), "never tried");
        assert_eq!(notes.0.len(), 4);
    }

    /// Notes are by what the driver is handed, not by the title's
    /// container: the same container translated anew (another translator,
    /// switch or host) is another pipeline, asked for again; and the notes
    /// of older builds, by container hash, hold nothing back.
    #[test]
    fn notes_are_kept_by_pipeline_not_by_container() {
        let container = "0011223344556677";
        let notes = Notes::parse(&format!(
            "begun {A} {container}\nrefused {B} {container}\n\
             begun {container}\nrefused {container}\n"
        ));
        assert!(!notes.retry(A) && !notes.retry(B));
        assert!(notes.retry(C), "same container, other translation");
        // The real names: a translation that changes changes the name.
        let name = |dxil: &[u8]| {
            let mut h = blake3::Hasher::new();
            for part in [&b"compute"[..], b"root", dxil, b""] {
                h.update(&(part.len() as u64).to_le_bytes());
                h.update(part);
            }
            h.finalize().to_hex()[..32].to_owned()
        };
        let (old, new) = (name(b"dxil of one build"), name(b"dxil of the next"));
        let notes = Notes::parse(&format!("begun {old} {container}\n"));
        assert!(!notes.retry(&old));
        assert!(notes.retry(&new));
    }

    #[test]
    fn notes_are_kept_per_driver_path() {
        let none = |_: &str| None;
        assert_eq!(driver_key("813621c016b18490", none), "813621c016b18490");
        let set = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(n, _)| *n == name)
                    .map(|(_, v)| std::ffi::OsString::from(v))
            }
        };
        let heap = driver_key(
            "813621c016b18490",
            set(&[("VKD3D_CONFIG", "no_descriptor_heap")]),
        );
        let other = driver_key(
            "3f5ac3173ea61333",
            set(&[("VKD3D_CONFIG", "no_descriptor_heap")]),
        );
        let unrelated = driver_key("813621c016b18490", set(&[("VKD3D_DEBUG", "trace")]));
        assert_ne!(heap, "813621c016b18490", "another driver path");
        assert_ne!(heap, other, "another driver build");
        assert_eq!(heap.len(), 16);
        assert_eq!(unrelated, "813621c016b18490");
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
        note(&file, Note::Begun, A, "0000000000000001");
        note(&file, Note::Begun, B, "0000000000000002");
        note(&file, Note::Made, B, "0000000000000002");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with(&format!("begun {A} 0000000000000001\n")));
        let notes = Notes::parse(&text);
        assert_eq!(notes.0.get(A), Some(&Note::Begun));
        assert_eq!(notes.0.get(B), Some(&Note::Made));
    }

    /// Notes are not kept through a link into a source tree (here: a
    /// folder holding the repository's marker), whether the link is the
    /// notes' folder or the note file, and whether or not its target exists.
    #[cfg(unix)]
    #[test]
    fn notes_are_not_kept_through_a_link_into_a_source_tree() {
        use std::os::unix::fs::symlink;
        let cache = tempfile::tempdir().unwrap();
        let tree = tempfile::tempdir().unwrap();
        std::fs::write(tree.path().join(victory_core::policy::ROOT_MARKER), "").unwrap();
        let pipelines = cache.path().join("pipelines");
        let notes = pipelines.join("warm-pipelines-x.txt");
        notes_allowed(&notes).unwrap();

        symlink(tree.path(), &pipelines).unwrap();
        assert!(notes_allowed(&notes).is_err(), "folder links into the tree");
        std::fs::remove_file(&pipelines).unwrap();

        std::fs::create_dir(&pipelines).unwrap();
        notes_allowed(&notes).unwrap();
        let there = tree.path().join("notes.txt");
        std::fs::write(&there, "").unwrap();
        symlink(&there, &notes).unwrap();
        assert!(
            notes_allowed(&notes).is_err(),
            "note file links into the tree"
        );
        std::fs::remove_file(&notes).unwrap();

        let nowhere = tree.path().join("not-yet.txt");
        symlink(&nowhere, &notes).unwrap();
        assert!(notes_allowed(&notes).is_err(), "note file links to nothing");
        assert!(!nowhere.exists());
    }

    /// A note file that is another name of a file elsewhere (a hard link,
    /// or a Unix link that the policy does not see under Wine) is replaced
    /// by a file of the warm-up's own: the file elsewhere is never written,
    /// and the earlier notes in it are kept.
    #[test]
    fn notes_replace_another_name_instead_of_writing_through_it() {
        type Alias = fn(&Path, &Path) -> std::io::Result<()>;
        let aliases: &[Alias] = &[
            |original, link| std::fs::hard_link(original, link),
            #[cfg(unix)]
            |original, link| std::os::unix::fs::symlink(original, link),
        ];
        for alias in aliases {
            let cache = tempfile::tempdir().unwrap();
            let tree = tempfile::tempdir().unwrap();
            let held = tree.path().join("held.txt");
            let earlier =
                format!("begun {A} 0000000000000001\nnot a note\nmade {B} 0000000000000002\n");
            std::fs::write(&held, &earlier).unwrap();
            let pipelines = cache.path().join("pipelines");
            std::fs::create_dir(&pipelines).unwrap();
            let notes = pipelines.join("warm-pipelines-x.txt");
            alias(&held, &notes).unwrap();

            let (read, file) = open_notes(&notes);
            let file = Mutex::new(Some(file.unwrap()));
            note(&file, Note::Begun, C, "0000000000000003");
            drop(file);

            assert_eq!(std::fs::read_to_string(&held).unwrap(), earlier);
            assert!(std::fs::symlink_metadata(&notes).unwrap().is_file());
            assert!(!read.retry(A) && read.retry(B) && read.retry(C));
            assert_eq!(
                std::fs::read_to_string(&notes).unwrap(),
                format!(
                    "begun {A} 0000000000000001\nmade {B} 0000000000000002\nbegun {C} 0000000000000003\n"
                )
            );
            let names: Vec<_> = std::fs::read_dir(&pipelines).unwrap().collect();
            assert_eq!(names.len(), 1, "no temporary file left");
        }
    }

    /// Notes that are there but cannot be read are left as they are.
    #[test]
    fn notes_that_cannot_be_read_are_not_replaced() {
        let cache = tempfile::tempdir().unwrap();
        let notes = cache.path().join("warm-pipelines-x.txt");
        std::fs::create_dir(&notes).unwrap();
        let (read, file) = open_notes(&notes);
        assert!(file.is_err());
        assert_eq!(read, Notes::default());
        assert!(notes.is_dir());
        assert_eq!(std::fs::read_dir(cache.path()).unwrap().count(), 1);
    }

    #[test]
    fn notes_start_where_there_were_none() {
        let cache = tempfile::tempdir().unwrap();
        let notes = cache.path().join("pipelines").join("warm-pipelines-x.txt");
        let (read, file) = open_notes(&notes);
        let file = Mutex::new(Some(file.unwrap()));
        note(&file, Note::Made, A, "0000000000000001");
        assert_eq!(read, Notes::default());
        assert_eq!(
            std::fs::read_to_string(&notes).unwrap(),
            format!("made {A} 0000000000000001\n")
        );
    }

    fn jobs(n: usize) -> Vec<(String, Vec<u8>)> {
        (0..n)
            .map(|i| (format!("{i:016x}"), vec![i as u8]))
            .collect()
    }

    #[test]
    fn workers_make_everything_and_count_outcomes() {
        let counts =
            run_workers(
                jobs(20),
                3,
                Duration::from_secs(30),
                None,
                4,
                |_, bytes| match bytes[0] % 4 {
                    0 => Outcome::Refused,
                    1 => Outcome::Skipped,
                    _ => Outcome::Made,
                },
            );
        assert_eq!(
            counts,
            PipelineCounts {
                made: 10,
                refused: 5,
                skipped: 5,
                stalled: 0,
                not_started: 0
            }
        );
    }

    #[test]
    fn a_pipeline_that_does_not_come_back_is_left_behind() {
        // Job 1 never returns: its worker is replaced and the rest are made.
        let counts = run_workers(
            jobs(6),
            2,
            Duration::from_millis(300),
            None,
            4,
            |_, bytes| {
                if bytes[0] == 1 {
                    std::thread::sleep(Duration::from_secs(3600));
                }
                Outcome::Made
            },
        );
        assert_eq!(
            counts,
            PipelineCounts {
                made: 5,
                stalled: 1,
                ..Default::default()
            }
        );
    }

    /// Stock vkd3d-proton 2576c0b4 never returns three of the title's
    /// compute pipelines (dxil-spirv #376): all three are left behind and
    /// everything else is made.
    #[test]
    fn three_that_never_come_back_are_all_left_behind() {
        let counts = run_workers(
            jobs(40),
            2,
            Duration::from_millis(200),
            None,
            MAX_LEFT_BEHIND,
            |_, bytes| {
                if [3, 4, 30].contains(&bytes[0]) {
                    std::thread::sleep(Duration::from_secs(3600));
                }
                Outcome::Made
            },
        );
        assert_eq!(
            counts,
            PipelineCounts {
                made: 37,
                stalled: 3,
                ..Default::default()
            }
        );
    }

    /// A worker whose pipeline was left behind and that comes back later
    /// takes no more work: its replacement has its place, and no more than
    /// `threads` workers take jobs at once.
    #[test]
    fn a_worker_left_behind_takes_no_more_work() {
        let ran: Arc<Mutex<Vec<(usize, std::thread::ThreadId, Instant)>>> = Arc::default();
        let busy = Arc::new(AtomicUsize::new(0));
        let most = Arc::new(AtomicUsize::new(0));
        let back = Arc::new(Mutex::new(None::<Instant>));
        let counts = {
            let (ran, busy, most, back) = (ran.clone(), busy.clone(), most.clone(), back.clone());
            run_workers(
                jobs(60),
                2,
                Duration::from_millis(150),
                None,
                4,
                move |_, bytes| {
                    let i = bytes[0] as usize;
                    lock(&ran).push((i, std::thread::current().id(), Instant::now()));
                    if i == 1 {
                        std::thread::sleep(Duration::from_millis(600));
                        *lock(&back) = Some(Instant::now());
                        return Outcome::Made;
                    }
                    let now = busy.fetch_add(1, Ordering::SeqCst) + 1;
                    most.fetch_max(now, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(40));
                    busy.fetch_sub(1, Ordering::SeqCst);
                    Outcome::Made
                },
            )
        };
        let back = lock(&back).expect("job 1 came back within the run");
        let ran = lock(&ran);
        let (_, stalled, _) = *ran.iter().find(|(i, ..)| *i == 1).unwrap();
        assert_eq!(
            ran.iter().filter(|(_, t, _)| *t == stalled).count(),
            1,
            "the worker left behind took more work after it came back"
        );
        assert!(
            ran.iter().any(|(_, _, at)| *at > back),
            "the run went on after job 1 came back"
        );
        assert!(most.load(Ordering::SeqCst) <= 2, "more than 2 at once");
        assert_eq!(
            counts,
            PipelineCounts {
                made: 59,
                stalled: 1,
                ..Default::default()
            }
        );
    }

    /// Pipelines that never come back are bounded: after `max_left_behind`
    /// no more start, and the pool returns.
    #[test]
    fn no_more_start_after_too_many_are_left_behind() {
        let asked = Arc::new(AtomicUsize::new(0));
        let began = Instant::now();
        let counts = {
            let asked = asked.clone();
            run_workers(
                jobs(30),
                2,
                Duration::from_millis(150),
                None,
                4,
                move |_, _| {
                    asked.fetch_add(1, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_secs(3600));
                    Outcome::Made
                },
            )
        };
        assert!(began.elapsed() < Duration::from_secs(10));
        assert_eq!(asked.load(Ordering::SeqCst), 4, "threads left behind");
        assert_eq!(
            counts,
            PipelineCounts {
                stalled: 4,
                not_started: 26,
                ..Default::default()
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
            4,
            |_, _| {
                std::thread::sleep(Duration::from_millis(100));
                Outcome::Made
            },
        );
        assert!(began.elapsed() < Duration::from_secs(5));
        assert!(counts.made >= 2 && counts.made < 20, "{counts:?}");
        assert_eq!(counts.made + counts.not_started, 100, "{counts:?}");
        assert_eq!(counts.stalled, 0);
    }

    #[test]
    fn no_work_returns_at_once() {
        let counts = run_workers(Vec::new(), 4, Duration::from_secs(30), None, 4, |_, _| {
            Outcome::Made
        });
        assert_eq!(counts, PipelineCounts::default());
    }

    #[test]
    fn kept_files_are_shared_by_the_workers() {
        let dir = tempfile::tempdir().unwrap();
        let files: Vec<Vec<u8>> = (0u8..10).map(|i| vec![i; 3 + i as usize]).collect();
        let paths: Vec<PathBuf> = files
            .iter()
            .enumerate()
            .map(|(i, bytes)| {
                let path = dir.path().join(format!("{i}.cdxil"));
                std::fs::write(&path, bytes).unwrap();
                path
            })
            .collect();
        // The first four files meet: only four workers at once get there.
        let arrived = Mutex::new(0);
        let all_here = Condvar::new();
        let seen = Mutex::new(Vec::new());
        warm_files(&paths, 4, &|| false, &|bytes: &[u8]| {
            let mut n = lock(&arrived);
            *n += 1;
            let (n, waited) = all_here
                .wait_timeout_while(n, Duration::from_secs(10), |n| *n < 4)
                .unwrap();
            all_here.notify_all();
            drop(n);
            lock(&seen).push((bytes.to_vec(), waited.timed_out()));
        });
        let mut seen = seen.into_inner().unwrap();
        assert!(seen.iter().all(|(_, timed_out)| !timed_out));
        seen.sort();
        let seen: Vec<Vec<u8>> = seen.into_iter().map(|(bytes, _)| bytes).collect();
        assert_eq!(seen, files);
    }
}
