//! Steps of a submission that read title memory flights in the air may
//! still write, done on the card in the flights' order instead of on the
//! CPU after a wait for them (`VICTORY_CARD_STEPS`, see [`stage`]).

use std::sync::Arc;

use super::*;
use crate::shadow::Shadow;

/// The stage (`VICTORY_CARD_STEPS`; off by default): `1` constant buffers
/// of descriptor tables (`Step::Fill` with a slot of its own), `2` the
/// title's buffer copies (`Step::Copy`) too, with the steps of the same
/// submission that depend on them. Only with the relay's second stage
/// (`relay_exact`).
///
/// Why: eleven dispatches a frame (the skinning passes, in two early
/// submissions) write through a view of the title's whole 174 MB heap, at
/// places they read out of job records in buffers: no bound of what they
/// write is known before they ran (frame capture replay2: 17af15c4015cb4b6,
/// 55679a0547649d0b, 89d08e561897d4e5, fdd56d97a806bcca and three more,
/// all through one view at offset 5920 of the title's bindless table). The
/// title's ring of constants lies in the same heap. One constant buffer of
/// 480 bytes and one copy of 20 KiB a frame, at a new place of the ring
/// each time, waited for these passes to land: 25-30 ms of a 105 ms frame,
/// the submitting thread standing (run record-1; with `VICTORY_FILL_TITLE=1`
/// in run relaycopy-1 the copy alone took over the wait).
///
/// What is done: a list of ours goes to the card in front of the
/// submission's lists, in the same call. It copies the bytes read from the
/// host copy that the flights write into a buffer of ours, and from there
/// to the destination's host copy (a copy), or the fill's descriptor slot
/// is pointed at that buffer (a fill; the slot is the fill's alone, the
/// root signatures say such descriptors are volatile, and the list that
/// names it has not gone to the card).
///
/// Why that is the value the CPU would have read after the wait. After the
/// wait title memory holds, for every byte read, what the title wrote, or
/// what the last of the flights that wrote it wrote (the landings put it
/// there, in order). The host copy on the card holds the same when our list
/// runs: every flight that may write one of these bytes writes this same
/// host copy ([`holder`]: a write through another host copy, a read-back
/// or a value written behind a flight there, and the old wait is taken),
/// they are all ahead of our list on the queue, and the title's own bytes
/// are brought up to date by a refresh step of our list, which is prepared
/// like any other list's (also under flights' declared writes:
/// `stale_under_flights`, which settles if it cannot). Nothing of this
/// submission writes these bytes earlier than our list: the steps are done
/// in front of all its lists in either case, and among themselves in the
/// order they stand in wherever one depends on another: [`plan`] takes
/// every copy and fill of the submission that writes what a taken one
/// reads or writes, or reads what a taken one writes. Every draw that
/// reads a buffer view of the heap relies on the same refresh and the same
/// order; what is new is only that a constant buffer and a copy do too.
/// Where a page is not mapped the host copy may hold other bytes than the
/// CPU's copy would have left: refused.
///
/// For a copy's destination our list declares a shader write, exactly the
/// bytes copied, after a refresh of them: from there on it is what any
/// shader's write is. The relay compares, and puts what changed into title
/// memory before the title is told anything of this flight (its
/// bottom-of-pipe writes and fences come after, in order); CPU steps of
/// later submissions that read it find it declared and wait, or are done
/// here. Refused: a step of the submission that cannot be taken (a texture,
/// vertices expanded, a fill of memory of another kind) reads the
/// destination behind the copy, or a refresh brings another host copy of it
/// up to date ([`plan`], [`elsewhere`]). While such a copy is in the air a
/// submission that refreshes another host copy of its destination, or
/// reads it in a step that is not checked otherwise, waits for it
/// ([`copied_read`]); with the copy done on the CPU it was there at once.
///
/// What stage 2 does not see: code of ours that reads title memory away
/// from a submission's steps (the pass over textures picked by index, while
/// a list is recorded). With the CPU's copy such a read of the destination
/// found the bytes at once; now it finds them when the flight has landed.
/// The landing tells the guards (`crate::watch::landing`), so a texture
/// made from older bytes is made again. This is why stage 2 is its own.
pub(super) fn stage() -> u32 {
    static STAGE: OnceLock<u32> = OnceLock::new();
    *STAGE.get_or_init(|| match std::env::var("VICTORY_CARD_STEPS").ok().as_deref() {
        Some("1") => 1,
        Some("2") => 2,
        _ => 0,
    })
}

/// What is done when the steps of a submission could all be taken but one of
/// its copies at their place ([`place`]) needs flights landed before the CPU
/// prepares it (`VICTORY_CARD_STEPS_GUARD`; off by default): `1` the steps
/// are refused and the submission waits as without this stage, `2` the case
/// is only counted and named ([`guard_seen`]), the steps are still taken.
///
/// Why: [`plan`] sees such a copy only for the order of what it takes. Taking
/// the steps skips the submission's wait for flights ([`late_blocked`]),
/// which [`blocked_by`] asks for two more reasons there: the copy's source
/// is taken into upload memory on the CPU while a flight may still write it
/// ([`place::Waits::Yes`]), or a step left on the CPU reads its destination,
/// which title memory is given only when the flights land
/// ([`place::read_by`]). Either reads bytes from before the flights.
fn guard() -> Guard {
    static GUARD: OnceLock<Guard> = OnceLock::new();
    *GUARD.get_or_init(|| match std::env::var("VICTORY_CARD_STEPS_GUARD").ok().as_deref() {
        Some("1") => Guard::Refuse,
        Some("2") => Guard::Count,
        _ => Guard::Off,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Guard {
    Off,
    Refuse,
    Count,
}

/// Why [`guarded`] refuses, by [`place::CpuWait`].
const GUARD_SOURCE: &str = "a copy at its place takes on the CPU a source flights write (VICTORY_CARD_STEPS_GUARD)";
const GUARD_READERS: &str = "a step on the CPU reads what a copy at its place writes behind flights (VICTORY_CARD_STEPS_GUARD)";

/// Whether steps that could all be taken are, with `wait` what a copy at its
/// place waits for flights for.
fn guarded(guard: Guard, wait: Option<&place::CpuWait>) -> Result<(), &'static str> {
    match (guard, wait) {
        (Guard::Refuse, Some(place::CpuWait::Source(..))) => Err(GUARD_SOURCE),
        (Guard::Refuse, Some(place::CpuWait::Readers)) => Err(GUARD_READERS),
        _ => Ok(()),
    }
}

/// Since the last frame-rate line, with [`guard`] on: submissions whose
/// steps could all be taken although a copy at its place takes a source
/// flights write, or has its destination read on the CPU; and how many of
/// them were refused.
static GUARDED: [std::sync::atomic::AtomicU64; 3] = [const { std::sync::atomic::AtomicU64::new(0) }; 3];

/// The first of those cases named one by one in the log; all of them, since
/// the start.
const GUARD_NAMED: u64 = 16;
static GUARD_SEEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Counts `wait` for [`log`], and names it while few have been.
fn guard_seen(wait: &place::CpuWait, refused: bool, steps: usize) {
    GUARDED[usize::from(matches!(wait, place::CpuWait::Readers))].fetch_add(1, Ordering::Relaxed);
    if refused {
        GUARDED[2].fetch_add(1, Ordering::Relaxed);
    }
    let n = GUARD_SEEN.fetch_add(1, Ordering::Relaxed);
    if n >= GUARD_NAMED {
        return;
    }
    let what = match *wait {
        place::CpuWait::Source(dst, src, len) => {
            format!("a copy at its place of {len:#x} bytes from {src:#x} to {dst:#x} takes its source on the CPU under flights' writes")
        }
        place::CpuWait::Readers => "a step on the CPU reads what a copy at its place writes, under flights' writes".to_owned(),
    };
    tracing::warn!(
        "steps on the card ({} of the first {GUARD_NAMED}): {what}; {steps} steps could be taken, {}",
        n + 1,
        if refused { "refused: the submission waits for the flights" } else { "taken anyway (VICTORY_CARD_STEPS_GUARD=2)" },
    );
}

/// Whether a copy at its place in a list ([`place`]) is only in the way of
/// what is done on the card in front of the lists if that stands behind it in
/// them (`VICTORY_PLACED_ORDER=1`; off: wherever it stands), and a
/// submission where it does is cut behind the copy's list ([`cut_for`]). Off:
/// the case did not come up once in runs submit-1 to submit-3, so it has only
/// its tests behind it.
///
/// The fills of constant buffers read the title's ring (7,500 a frame in the
/// opening), and one copy of a few hundred bytes a frame writes into it at its
/// place in a list. The whole submission was refused for it, and stood 40 to
/// 60 ms until the flights had landed (run n19).
fn placed_order() -> bool {
    #[cfg(feature = "diagnostics")]
    if let Some(on) = crate::ab::pick("VICTORY_PLACED_ORDER") {
        return on;
    }
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| crate::switch::on("VICTORY_PLACED_ORDER"))
}

/// Why [`plan`] refuses for [`place`].
const BEHIND_PLACED: &str = "a copy at its place in a list writes what it reads, or reads or writes what it writes";

/// The bytes of our buffer between reading and writing: what one submission
/// may do this way (each place read once, however many fills take it).
const BETWEEN: u64 = 4 << 20;

/// The most steps of a submission done this way.
const MOST: usize = 12288;

/// How a fill's bytes get where its list reads them, if the card brings
/// them.
#[derive(Clone)]
pub(super) enum FillTo {
    /// A descriptor slot that shows the fill's memory, and nothing else does
    /// (title heap, slot): it is made to show our buffer instead.
    Slot(usize, u32),
    /// A buffer the card can write, filled from its beginning.
    Buffer(ID3D12Resource),
}

/// Title memory something in the air writes: the host copy it goes through
/// (`None`: none, the bytes reach title memory when the flight lands), from,
/// to, and whether it is the destination of a copy done here.
pub(super) struct Writer {
    shadow: Option<Arc<Shadow>>,
    from: usize,
    to: usize,
    copy: bool,
}

/// What `items` of the line write.
pub(super) fn writers(items: &[Late]) -> Vec<Writer> {
    let mut out = Vec::new();
    for item in items {
        match item {
            Late::Run(flight) => {
                for (shadow, ranges) in &flight.back {
                    out.extend(ranges.iter().map(|&(from, to)| Writer {
                        shadow: Some(shadow.clone()),
                        from,
                        to,
                        copy: false,
                    }));
                }
                for &i in &flight.order {
                    for &(_, at, [row, rows, pitch]) in &flight.lists[i].host().deferred().backs {
                        out.push(Writer {
                            shadow: None,
                            from: at,
                            to: at + rows.saturating_sub(1) * pitch + row,
                            copy: false,
                        });
                    }
                }
                out.extend(flight.own.iter().flat_map(Taken::copied));
            }
            Late::Write(at, _, bytes) => out.push(Writer {
                shadow: None,
                from: *at,
                to: *at + *bytes,
                copy: false,
            }),
            Late::Signal(..) => {}
        }
    }
    out
}

/// The destinations of copies done here that have not landed.
pub(super) fn copies(line: &Line) -> Vec<Writer> {
    if stage() < 2 {
        return Vec::new();
    }
    let flying = line.line.iter().filter_map(|l| match l {
        Late::Run(flight) => flight.own.as_ref(),
        _ => None,
    });
    let landing = line.landing.iter().filter(|w| w.copy).map(|w| Writer {
        shadow: w.shadow.clone(),
        ..*w
    });
    flying.flat_map(Taken::copied).chain(landing).collect()
}

/// Whether `runs` refresh another host copy of a destination in `copies`,
/// or read it in a step that flights are not waited for otherwise
/// (`Step::Host`): that reads title memory, where the copy is not yet.
pub(super) fn copied_read(copies: &[Writer], runs: &[&HostList]) -> bool {
    if copies.is_empty() {
        return false;
    }
    runs.iter().any(|list| {
        list.deferred().copies.iter().any(|step| match step {
            Step::Refresh(shadow, at, len) => copies
                .iter()
                .any(|w| w.from < *at + *len && *at < w.to && w.shadow.as_ref().is_none_or(|s| !Arc::ptr_eq(s, shadow))),
            Step::Host(_, (from, to)) => copies.iter().any(|w| w.from < *to && *from < w.to),
            _ => false,
        })
    })
}

/// What a step of a submission does to title memory, as the plan sees it.
/// `asked`: flights in the air stop it from being done on the CPU now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Does {
    Copy { dst: usize, src: usize, len: usize, asked: bool },
    /// `to`: its memory can be given the bytes on the card.
    Fill { src: usize, len: usize, asked: bool, to: bool },
    /// Another read on the CPU (a texture, vertices expanded).
    Read { from: usize, to: usize, asked: bool },
    /// A refresh of a host copy (named by its address).
    Refresh { shadow: usize, from: usize, to: usize },
    /// A copy that its list does on the card ([`place`]).
    Placed { dst: usize, src: usize, len: usize },
    /// Anything, anywhere.
    Run,
}

fn meet(a: (usize, usize), b: (usize, usize)) -> bool {
    a.0 < b.1 && b.0 < a.1
}

/// The bytes `d` reads and writes as a copy or a fill (empty ranges for
/// what it does not).
fn reads_writes(d: &Does) -> Option<((usize, usize), (usize, usize))> {
    match *d {
        Does::Copy { dst, src, len, .. } => Some(((src, src + len), (dst, dst + len))),
        Does::Fill { src, len, .. } => Some(((src, src + len), (0, 0))),
        _ => None,
    }
}

fn touched(ranges: &[(usize, usize)], r: (usize, usize)) -> bool {
    touches(ranges, r.0, r.1)
}

/// The steps of `does` (by place) to be done on the card, in order, each
/// with its level; or why not. All that are asked for, and every copy and
/// fill that is in the way of one that is done there: it writes what that
/// one reads or writes, or reads what it writes, whichever comes first. So
/// what is left on the CPU writes none of their bytes, and reads of them
/// only what a taken copy has not been told to write yet: bytes a step
/// ahead of it reads, in title memory as it is.
///
/// `behind`: for [`BEHIND_PLACED`], the copy at its place and the first step
/// behind it that is in its way.
///
/// Steps of a level are done together, every read before every write; a
/// step that writes what an earlier one reads or writes, or reads what it
/// writes, is of a higher level. Our list does them level by level.
fn plan(does: &[Does], stage: u32, by_order: bool, behind: &mut Option<(usize, usize)>) -> Result<Vec<(usize, u32)>, &'static str> {
    let takes = |d: &Does| match *d {
        Does::Copy { .. } if stage < 2 => Err("a buffer copy (VICTORY_CARD_STEPS=2 does it on the card)"),
        Does::Fill { to: false, .. } => Err("a fill of memory the card cannot be given the bytes of"),
        _ => Ok(()),
    };
    let mut taken = vec![false; does.len()];
    let (mut read, mut written) = (Vec::new(), Vec::new());
    let mut more = Vec::new();
    for (i, d) in does.iter().enumerate() {
        match *d {
            Does::Run => return Err("a step of ours runs"),
            Does::Read { asked: true, .. } => return Err("a texture reads it"),
            Does::Copy { asked: true, .. } | Does::Fill { asked: true, .. } => more.push(i),
            _ => {}
        }
    }
    while !more.is_empty() {
        // (Joined once for the round: one by one the 2,800 fills of a frame
        // sorted the ranges each time, 16 ms of the submitting thread.)
        let (mut reads, mut writes) = (Vec::new(), Vec::new());
        for i in std::mem::take(&mut more) {
            takes(&does[i])?;
            taken[i] = true;
            if let Some((r, w)) = reads_writes(&does[i]) {
                if w.0 < w.1 && meet(r, w) {
                    return Err("a copy onto itself");
                }
                reads.push(r);
                writes.push(w);
            }
        }
        join_ranges(&mut read, reads);
        if writes.iter().any(|w| w.0 < w.1) {
            join_ranges(&mut written, writes);
        }
        if taken.iter().filter(|t| **t).count() > MOST {
            return Err("too many steps depend on it");
        }
        for (i, d) in does.iter().enumerate() {
            // A fill that cannot be taken is looked at below.
            if taken[i] || matches!(d, Does::Fill { to: false, .. }) {
                continue;
            }
            if let Some((r, w)) = reads_writes(d)
                && (touched(&written, r) || touched(&written, w) || touched(&read, w))
            {
                more.push(i);
            }
        }
    }
    // A copy at its place in a list comes after what is done in front of all
    // of them: as it should after the steps ahead of it, but not after those
    // behind it that read or write its bytes.
    for (p, d) in does.iter().enumerate() {
        if let Does::Placed { dst, src, len } = *d
            && (touched(&read, (dst, dst + len)) || touched(&written, (dst, dst + len)) || touched(&written, (src, src + len)))
        {
            let (to, from) = ((dst, dst + len), (src, src + len));
            let in_the_way = |i: usize| {
                reads_writes(&does[i]).is_some_and(|(r, w)| meet(r, to) || w.0 < w.1 && (meet(w, to) || meet(w, from)))
            };
            let found = (p + 1..does.len()).find(|&i| taken[i] && in_the_way(i));
            *behind = found.map(|i| (p, i));
            if !by_order || found.is_some() {
                return Err(BEHIND_PLACED);
            }
        }
    }
    let order: Vec<usize> = (0..does.len()).filter(|&i| taken[i]).collect();
    // What stays on the CPU and reads what an earlier taken copy writes
    // would read it too early.
    let mut behind: Vec<(usize, usize)> = Vec::new();
    for (i, d) in does.iter().enumerate() {
        if taken[i] {
            if let Some((_, w)) = reads_writes(d)
                && w.0 < w.1
            {
                join_ranges(&mut behind, [w]);
            }
            continue;
        }
        match *d {
            Does::Read { from, to, .. } if touched(&behind, (from, to)) => {
                return Err("a texture or expanded vertices read what it writes");
            }
            Does::Fill { src, len, .. } if touched(&behind, (src, src + len)) => {
                return Err("a fill of memory the card cannot be given the bytes of reads what it writes");
            }
            _ => {}
        }
    }
    let mut out: Vec<(usize, u32)> = Vec::with_capacity(order.len());
    // The copies among them, which alone put others behind them or have to
    // stand behind others.
    let mut copies: Vec<usize> = Vec::new();
    for &i in &order {
        let Some((r, w)) = reads_writes(&does[i]) else {
            continue;
        };
        let writes = w.0 < w.1;
        let mut level = 0;
        // Behind what writes what this reads or writes, and, if this
        // writes, behind what reads it.
        let ahead: Box<dyn Iterator<Item = usize>> =
            if writes { Box::new(0..out.len()) } else { Box::new(copies.iter().copied()) };
        for k in ahead {
            let (j, at): (usize, u32) = out[k];
            let Some((jr, jw)) = reads_writes(&does[j]) else {
                continue;
            };
            if at >= level && (meet(jw, r) || meet(jw, w) || meet(jr, w)) {
                level = at + 1;
            }
        }
        if writes {
            copies.push(out.len());
        }
        out.push((i, level));
    }
    Ok(out)
}

/// Whether a refresh of `does` brings a host copy other than `shadow` up to
/// date in `[from, to)`.
fn elsewhere(does: &[Does], shadow: usize, from: usize, to: usize) -> bool {
    does.iter().any(|d| match *d {
        Does::Refresh { shadow: s, from: a, to: b } => s != shadow && meet((a, b), (from, to)),
        _ => false,
    })
}

/// The host copy that holds `[from, to)` on the card once what `writers`
/// write is there: the one they all write it through, or without any the
/// one shaders would be given.
fn holder(writers: &[Writer], from: usize, to: usize) -> Result<Arc<Shadow>, &'static str> {
    let mut found: Option<&Arc<Shadow>> = None;
    for w in writers.iter().filter(|w| meet((w.from, w.to), (from, to))) {
        match (&w.shadow, found) {
            (None, _) => return Err("a read-back or a value behind a flight is written there"),
            (Some(s), Some(f)) if !Arc::ptr_eq(s, f) => return Err("flights write it through two host copies"),
            (Some(s), _) => found = Some(s),
        }
    }
    let shadow = match found {
        Some(s) => s.clone(),
        None => crate::shadow::find(from as u64, (to - from) as u64).ok_or("no host copy of it")?,
    };
    let (start, len) = shadow.span();
    if shadow.in_place() || (from as u64) < start || to as u64 > start + len {
        return Err("no host copy holds all of it");
    }
    Ok(shadow)
}

/// What the steps of `runs` do, and which step each is (its list by address,
/// its place in it). `written`: what flights may write.
fn does_of(written: &[(usize, usize)], runs: &[&HostList]) -> (Vec<Does>, Vec<(usize, usize)>) {
    let mut does = Vec::new();
    // Which step each is. One list's lock at a time: a submission may
    // name a list twice.
    let mut steps = Vec::new();
    for list in runs {
        for (i, step) in list.deferred().copies.iter().enumerate() {
            does.push(match step {
                &Step::Copy(dst, src, len) => Does::Copy {
                    dst,
                    src,
                    len,
                    asked: copy_blocked(written, dst, src, len).is_some(),
                },
                Step::Fill(_, src, len, to) => Does::Fill {
                    src: *src,
                    len: *len,
                    asked: read_blocked(written, *src, *len),
                    to: to.is_some(),
                },
                Step::Texture(staging) => {
                    let (at, len) = staging.memory();
                    Does::Read {
                        from: at,
                        to: at + len,
                        asked: touches(written, at, at + len),
                    }
                }
                &Step::Host(_, (from, to)) if from < to => Does::Read { from, to, asked: false },
                Step::Refresh(shadow, at, len) => Does::Refresh {
                    shadow: Arc::as_ptr(shadow) as usize,
                    from: *at,
                    to: *at + *len,
                },
                Step::Run(_) => Does::Run,
                Step::Placed(p) => Does::Placed { dst: p.dst, src: p.src, len: p.len },
                Step::GpuWrite(..) | Step::Host(..) => continue,
            });
            steps.push((std::ptr::from_ref::<HostList>(list) as usize, i));
        }
    }
    (does, steps)
}

/// With flights in the air: before which of `runs` the submission is cut so
/// that what reads or writes the bytes of a copy at its place in an earlier
/// list ([`BEHIND_PLACED`]) is done on the card in front of a submission of
/// its own, behind that copy.
pub(super) fn cut_for(runs: &[&HostList]) -> Option<usize> {
    if stage() == 0 || !relay_exact() || !placed_order() || runs.len() < 2 {
        return None;
    }
    // (Most submissions have none.)
    let first = runs.iter().position(|l| l.deferred().copies.iter().any(|s| matches!(s, Step::Placed(_))))?;
    if first + 1 == runs.len() {
        return None;
    }
    let written = line().written.clone();
    blocked_by(&written, runs, false)?;
    let (does, steps) = does_of(&written, runs);
    let mut behind = None;
    if plan(&does, stage(), true, &mut behind) != Err(BEHIND_PLACED) {
        return None;
    }
    let (placed, reader) = behind?;
    let place = |i: usize| runs.iter().position(|l| std::ptr::from_ref::<HostList>(l) as usize == steps[i].0);
    let cut = place(placed)? + 1;
    (place(reader)? >= cut).then_some(cut)
}

/// Whether every page of `[from, to)` can be read (through the kernel:
/// asking Windows walks the page tables, milliseconds in the title's heap).
/// `known`: pages that could be, of the same submission: its fills' places
/// lie eight to a page, and each was read whole to find out.
fn mapped(from: usize, to: usize, known: &mut HashMap<usize, ()>) -> bool {
    const PAGE: usize = 4096;
    (from / PAGE..to.div_ceil(PAGE)).all(|page| {
        known.contains_key(&page) || {
            let there = read_into((page * PAGE).max(from) as *const u8, &mut [0]);
            if there {
                known.insert(page, ());
            }
            there
        }
    })
}

/// A table by addresses and numbers of ours.
type HashMap<K, V> = std::collections::HashMap<K, V, std::hash::BuildHasherDefault<AddressHash>>;

/// Where the bytes of the fill that is `step` (its list by address, its place
/// in it) go.
fn fill_to(runs: &[&HostList], step: (usize, usize)) -> Option<FillTo> {
    let list = runs.iter().find(|l| std::ptr::from_ref::<HostList>(l) as usize == step.0)?;
    match list.deferred().copies.get(step.1)? {
        Step::Fill(_, _, _, to) => to.clone(),
        _ => None,
    }
}

/// A step done on the card.
struct Op {
    from: (Arc<Shadow>, usize),
    len: usize,
    /// Its place in the buffer between.
    at: u64,
    level: u32,
    to: To,
}

enum To {
    /// A host copy, and the title address.
    Copy(Arc<Shadow>, usize),
    /// A descriptor slot (title heap, slot).
    Slot(usize, u32),
    /// A buffer's beginning.
    Buffer(ID3D12Resource),
}

/// Whether `between` is also left readable by shaders as a resource
/// (`VICTORY_CARD_STEPS_SRV=1`; off by default), for an A/B on the card.
///
/// Why: a fill's slot is a constant buffer view of `between`, and
/// `constant_at` makes its raw twin a raw buffer view of the same bytes.
/// Translated shaders read the twin where a table's entry is a raw buffer,
/// and every table's constant buffer through it with renderer port 5
/// (`VICTORY_PORT_CBV=1`). Our list and the lists of the submission's
/// first part go to the card in one `ExecuteCommandLists`, and a buffer
/// decays to the common state only at the end of one: those lists find
/// `between` in the state [`record`] left, which without this has no
/// shader resource bit for a read through the twin.
fn srv() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("VICTORY_CARD_STEPS_SRV").is_ok_and(|v| v == "1"))
}

/// The state [`record`] leaves `between` in: a copy's source and constants,
/// and with `srv` ([`srv`]) a shader resource of every stage too.
fn readable(srv: bool) -> D3D12_RESOURCE_STATES {
    let constants = D3D12_RESOURCE_STATE_COPY_SOURCE | D3D12_RESOURCE_STATE_VERTEX_AND_CONSTANT_BUFFER;
    if srv { constants | D3D12_RESOURCE_STATE_ALL_SHADER_RESOURCE } else { constants }
}

/// Records a level of what [`stage`] describes into `list` (open): `reads`
/// are copied into `between` (source, offset in it, offset in `between`,
/// bytes), then `writes` out of it (destination, offset in it, offset in
/// `between`, bytes). `between` is left [`readable`] (and must be so,
/// unless this is the `first` level), the others in the common state, as
/// the submission's lists expect them.
fn record(
    first: bool,
    list: &ID3D12GraphicsCommandList,
    between: &ID3D12Resource,
    reads: &[(&ID3D12Resource, u64, u64, u64)],
    writes: &[(&ID3D12Resource, u64, u64, u64)],
) {
    let common = D3D12_RESOURCE_STATE_COMMON;
    let (source, dest) = (D3D12_RESOURCE_STATE_COPY_SOURCE, D3D12_RESOURCE_STATE_COPY_DEST);
    let mut sources: Vec<&ID3D12Resource> = Vec::new();
    let mut dests: Vec<&ID3D12Resource> = Vec::new();
    for (all, list) in [(reads, &mut sources), (writes, &mut dests)] {
        for &(buffer, ..) in all {
            if !list.iter().any(|b| b.as_raw() == buffer.as_raw()) {
                list.push(buffer);
            }
        }
    }
    let both = |b: &ID3D12Resource| dests.iter().any(|d| d.as_raw() == b.as_raw());
    let constants = readable(srv());
    let mut barriers = vec![transition(between, dest, constants)];
    barriers.extend(sources.iter().map(|b| transition(b, source, if both(b) { dest } else { common })));
    let done: Vec<D3D12_RESOURCE_BARRIER> = dests.iter().map(|b| transition(b, dest, common)).collect();
    // SAFETY: an open list; live buffers, and the callers' ranges lie in
    // them. The buffers start in the common state (the list is the first of
    // its call) and are promoted by their first use.
    unsafe {
        if first {
            // What earlier lists' shaders wrote, first.
            list.ResourceBarrier(&[D3D12_RESOURCE_BARRIER {
                Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
                Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
                Anonymous: D3D12_RESOURCE_BARRIER_0 {
                    UAV: std::mem::ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER {
                        pResource: std::mem::ManuallyDrop::new(None),
                    }),
                },
            }]);
        } else {
            list.ResourceBarrier(&[transition(between, constants, dest)]);
        }
        for &(buffer, offset, at, len) in reads {
            list.CopyBufferRegion(between, at, buffer, offset, len);
        }
        list.ResourceBarrier(&barriers);
        for &(buffer, offset, at, len) in writes {
            list.CopyBufferRegion(buffer, offset, between, at, len);
        }
        if !done.is_empty() {
            list.ResourceBarrier(&done);
        }
    }
}

/// The list in front of a submission's, and its buffer ([`BETWEEN`] bytes).
struct Own {
    list: HostList,
    between: ID3D12Resource,
}

// SAFETY: D3D12 objects are free-threaded, and the list is touched by one
// thread at a time (the submitting one until it is sent, then the one that
// lands it, then nobody until it is taken out of `POOL`).
unsafe impl Send for Own {}

/// Those that have landed: closed, their steps dropped.
static POOL: Mutex<Vec<Own>> = Mutex::new(Vec::new());

/// Since the last frame-rate line: fills and copies done on the card, their
/// bytes, and submissions that waited instead, refused by a check.
static DONE: [std::sync::atomic::AtomicU64; 4] = [const { std::sync::atomic::AtomicU64::new(0) }; 4];

/// What [`Host::card_steps`] took off a submission: the list that does it,
/// for the submission to run first, and the steps the CPU leaves out (their
/// list by address, their place in it).
pub(super) struct Taken {
    own: Own,
    steps: std::collections::HashSet<(usize, usize), std::hash::BuildHasherDefault<AddressHash>>,
    copied: Vec<(Arc<Shadow>, usize, usize)>,
}

impl Taken {
    pub(super) fn list(&self) -> &HostList {
        &self.own.list
    }

    /// Whether step `i` of `list` is done by this.
    pub(super) fn takes(&self, list: &HostList, i: usize) -> bool {
        self.steps.contains(&(std::ptr::from_ref(list) as usize, i))
    }

    fn copied(&self) -> impl Iterator<Item = Writer> {
        self.copied.iter().map(|(shadow, from, to)| Writer {
            shadow: Some(shadow.clone()),
            from: *from,
            to: *to,
            copy: true,
        })
    }

    /// The submission has run and landed: the list and its buffer are free.
    pub(super) fn landed(self) {
        // The host copies its steps held go; its commands stay until the
        // next use resets it.
        drop(std::mem::take(&mut *self.own.list.deferred()));
        POOL.lock().unwrap_or_else(|e| e.into_inner()).push(self.own);
    }
}

impl Host {
    /// With flights in the air: if what keeps `runs` from being prepared
    /// now is reads of title memory that can all be done on the card
    /// ([`stage`]), the list that does them. `None` if nothing is in the
    /// way or the old wait is due. Under the submission lock.
    pub(super) fn card_steps(&self, runs: &[&HostList]) -> Option<Taken> {
        if stage() == 0 || !relay_exact() {
            return None;
        }
        let (written, copies) = {
            let line = line();
            if line.written.is_empty() {
                return None;
            }
            (line.written.clone(), copies(&line))
        };
        let looked = crate::timeline::span("steps on the card: whether in the way");
        blocked_by(&written, runs, false)?;
        let read = copied_read(&copies, runs);
        looked.end();
        let taken = if read {
            Err("a copy done on the card is read through another host copy")
        } else {
            self.take_steps(&written, runs)
        };
        match taken {
            Ok(taken) => taken,
            Err(why) => {
                DONE[3].fetch_add(1, Ordering::Relaxed);
                #[cfg(feature = "diagnostics")]
                refused(why);
                let _ = why;
                None
            }
        }
    }

    fn take_steps(&self, written: &[(usize, usize)], runs: &[&HostList]) -> Result<Option<Taken>, &'static str> {
        let mut spent = crate::timeline::span("steps on the card: chosen");
        let (does, steps) = does_of(written, runs);
        let chosen = plan(&does, stage(), placed_order(), &mut None)?;
        if chosen.is_empty() {
            return Ok(None);
        }
        spent.next("steps on the card: what holds their bytes");
        let mut writers = {
            let line = line();
            let mut all = writers(&line.line);
            all.extend(line.landing.iter().map(|w| Writer {
                shadow: w.shadow.clone(),
                ..*w
            }));
            all
        };
        let mut ops: Vec<Op> = Vec::with_capacity(chosen.len());
        // The places read (host copy, address, bytes, level): where in the
        // buffer between their bytes are.
        let mut places: HashMap<(usize, usize, usize, u32), u64> = HashMap::default();
        // Some 2,800 fills a frame, of a few hundred places: what holds each
        // is looked for once, as long as no copy is added to the writers.
        let mut held: HashMap<(usize, usize), Arc<Shadow>> = HashMap::default();
        let mut room = HashMap::default();
        let mut end = 0u64;
        for &(i, level) in &chosen {
            let Some(((src, _), (dst, _))) = reads_writes(&does[i]) else {
                continue;
            };
            let (Does::Copy { len, .. } | Does::Fill { len, .. }) = does[i] else {
                continue;
            };
            if len == 0 {
                return Err("nothing to read");
            }
            let from = match held.get(&(src, len)) {
                Some(from) => from.clone(),
                None => {
                    let from = holder(&writers, src, src + len)?;
                    held.insert((src, len), from.clone());
                    from
                }
            };
            let place = (Arc::as_ptr(&from) as usize, src, len, level);
            let at = match places.get(&place) {
                Some(&at) => at,
                None => {
                    if end + len as u64 > BETWEEN {
                        return Err("too much at once");
                    }
                    if !mapped(src, src + len, &mut room) {
                        return Err("not all of what it reads is mapped");
                    }
                    let at = end;
                    end = (end + len as u64).next_multiple_of(256);
                    places.insert(place, at);
                    at
                }
            };
            let to = match does[i] {
                Does::Copy { .. } => {
                    let to = holder(&writers, dst, dst + len)?;
                    if elsewhere(&does, Arc::as_ptr(&to) as usize, dst, dst + len) {
                        return Err("another host copy of what it writes is read");
                    }
                    if !mapped(dst, dst + len, &mut room) {
                        return Err("not all of what it writes is mapped");
                    }
                    // What the steps behind it read of it is in that host
                    // copy, or nowhere.
                    writers.push(Writer {
                        shadow: Some(to.clone()),
                        from: dst,
                        to: dst + len,
                        copy: true,
                    });
                    held.clear();
                    To::Copy(to, dst)
                }
                _ => match fill_to(runs, steps[i]) {
                    Some(FillTo::Slot(heap, index)) => To::Slot(heap, index),
                    Some(FillTo::Buffer(buffer)) => To::Buffer(buffer),
                    None => continue,
                },
            };
            ops.push(Op {
                from: (from, src),
                len,
                at,
                level,
                to,
            });
        }
        // Before anything is recorded or a slot pointed elsewhere: refused,
        // the submission is prepared as if this stage were off.
        if guard() != Guard::Off {
            spent.next("steps on the card: copies at their place");
            let wait = place::cpu_wait(runs, written);
            let verdict = guarded(guard(), wait.as_ref());
            if let Some(wait) = &wait {
                guard_seen(wait, verdict.is_err(), chosen.len());
            }
            verdict?;
        }
        spent.next("steps on the card: the list");
        let Some(own) = self.own_list() else {
            return Err("no host list or buffer for it");
        };
        // First every refresh, then the writes: the bytes compared with are
        // the title's. (One step for each host copy, over all that is read of
        // it: a flush brings a copy up to date from the first range asked for
        // to the last, whatever the steps.)
        let mut spans: ByCopy<(Arc<Shadow>, usize, usize)> = ByCopy::default();
        for op in &ops {
            let to = match &op.to {
                To::Copy(to, dst) => Some((to, *dst)),
                _ => None,
            };
            for (shadow, at) in std::iter::once((&op.from.0, op.from.1)).chain(to) {
                let span = spans.entry(Arc::as_ptr(shadow) as usize).or_insert_with(|| (shadow.clone(), at, at + op.len));
                (span.1, span.2) = (span.1.min(at), span.2.max(at + op.len));
            }
        }
        for (shadow, from, to) in spans.into_values() {
            shadow.refresh(&own.list, from as u64, (to - from) as u64);
        }
        let mut copied = Vec::new();
        for op in &ops {
            if let To::Copy(to, dst) = &op.to {
                to.gpu_writes(&own.list, *dst as u64, op.len as u64);
                copied.push((to.clone(), *dst, *dst + op.len));
            }
        }
        #[cfg(feature = "diagnostics")]
        own.list.note_op(|| format!("{} copies of title memory done on the card, {end} bytes (VICTORY_CARD_STEPS)", ops.len()));
        let levels = ops.iter().map(|op| op.level).max().unwrap_or(0);
        for level in 0..=levels {
            let mut reads = Vec::new();
            let mut writes = Vec::new();
            for op in ops.iter().filter(|op| op.level == level) {
                let len = op.len as u64;
                let read = (&op.from.0.buffer, op.from.0.offset(op.from.1 as u64), op.at, len);
                // (Each place once.)
                if places.remove(&(Arc::as_ptr(&op.from.0) as usize, op.from.1, op.len, level)).is_some() {
                    reads.push(read);
                }
                match &op.to {
                    To::Copy(to, dst) => writes.push((&to.buffer, to.offset(*dst as u64), op.at, len)),
                    To::Buffer(buffer) => writes.push((buffer, 0, op.at, len)),
                    To::Slot(..) => {}
                }
            }
            record(level == 0, &own.list.list, &own.between, &reads, &writes);
        }
        own.list.close();
        // SAFETY: a live buffer.
        let gpu = unsafe { own.between.GetGPUVirtualAddress() };
        spent.next("steps on the card: the slots");
        // (11,000 slots a frame in a street, two descriptors each: on
        // several threads, a slot always on the same one, in the steps'
        // order.)
        let parts = if ops.len() < 256 { 1 } else { 8 };
        crate::land::each_under(parts, &|part| {
            for op in &ops {
                if let To::Slot(heap, index) = &op.to
                    && *index as usize / 4 % parts == part
                {
                    // (A heap that is gone: nothing can read the slot.)
                    crate::descriptor::constant_slot(*heap, *index, &own.between, gpu + op.at, op.len as u32);
                }
            }
        });
        let copies = ops.iter().filter(|op| matches!(op.to, To::Copy(..))).count();
        DONE[0].fetch_add((ops.len() - copies) as u64, Ordering::Relaxed);
        DONE[1].fetch_add(copies as u64, Ordering::Relaxed);
        DONE[2].fetch_add(ops.iter().map(|op| op.len as u64).sum(), Ordering::Relaxed);
        #[cfg(feature = "diagnostics")]
        noted(&ops);
        spent.end();
        Ok(Some(Taken {
            own,
            steps: chosen.iter().map(|&(i, _)| steps[i]).collect(),
            copied,
        }))
    }

    /// A list of ours open for recording, and its buffer.
    fn own_list(&self) -> Option<Own> {
        let pooled = POOL.lock().unwrap_or_else(|e| e.into_inner()).pop();
        if let Some(own) = pooled {
            own.list.reset();
            return Some(own);
        }
        let between = self.uav_buffer(BETWEEN)?;
        name(&between, || "copies of title memory on the card".into());
        let list = self.new_list().ok()?;
        Some(Own { list, between })
    }
}

/// Diagnostics: the copies and fills done on the card since the last
/// frame-rate line, by bytes, host copy read and host copy written (start
/// and length; zeros for a fill): how many, and the last title addresses.
#[cfg(feature = "diagnostics")]
type Noted = std::collections::HashMap<(usize, (u64, u64), (u64, u64)), (u64, usize, usize)>;
#[cfg(feature = "diagnostics")]
static NOTED: Mutex<Option<Noted>> = Mutex::new(None);

#[cfg(feature = "diagnostics")]
fn noted(ops: &[Op]) {
    let mut noted = NOTED.lock().unwrap_or_else(|e| e.into_inner());
    let noted = noted.get_or_insert_with(Default::default);
    for op in ops {
        let (to, dst) = match &op.to {
            To::Copy(to, dst) => (to.span(), *dst),
            To::Slot(..) | To::Buffer(_) => ((0, 0), 0),
        };
        let e = noted.entry((op.len, op.from.0.span(), to)).or_default();
        *e = (e.0 + 1, op.from.1, dst);
    }
}

/// For the frame-rate line.
pub(super) fn log(frames: f64) {
    if stage() == 0 {
        return;
    }
    let d: Vec<f64> = DONE.iter().map(|d| d.swap(0, Ordering::Relaxed) as f64 / frames).collect();
    tracing::info!(
        "steps done on the card instead of waiting for flights, per frame (VICTORY_CARD_STEPS={}): {:.2} fills, {:.2} buffer \
         copies, {:.1} KB; {:.2} submissions refused and waited",
        stage(),
        d[0],
        d[1],
        d[2] / 1024.0,
        d[3],
    );
    if guard() != Guard::Off {
        let g: Vec<f64> = GUARDED.iter().map(|g| g.swap(0, Ordering::Relaxed) as f64 / frames).collect();
        tracing::info!(
            "steps that could all be taken while a copy at its place needs flights landed, per frame \
             (VICTORY_CARD_STEPS_GUARD={}): {:.2} by its source, {:.2} by a reader of its destination; {:.2} refused; {} since the start",
            if guard() == Guard::Refuse { "1, refused" } else { "2, counted only" },
            g[0],
            g[1],
            g[2],
            GUARD_SEEN.load(Ordering::Relaxed),
        );
    }
    #[cfg(feature = "diagnostics")]
    {
        let r: Vec<f64> = place::READERS.iter().map(|r| r.swap(0, Ordering::Relaxed) as f64 / frames).collect();
        if r.iter().any(|&n| n > 0.0) {
            tracing::info!(
                "what a copy at its place writes, found read on the CPU behind it, per frame (looks, all threads): {:.2} by \
                 another such copy, {:.2} by a buffer copy, {:.2} by a fill, {:.2} by a texture, {:.2} by a step of ours",
                r[0],
                r[1],
                r[2],
                r[3],
                r[4]
            );
        }
        let noted = NOTED.lock().unwrap_or_else(|e| e.into_inner()).take().unwrap_or_default();
        let mut noted: Vec<_> = noted.into_iter().collect();
        noted.sort_unstable_by_key(|(_, e)| std::cmp::Reverse(e.0));
        for ((len, from, to), (n, src, dst)) in noted.into_iter().take(5) {
            let n = n as f64 / frames;
            if to.1 == 0 {
                tracing::info!("done on the card, per frame: {n:.2} fills of {len:#x} bytes from the host copy of {:#x}+{:#x} (last {src:#x})", from.0, from.1);
            } else {
                tracing::info!(
                    "done on the card, per frame: {n:.2} copies of {len:#x} bytes from the host copy of {:#x}+{:#x} (last {src:#x}) \
                     to that of {:#x}+{:#x} (last {dst:#x})",
                    from.0,
                    from.1,
                    to.0,
                    to.1
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn copy(dst: usize, src: usize, len: usize, asked: bool) -> Does {
        Does::Copy { dst, src, len, asked }
    }

    fn fill(src: usize, len: usize, asked: bool) -> Does {
        Does::Fill { src, len, asked, to: true }
    }

    fn stuck(src: usize, len: usize) -> Does {
        Does::Fill { src, len, asked: false, to: false }
    }

    /// What is done on the card, as (place, level), in stage 2.
    fn planned(does: &[Does]) -> Vec<(usize, u32)> {
        plan(does, 2, true, &mut None).unwrap()
    }

    /// Only what is asked for is taken, all of it, and a step that stands
    /// in none of their ways stays where it is.
    #[test]
    fn steps_apart_from_one_another_are_taken() {
        let does = [
            copy(1000, 5000, 100, false),
            fill(2000, 64, true),
            Does::Refresh { shadow: 1, from: 0, to: 9000 },
            copy(3000, 6000, 100, true),
            fill(1000, 64, false),
            Does::Read { from: 3100, to: 4000, asked: false },
            stuck(3100, 5),
        ];
        assert_eq!(planned(&does), [(1, 0), (3, 0)]);
        // Two fills of the same bytes.
        assert_eq!(plan(&[fill(0, 64, true), fill(32, 64, true)], 1, true, &mut None), Ok(vec![(0, 0), (1, 0)]));
        assert_eq!(plan(&[fill(0, 64, false)], 1, true, &mut None), Ok(vec![]));
        // Ends that touch are apart.
        let does = [
            copy(1000, 5000, 100, true),
            copy(900, 9000, 100, false),
            copy(1100, 9000, 5, false),
            copy(4900, 9000, 100, false),
        ];
        assert_eq!(planned(&does), [(0, 0)]);
    }

    /// What depends on a taken step, or it on it, is taken too, on the level
    /// the order of the steps asks for.
    #[test]
    fn steps_that_depend_on_one_another_are_taken_in_order() {
        let taken = copy(1000, 5000, 100, true);
        // Reads what it writes, behind and ahead of it.
        assert_eq!(planned(&[taken, fill(1099, 64, false)]), [(0, 0), (1, 1)]);
        assert_eq!(planned(&[fill(1099, 64, false), taken]), [(0, 0), (1, 1)]);
        assert_eq!(planned(&[taken, copy(7000, 1050, 10, false)]), [(0, 0), (1, 1)]);
        // Writes what it reads, and what it writes.
        assert_eq!(planned(&[copy(5099, 9000, 1, false), taken]), [(0, 0), (1, 1)]);
        assert_eq!(planned(&[taken, copy(5099, 9000, 1, false)]), [(0, 0), (1, 1)]);
        assert_eq!(planned(&[taken, copy(999, 9000, 2, false)]), [(0, 0), (1, 1)]);
        // The same list twice.
        assert_eq!(planned(&[taken, taken]), [(0, 0), (1, 1)]);
        // A chain, with steps apart from it in between, and two fills of one
        // copy on one level.
        let does = [
            copy(8000, 7000, 50, false),
            taken,
            copy(2000, 1010, 20, false),
            fill(90_000, 10, false),
            fill(2000, 20, false),
            fill(2010, 20, true),
            copy(1000, 2005, 5, false),
            fill(1000, 100, false),
        ];
        assert_eq!(planned(&does), [(1, 0), (2, 1), (4, 2), (5, 2), (6, 2), (7, 3)]);
        // A fill is in nobody's way.
        assert_eq!(planned(&[taken, fill(5000, 100, false)]), [(0, 0)]);
        // A copy ahead that a taken one only shares its source with.
        assert_eq!(planned(&[copy(7000, 5000, 10, false), taken]), [(1, 0)]);
    }

    /// A copy at its place in a list is in the way of what reads or writes its
    /// bytes behind it, not of what did ahead of it.
    #[test]
    fn copies_at_their_place_stand_in_the_way_of_what_is_behind_them() {
        let placed = Does::Placed { dst: 2000, src: 3000, len: 64 };
        let behind = |does: &[Does]| {
            let mut found = None;
            let refused = plan(does, 2, true, &mut found);
            assert!(refused.is_ok() || found.is_some() && refused == Err(BEHIND_PLACED));
            assert_eq!(plan(does, 2, false, &mut None).is_err(), does.len() > 1 && !matches!(does, [_, Does::Fill { src: 5000, .. }]));
            found
        };
        assert_eq!(behind(&[fill(2032, 64, true), placed]), None);
        assert_eq!(behind(&[placed, fill(2032, 64, true)]), Some((0, 1)));
        assert_eq!(behind(&[placed, fill(5000, 64, true)]), None);
        assert_eq!(behind(&[fill(2000, 8, true), placed, fill(2000, 8, true)]), Some((1, 2)));
        // One that writes what it copies from, and one that writes over it.
        assert_eq!(behind(&[copy(3000, 5000, 8, true), placed]), None);
        assert_eq!(behind(&[placed, copy(3000, 5000, 8, true)]), Some((0, 1)));
        assert_eq!(behind(&[copy(2060, 5000, 8, true), placed]), None);
        assert_eq!(behind(&[placed, copy(2060, 5000, 8, true)]), Some((0, 1)));
    }

    /// Whatever the steps, done level by level (in a level every read
    /// before every write) they read and leave what they read and leave one
    /// after the other, the CPU's steps ahead of all of them.
    #[test]
    fn levels_do_what_the_order_does() {
        let mut seed = 12345u32;
        let mut random = move |n: u32| {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
            (seed >> 16) % n
        };
        let (mut done, mut refused) = (0, 0);
        for _ in 0..4000 {
            let does: Vec<Does> = (0..2 + random(9))
                .map(|_| {
                    let (at, other, len) = (random(60) as usize, random(60) as usize, 1 + random(8) as usize);
                    let asked = random(4) == 0;
                    match random(8) {
                        0..4 => copy(at, other, len, asked),
                        4..6 => fill(at, len, asked),
                        6 => stuck(at, len),
                        _ => Does::Read { from: at, to: at + len, asked: false },
                    }
                })
                .collect();
            let Ok(chosen) = plan(&does, 2, true, &mut None) else {
                refused += 1;
                continue;
            };
            done += 1;
            let start: Vec<u32> = (0..80).collect();
            // One after the other: what each reads, and what is left.
            let mut memory = start.clone();
            let mut wanted = Vec::new();
            for d in &does {
                let (from, to) = match *d {
                    Does::Read { from, to, .. } => (from, to),
                    _ => reads_writes(d).unwrap().0,
                };
                wanted.push(memory[from..to].to_vec());
                if let Does::Copy { dst, src, len, .. } = *d {
                    memory.copy_within(src..src + len, dst);
                }
            }
            // The CPU's, then the card's.
            let mut cpu = start.clone();
            let taken = |i: usize| chosen.iter().any(|c| c.0 == i);
            for (i, d) in does.iter().enumerate().filter(|&(i, _)| !taken(i)) {
                let (from, to) = match *d {
                    Does::Read { from, to, .. } => (from, to),
                    _ => reads_writes(d).unwrap().0,
                };
                assert_eq!(cpu[from..to], wanted[i][..], "step {i} of {does:?}");
                if let Does::Copy { dst, src, len, .. } = *d {
                    cpu.copy_within(src..src + len, dst);
                }
            }
            let mut card = cpu.clone();
            for level in 0..=chosen.iter().map(|c| c.1).max().unwrap_or(0) {
                let of: Vec<usize> = chosen.iter().filter(|c| c.1 == level).map(|c| c.0).collect();
                let before = card.clone();
                for i in of {
                    let ((from, to), (dst, _)) = reads_writes(&does[i]).unwrap();
                    assert_eq!(before[from..to], wanted[i][..], "step {i} of {does:?}: {chosen:?}");
                    if matches!(does[i], Does::Copy { .. }) {
                        card[dst..dst + to - from].copy_from_slice(&before[from..to]);
                    }
                }
            }
            assert_eq!(card, memory, "{does:?}: {chosen:?}");
        }
        assert!(done > 500 && refused > 100, "{done} {refused}");
    }

    /// What cannot be done on the card refuses the lot, where it stands in
    /// the way.
    #[test]
    fn steps_in_the_way_are_refused() {
        let taken = copy(1000, 5000, 100, true);
        assert!(plan(&[taken, Does::Run], 2, true, &mut None).is_err());
        // A texture that itself waits for flights; a fill that cannot be
        // taken but waits.
        assert!(plan(&[taken, Does::Read { from: 1, to: 2, asked: true }], 2, true, &mut None).is_err());
        assert!(plan(&[Does::Fill { src: 1, len: 1, asked: true, to: false }], 2, true, &mut None).is_err());
        // Reads on the CPU of what it writes: behind it, not ahead of it.
        for read in [Does::Read { from: 900, to: 1001, asked: false }, stuck(1050, 3)] {
            assert!(plan(&[taken, read], 2, true, &mut None).is_err(), "{read:?} behind");
            assert_eq!(planned(&[read, taken]), [(1, 0)], "{read:?} ahead");
        }
        // Also of what is taken along.
        assert!(plan(&[taken, copy(3000, 1000, 10, false), stuck(3005, 1)], 2, true, &mut None).is_err());
        // Into itself, and copies without stage 2.
        assert!(plan(&[copy(1000, 1050, 100, true)], 2, true, &mut None).is_err());
        assert!(plan(&[taken], 1, true, &mut None).is_err());
        assert!(plan(&[fill(1000, 4, true), copy(1000, 5000, 100, false)], 1, true, &mut None).is_err());
    }

    /// A copy at its place is seen by the plan only for the order: beside a
    /// fill that reads what flights write it is in nobody's way, and the fill
    /// is taken whether the copy takes its source on the CPU there too or a
    /// step on the CPU reads its destination behind it. The guard refuses
    /// those (`=1`) and only those; counting alone (`=2`) and off take them.
    #[test]
    fn copies_at_their_place_that_wait_for_flights_are_left_to_the_guard() {
        let placed = Does::Placed { dst: 9000, src: 1100, len: 64 };
        assert_eq!(planned(&[fill(1000, 64, true), placed]), [(0, 0)]);
        assert_eq!(planned(&[fill(1000, 64, true), placed, Does::Read { from: 9000, to: 9064, asked: false }]), [(0, 0)]);
        let source = place::CpuWait::Source(9000, 1100, 64);
        assert_eq!(guarded(Guard::Refuse, Some(&source)), Err(GUARD_SOURCE));
        assert_eq!(guarded(Guard::Refuse, Some(&place::CpuWait::Readers)), Err(GUARD_READERS));
        assert_eq!(guarded(Guard::Refuse, None), Ok(()));
        for guard in [Guard::Off, Guard::Count] {
            assert_eq!(guarded(guard, Some(&source)), Ok(()));
            assert_eq!(guarded(guard, Some(&place::CpuWait::Readers)), Ok(()));
        }
    }

    #[test]
    fn a_refresh_of_another_host_copy_is_seen() {
        let does = [Does::Refresh { shadow: 7, from: 100, to: 200 }];
        assert!(!elsewhere(&does, 7, 100, 200));
        assert!(elsewhere(&does, 8, 199, 300));
        assert!(!elsewhere(&does, 8, 200, 300));
    }

    /// A value written behind a flight, or a read-back, is in no host copy
    /// before it lands.
    #[test]
    fn what_no_host_copy_holds_is_refused() {
        let writers = [Writer { shadow: None, from: 100, to: 108, copy: false }];
        assert!(holder(&writers, 104, 120).is_err());
    }

    /// Reads are all before writes, between two buffers and within one, and
    /// the buffers are usable in the lists that follow in the same call.
    #[test]
    fn the_card_copies_what_is_recorded() {
        let Some(host) = host() else {
            return; // no host GPU
        };
        let (a, a_data) = host.shared_buffer(1 << 16).unwrap();
        let (b, b_data) = host.shared_buffer(1 << 16).unwrap();
        // SAFETY: mappings of 64 KiB.
        let (a_bytes, b_bytes) =
            unsafe { (std::slice::from_raw_parts_mut(a_data, 1 << 16), std::slice::from_raw_parts_mut(b_data, 1 << 16)) };
        for (i, (x, y)) in a_bytes.iter_mut().zip(b_bytes.iter_mut()).enumerate() {
            (*x, *y) = (i as u8 ^ (i >> 8) as u8, 0xee);
        }
        let was = a_bytes.to_vec();
        let own = host.own_list().unwrap();
        // Within `a` (there and back), from `a` to `b`, and a read alone.
        record(
            true,
            &own.list.list,
            &own.between,
            &[(&a, 33, 0, 100), (&a, 2000, 256, 100), (&a, 5001, 512, 7), (&a, 9, 1024, 4)],
            &[(&a, 2000, 0, 100), (&a, 33, 256, 100), (&b, 77, 512, 7)],
        );
        // A level behind it, of what it wrote.
        record(false, &own.list.list, &own.between, &[(&b, 77, 2048, 7)], &[(&b, 1077, 2048, 7)]);
        own.list.close();
        // A list behind it that uses the buffers in other ways.
        let next = host.new_list().unwrap();
        let (out, out_data) = host.shared_buffer(1 << 16).unwrap();
        // SAFETY: an open list; live buffers.
        unsafe {
            next.list.CopyBufferRegion(&out, 0, &own.between, 1024, 4);
            next.list.CopyBufferRegion(&b, 4096, &a, 33, 100);
        }
        next.close();
        host.execute(&[&own.list, &next]);
        // With the debug layer (`VICTORY_D3D12_DEBUG=1`, diagnostics).
        assert_eq!(host.log_messages(), None);
        assert_eq!(a_bytes[2000..2100], was[33..133]);
        assert_eq!(a_bytes[33..133], was[2000..2100]);
        assert_eq!(b_bytes[77..84], was[5001..5008]);
        assert_eq!(b_bytes[1077..1084], was[5001..5008]);
        assert_eq!(b_bytes[4096..4196], was[2000..2100]);
        // SAFETY: a mapping of 64 KiB.
        assert_eq!(unsafe { std::slice::from_raw_parts(out_data, 4) }, &was[9..13]);
        assert!(b_bytes[..77].iter().all(|&x| x == 0xee));
        a_bytes[2000..2100].copy_from_slice(&was[2000..2100]);
        a_bytes[33..133].copy_from_slice(&was[33..133]);
        assert_eq!(a_bytes, &was[..]);
        // As the landing does: its next use records anew.
        POOL.lock().unwrap().push(own);
        let own = host.own_list().unwrap();
        own.list.close();
    }

    /// Only with `VICTORY_CARD_STEPS_SRV=1` does `between` stay readable
    /// through a raw view, and either way as constants and a copy's source.
    #[test]
    fn the_buffer_between_is_a_shader_resource_only_when_asked() {
        let all = D3D12_RESOURCE_STATE_ALL_SHADER_RESOURCE.0;
        let constants = (D3D12_RESOURCE_STATE_COPY_SOURCE | D3D12_RESOURCE_STATE_VERTEX_AND_CONSTANT_BUFFER).0;
        assert_eq!(readable(false).0, constants);
        assert_eq!(readable(true).0, constants | all);
    }

    /// What our list puts into `between` is read, through a raw view of it
    /// made as `constant_at` makes a fill's twin, by a shader of a list that
    /// follows it in the same `ExecuteCommandLists`, as a title's list of the
    /// submission's first part does. `between` is in the state [`record`]
    /// left: with `VICTORY_CARD_STEPS_SRV=1` one that allows the read. (The
    /// bytes are checked either way; the layer's GPU-based validation runs
    /// without state tracking, so it does not tell the two apart.)
    #[test]
    fn a_raw_view_of_the_buffer_between_reads_what_our_list_put() {
        use windows::Win32::Graphics::Direct3D::{Fxc::D3DCompile, ID3DBlob};
        const AT: u64 = 512;
        const LEN: u64 = 256;
        const SOURCE: &str = "#define RS \"DescriptorTable(SRV(t0)), UAV(u0)\"\nByteAddressBuffer s : register(t0);\n\
            RWByteAddressBuffer o : register(u0);\n[RootSignature(RS)]\n[numthreads(64, 1, 1)]\n\
            void main(uint i : SV_DispatchThreadID) { o.Store(4 * i, s.Load(4 * i)); }\n";
        let Some(host) = host() else {
            return; // no host GPU
        };
        let (a, a_data) = host.shared_buffer(1 << 16).unwrap();
        let (out, out_data) = host.shared_buffer(LEN).unwrap();
        // SAFETY: mappings of 64 KiB and `LEN` bytes.
        let (a_bytes, out_bytes) =
            unsafe { (std::slice::from_raw_parts_mut(a_data, 1 << 16), std::slice::from_raw_parts_mut(out_data, LEN as usize)) };
        for (i, x) in a_bytes.iter_mut().enumerate() {
            *x = (i * 7 + i / 253) as u8;
        }
        out_bytes.fill(0xee);
        let own = host.own_list().unwrap();
        // A fill: what the title's list reads, from `a` into `between`.
        record(true, &own.list.list, &own.between, &[(&a, 4000, AT, LEN)], &[]);
        own.list.close();
        let mut code: Option<ID3DBlob> = None;
        // SAFETY: the source and names live through the call; then the
        // blob's own bytes, which hold the root signature too; a heap of one
        // slot, the view inside `between`.
        let (root, pso, heap): (ID3D12RootSignature, ID3D12PipelineState, ID3D12DescriptorHeap) = unsafe {
            let (name, entry, target) = (windows::core::s!("between"), windows::core::s!("main"), windows::core::s!("cs_5_1"));
            D3DCompile(SOURCE.as_ptr().cast(), SOURCE.len(), name, None, None, entry, target, 0, 0, &mut code, None).unwrap();
            let code = code.unwrap();
            let bytes = std::slice::from_raw_parts(code.GetBufferPointer().cast::<u8>(), code.GetBufferSize());
            let root: ID3D12RootSignature = host.device.CreateRootSignature(0, bytes).unwrap();
            let pso = host
                .device
                .CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                    pRootSignature: std::mem::transmute_copy(&root),
                    CS: D3D12_SHADER_BYTECODE { pShaderBytecode: bytes.as_ptr().cast(), BytecodeLength: bytes.len() },
                    ..Default::default()
                })
                .unwrap();
            let heap: ID3D12DescriptorHeap = host
                .device
                .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                    NumDescriptors: 1,
                    Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                    NodeMask: 0,
                })
                .unwrap();
            let raw = D3D12_SHADER_RESOURCE_VIEW_DESC {
                Format: DXGI_FORMAT_R32_TYPELESS,
                ViewDimension: D3D12_SRV_DIMENSION_BUFFER,
                Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                    Buffer: D3D12_BUFFER_SRV {
                        FirstElement: AT / 4,
                        NumElements: (LEN / 4) as u32,
                        StructureByteStride: 0,
                        Flags: D3D12_BUFFER_SRV_FLAG_RAW,
                    },
                },
            };
            host.device.CreateShaderResourceView(&own.between, Some(&raw), heap.GetCPUDescriptorHandleForHeapStart());
            (root, pso, heap)
        };
        // The title's list: no barrier of its own for `between`.
        let title = host.new_list().unwrap();
        // SAFETY: an open list; live objects, the view and `out` hold what
        // the dispatch reads and writes.
        unsafe {
            title.list.SetDescriptorHeaps(&[Some(heap.clone())]);
            title.list.SetComputeRootSignature(&root);
            title.list.SetPipelineState(&pso);
            title.list.SetComputeRootDescriptorTable(0, heap.GetGPUDescriptorHandleForHeapStart());
            title.list.SetComputeRootUnorderedAccessView(1, out.GetGPUVirtualAddress());
            title.list.Dispatch(1, 1, 1);
        }
        title.close();
        // One call, waited for.
        host.execute(&[&own.list, &title]);
        assert_eq!(host.log_messages(), None);
        assert_eq!(out_bytes, &a_bytes[4000..4000 + LEN as usize], "VICTORY_CARD_STEPS_SRV={}", srv());
        POOL.lock().unwrap().push(own);
    }

    /// The whole way, in a process of its own with the relay on:
    /// `VICTORY_RELAY=2 VICTORY_CARD_STEPS=2 cargo test -p victory-gpu --lib
    /// card_steps -- --ignored --test-threads=1`. A flight's list writes
    /// part of what the next submission's copy reads, the title wrote the
    /// rest; the copy is done on the card behind the flight, and a second
    /// one reads what the first wrote. Title memory ends as on the CPU.
    #[test]
    #[ignore = "wants VICTORY_RELAY=2 VICTORY_CARD_STEPS=2 and a process of its own"]
    fn a_copy_behind_a_flight_reads_what_the_flight_wrote() {
        use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc};
        const LEN: usize = 4 << 20;
        // Without it (`TEST_UNWATCHED=1`) the title's bytes cannot be put in
        // under a flight's writes: the flights that declared them are waited
        // for.
        let write_watch = if std::env::var_os("TEST_UNWATCHED").is_some() { 0 } else { 0x20_0000 };
        let Some(host) = host() else {
            return;
        };
        assert!(relay_exact() && stage() == 2, "set VICTORY_RELAY=2 VICTORY_CARD_STEPS=2");
        // Two title buffers: in the first a copy within it, from it into the
        // second, and back.
        let (title, start) = {
            // SAFETY: memory of our own, never freed.
            let base = unsafe { VirtualAlloc(std::ptr::null(), LEN, MEM_RESERVE | MEM_COMMIT | write_watch, PAGE_READWRITE) }.cast::<u8>();
            assert!(!base.is_null());
            // SAFETY: `LEN` bytes just committed.
            (unsafe { std::slice::from_raw_parts_mut(base, LEN) }, base as usize)
        };
        const HALF: usize = LEN / 2;
        crate::shadow::register(0x1111, start as u64, HALF as u64);
        crate::shadow::register(0x2222, (start + HALF) as u64, HALF as u64);
        let first = crate::shadow::find(start as u64, 64).unwrap();
        let (slow_from, slow_to) = (host.uav_buffer(64 << 20).unwrap(), host.uav_buffer(64 << 20).unwrap());
        let before = DONE[1].load(Ordering::Relaxed);
        let mut lists = Vec::new();
        for round in 0..8usize {
            // The title writes; then a "shader" of a flight writes 300 bytes
            // of what the copy reads, behind some work.
            let seed = |i: usize| (i * 7 + round * 13 + i / 251) as u8;
            for (i, b) in title[4000..6000].iter_mut().enumerate() {
                *b = seed(i);
            }
            let (upload, data) = host.upload_buffer(4096).unwrap();
            for i in 0..300 {
                // SAFETY: inside the mapping.
                unsafe { data.add(i).write(0x80 | (round * 31 + i) as u8) };
            }
            let a = host.new_list().unwrap();
            first.refresh(&a, start as u64, HALF as u64);
            first.gpu_writes(&a, start as u64, HALF as u64);
            // SAFETY: an open list; live buffers.
            unsafe {
                for _ in 0..24 {
                    a.list.CopyBufferRegion(&slow_to, 0, &slow_from, 0, 64 << 20);
                }
                a.list.CopyBufferRegion(&first.buffer, 4500, &upload, 0, 300);
                a.list
                    .ResourceBarrier(&[transition(&first.buffer, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_COMMON)]);
            }
            a.close();
            host.execute_own(&a);
            // With the flight in the air, beside what it writes.
            for (i, b) in title[5300..5400].iter_mut().enumerate() {
                *b = seed(1300 + i) ^ 0x5c;
            }
            // Thousands of runs of bytes of its own.
            for b in title[1_700_000..1_708_000].iter_mut().step_by(2) {
                *b = 1 + round as u8;
            }
            // Into the second buffer, and within the first.
            let b = host.new_list().unwrap();
            b.defer_copy((start + HALF + 100) as *mut u8, (start + 4001) as *const u8, 1500);
            b.defer_copy((start + 900_001) as *mut u8, (start + 4400) as *const u8, 777);
            b.defer_copy((start + HALF + 10_000) as *mut u8, (start + 1_700_000) as *const u8, 8000);
            // What depends on them: a copy of the first copy, upload memory
            // filled from that, and the first one's bytes copied over.
            b.defer_copy((start + 1_500_000) as *mut u8, (start + HALF + 600) as *const u8, 700);
            let (staging, staged) = b.upload(host, 4096).unwrap();
            b.defer_fill_buffer(&staging, staged, (start + 1_500_100) as *const u8, 500);
            b.defer_copy((start + HALF + 300) as *mut u8, (start + 8000) as *const u8, 64);
            b.close();
            host.execute_own(&b);
            // What the first wrote, read by the next submission.
            let c = host.new_list().unwrap();
            c.defer_copy((start + 1_200_000) as *mut u8, (start + HALF + 100) as *const u8, 1500);
            c.close();
            let number = host.execute_own(&c);
            settle_now(WHY_SYNC);
            assert!(FINISHED.load(Ordering::Acquire) >= number);
            let wanted: Vec<u8> = (0..2000)
                .map(|i| match i {
                    500..800 => 0x80 | (round * 31 + i - 500) as u8,
                    1300..1400 => seed(i) ^ 0x5c,
                    _ => seed(i),
                })
                .collect();
            assert!(title[4000..6000] == wanted[..], "what the flight wrote, round {round}");
            // (The copy over the first one's bytes came last.)
            let mut over = wanted[1..1501].to_vec();
            over[200..264].fill(0);
            assert!(title[HALF + 100..HALF + 1600] == over[..], "the copy to the other buffer, round {round}");
            assert!(title[1_500_000..1_500_700] == wanted[501..1201], "the copy behind a copy, round {round}");
            // SAFETY: the mapping, which the flight wrote and has landed.
            let staged = unsafe { std::slice::from_raw_parts(staged, 500) };
            assert!(staged == &wanted[601..1101], "the fill behind it, round {round}");
            assert!(title[900_001..900_778] == wanted[400..1177], "the copy within the buffer, round {round}");
            assert!(title[1_200_000..1_201_500] == over[..], "the copy of the copy, round {round}");
            assert!(title[HALF..HALF + 100].iter().all(|&x| x == 0));
            assert!(title[HALF + 1600..HALF + 10_000].iter().all(|&x| x == 0));
            assert!(title[HALF + 18_000..].iter().all(|&x| x == 0));
            let runs = &title[HALF + 10_000..HALF + 18_000];
            assert!(runs.chunks(2).all(|b| b == [1 + round as u8, 0]), "the runs, round {round}");
            a.keep(&upload);
            lists.extend([a, b, c]);
        }
        assert_eq!(host.log_messages(), None);
        let done = DONE[1].load(Ordering::Relaxed) - before;
        eprintln!("{done} of 48 copies were done on the card, {} submissions waited", DONE[3].load(Ordering::Relaxed));
        eprintln!("settles for title memory under shader writes: {}", LATE_STATS[WHY_STALE].load(Ordering::Relaxed));
        assert!(done >= 20, "only {done} copies were done on the card");
    }

    /// The guard the whole way, in a process of its own:
    /// `VICTORY_RELAY=2 VICTORY_CARD_STEPS=2 VICTORY_CARD_STEPS_GUARD=1 cargo
    /// test -p victory-gpu --lib card_steps -- --ignored --test-threads=1
    /// a_copy_at_its_place`. A flight writes what a fill of the next
    /// submission reads, so the fill can be done on the card; a copy at its
    /// place in the same list takes its source, which the flight writes too,
    /// into upload memory on the CPU. Refused, the submission waits and the
    /// copy finds what the flight wrote; without the guard it takes the
    /// bytes from before the flight. Then a copy at its place of memory no
    /// flight writes beside the same fill: the fill is still taken.
    #[test]
    #[ignore = "wants VICTORY_RELAY=2 VICTORY_CARD_STEPS=2 VICTORY_CARD_STEPS_GUARD=1 and a process of its own"]
    fn a_copy_at_its_place_behind_a_flight_waits_for_it() {
        use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc};
        const PART: usize = 1 << 20;
        let Some(host) = host() else {
            return;
        };
        assert!(relay_exact() && stage() == 2, "set VICTORY_RELAY=2 VICTORY_CARD_STEPS=2");
        // SAFETY: memory of our own, never freed.
        let base = unsafe { VirtualAlloc(std::ptr::null(), 3 * PART, MEM_RESERVE | MEM_COMMIT | 0x20_0000, PAGE_READWRITE) }.cast::<u8>();
        assert!(!base.is_null());
        // SAFETY: just committed.
        let title = unsafe { std::slice::from_raw_parts_mut(base, 3 * PART) };
        let start = base as usize;
        // What the flights write, where the copies go, what nothing writes.
        let (written, to, apart) = (start, start + PART, start + 2 * PART);
        for (i, at) in [written, to, apart].into_iter().enumerate() {
            crate::shadow::register(0x5550 + i, at as u64, PART as u64);
        }
        let shaders = crate::shadow::find(written as u64, 64).unwrap();
        assert!(crate::shadow::find(to as u64, 64).is_some() && crate::shadow::find(apart as u64, 64).is_some());
        let slow = (host.uav_buffer(64 << 20).unwrap(), host.uav_buffer(64 << 20).unwrap());
        // The flight: 1 KiB at +0x1000 of the first buffer, behind some work.
        let wrote = |round: u8, i: usize| (round ^ (i * 7) as u8) | 0x80;
        let flight = |round: u8| {
            let (upload, data) = host.upload_buffer(4096).unwrap();
            for i in 0..1024 {
                // SAFETY: inside the mapping.
                unsafe { data.add(i).write(wrote(round, i)) };
            }
            let a = host.new_list().unwrap();
            shaders.refresh(&a, written as u64, PART as u64);
            shaders.gpu_writes(&a, written as u64, PART as u64);
            // SAFETY: an open list; live buffers.
            unsafe {
                for _ in 0..24 {
                    a.list.CopyBufferRegion(&slow.1, 0, &slow.0, 0, 64 << 20);
                }
                a.list.CopyBufferRegion(&shaders.buffer, shaders.offset((written + 0x1000) as u64), &upload, 0, 1024);
                a.list
                    .ResourceBarrier(&[transition(&shaders.buffer, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_COMMON)]);
            }
            a.close();
            a.keep(&upload);
            a
        };
        for (i, b) in title[..0x2000].iter_mut().enumerate() {
            *b = 0x11 ^ i as u8;
        }
        for (i, b) in title[2 * PART..2 * PART + 0x100].iter_mut().enumerate() {
            *b = 0x22 ^ i as u8;
        }
        let (fills, waited, refused) =
            (DONE[0].load(Ordering::Relaxed), DONE[3].load(Ordering::Relaxed), GUARDED[2].load(Ordering::Relaxed));
        // Recorded while no shader has written the first buffer: the copy
        // takes its source on the CPU.
        let b = host.new_list().unwrap();
        let (staging, staged) = b.upload(host, 4096).unwrap();
        b.defer_fill_buffer(&staging, staged, (written + 0x1200) as *const u8, 512);
        assert!(b.place_copy(to + 0x100, written + 0x1000, 256), "a copy at its place");
        b.close();
        let a = flight(1);
        host.execute_own(&a);
        let number = host.execute_own(&b);
        settle_now(WHY_SYNC);
        assert!(FINISHED.load(Ordering::Acquire) >= number);
        // SAFETY: the mapping, which the submission's fill wrote.
        let staged = unsafe { std::slice::from_raw_parts(staged, 512) };
        assert!(staged.iter().enumerate().all(|(i, &x)| x == wrote(1, 0x200 + i)), "the fill");
        match guard() {
            Guard::Refuse => {
                assert_eq!(GUARDED[2].load(Ordering::Relaxed), refused + 1, "the guard refused it");
                assert_eq!(DONE[3].load(Ordering::Relaxed), waited + 1, "and the submission waited");
                assert_eq!(DONE[0].load(Ordering::Relaxed), fills, "the fill was done on the CPU");
            }
            _ => assert_eq!(DONE[0].load(Ordering::Relaxed), fills + 1, "the fill was done on the card"),
        }
        let copied = &title[PART + 0x100..PART + 0x200];
        assert!(
            copied.iter().enumerate().all(|(i, &x)| x == wrote(1, i)),
            "the copy at its place holds the bytes from before the flight (VICTORY_CARD_STEPS_GUARD=1 refuses this): {:x?}",
            &copied[..8]
        );
        // Beside a copy no flight is in the way of.
        let refused = GUARDED[2].load(Ordering::Relaxed);
        let (fills, waited) = (DONE[0].load(Ordering::Relaxed), DONE[3].load(Ordering::Relaxed));
        let c = host.new_list().unwrap();
        let (staging, staged) = c.upload(host, 4096).unwrap();
        c.defer_fill_buffer(&staging, staged, (written + 0x1200) as *const u8, 512);
        assert!(c.place_copy(to + 0x400, apart, 256), "a copy at its place");
        c.close();
        let a2 = flight(2);
        host.execute_own(&a2);
        let number = host.execute_own(&c);
        settle_now(WHY_SYNC);
        assert!(FINISHED.load(Ordering::Acquire) >= number);
        // SAFETY: as above.
        let staged = unsafe { std::slice::from_raw_parts(staged, 512) };
        assert!(staged.iter().enumerate().all(|(i, &x)| x == wrote(2, 0x200 + i)), "the fill beside it");
        assert_eq!(DONE[0].load(Ordering::Relaxed), fills + 1, "the fill beside it was done on the card");
        assert_eq!((DONE[3].load(Ordering::Relaxed), GUARDED[2].load(Ordering::Relaxed)), (waited, refused), "nothing refused");
        assert!(title[PART + 0x400..PART + 0x500].iter().enumerate().all(|(i, &x)| x == 0x22 ^ i as u8), "the copy beside it");
        assert_eq!(host.log_messages(), None);
    }
}
