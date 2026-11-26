//! Which title memory is a placeholder as the runtime that made it knows
//! (`VICTORY_PLACEHOLDER_RANGES=1`), for when Windows will not say.
//!
//! The guards keep a block that is a hole of a placeholder as "held"
//! (`crate::watch`), asking Windows once (`NtQueryVirtualMemory` class 7)
//! instead of at every asking. Wine answers that class with an error, so
//! there every hole is asked about again and again. Every placeholder of the
//! title's is made, filled, given back and released by `xmem` (and the
//! title's `VirtualFree`, which goes through it), which tells us here:
//!
//! - [`title_placeholder_reserved`] and [`title_placeholder_returned`] after
//!   the system made the range a placeholder (a reservation; a view unmapped
//!   with its placeholder kept);
//! - [`title_placeholder_leaving`] before the system makes it anything else
//!   (a view or an allocation in its place; a release).
//!
//! So what is here is a placeholder, or about to stop being one with the
//! runtime's notice of that still to come (which takes the guards' lock and
//! forgets what was held). A block is said to be one only if it is here,
//! Windows says it is reserved private memory in no view, all of that
//! region is here too, and nothing here changed for it meanwhile
//! ([`known`]). What is not here (made before the switch was read, by
//! another way, or left uncertain by a failed change) is asked about as
//! before.
//!
//! Off by default; `VICTORY_PLACEHOLDER_RANGES=0` counts only, for the other
//! side of an A/B. Read once a run: who would turn it off midway would leave
//! what it tracked untold.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};

const BLOCK: usize = 64 << 10;

/// 0 not read yet; 1 unset; 2 counted only; 3 on.
static MODE: AtomicU8 = AtomicU8::new(0);

fn mode() -> u8 {
    match MODE.load(Ordering::Acquire) {
        0 => {
            let read = match std::env::var_os("VICTORY_PLACEHOLDER_RANGES") {
                None => 1,
                Some(v) if v == "1" => 3,
                Some(_) => 2,
            };
            match MODE.compare_exchange(0, read, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => read,
                Err(now) => now,
            }
        }
        mode => mode,
    }
}

/// Whether the runtime's placeholders are tracked and the fallback asked.
pub fn placeholder_ranges() -> bool {
    mode() == 3
}

fn counted() -> bool {
    mode() >= 2
}

/// Tests: on from now. (Only ever off to on: a range made before is not
/// here, which is asked about as when off.)
#[doc(hidden)]
pub fn placeholder_ranges_for_tests() {
    MODE.store(3, Ordering::Release);
}

/// The placeholder blocks `[first, last)` by `first`: `last` and the number
/// they were told under. Ranges are kept as told, never joined: a block's
/// number changes only when it leaves and comes back.
struct Ranges {
    by_first: BTreeMap<usize, (usize, u64)>,
    told: u64,
}

impl Ranges {
    /// The range holding `block`.
    fn holding(&self, block: usize) -> Option<(usize, usize, u64)> {
        let (&first, &(last, told)) = self.by_first.range(..=block).next_back()?;
        (block < last).then_some((first, last, told))
    }

    /// Whether every block of `[first, last)` is here.
    fn covers(&self, mut first: usize, last: usize) -> bool {
        while first < last {
            match self.holding(first) {
                Some((_, end, _)) => first = end,
                None => return false,
            }
        }
        true
    }

    /// `[first, last)` is not here any more; what is beside it keeps its number.
    fn cut(&mut self, first: usize, last: usize) {
        if first >= last {
            return;
        }
        let mut hit: Vec<usize> = self.by_first.range(first..last).map(|(&f, _)| f).collect();
        if let Some((before, _, _)) = self.holding(first) {
            hit.push(before);
        }
        for from in hit {
            let Some((to, told)) = self.by_first.remove(&from) else {
                continue;
            };
            if from < first {
                self.by_first.insert(from, (first, told));
            }
            if to > last {
                self.by_first.insert(last, (to, told));
            }
        }
    }

    fn add(&mut self, first: usize, last: usize) {
        if first >= last {
            return;
        }
        self.cut(first, last);
        self.told += 1;
        self.by_first.insert(first, (last, self.told));
    }

    /// The run of ranges side by side that holds `block`, as blocks.
    fn run(&self, block: usize) -> Option<(usize, usize)> {
        let (mut first, mut last, _) = self.holding(block)?;
        while let Some((before, _, _)) = first.checked_sub(1).and_then(|b| self.holding(b)) {
            first = before;
        }
        while let Some((_, after, _)) = self.holding(last) {
            last = after;
        }
        Some((first, last))
    }
}

/// A leaf lock: nothing else is taken while it is held.
static RANGES: Mutex<Ranges> = Mutex::new(Ranges {
    by_first: BTreeMap::new(),
    told: 0,
});

fn ranges() -> std::sync::MutexGuard<'static, Ranges> {
    RANGES.lock().unwrap_or_else(|e| e.into_inner())
}

/// The system made `[at, at + len)` a placeholder (a reservation of one, or
/// a view unmapped with its placeholder kept). Its whole blocks are tracked.
pub fn title_placeholder_reserved(at: usize, len: usize) {
    if placeholder_ranges() && len != 0 {
        ranges().add(at.div_ceil(BLOCK), at.saturating_add(len) / BLOCK);
    }
}

/// See [`title_placeholder_reserved`]: a view in a placeholder unmapped, the
/// placeholder kept.
pub fn title_placeholder_returned(at: usize, len: usize) {
    title_placeholder_reserved(at, len);
}

/// The system is about to put something else in `[at, at + len)` (a view, an
/// allocation) or release it; with no length, the allocation at `at`, whose
/// end is not known here, so every range side by side with it goes. To be
/// told before the change: a block that is here after it would be taken for
/// a placeholder.
pub fn title_placeholder_leaving(at: usize, len: usize) {
    if !placeholder_ranges() {
        return;
    }
    let mut r = ranges();
    if len != 0 {
        r.cut(at / BLOCK, at.saturating_add(len).div_ceil(BLOCK));
    } else if let Some((first, last)) = r.run(at / BLOCK) {
        r.cut(first, last);
    }
}

/// The system changed some range of the title's that was not told (a call
/// whose arguments could not be read, which succeeded all the same): every
/// range goes, as though each had left. What is held of them is asked about
/// again ([`held_untracked`]); what is told from now on is tracked again.
pub fn title_placeholders_lost() {
    if placeholder_ranges() {
        ranges().by_first.clear();
    }
}

/// Windows has refused a class-7 question in this run (it always does under
/// Wine, never on Windows).
static REFUSES: AtomicBool = AtomicBool::new(false);

/// For the guards' look at what they hold: whether a held block is one the
/// switch may have said was a placeholder that the runtime tracks no longer.
/// Such a block is asked about again.
pub(crate) fn held_untracked(block: usize) -> bool {
    placeholder_ranges() && REFUSES.load(Ordering::Relaxed) && ranges().holding(block).is_none()
}

/// Since the last line: class-7 questions; of those, refused; then of the
/// refused, known by the ranges, and not for want of the range, for what
/// Windows says of the region, or for a change meanwhile; held blocks the
/// guards forgot as no longer tracked.
static COUNTS: [AtomicU64; 7] = [const { AtomicU64::new(0) }; 7];
const ASKED: usize = 0;
const REFUSED: usize = 1;
const KNOWN: usize = 2;
const UNTRACKED: usize = 3;
const REGION: usize = 4;
const CHANGED: usize = 5;
const DROPPED: usize = 6;

fn count(what: usize) {
    COUNTS[what].fetch_add(1, Ordering::Relaxed);
}

/// A class-7 question about a placeholder was put to Windows.
pub(crate) fn asked() {
    if counted() {
        count(ASKED);
    }
}

/// The guards forgot a held block that is tracked no longer.
pub(crate) fn dropped() {
    count(DROPPED);
}

/// Windows refused the class-7 question about `block`: whether it is a
/// placeholder all the same, by the ranges. With the guards' lock held, so
/// that what the runtime tells of a change after this (which takes it) comes
/// after what the caller notes.
pub(crate) fn known(block: usize) -> bool {
    if counted() {
        count(REFUSED);
    }
    if !placeholder_ranges() {
        return false;
    }
    REFUSES.store(true, Ordering::Relaxed);
    let Some((_, _, told)) = ranges().holding(block) else {
        count(UNTRACKED);
        return false;
    };
    // What is there now. A view (even one not committed), committed memory,
    // free address space, or a reservation that reaches past what is here:
    // not ours to say.
    let Some((first, last)) = reserved_private(block) else {
        count(REGION);
        return false;
    };
    #[cfg(test)]
    tests::BETWEEN.with(|between| between.take().map(|f| f(block)));
    // And nothing told of it since the look at the ranges: a change of what
    // is there is told before it is made.
    let r = ranges();
    if r.holding(block).is_none_or(|(_, _, now)| now != told) {
        count(CHANGED);
        return false;
    }
    if !r.covers(first, last) {
        count(REGION);
        return false;
    }
    count(KNOWN);
    true
}

/// Tests of other crates: whether `at`'s block is tracked now.
#[doc(hidden)]
pub fn title_placeholder_tracked(at: usize) -> bool {
    ranges().holding(at / BLOCK).is_some()
}

/// Tests of other crates: what [`known`] answers for `at`'s block (as when
/// Windows refuses the question).
#[doc(hidden)]
pub fn title_placeholder_known(at: usize) -> bool {
    known(at / BLOCK)
}

/// The blocks of the region at `block` if Windows says it is reserved,
/// private and no-access (what a placeholder is): first, past the last.
fn reserved_private(block: usize) -> Option<(usize, usize)> {
    use windows_sys::Win32::System::Memory::{
        MEM_PRIVATE, MEM_RESERVE, MEMORY_BASIC_INFORMATION, PAGE_NOACCESS, VirtualQuery,
    };
    // SAFETY: plain data filled in by the query.
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: a valid out pointer of the right size.
    let n = unsafe { VirtualQuery((block * BLOCK) as *const _, &mut info, size_of_val(&info)) };
    let base = info.BaseAddress as usize;
    (n != 0
        && info.State == MEM_RESERVE
        && info.Type == MEM_PRIVATE
        && info.AllocationProtect == PAGE_NOACCESS
        && info.RegionSize != 0)
        .then(|| {
            (
                base / BLOCK,
                base.saturating_add(info.RegionSize).div_ceil(BLOCK),
            )
        })
}

/// The line of the frame lines, per frame (only with the switch set, either
/// way). Counts only: no addresses.
pub(crate) fn line(frames: f64) {
    if !counted() {
        return;
    }
    let n: Vec<f64> = COUNTS
        .iter()
        .map(|c| c.swap(0, Ordering::Relaxed) as f64 / frames)
        .collect();
    let (held, ranges) = (crate::watch::held_now(), ranges().by_first.len());
    tracing::info!(
        "placeholder ranges (VICTORY_PLACEHOLDER_RANGES={}) per frame: class-7 questions {:.1}, refused {:.1}; of those known by \
         the ranges {:.1}, not tracked {:.1}, not so by the region {:.1}, changed meanwhile {:.2}; held blocks forgot as untracked \
         {:.2}; held now {held}, ranges tracked {ranges}",
        if placeholder_ranges() { 1 } else { 0 },
        n[ASKED],
        n[REFUSED],
        n[KNOWN],
        n[UNTRACKED],
        n[REGION],
        n[CHANGED],
        n[DROPPED],
    );
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    thread_local! {
        /// Run by [`known`] between its look at the ranges and its look
        /// again, on this thread, once.
        pub(crate) static BETWEEN: std::cell::Cell<Option<fn(usize)>> = const { std::cell::Cell::new(None) };
    }

    fn all(r: &Ranges) -> Vec<(usize, usize, u64)> {
        r.by_first.iter().map(|(&f, &(l, t))| (f, l, t)).collect()
    }

    #[test]
    fn ranges_cut_and_keep_their_numbers() {
        let mut r = Ranges {
            by_first: BTreeMap::new(),
            told: 0,
        };
        r.add(10, 20);
        r.add(20, 22);
        assert_eq!(all(&r), [(10, 20, 1), (20, 22, 2)]);
        assert!(r.covers(10, 22) && !r.covers(9, 12) && !r.covers(21, 23));
        assert_eq!(r.run(21), Some((10, 22)));
        // A part out of the middle: what is beside it keeps its number.
        r.cut(12, 14);
        assert_eq!(all(&r), [(10, 12, 1), (14, 20, 1), (20, 22, 2)]);
        assert_eq!(r.holding(13), None);
        assert_eq!(r.run(15), Some((14, 22)));
        // Back again: a number of its own.
        r.add(12, 14);
        assert_eq!(r.holding(13), Some((12, 14, 3)));
        // Over several, and past the end.
        r.cut(11, 30);
        assert_eq!(all(&r), [(10, 11, 1)]);
        // Added over what is there: that goes.
        r.add(5, 15);
        assert_eq!(all(&r), [(5, 15, 4)]);
        r.cut(0, 0);
        r.add(7, 7);
        assert_eq!(all(&r), [(5, 15, 4)]);
    }

    use windows_sys::Win32::System::Memory::{
        CreateFileMappingW, MEM_COMMIT, MEM_PRESERVE_PLACEHOLDER, MEM_RELEASE,
        MEM_REPLACE_PLACEHOLDER, MEM_RESERVE, MEM_RESERVE_PLACEHOLDER, MEMORY_MAPPED_VIEW_ADDRESS,
        MapViewOfFile3, PAGE_NOACCESS, PAGE_READWRITE, UnmapViewOfFile2, VirtualAlloc,
        VirtualAlloc2, VirtualFree,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    /// A placeholder of `blocks` blocks, each its own (`split`) or one whole;
    /// not tracked.
    fn untracked(blocks: usize, split: bool) -> usize {
        // SAFETY: a fresh placeholder of our own.
        unsafe {
            let at = VirtualAlloc2(
                GetCurrentProcess(),
                std::ptr::null(),
                blocks * BLOCK,
                MEM_RESERVE | MEM_RESERVE_PLACEHOLDER,
                PAGE_NOACCESS,
                std::ptr::null_mut(),
                0,
            ) as usize;
            assert_ne!(at, 0);
            for block in 0..blocks - 1 {
                if split {
                    assert_ne!(
                        VirtualFree(
                            (at + block * BLOCK) as *mut _,
                            BLOCK,
                            MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER
                        ),
                        0
                    );
                }
            }
            at
        }
    }

    /// [`untracked`], told as `xmem` tells of one.
    fn tracked(blocks: usize) -> usize {
        placeholder_ranges_for_tests();
        let at = untracked(blocks, true);
        title_placeholder_reserved(at, blocks * BLOCK);
        at
    }

    /// Released as the title's `VirtualFree` releases: told first. (By its
    /// length: with none, a range of another test's beside it would go too.)
    fn release(at: usize, blocks: usize) {
        title_placeholder_leaving(at, blocks * BLOCK);
        for block in 0..blocks {
            // SAFETY: our own pieces.
            unsafe { VirtualFree((at + block * BLOCK) as *mut _, 0, MEM_RELEASE) };
        }
    }

    fn section(blocks: usize) -> usize {
        // SAFETY: a pagefile section of our own, which stays.
        let section = unsafe {
            CreateFileMappingW(
                -1isize as _,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                (blocks * BLOCK) as u32,
                std::ptr::null(),
            )
        };
        assert!(!section.is_null());
        section as usize
    }

    /// A view of `section`'s first block in place of the placeholder at `at`.
    fn view_at(section: usize, at: usize) {
        // SAFETY: our placeholder and section.
        let view = unsafe {
            MapViewOfFile3(
                section as _,
                GetCurrentProcess(),
                at as *const _,
                0,
                BLOCK,
                MEM_REPLACE_PLACEHOLDER,
                PAGE_READWRITE,
                std::ptr::null_mut(),
                0,
            )
        };
        assert_eq!(view.Value as usize, at);
    }

    fn unview_at(at: usize) {
        let view = MEMORY_MAPPED_VIEW_ADDRESS {
            Value: at as *mut _,
        };
        // SAFETY: our view; it becomes a placeholder again.
        assert_ne!(
            unsafe { UnmapViewOfFile2(GetCurrentProcess(), view, MEM_PRESERVE_PLACEHOLDER) },
            0
        );
    }

    /// What the fallback relies on, as this system has it (Windows, Wine,
    /// Proton): a placeholder looks like an ordinary reservation to
    /// `VirtualQuery` (so only the ranges can tell the two apart), nothing
    /// commits in it, and a view in its place, committed memory and free
    /// address space look like something else.
    #[test]
    fn what_the_system_says_of_a_placeholder() {
        let at = untracked(3, true);
        let first = at / BLOCK;
        eprintln!("class 7 here: {:?}", crate::watch::class7(first));
        for block in first..first + 3 {
            let (from, to) =
                reserved_private(block).expect("a placeholder is reserved, private, no-access");
            assert!(
                from <= block && block < to && first <= from && to <= first + 3,
                "{from}..{to} for {block}"
            );
        }
        // SAFETY (all): our own memory.
        unsafe {
            assert!(
                VirtualAlloc(at as *const _, BLOCK, MEM_COMMIT, PAGE_READWRITE).is_null(),
                "a commit fills no placeholder"
            );
            let plain = VirtualAlloc(std::ptr::null(), BLOCK, MEM_RESERVE, PAGE_NOACCESS) as usize;
            assert!(
                reserved_private(plain / BLOCK).is_some(),
                "an ordinary reservation looks the same"
            );
            assert!(!VirtualAlloc(plain as *const _, BLOCK, MEM_COMMIT, PAGE_READWRITE).is_null());
            assert_eq!(reserved_private(plain / BLOCK), None, "committed");
            VirtualFree(plain as *mut _, 0, MEM_RELEASE);
            view_at(section(1), at + BLOCK);
            assert_eq!(reserved_private(first + 1), None, "a view");
            unview_at(at + BLOCK);
            assert!(reserved_private(first + 1).is_some());
            release(at, 3);
            assert_eq!(reserved_private(first), None, "free");
        }
    }

    /// A tracked placeholder's blocks are known; one that is not tracked, or
    /// that left, is not.
    #[test]
    fn tracked_holes_are_known_and_others_are_not() {
        let at = tracked(3);
        let known_at = |block: usize| known(at / BLOCK + block);
        assert!((0..3).all(known_at));
        let other = untracked(1, false);
        assert!(!known(other / BLOCK), "not tracked");
        title_placeholder_leaving(at + BLOCK, BLOCK);
        assert!(known_at(0) && !known_at(1) && known_at(2));
        let section = section(1);
        view_at(section, at + BLOCK);
        assert!(!known_at(1));
        unview_at(at + BLOCK);
        assert!(!known_at(1), "back, untold");
        title_placeholder_returned(at + BLOCK, BLOCK);
        assert!((0..3).all(known_at));
        release(at, 3);
        assert!(!(0..3).any(known_at));
        release(other, 1);
    }

    /// Something put in a tracked placeholder's place untold (a view, private
    /// memory) is no hole: Windows says what it is.
    #[test]
    fn what_was_put_there_untold_is_no_hole() {
        let at = tracked(2);
        view_at(section(1), at);
        assert!(
            title_placeholder_tracked(at) && !known(at / BLOCK),
            "a view"
        );
        unview_at(at);
        assert!(known(at / BLOCK), "a placeholder again");
        // SAFETY: our placeholder.
        let got = unsafe {
            VirtualAlloc2(
                GetCurrentProcess(),
                (at + BLOCK) as *const _,
                BLOCK,
                MEM_RESERVE | MEM_COMMIT | MEM_REPLACE_PLACEHOLDER,
                PAGE_READWRITE,
                std::ptr::null_mut(),
                0,
            )
        };
        assert_eq!(got as usize, at + BLOCK);
        assert!(!known(at / BLOCK + 1), "committed");
        release(at, 2);
    }

    /// A region Windows reports past what is tracked is not taken for one.
    #[test]
    fn a_region_past_the_ranges_is_no_hole() {
        placeholder_ranges_for_tests();
        let at = untracked(2, false);
        title_placeholder_reserved(at, BLOCK);
        assert!(title_placeholder_tracked(at) && !known(at / BLOCK));
        title_placeholder_reserved(at + BLOCK, BLOCK);
        assert!(known(at / BLOCK), "all of it, in two ranges");
        release(at, 1);
    }

    /// A change told between the look at the ranges and the look again (the
    /// runtime's, as the system changes the mapping) is no hole.
    #[test]
    fn a_change_between_the_looks_is_no_hole() {
        let at = tracked(1);
        let block = at / BLOCK;
        let between = |f: fn(usize)| BETWEEN.with(|b| b.set(Some(f)));
        between(|_| {});
        assert!(known(block));
        between(|block| title_placeholder_leaving(block * BLOCK, BLOCK));
        assert!(!known(block), "left");
        assert!(!known(block), "and not back");
        title_placeholder_returned(at, BLOCK);
        between(|block| {
            title_placeholder_leaving(block * BLOCK, BLOCK);
            title_placeholder_returned(block * BLOCK, BLOCK);
        });
        assert!(!known(block), "left and came back meanwhile");
        assert!(known(block));
        release(at, 1);
    }

    /// An address released (told) and reserved again by another as ordinary
    /// memory is no hole; a placeholder made there again is, as a new one.
    #[test]
    fn a_released_address_used_again_is_no_hole() {
        let at = tracked(1);
        assert!(known(at / BLOCK));
        release(at, 1);
        // SAFETY (all): the address just given up, ours again if we get it.
        unsafe {
            let again = VirtualAlloc(at as *const _, BLOCK, MEM_RESERVE, PAGE_NOACCESS) as usize;
            if again != at {
                return; // another thread took it
            }
            assert!(!known(at / BLOCK), "ordinary");
            VirtualFree(at as *mut _, 0, MEM_RELEASE);
            let placeholder = VirtualAlloc2(
                GetCurrentProcess(),
                at as *const _,
                BLOCK,
                MEM_RESERVE | MEM_RESERVE_PLACEHOLDER,
                PAGE_NOACCESS,
                std::ptr::null_mut(),
                0,
            ) as usize;
            if placeholder != at {
                return;
            }
            assert!(!known(at / BLOCK), "not told yet");
            title_placeholder_reserved(at, BLOCK);
            assert!(known(at / BLOCK));
        }
        release(at, 1);
    }
}
