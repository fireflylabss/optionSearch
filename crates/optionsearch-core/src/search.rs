//! Parallel matcher, ranking and top-K selection.

use std::time::Instant;

use rayon::prelude::*;

use crate::index::Index;
use crate::model::{Hit, SearchResults, flags};
use crate::query::{Kind, Query, Term};

const CHUNK: usize = 16 * 1024;

/// Ranking tiers, best first.
const S_EXACT: u32 = 100;
const S_PREFIX: u32 = 80;
const S_WORD: u32 = 60;
const S_SUB: u32 = 40;
const S_PATH: u32 = 20;

/// Cap on how many matching ids are remembered for incremental refinement.
///
/// Kept modest on purpose: collecting ids costs the scan real time, and a full
/// rescan is already fast enough that remembering a huge result set would cost
/// more than it saves.
pub const COLLECT_CAP: usize = 150_000;

pub fn search(index: &Index, q: &Query, limit: usize) -> SearchResults {
    run(index, q, limit, None, false).0
}

/// Same as [`search`], but also returns every matching id (in ascending order)
/// when the match count stays below [`COLLECT_CAP`], so a follow-up query that
/// only narrows the result can be answered with [`search_within`].
pub fn search_collect(index: &Index, q: &Query, limit: usize) -> (SearchResults, Option<Vec<u32>>) {
    run(index, q, limit, None, true)
}

pub fn search_within(
    index: &Index,
    q: &Query,
    subset: &[u32],
    limit: usize,
) -> (SearchResults, Option<Vec<u32>>) {
    run(index, q, limit, Some(subset), true)
}

/// Precomputed per-directory answers for one path term.
///
/// A match of a term containing `/` either lies entirely inside the directory
/// path (`dir_hit`) or straddles the last separator, in which case the directory
/// path must end with everything up to and including the term's last `/`
/// (`cross`) and the name must start with `rest`.
struct PathPlan {
    dir_hit: Vec<bool>,
    cross: Vec<bool>,
    rest: Vec<u8>,
    /// Folded first byte of `rest`, or 0 when there is nothing to check. Lets
    /// the scan reject a crossing candidate from the first-byte column.
    rest_first: u8,
    ascii: bool,
    /// Share of directories where the name has to be inspected. Above a
    /// threshold it pays to prefetch names for the whole scan.
    cross_ratio: f32,
}

impl PathPlan {
    fn build(index: &Index, term: &Term) -> PathPlan {
        let paths = index.dir_paths();
        let cut = term.lower.iter().rposition(|&b| b == b'/').unwrap_or(0) + 1;
        let prefix = &term.lower[..cut];
        let rest = term.lower[cut..].to_vec();
        let n = paths.len();
        let mut dir_hit = vec![false; n];
        let mut cross = vec![false; n];
        dir_hit
            .par_iter_mut()
            .zip(cross.par_iter_mut())
            .enumerate()
            .for_each(|(d, (hit, cross))| {
                let p = paths.get(d as u32);
                *hit = contains_bytes(p, &term.lower, term.ascii);
                *cross = !rest.is_empty() && ends_with_ci(p, prefix, term.ascii);
            });
        let crossing = cross
            .iter()
            .zip(dir_hit.iter())
            .filter(|(c, h)| **c && !**h)
            .count();
        let rest_first = match rest.first() {
            Some(&b) if term.ascii => b | 0x20,
            _ => 0,
        };
        PathPlan {
            rest_first,
            cross_ratio: crossing as f32 / n.max(1) as f32,
            dir_hit,
            cross,
            rest,
            ascii: term.ascii,
        }
    }
}

struct Ctx<'a> {
    index: &'a Index,
    q: &'a Query,
    name_need: u64,
    name_need2: u32,
    name_terms: Vec<&'a Term>,
    plans: Vec<PathPlan>,
    materialize_path: bool,
    has_unicode: bool,
    /// Whether any clause actually needs the entry's name.
    needs_name: bool,
    /// Number of terms, used to average the per-term scores.
    nterms: u32,
    /// Set for a lone single-letter term, whose presence the signature already
    /// proves; see [`single_byte`].
    single: Option<u8>,
}

#[derive(Default)]
struct Scratch {
    path: Vec<u8>,
    lower: String,
}

impl<'a> Ctx<'a> {
    /// Returns the rank score when the entry matches every clause.
    #[inline]
    fn eval(&self, i: usize, s: &mut Scratch) -> Option<u32> {
        let idx = self.index;
        if idx.eflags[i] & flags::DEAD != 0 {
            return None;
        }
        match self.q.kind {
            Some(Kind::Dir) if idx.eflags[i] & flags::DIR == 0 => return None,
            Some(Kind::File) if idx.eflags[i] & flags::DIR != 0 => return None,
            _ => {}
        }
        if let Some(f) = self.q.size {
            if !f.matches(idx.size[i]) {
                return None;
            }
        }

        if let Some(b) = self.single {
            return Some(if idx.first[i] == b {
                if idx.len[i] == 1 { S_EXACT } else { S_PREFIX }
            } else {
                S_SUB
            });
        }

        let mut path_score = 0u32;
        let mut check_rest = false;
        if !self.plans.is_empty() {
            let d = idx.edir[i] as usize;
            for plan in &self.plans {
                if plan.dir_hit[d] {
                    path_score += S_PATH;
                } else if plan.cross[d] {
                    if plan.rest_first != 0 && idx.first[i] != plan.rest_first {
                        return None;
                    }
                    path_score += S_WORD;
                    check_rest = true;
                } else {
                    return None;
                }
            }
        }

        // Reading the name is a trip into the 88 MiB arena; queries made only of
        // column filters (`dir:`, `size:`, a path term satisfied by the
        // directory alone) must never pay for it.
        if !self.needs_name && !check_rest {
            return Some(self.average(path_score));
        }
        let name = idx.names_arena().get(idx.off[i], idx.len[i]);

        if !self.q.exts.is_empty() && !ext_matches(name, &self.q.exts) {
            return None;
        }
        if check_rest {
            let d = idx.edir[i] as usize;
            for plan in &self.plans {
                if !plan.dir_hit[d] && !starts_with_ci(name, &plan.rest, plan.ascii) {
                    return None;
                }
            }
        }

        if self.materialize_path {
            idx.path_into(i as u32, &mut s.path);
        }
        if self.has_unicode {
            s.lower.clear();
            for c in String::from_utf8_lossy(name).chars() {
                s.lower.extend(c.to_lowercase());
            }
        }

        let mut total = path_score;
        for t in &self.name_terms {
            total += if t.ascii {
                score_term(name, t)?
            } else {
                score_term(s.lower.as_bytes(), t)?
            };
        }

        if let Some(re) = &self.q.regex {
            let hay: &[u8] = if self.q.regex_on_path { &s.path } else { name };
            if !re.is_match(hay) {
                return None;
            }
            if self.q.terms.is_empty() {
                total = S_SUB;
            }
        }

        Some(self.average(total))
    }

    /// Integer division is expensive enough to matter at millions of candidates
    /// per query, and a single term is by far the common case.
    #[inline]
    fn average(&self, total: u32) -> u32 {
        if self.nterms == 1 {
            total
        } else {
            total / self.nterms
        }
    }

    #[inline]
    fn sort_key(&self, i: usize, score: u32) -> u64 {
        let idx = self.index;
        let depth = idx.dir_node(idx.edir[i]).depth as u32;
        let plen = (depth * 16 + idx.len[i] as u32).min(0xFFFF);
        let mtime = idx.mtime[i].clamp(0, u32::MAX as i64) as u64;
        ((score as u64) << 48) | ((0xFFFF - plen as u64) << 32) | mtime
    }
}

fn run(
    index: &Index,
    q: &Query,
    limit: usize,
    subset: Option<&[u32]>,
    collect: bool,
) -> (SearchResults, Option<Vec<u32>>) {
    let started = Instant::now();
    let scanned = subset.map_or(index.live_len(), |s| s.len());
    if q.is_empty() || q.error.is_some() {
        return (
            SearchResults {
                scanned: 0,
                elapsed: started.elapsed(),
                ..Default::default()
            },
            None,
        );
    }

    let (name_need, name_need2) = q.name_sig();
    let ctx = Ctx {
        index,
        q,
        name_need,
        name_need2,
        name_terms: q.terms.iter().filter(|t| !t.on_path).collect(),
        plans: q
            .terms
            .iter()
            .filter(|t| t.on_path)
            .map(|t| PathPlan::build(index, t))
            .collect(),
        materialize_path: q.regex_on_path,
        needs_name: !q.exts.is_empty() || q.regex.is_some() || q.terms.iter().any(|t| !t.on_path),
        has_unicode: q.terms.iter().any(|t| !t.ascii && !t.on_path),
        nterms: q.terms.len().max(1) as u32,
        single: single_byte(q),
    };

    let make = || Part::new(limit);
    let merged = match subset {
        Some(sub) => sub
            .par_chunks(CHUNK)
            .fold(make, |acc, c| {
                fold_ids(&ctx, collect, acc, c.iter().copied())
            })
            .reduce(make, Part::merge),
        None => {
            let n = index.len() as u32;
            (0..n)
                .into_par_iter()
                .step_by(CHUNK)
                .fold(make, |acc, base| {
                    let end = (base + CHUNK as u32).min(n);
                    fold_ids(&ctx, collect, acc, base..end)
                })
                .reduce(make, Part::merge)
        }
    };

    let (keys, total, ids, overflow) = (merged.top, merged.total, merged.ids, merged.overflow);
    let ranked = keys.into_sorted();
    let mut buf = Vec::with_capacity(160);
    let hits = ranked
        .iter()
        .map(|&(key, id)| {
            index.path_into(id, &mut buf);
            let name = index.name(id);
            Hit {
                id,
                path: std::path::PathBuf::from(
                    <std::ffi::OsString as std::os::unix::ffi::OsStringExt>::from_vec(buf.clone()),
                ),
                name: String::from_utf8_lossy(name).into_owned(),
                size: index.size[id as usize],
                mtime: index.mtime[id as usize],
                is_dir: index.eflags[id as usize] & flags::DIR != 0,
                score: (key >> 48) as u32,
            }
        })
        .collect();

    let results = SearchResults {
        hits,
        total,
        truncated: total > limit,
        scanned,
        elapsed: started.elapsed(),
    };
    let ids = if collect && !overflow {
        let mut ids = ids;
        ids.sort_unstable();
        Some(ids)
    } else {
        None
    };
    (results, ids)
}

/// A lone `a`–`z` or `.` term whose signature bit is unique, which means the
/// prefilter already proves containment: the scan can rank straight from the
/// first-byte column and never read a single name. The first keystroke of a
/// search is exactly this case, and it is also the one that matches millions of
/// entries. The only cost is that word-start matches rank as plain substring
/// matches for one-character queries.
fn single_byte(q: &Query) -> Option<u8> {
    if q.terms.len() != 1 || !q.exts.is_empty() || q.regex.is_some() {
        return None;
    }
    let t = &q.terms[0];
    if !t.ascii || t.on_path || t.lower.len() != 1 {
        return None;
    }
    match t.lower[0] {
        b @ (b'a'..=b'z' | b'.') => Some(b | 0x20),
        _ => None,
    }
}

/// How many candidates are prefetched before being evaluated.
const BATCH: usize = 24;

fn fold_ids<I: Iterator<Item = u32>>(ctx: &Ctx, collect: bool, mut acc: Part, ids: I) -> Part {
    let mut s = std::mem::take(&mut acc.scratch);
    let sigs = &ctx.index.sig;
    let sigs2 = &ctx.index.sig2;
    let need = ctx.name_need;
    let need2 = ctx.name_need2;
    let mut batch = [0u32; BATCH];
    let mut n = 0usize;
    let prefetch =
        ctx.single.is_none() && (ctx.needs_name || ctx.plans.iter().any(|p| p.cross_ratio > 0.25));
    for id in ids {
        if sigs[id as usize] & need != need {
            continue;
        }
        if need2 != 0 && sigs2[id as usize] & need2 != need2 {
            continue;
        }
        if prefetch {
            ctx.index.prefetch_name(id);
        }
        batch[n] = id;
        n += 1;
        if n == BATCH {
            eval_batch(ctx, collect, &mut acc, &batch, &mut s);
            n = 0;
        }
    }
    eval_batch(ctx, collect, &mut acc, &batch[..n], &mut s);
    acc.scratch = s;
    acc
}

#[inline]
fn eval_batch(ctx: &Ctx, collect: bool, acc: &mut Part, batch: &[u32], s: &mut Scratch) {
    for &id in batch {
        let i = id as usize;
        if let Some(score) = ctx.eval(i, s) {
            acc.total += 1;
            // Building the sort key reads mtime and the directory node; skip it
            // for entries that cannot beat the worst kept hit.
            if acc.top.accepts(score) {
                acc.top.push(ctx.sort_key(i, score), id);
            }
            if collect {
                if acc.ids.len() < COLLECT_CAP {
                    acc.ids.push(id);
                } else {
                    acc.overflow = true;
                }
            }
        }
    }
}

struct Part {
    top: TopK,
    total: usize,
    ids: Vec<u32>,
    overflow: bool,
    scratch: Scratch,
}

impl Part {
    fn new(limit: usize) -> Part {
        Part {
            top: TopK::new(limit),
            total: 0,
            ids: Vec::new(),
            overflow: false,
            scratch: Scratch::default(),
        }
    }

    fn merge(mut a: Part, b: Part) -> Part {
        a.top.merge(b.top);
        a.total += b.total;
        a.overflow |= b.overflow;
        if a.overflow || a.ids.len() + b.ids.len() > COLLECT_CAP {
            a.overflow = true;
            a.ids = Vec::new();
        } else {
            a.ids.extend_from_slice(&b.ids);
        }
        a
    }
}

/// Bounded max-selection over `(key, id)` using a min-heap of the current best.
struct TopK {
    limit: usize,
    heap: std::collections::BinaryHeap<std::cmp::Reverse<(u64, u32)>>,
}

impl TopK {
    fn new(limit: usize) -> TopK {
        TopK {
            limit: limit.max(1),
            heap: std::collections::BinaryHeap::with_capacity(limit.max(1) + 1),
        }
    }

    /// Cheap pre-check: can an entry with this score still make the cut?
    #[inline]
    fn accepts(&self, score: u32) -> bool {
        if self.heap.len() < self.limit {
            return true;
        }
        match self.heap.peek() {
            Some(&std::cmp::Reverse((worst, _))) => score as u64 >= worst >> 48,
            None => true,
        }
    }

    #[inline]
    fn push(&mut self, key: u64, id: u32) {
        if self.heap.len() < self.limit {
            self.heap.push(std::cmp::Reverse((key, id)));
        } else if let Some(&std::cmp::Reverse((worst, _))) = self.heap.peek() {
            if key > worst {
                self.heap.pop();
                self.heap.push(std::cmp::Reverse((key, id)));
            }
        }
    }

    fn merge(&mut self, other: TopK) {
        for std::cmp::Reverse((k, id)) in other.heap {
            self.push(k, id);
        }
    }

    fn into_sorted(self) -> Vec<(u64, u32)> {
        let mut v: Vec<(u64, u32)> = self.heap.into_iter().map(|r| r.0).collect();
        v.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        v
    }
}

#[inline]
fn ext_matches(name: &[u8], exts: &[String]) -> bool {
    let Some(dot) = memchr::memrchr(b'.', name) else {
        return false;
    };
    let ext = &name[dot + 1..];
    exts.iter()
        .any(|e| e.len() == ext.len() && e.as_bytes().eq_ignore_ascii_case(ext))
}

/// Case-insensitive substring position, without allocating for ASCII needles.
#[inline]
fn find_ci(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    if needle.len() > hay.len() {
        return None;
    }
    let lo = needle[0];
    let up = lo.to_ascii_uppercase();
    let tail = &needle[1..];
    let last = hay.len() - needle.len();
    // File names are short; a scalar loop beats memchr's SIMD setup cost here,
    // and this is the hottest path in the whole engine.
    if hay.len() <= 64 {
        for at in 0..=last {
            let c = hay[at];
            if (c == lo || c == up) && hay[at + 1..at + needle.len()].eq_ignore_ascii_case(tail) {
                return Some(at);
            }
        }
        return None;
    }
    let mut start = 0usize;
    while start <= last {
        let window = &hay[start..=last];
        let found = if lo == up {
            memchr::memchr(lo, window)
        } else {
            memchr::memchr2(lo, up, window)
        }?;
        let at = start + found;
        if hay[at + 1..at + needle.len()].eq_ignore_ascii_case(tail) {
            return Some(at);
        }
        start = at + 1;
    }
    None
}

#[inline]
fn contains_bytes(hay: &[u8], needle: &[u8], ascii: bool) -> bool {
    if ascii {
        find_ci(hay, needle).is_some()
    } else {
        // Unicode needle: compare against a lowercased copy of the haystack.
        let lower = String::from_utf8_lossy(hay).to_lowercase();
        find_sub(lower.as_bytes(), needle).is_some()
    }
}

#[inline]
fn ends_with_ci(hay: &[u8], suffix: &[u8], ascii: bool) -> bool {
    if hay.len() < suffix.len() {
        return false;
    }
    let tail = &hay[hay.len() - suffix.len()..];
    if ascii {
        tail.eq_ignore_ascii_case(suffix)
    } else {
        String::from_utf8_lossy(tail).to_lowercase().as_bytes() == suffix
    }
}

#[inline]
fn starts_with_ci(hay: &[u8], prefix: &[u8], ascii: bool) -> bool {
    if hay.len() < prefix.len() {
        return false;
    }
    let head = &hay[..prefix.len()];
    if ascii {
        head.eq_ignore_ascii_case(prefix)
    } else {
        String::from_utf8_lossy(hay)
            .to_lowercase()
            .as_bytes()
            .starts_with(prefix)
    }
}

#[inline]
fn is_boundary(b: u8) -> bool {
    matches!(b, b'_' | b'-' | b'.' | b' ' | b'/' | b'@' | b'+' | b',')
}

/// Rank tier for a single term against a name.
#[inline]
fn score_term(name: &[u8], t: &Term) -> Option<u32> {
    let pos = if t.ascii {
        find_ci(name, &t.lower)?
    } else {
        find_sub(name, &t.lower)?
    };
    if pos == 0 {
        if name.len() == t.lower.len() {
            return Some(S_EXACT);
        }
        // Also treat "name matches everything before the extension" as exact.
        if let Some(dot) = memchr::memrchr(b'.', name) {
            if dot == t.lower.len() {
                return Some(S_EXACT - 5);
            }
        }
        return Some(S_PREFIX);
    }
    if is_boundary(name[pos - 1]) {
        return Some(S_WORD);
    }
    Some(S_SUB)
}

#[inline]
fn find_sub(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn build(files: &[(&str, u64, i64, bool)]) -> Index {
        let mut idx = Index::new();
        for &(p, size, mtime, is_dir) in files {
            let path = Path::new(p);
            let dir = idx.ensure_dir_path(path.parent().unwrap());
            let name = path.file_name().unwrap().to_str().unwrap();
            let f = if is_dir { flags::DIR } else { 0 };
            idx.push_entry(dir, name.as_bytes(), f, size, mtime);
        }
        idx
    }

    fn names(index: &Index, q: &str, limit: usize) -> Vec<String> {
        search(index, &Query::parse(q), limit)
            .hits
            .into_iter()
            .map(|h| h.path.to_string_lossy().into_owned())
            .collect()
    }

    fn sample() -> Index {
        build(&[
            ("/home/u/report.pdf", 2_000_000, 100, false),
            ("/home/u/deep/nested/report_final.pdf", 10, 200, false),
            ("/home/u/my_report.txt", 10, 300, false),
            ("/home/u/preport.txt", 10, 400, false),
            ("/home/u/pics/IMG_0042.JPG", 5_000_000, 500, false),
            ("/home/u/reports", 4096, 600, true),
        ])
    }

    #[test]
    fn ranks_exact_then_prefix_then_word_then_substring() {
        let idx = sample();
        let got = names(&idx, "report", 10);
        assert_eq!(
            got,
            vec![
                "/home/u/report.pdf",
                "/home/u/reports",
                "/home/u/deep/nested/report_final.pdf",
                "/home/u/my_report.txt",
                "/home/u/preport.txt",
            ]
        );
    }

    #[test]
    fn single_letter_queries_rank_exact_then_prefix() {
        let idx = build(&[
            ("/a/x", 1, 1, false),
            ("/a/xylophone", 1, 1, false),
            ("/a/box", 1, 1, false),
            ("/a/none", 1, 1, false),
        ]);
        assert_eq!(names(&idx, "x", 10), vec!["/a/x", "/a/xylophone", "/a/box"]);
        assert_eq!(search(&idx, &Query::parse("x"), 10).total, 3);
    }

    #[test]
    fn case_insensitive_matching() {
        let idx = sample();
        assert_eq!(
            names(&idx, "img_0042", 10),
            vec!["/home/u/pics/IMG_0042.JPG"]
        );
        assert_eq!(names(&idx, "REPORTS", 10), vec!["/home/u/reports"]);
    }

    #[test]
    fn terms_are_anded() {
        let idx = sample();
        assert_eq!(names(&idx, "report final", 10).len(), 1);
        assert!(names(&idx, "report nothing", 10).is_empty());
    }

    #[test]
    fn ext_size_and_kind_filters() {
        let idx = sample();
        assert_eq!(names(&idx, "ext:pdf report", 10).len(), 2);
        assert_eq!(
            names(&idx, "report size:>1mb", 10),
            vec!["/home/u/report.pdf"]
        );
        assert_eq!(names(&idx, "dir: report", 10), vec!["/home/u/reports"]);
        assert_eq!(
            names(&idx, "file: ext:jpg", 10),
            vec!["/home/u/pics/IMG_0042.JPG"]
        );
    }

    #[test]
    fn path_terms_match_full_path() {
        let idx = sample();
        assert_eq!(
            names(&idx, "nested/report", 10),
            vec!["/home/u/deep/nested/report_final.pdf"]
        );
        assert!(names(&idx, "u/pics/", 10).contains(&"/home/u/pics/IMG_0042.JPG".to_string()));
    }

    #[test]
    fn regex_terms() {
        let idx = sample();
        assert_eq!(
            names(&idx, r"re:^img_\d+", 10),
            vec!["/home/u/pics/IMG_0042.JPG"]
        );
    }

    #[test]
    fn quoted_phrase_with_space() {
        let idx = build(&[
            ("/a/my file.txt", 1, 1, false),
            ("/a/myfile.txt", 1, 1, false),
        ]);
        assert_eq!(names(&idx, "\"my file\"", 10), vec!["/a/my file.txt"]);
    }

    #[test]
    fn shorter_path_wins_then_newer_mtime() {
        let idx = build(&[
            ("/a/x/data.bin", 1, 10, false),
            ("/a/data.bin", 1, 5, false),
            ("/b/data.bin", 1, 50, false),
        ]);
        let got = names(&idx, "data.bin", 10);
        assert_eq!(got[0], "/b/data.bin");
        assert_eq!(got[1], "/a/data.bin");
        assert_eq!(got[2], "/a/x/data.bin");
    }

    #[test]
    fn truncation_reports_total() {
        let idx = sample();
        let r = search(&idx, &Query::parse("report"), 2);
        assert_eq!(r.hits.len(), 2);
        assert_eq!(r.total, 5);
        assert!(r.truncated);
    }

    #[test]
    fn empty_query_returns_nothing() {
        let idx = sample();
        let r = search(&idx, &Query::parse(""), 10);
        assert!(r.hits.is_empty() && r.total == 0);
    }

    #[test]
    fn tombstoned_entries_are_skipped() {
        let mut idx = sample();
        idx.remove_entry(0);
        assert!(!names(&idx, "report", 10).contains(&"/home/u/report.pdf".to_string()));
    }

    #[test]
    fn refinement_over_subset_matches_full_scan() {
        let idx = sample();
        let (_, ids) = search_collect(&idx, &Query::parse("report"), 100);
        let ids = ids.unwrap();
        let (a, _) = search_within(&idx, &Query::parse("report_f"), &ids, 100);
        let b = search(&idx, &Query::parse("report_f"), 100);
        assert_eq!(a.hits.len(), b.hits.len());
        assert_eq!(a.hits[0].path, b.hits[0].path);
    }

    #[test]
    fn unicode_terms_match_case_insensitively() {
        let idx = build(&[("/a/Ação.txt", 1, 1, false), ("/a/acao.txt", 1, 1, false)]);
        assert_eq!(names(&idx, "ação", 10), vec!["/a/Ação.txt"]);
    }

    #[test]
    fn find_ci_edge_cases() {
        assert_eq!(find_ci(b"HelloWorld", b"world"), Some(5));
        assert_eq!(find_ci(b"aaab", b"aab"), Some(1));
        assert_eq!(find_ci(b"abc", b"abcd"), None);
        assert_eq!(find_ci(b"abc", b""), Some(0));
    }
}
