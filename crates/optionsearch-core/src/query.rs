//! Query language parser.
//!
//! Whitespace-separated tokens are ANDed. A token containing `/` is matched
//! against the full path instead of just the name.
//!
//! * `ext:pdf` / `ext:jpg,png` — extension is one of
//! * `size:>10mb`, `size:<1k`, `size:>=4096` — byte size (k/m/g/t are 1024-based)
//! * `dir:` / `file:` — only directories / only files (an optional value is
//!   also used as a name term: `dir:src`)
//! * `"exact phrase"` — literal, spaces included
//! * `re:^IMG_\d+` — regex over the name (or the path when it contains `/`)

use regex::bytes::{Regex, RegexBuilder};

use crate::model::{signature, signature2};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cmp {
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SizeFilter {
    pub op: Cmp,
    pub bytes: u64,
}

impl SizeFilter {
    #[inline]
    pub fn matches(&self, size: u64) -> bool {
        match self.op {
            Cmp::Lt => size < self.bytes,
            Cmp::Le => size <= self.bytes,
            Cmp::Gt => size > self.bytes,
            Cmp::Ge => size >= self.bytes,
            Cmp::Eq => size == self.bytes,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Dir,
    File,
}

#[derive(Debug, Clone)]
pub struct Term {
    /// Lowercased needle. ASCII terms are matched byte-wise without allocating.
    pub lower: Vec<u8>,
    pub sig: u64,
    pub sig2: u32,
    pub on_path: bool,
    pub ascii: bool,
}

impl Term {
    fn new(text: &str) -> Option<Term> {
        if text.is_empty() {
            return None;
        }
        let ascii = text.is_ascii();
        let lower = if ascii {
            text.to_ascii_lowercase().into_bytes()
        } else {
            text.to_lowercase().into_bytes()
        };
        Some(Term {
            sig: signature(&lower),
            sig2: signature2(&lower),
            on_path: text.contains('/'),
            ascii,
            lower,
        })
    }
}

#[derive(Debug, Default)]
pub struct Query {
    pub raw: String,
    pub terms: Vec<Term>,
    pub exts: Vec<String>,
    pub size: Option<SizeFilter>,
    pub kind: Option<Kind>,
    pub regex: Option<Regex>,
    pub regex_on_path: bool,
    pub error: Option<String>,
    /// Bits any string matching the regex must contain (0 = no filter).
    regex_sig: u64,
    regex_sig2: u32,
}

impl Query {
    pub fn parse(input: &str) -> Query {
        let mut q = Query {
            raw: input.to_string(),
            ..Default::default()
        };
        for (tok, quoted) in tokenize(input) {
            if quoted {
                q.terms.extend(Term::new(&tok));
                continue;
            }
            match split_prefix(&tok) {
                Some(("ext", val)) => {
                    for e in val.split(',').filter(|e| !e.is_empty()) {
                        q.exts.push(e.trim_start_matches('.').to_ascii_lowercase());
                    }
                }
                Some(("size", val)) => match parse_size(val) {
                    Ok(f) => q.size = Some(f),
                    Err(e) => q.error = Some(e),
                },
                Some(("dir", val)) => {
                    q.kind = Some(Kind::Dir);
                    q.terms.extend(Term::new(val));
                }
                Some(("file", val)) => {
                    q.kind = Some(Kind::File);
                    q.terms.extend(Term::new(val));
                }
                Some(("re", val)) => {
                    q.regex_on_path = val.contains('/');
                    match RegexBuilder::new(val).case_insensitive(true).build() {
                        Ok(re) => {
                            (q.regex_sig, q.regex_sig2) = regex_signature(val);
                            q.regex = Some(re);
                        }
                        Err(e) => q.error = Some(format!("invalid regex: {e}")),
                    }
                }
                _ => q.terms.extend(Term::new(&tok)),
            }
        }
        q
    }

    pub fn is_empty(&self) -> bool {
        self.terms.is_empty()
            && self.exts.is_empty()
            && self.size.is_none()
            && self.kind.is_none()
            && self.regex.is_none()
    }

    /// Bits every candidate name must have set. Combines the name terms, the
    /// extension filter (bits shared by every accepted extension, plus the dot)
    /// and the regex's required literals.
    pub fn name_sig(&self) -> (u64, u32) {
        let mut sig = 0u64;
        let mut sig2 = 0u32;
        for t in self.terms.iter().filter(|t| !t.on_path && t.ascii) {
            sig |= t.sig;
            sig2 |= t.sig2;
        }
        if !self.exts.is_empty() {
            let mut common = u64::MAX;
            let mut common2 = u32::MAX;
            for e in &self.exts {
                common &= signature(e.as_bytes());
                common2 &= signature2(e.as_bytes());
            }
            sig |= common | signature(b".");
            sig2 |= common2;
        }
        if !self.regex_on_path {
            sig |= self.regex_sig;
            sig2 |= self.regex_sig2;
        }
        (sig, sig2)
    }

    pub fn has_path_terms(&self) -> bool {
        self.regex_on_path || self.terms.iter().any(|t| t.on_path)
    }

    fn plain_terms_only(&self) -> bool {
        self.exts.is_empty()
            && self.size.is_none()
            && self.kind.is_none()
            && self.regex.is_none()
            && !self.terms.iter().any(|t| t.on_path)
    }

    /// True when every match of `self` is guaranteed to also match `prev`, so a
    /// search can filter the previous result set instead of rescanning.
    pub fn refines(&self, prev: &Query) -> bool {
        !prev.is_empty()
            && self.raw.starts_with(&prev.raw)
            && self.plain_terms_only()
            && prev.plain_terms_only()
    }
}

/// Bits shared by every possible prefix of a regex match, used to prefilter the
/// scan. Returns 0 when the pattern has no usable literals.
fn regex_signature(pattern: &str) -> (u64, u32) {
    use regex_syntax::hir::literal::{ExtractKind, Extractor};
    let Ok(hir) = regex_syntax::ParserBuilder::new()
        .case_insensitive(true)
        .build()
        .parse(pattern)
    else {
        return (0, 0);
    };
    let seq = Extractor::new().kind(ExtractKind::Prefix).extract(&hir);
    let Some(literals) = seq.literals() else {
        return (0, 0);
    };
    if literals.is_empty() {
        return (0, 0);
    }
    let mut sig = u64::MAX;
    let mut sig2 = u32::MAX;
    for l in literals {
        sig &= signature(l.as_bytes());
        sig2 &= signature2(l.as_bytes());
    }
    (sig, sig2)
}

fn tokenize(input: &str) -> Vec<(String, bool)> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut in_quote = false;
    for ch in input.chars() {
        match ch {
            '"' => {
                in_quote = !in_quote;
                quoted = true;
            }
            c if c.is_whitespace() && !in_quote => {
                if !cur.is_empty() {
                    out.push((std::mem::take(&mut cur), quoted));
                }
                quoted = false;
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push((cur, quoted));
    }
    out
}

fn split_prefix(tok: &str) -> Option<(&str, &str)> {
    let (head, rest) = tok.split_once(':')?;
    if head.is_empty() || !head.chars().all(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    Some((head, rest))
}

fn parse_size(val: &str) -> Result<SizeFilter, String> {
    let (op, rest) = if let Some(r) = val.strip_prefix(">=") {
        (Cmp::Ge, r)
    } else if let Some(r) = val.strip_prefix("<=") {
        (Cmp::Le, r)
    } else if let Some(r) = val.strip_prefix('>') {
        (Cmp::Gt, r)
    } else if let Some(r) = val.strip_prefix('<') {
        (Cmp::Lt, r)
    } else if let Some(r) = val.strip_prefix('=') {
        (Cmp::Eq, r)
    } else {
        (Cmp::Ge, val)
    };
    let rest = rest.trim();
    let digits_end = rest
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(rest.len());
    let (num, unit) = rest.split_at(digits_end);
    let num: f64 = num.parse().map_err(|_| format!("invalid size: {val:?}"))?;
    let mult: f64 = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1.0,
        "k" | "kb" | "kib" => 1024.0,
        "m" | "mb" | "mib" => 1024.0 * 1024.0,
        "g" | "gb" | "gib" => 1024.0 * 1024.0 * 1024.0,
        "t" | "tb" | "tib" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        other => return Err(format!("unknown size unit: {other:?}")),
    };
    Ok(SizeFilter {
        op,
        bytes: (num * mult) as u64,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_terms_are_lowercased_and_anded() {
        let q = Query::parse("Foo BAR");
        assert_eq!(q.terms.len(), 2);
        assert_eq!(q.terms[0].lower, b"foo");
        assert_eq!(q.terms[1].lower, b"bar");
        assert!(!q.terms[0].on_path);
    }

    #[test]
    fn slash_makes_a_path_term() {
        let q = Query::parse("src/main");
        assert!(q.terms[0].on_path);
        assert!(q.has_path_terms());
    }

    #[test]
    fn ext_filter_accepts_lists_and_dots() {
        let q = Query::parse("ext:.PDF,jpg");
        assert_eq!(q.exts, vec!["pdf", "jpg"]);
        assert!(q.terms.is_empty());
    }

    #[test]
    fn size_filters_parse_units_and_operators() {
        assert_eq!(
            Query::parse("size:>10mb").size,
            Some(SizeFilter {
                op: Cmp::Gt,
                bytes: 10 * 1024 * 1024
            })
        );
        assert_eq!(
            Query::parse("size:<1k").size,
            Some(SizeFilter {
                op: Cmp::Lt,
                bytes: 1024
            })
        );
        assert_eq!(
            Query::parse("size:4096").size,
            Some(SizeFilter {
                op: Cmp::Ge,
                bytes: 4096
            })
        );
        assert!(Query::parse("size:abc").error.is_some());
    }

    #[test]
    fn size_filter_comparison() {
        let f = SizeFilter {
            op: Cmp::Gt,
            bytes: 100,
        };
        assert!(f.matches(101));
        assert!(!f.matches(100));
    }

    #[test]
    fn kind_filters() {
        assert_eq!(Query::parse("dir:").kind, Some(Kind::Dir));
        let q = Query::parse("file:notes");
        assert_eq!(q.kind, Some(Kind::File));
        assert_eq!(q.terms[0].lower, b"notes");
    }

    #[test]
    fn quoted_phrases_keep_spaces_and_ignore_prefixes() {
        let q = Query::parse("\"my report\" ext:pdf");
        assert_eq!(q.terms.len(), 1);
        assert_eq!(q.terms[0].lower, b"my report");
        assert_eq!(q.exts, vec!["pdf"]);
        let q = Query::parse("\"ext:pdf\"");
        assert_eq!(q.terms[0].lower, b"ext:pdf");
        assert!(q.exts.is_empty());
    }

    #[test]
    fn regex_terms_compile_and_report_errors() {
        let q = Query::parse(r"re:^IMG_\d+");
        assert!(q.regex.is_some());
        assert!(q.regex.unwrap().is_match(b"img_0042.jpg"));
        assert!(Query::parse("re:[unclosed").error.is_some());
    }

    #[test]
    fn non_ascii_terms_are_marked() {
        let q = Query::parse("Ação");
        assert!(!q.terms[0].ascii);
        assert_eq!(q.terms[0].lower, "ação".as_bytes());
    }

    #[test]
    fn empty_query_detection() {
        assert!(Query::parse("   ").is_empty());
        assert!(!Query::parse("a").is_empty());
    }

    #[test]
    fn refinement_only_for_plain_prefix_queries() {
        assert!(Query::parse("foob").refines(&Query::parse("foo")));
        assert!(Query::parse("foo bar").refines(&Query::parse("foo")));
        assert!(!Query::parse("food").refines(&Query::parse("foo ba")));
        assert!(!Query::parse("ext:pdf").refines(&Query::parse("ext:pd")));
        assert!(!Query::parse("src/m").refines(&Query::parse("src/")));
        assert!(!Query::parse("a").refines(&Query::parse("")));
    }

    #[test]
    fn name_sig_covers_name_terms_only() {
        let q = Query::parse("alpha beta/gamma");
        assert_eq!(q.name_sig().0, signature(b"alpha"));
    }

    #[test]
    fn name_sig_includes_extension_and_regex_literals() {
        let q = Query::parse("ext:pdf");
        let want = signature(b"pdf") | signature(b".");
        assert_eq!(q.name_sig().0 & want, want);

        // Only the bits shared by every accepted extension may be required.
        let q = Query::parse("ext:pdf,jpg");
        assert_eq!(q.name_sig().0 & signature(b"p"), signature(b"p"));
        assert_eq!(q.name_sig().0 & signature(b"d"), 0);

        let q = Query::parse(r"re:^IMG_\d+");
        let want = signature(b"img_");
        assert_eq!(q.name_sig().0 & want, want);
        assert_eq!(Query::parse("re:.*").name_sig(), (0, 0));
    }
}
