//! pi adapter — also used for pi forks that keep pi's on-disk layout (e.g. omp).
//! Identity is derived from the path alone (no transcript parsing), from the
//! second path component under the session root: the file stem for a main
//! session file, the artifact dir name for a nested artifact file — both shaped
//! `<ts>_<sessionId>` (DECISIONS §9). session_id = uuid after the last `_` in
//! that component, project_id = the `<encoded-cwd>` first component. The
//! cwd-encoding scheme differs between pi and omp, but identity never decodes
//! it — the dir name is opaque. Format reference: docs/pi-format-notes.md.

use std::path::{Path, PathBuf};

use anyhow::anyhow;

use crate::{Adapter, SessionIdentity, stem_str};

/// A pi-layout session store, labelled by the agent that owns it (`pi`, `omp`).
#[derive(Debug)]
pub struct PiAdapter {
    agent: String,
    session_root: PathBuf,
}

impl PiAdapter {
    pub fn new(agent: impl Into<String>, session_root: impl Into<PathBuf>) -> Self {
        Self {
            agent: agent.into(),
            session_root: session_root.into(),
        }
    }

    /// `<encoded-cwd>` and the session-bearing `<ts>_<sessionId>` stem of a
    /// root-relative path: the file stem for a main session file, the artifact
    /// dir name for a file nested inside a session's artifact dir (subagent
    /// transcripts, `__advisor.jsonl` — they carry their session's identity).
    fn session_parts(rel: &Path) -> anyhow::Result<(&str, &str)> {
        let mut comps = rel.components().map(|c| c.as_os_str().to_str());
        let (Some(Some(project)), Some(Some(second))) = (comps.next(), comps.next()) else {
            return Err(anyhow!("{}: no <encoded-cwd> parent dir", rel.display()));
        };
        let stem = if comps.next().is_none() {
            stem_str(rel)?
        } else {
            second
        };
        Ok((project, stem))
    }
}

impl Adapter for PiAdapter {
    fn agent(&self) -> &str {
        &self.agent
    }

    fn session_root(&self) -> &Path {
        &self.session_root
    }

    fn identify(&self, path: &Path) -> anyhow::Result<SessionIdentity> {
        let relative_path = self.relative_to_root(path)?;
        let (project, stem) = Self::session_parts(&relative_path)?;
        let session_id = stem
            .rsplit_once('_')
            .map(|(_ts, id)| id.to_string())
            .ok_or_else(|| anyhow!("{stem}: expected <ts>_<sessionId>"))?;
        let project_id = project.to_string();

        Ok(SessionIdentity {
            agent: self.agent.clone(),
            session_id,
            project_id,
            relative_path,
        })
    }

    fn append_only(&self) -> bool {
        true
    }

    fn is_session_file(&self, path: &Path) -> bool {
        path.extension().and_then(|e| e.to_str()) == Some("jsonl")
    }

    /// Session-stem prefix `YYYY-MM-DDTHH-MM-SS-mmmZ` → creation time. Files in
    /// a session's artifact dir take the dir name's prefix, so artifacts age
    /// with their session and cleanup selects them together.
    fn created_at(&self, path: &Path) -> Option<std::time::SystemTime> {
        let rel = self.relative_to_root(path).ok()?;
        let (_, stem) = Self::session_parts(&rel).ok()?;
        let ts = stem.split('_').next()?;
        parse_pi_timestamp(ts)
    }

    /// pi forks with session titles (omp) keep a `{"type":"title",…}` record on
    /// line 1 of the main session file; plain pi has none. Artifact files carry
    /// their session's title, read from the sibling main file. One leading
    /// line, length-capped — the header-field carve-out, not transcript parsing.
    fn title(&self, path: &Path) -> Option<String> {
        use std::io::{BufRead, BufReader, Read};
        let rel = self.relative_to_root(path).ok()?;
        let (project, stem) = Self::session_parts(&rel).ok()?;
        let main = self
            .session_root
            .join(project)
            .join(format!("{stem}.jsonl"));
        let file = std::fs::File::open(main).ok()?;
        let mut line = String::new();
        Read::take(BufReader::new(file), 64 * 1024)
            .read_line(&mut line)
            .ok()?;
        if !line.contains("\"type\":\"title\"") {
            return None;
        }
        json_string_field(&line, "title")
    }

    fn maps_paths(&self) -> bool {
        true
    }

    /// Scan the leading lines for the `type:session` header record (pi: line
    /// 1; omp: line 2, after its `type:title` record) and read its cwd.
    fn header_cwd(&self, bytes: &[u8]) -> Option<String> {
        let (start, end) = header_line_range(bytes)?;
        json_string_field(std::str::from_utf8(&bytes[start..end]).ok()?, "cwd")
    }

    /// Splice a new cwd into the header record; every other byte survives
    /// verbatim, so canonical→local→canonical reproduces the wire bytes.
    fn rewrite_header_cwd(&self, bytes: &[u8], cwd: &str) -> Option<Vec<u8>> {
        let (start, end) = header_line_range(bytes)?;
        let line = std::str::from_utf8(&bytes[start..end]).ok()?;
        let (vstart, vend) = json_string_span(line, "cwd")?;
        let mut escaped = String::with_capacity(cwd.len());
        for c in cwd.chars() {
            match c {
                '"' => escaped.push_str("\\\""),
                '\\' => escaped.push_str("\\\\"),
                other => escaped.push(other),
            }
        }
        let mut out = Vec::with_capacity(bytes.len() + escaped.len());
        out.extend_from_slice(&bytes[..start + vstart]);
        out.extend_from_slice(escaped.as_bytes());
        out.extend_from_slice(&bytes[start + vend..]);
        Some(out)
    }

    /// pi: absolute lossy form always. omp: home-relative under `home`,
    /// legacy absolute outside it — and `None` without a home context, since
    /// the two forms cannot be told apart safely (docs/pi-format-notes.md).
    fn encode_cwd(&self, cwd: &str, home: Option<&Path>) -> Option<String> {
        let dashed = |p: &str| p.replace(['/', '\\', ':'], "-");
        let legacy = |p: &str| format!("--{}--", dashed(p.trim_start_matches(['/', '\\'])));
        if self.agent == "omp" {
            let home = home?.to_str()?;
            if cwd == home {
                return Some("-".to_string());
            }
            match cwd.strip_prefix(home).and_then(|r| r.strip_prefix('/')) {
                Some(rel) => Some(format!("-{}", dashed(rel))),
                None => Some(legacy(cwd)),
            }
        } else {
            Some(legacy(cwd))
        }
    }
}

/// `YYYY-MM-DDTHH-MM-SS-mmmZ` (UTC) → SystemTime, no external date crate.
/// Public for the CLI's `cleanup --before` date parsing (one parser, one format).
pub fn parse_pi_timestamp(ts: &str) -> Option<std::time::SystemTime> {
    let ts = ts.strip_suffix('Z')?;
    let mut parts = ts.splitn(2, 'T');
    let date: Vec<u64> = parts
        .next()?
        .splitn(3, '-')
        .map(str::parse)
        .collect::<Result<_, _>>()
        .ok()?;
    let time: Vec<u64> = parts
        .next()?
        .splitn(4, '-')
        .map(str::parse)
        .collect::<Result<_, _>>()
        .ok()?;
    let (&[y, m, d], &[h, min, s, ms]) = (date.as_slice(), time.as_slice()) else {
        return None;
    };
    if !(1970..=9999).contains(&y) || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    if h > 23 || min > 59 || s > 60 || ms > 999 {
        return None;
    }
    // days-from-civil (Howard Hinnant), valid for all Gregorian dates
    let (y, m, d) = (y as i64, m as i64, d as i64);
    let y = if m <= 2 { y - 1 } else { y };
    let era = y / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    let secs = days as u64 * 86400 + h * 3600 + min * 60 + s;
    Some(std::time::UNIX_EPOCH + std::time::Duration::from_millis(secs * 1000 + ms))
}

/// Locate the `type:session` header record within the first three lines;
/// returns its byte range (line start..line end, newline excluded).
fn header_line_range(bytes: &[u8]) -> Option<(usize, usize)> {
    let mut start = 0;
    for _ in 0..3 {
        if start >= bytes.len() {
            return None;
        }
        let end = bytes[start..]
            .iter()
            .position(|&b| b == b'\n')
            .map_or(bytes.len(), |i| start + i);
        if let Ok(line) = std::str::from_utf8(&bytes[start..end])
            && line.contains("\"type\":\"session\"")
            && line.contains("\"cwd\":\"")
        {
            return Some((start, end));
        }
        start = end + 1;
    }
    None
}

/// Byte span of the raw (still-escaped) value of `"key":"…"` within `line`.
fn json_string_span(line: &str, key: &str) -> Option<(usize, usize)> {
    let vstart = line.find(&format!("\"{key}\":\""))? + key.len() + 4;
    let bytes = line.as_bytes();
    let mut i = vstart;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => return Some((vstart, i)),
            b'\\' => i += 2,
            _ => i += 1,
        }
    }
    None
}

/// Extract `"key":"value"` from one JSON line, handling `\"` escapes.
/// Best-effort: returns the raw string with simple escapes resolved.
fn json_string_field(line: &str, key: &str) -> Option<String> {
    let start = line.find(&format!("\"{key}\":\""))? + key.len() + 4;
    let rest = &line[start..];
    let mut out = String::new();
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => return Some(out),
            '\\' => match chars.next()? {
                'n' => out.push('\n'),
                't' => out.push('\t'),
                other => out.push(other),
            },
            other => out.push(other),
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const PI_SESSION: &[u8] = b"{\"type\":\"session\",\"version\":3,\"id\":\"019e\",\"cwd\":\"/srv/work/x\"}\n{\"msg\":\"body /srv/work/x stays\"}\n";
    const OMP_SESSION: &[u8] = b"{\"type\":\"title\",\"v\":1,\"title\":\"t\"}\n{\"type\":\"session\",\"version\":3,\"id\":\"019e\",\"cwd\":\"/srv/work/x\"}\n{\"msg\":\"body\"}\n";

    #[test]
    fn header_cwd_reads_pi_line1_and_omp_line2() {
        let pi = PiAdapter::new("pi", "/r");
        assert_eq!(pi.header_cwd(PI_SESSION).as_deref(), Some("/srv/work/x"));
        let omp = PiAdapter::new("omp", "/r");
        assert_eq!(omp.header_cwd(OMP_SESSION).as_deref(), Some("/srv/work/x"));
        // no session header at all (e.g. __advisor.jsonl) → None
        assert_eq!(pi.header_cwd(b"{\"type\":\"other\"}\n"), None);
    }

    #[test]
    fn rewrite_header_cwd_touches_only_the_header_record() {
        let pi = PiAdapter::new("pi", "/r");
        let out = pi
            .rewrite_header_cwd(PI_SESSION, "/home/simon/Projects/x")
            .unwrap();
        assert_eq!(
            pi.header_cwd(&out).as_deref(),
            Some("/home/simon/Projects/x")
        );
        // body bytes untouched (#45: stale body paths are inert)
        assert!(out.ends_with(b"{\"msg\":\"body /srv/work/x stays\"}\n"));

        let omp = PiAdapter::new("omp", "/r");
        let out = omp
            .rewrite_header_cwd(OMP_SESSION, "/home/simon/Projects/x")
            .unwrap();
        assert_eq!(
            omp.header_cwd(&out).as_deref(),
            Some("/home/simon/Projects/x")
        );
        // the title record on line 1 survives verbatim
        assert!(out.starts_with(b"{\"type\":\"title\",\"v\":1,\"title\":\"t\"}\n"));
    }

    #[test]
    fn rewrite_round_trips_bytes_exactly() {
        // canonical→local→canonical must reproduce the wire bytes, or the
        // write-back re-import would spuriously republish every tick.
        let pi = PiAdapter::new("pi", "/r");
        let local = pi.rewrite_header_cwd(PI_SESSION, "/l/x").unwrap();
        let back = pi.rewrite_header_cwd(&local, "/srv/work/x").unwrap();
        assert_eq!(back, PI_SESSION);
    }

    #[test]
    fn rewrite_escapes_json_special_chars() {
        let pi = PiAdapter::new("pi", "/r");
        let out = pi.rewrite_header_cwd(PI_SESSION, "/odd\"path\\x").unwrap();
        assert_eq!(pi.header_cwd(&out).as_deref(), Some("/odd\"path\\x"));
    }

    #[test]
    fn encode_cwd_is_agent_specific() {
        let pi = PiAdapter::new("pi", "/r");
        // pi: absolute lossy form, no home context needed
        assert_eq!(
            pi.encode_cwd("/home/simon/Projects/x", Some(Path::new("/home/simon"))),
            Some("--home-simon-Projects-x--".to_string())
        );
        assert_eq!(
            pi.encode_cwd("/srv/work/x", None),
            Some("--srv-work-x--".to_string())
        );

        let omp = PiAdapter::new("omp", "/r");
        // omp: home-relative under the given home
        assert_eq!(
            omp.encode_cwd("/home/simon/Projects/x", Some(Path::new("/home/simon"))),
            Some("-Projects-x".to_string())
        );
        // outside the home: legacy absolute form
        assert_eq!(
            omp.encode_cwd("/srv/work/x", Some(Path::new("/home/simon"))),
            Some("--srv-work-x--".to_string())
        );
        // no home context: omp cannot decide home-relative vs legacy, so it
        // refuses entirely (the engine turns this into a per-key skip with a
        // canonical_home hint) — even for paths outside any plausible home.
        assert_eq!(omp.encode_cwd("/home/simon/Projects/x", None), None);
        assert_eq!(omp.encode_cwd("/srv/work/x", None), None);
    }

    #[test]
    fn identifies_pi_session_from_path() {
        let root = Path::new("/home/simon/.pi/agent/sessions");
        let adapter = PiAdapter::new("pi", root);
        let path = root
            .join("--home-simon-Projects-nixfiles--")
            .join("2026-05-23T06-55-21-771Z_019e539d-f6ab-71ac-be20-d3ae2b23ea4a.jsonl");

        let id = adapter.identify(&path).unwrap();
        assert_eq!(id.agent, "pi");
        assert_eq!(id.session_id, "019e539d-f6ab-71ac-be20-d3ae2b23ea4a");
        assert_eq!(id.project_id, "--home-simon-Projects-nixfiles--");
        assert_eq!(
            id.relative_path,
            Path::new("--home-simon-Projects-nixfiles--")
                .join("2026-05-23T06-55-21-771Z_019e539d-f6ab-71ac-be20-d3ae2b23ea4a.jsonl")
        );
        assert!(adapter.is_session_file(&path));
        assert!(adapter.append_only());
    }

    #[test]
    fn created_at_comes_from_the_filename_timestamp() {
        let adapter = PiAdapter::new("pi", "/tmp/x");
        let path = Path::new(
            "/tmp/x/--p--/2026-05-23T06-55-21-771Z_019e539d-f6ab-71ac-be20-d3ae2b23ea4a.jsonl",
        );
        let t = adapter.created_at(path).expect("parseable timestamp");
        // 2026-05-23T06:55:21.771Z as unix seconds
        let secs = t.duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        assert_eq!(secs, 1779519321); // date -u -d '2026-05-23T06:55:21Z' +%s
        assert!(
            adapter
                .created_at(Path::new("/tmp/x/--p--/garbage.jsonl"))
                .is_none()
        );
    }

    #[test]
    fn title_reads_only_a_leading_title_record() {
        let dir = std::env::temp_dir().join(format!("ssync-pi-title-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("--p--")).unwrap();
        let adapter = PiAdapter::new("omp", &dir);

        let named = dir
            .join("--p--")
            .join("2026-05-23T06-55-21-771Z_019e539d-f6ab-71ac-be20-d3ae2b23ea4a.jsonl");
        std::fs::write(&named, "{\"type\":\"title\",\"v\":1,\"title\":\"my session\",\"pad\":\"   \"}\n{\"type\":\"session\",\"version\":3}\n").unwrap();
        assert_eq!(adapter.title(&named).as_deref(), Some("my session"));

        let unnamed = dir
            .join("--p--")
            .join("2026-05-23T06-55-21-771Z_019e539d-f6ab-71ac-be20-d3ae2b23ea4b.jsonl");
        std::fs::write(&unnamed, "{\"type\":\"title\",\"v\":1,\"title\":\"\",\"pad\":\" \"}\n{\"type\":\"session\",\"version\":3}\n").unwrap();
        assert_eq!(adapter.title(&unnamed).as_deref(), Some(""));

        // pi layout: no title record at all
        let plain = dir
            .join("--p--")
            .join("2026-05-23T06-55-21-771Z_019e539d-f6ab-71ac-be20-d3ae2b23ea4c.jsonl");
        std::fs::write(&plain, "{\"type\":\"session\",\"version\":3}\n").unwrap();
        assert_eq!(adapter.title(&plain), None);
    }

    #[test]
    fn identifies_nested_camelcase_artifact() {
        // omp keeps subagent transcripts in a per-session artifact dir nested one
        // level below the main session file: <root>/<enc>/<ts>_<uuid>/<Name>.jsonl.
        let root = Path::new("/home/simon/.pi/agent/sessions");
        let adapter = PiAdapter::new("omp", root);
        let path = root
            .join("--home-simon-Projects-nixfiles--")
            .join("2026-05-23T06-55-21-771Z_019e539d-f6ab-71ac-be20-d3ae2b23ea4a")
            .join("Tests.jsonl");

        let id = adapter.identify(&path).unwrap();
        assert_eq!(id.agent, "omp");
        // project_id is the FIRST (encoded-cwd) component, not the artifact dir.
        assert_eq!(id.project_id, "--home-simon-Projects-nixfiles--");
        // session_id is the uuid after the last `_` of the artifact DIRECTORY name.
        assert_eq!(id.session_id, "019e539d-f6ab-71ac-be20-d3ae2b23ea4a");
        assert_eq!(
            id.relative_path,
            Path::new("--home-simon-Projects-nixfiles--")
                .join("2026-05-23T06-55-21-771Z_019e539d-f6ab-71ac-be20-d3ae2b23ea4a")
                .join("Tests.jsonl")
        );
    }

    #[test]
    fn identifies_nested_advisor_artifact() {
        // `__advisor.jsonl` has underscores in its own stem; identity must still
        // come from the artifact DIRECTORY name, never `rsplit_once('_')` on the
        // filename (which yields session_id "advisor" and the dir as project_id).
        let root = Path::new("/home/simon/.pi/agent/sessions");
        let adapter = PiAdapter::new("omp", root);
        let path = root
            .join("--home-simon-Projects-nixfiles--")
            .join("2026-05-23T06-55-21-771Z_019e539d-f6ab-71ac-be20-d3ae2b23ea4a")
            .join("__advisor.jsonl");

        let id = adapter.identify(&path).unwrap();
        assert_eq!(id.agent, "omp");
        assert_eq!(id.project_id, "--home-simon-Projects-nixfiles--");
        assert_eq!(id.session_id, "019e539d-f6ab-71ac-be20-d3ae2b23ea4a");
        assert_eq!(
            id.relative_path,
            Path::new("--home-simon-Projects-nixfiles--")
                .join("2026-05-23T06-55-21-771Z_019e539d-f6ab-71ac-be20-d3ae2b23ea4a")
                .join("__advisor.jsonl")
        );
    }

    #[test]
    fn identify_errors_on_depth_one_stray() {
        // A jsonl directly under the session root has no <encoded-cwd> component;
        // identity cannot be derived and identify must error (unchanged behavior).
        let root = Path::new("/home/simon/.pi/agent/sessions");
        let adapter = PiAdapter::new("pi", root);
        assert!(adapter.identify(&root.join("stray.jsonl")).is_err());
    }

    #[test]
    fn created_at_of_nested_artifact_from_dir_name() {
        // A depth-3 artifact file carries no timestamp in its own stem; creation
        // time comes from the artifact DIRECTORY name (the session component).
        let root = Path::new("/tmp/x");
        let adapter = PiAdapter::new("omp", root);
        let path = root
            .join("--p--")
            .join("2026-05-23T06-55-21-771Z_019e539d-f6ab-71ac-be20-d3ae2b23ea4a")
            .join("Tests.jsonl");
        let t = adapter.created_at(&path).expect("dir-name timestamp");
        let secs = t.duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        assert_eq!(secs, 1779519321); // 2026-05-23T06:55:21Z
    }

    #[test]
    fn title_of_nested_artifact_comes_from_sibling_main() {
        let dir =
            std::env::temp_dir().join(format!("ssync-pi-title-nested-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let enc = dir.join("--home-simon-Projects-nixfiles--");
        let adapter = PiAdapter::new("omp", &dir);

        // Session A: sibling main file <enc>/<ts>_<uuid>.jsonl carries the title.
        let sess_a = "2026-05-23T06-55-21-771Z_019e539d-f6ab-71ac-be20-d3ae2b23ea4a";
        std::fs::create_dir_all(enc.join(sess_a)).unwrap();
        std::fs::write(
            enc.join(format!("{sess_a}.jsonl")),
            "{\"type\":\"title\",\"v\":1,\"title\":\"nested session\",\"pad\":\"  \"}\n{\"type\":\"session\",\"version\":3}\n",
        )
        .unwrap();
        // The artifact's OWN first line is a title record that MUST be ignored:
        // the title comes from the sibling main file, not the subagent transcript.
        let artifact_a = enc.join(sess_a).join("Tests.jsonl");
        std::fs::write(
            &artifact_a,
            "{\"type\":\"title\",\"v\":1,\"title\":\"subagent transcript\"}\n",
        )
        .unwrap();
        assert_eq!(
            adapter.title(&artifact_a).as_deref(),
            Some("nested session")
        );

        // Session B: sibling main file absent → None (even though the artifact
        // itself has a title record, which must not be used as a fallback).
        let sess_b = "2026-06-01T00-00-00-000Z_019e539d-f6ab-71ac-be20-d3ae2b23ea4b";
        std::fs::create_dir_all(enc.join(sess_b)).unwrap();
        let artifact_b = enc.join(sess_b).join("Tests.jsonl");
        std::fs::write(
            &artifact_b,
            "{\"type\":\"title\",\"v\":1,\"title\":\"orphan\"}\n",
        )
        .unwrap();
        assert_eq!(adapter.title(&artifact_b), None);
    }
}
