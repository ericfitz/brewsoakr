//! `brewsoak settings`: edits of the config file as a `toml_edit::DocumentMut`.
//! No I/O here: the caller reads the file, applies one edit, and writes the
//! document back only when `Edit::changed`. Comments, key order, and unknown
//! keys survive because the document is edited in place, never regenerated.

use crate::config;
use crate::nosoak;
use crate::{Error, SoakHours};
use std::path::Path;
use toml_edit::{Array, ArrayOfTables, DocumentMut, Item, Table, Value, value};

/// What one edit did. `messages` is one line per token or key, in order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Edit {
    pub changed: bool,
    pub messages: Vec<String>,
}

impl Edit {
    fn unchanged(message: impl Into<String>) -> Self {
        Self {
            changed: false,
            messages: vec![message.into()],
        }
    }

    fn changed_with(message: impl Into<String>) -> Self {
        Self {
            changed: true,
            messages: vec![message.into()],
        }
    }
}

/// `SOAK_HOURS = N`. The default (24) removes the key instead, the same rule
/// `--soak-hours` follows.
pub fn set_soak_hours(doc: &mut DocumentMut, hours: SoakHours) -> Edit {
    if hours == SoakHours::DEFAULT {
        return clear_soak_hours(doc);
    }
    let n = i64::from(hours.get());
    if doc.get("SOAK_HOURS").and_then(Item::as_integer) == Some(n) {
        return Edit::unchanged(format!("SOAK_HOURS is already {n}"));
    }
    let had_values = root_has_values(doc);
    set_integer(doc.as_table_mut(), "SOAK_HOURS", n);
    separate_taps(doc, had_values);
    Edit::changed_with(format!("SOAK_HOURS = {n}"))
}

pub fn clear_soak_hours(doc: &mut DocumentMut) -> Edit {
    // The comment above a key is its prefix decor; removing the key would
    // take it along, so hand it to whatever follows.
    let table = doc.as_table_mut();
    let Some(idx) = table.iter().position(|(k, _)| k == "SOAK_HOURS") else {
        return Edit::unchanged("SOAK_HOURS is not set; the default is 24");
    };
    let comment = table
        .key("SOAK_HOURS")
        .map(|k| prefix_of(k.leaf_decor()))
        .unwrap_or_default();
    table.remove("SOAK_HOURS");
    if !comment.is_empty() {
        prepend_at_root(doc, idx, &comment);
    }
    Edit::changed_with("SOAK_HOURS removed; the default is 24")
}

/// Put `text` in front of the root item now at `idx` (a key, a `[table]` or
/// the first `[[TAP]]`), or at the start of the trailing text when none follows.
fn prepend_at_root(doc: &mut DocumentMut, idx: usize, text: &str) {
    let done = match doc.as_table_mut().iter_mut().nth(idx) {
        Some((mut key, Item::Value(_))) => {
            let old = prefix_of(key.leaf_decor());
            key.leaf_decor_mut().set_prefix(format!("{text}{old}"));
            true
        }
        Some((_, Item::Table(t))) => {
            let old = prefix_of(t.decor());
            t.decor_mut().set_prefix(format!("{text}{old}"));
            true
        }
        Some((_, Item::ArrayOfTables(tables))) => match tables.get_mut(0) {
            Some(first) => {
                let old = prefix_of(first.decor());
                first.decor_mut().set_prefix(format!("{text}{old}"));
                true
            }
            None => false,
        },
        _ => false,
    };
    if !done {
        prepend_trailing(doc, text);
    }
}

fn prepend_trailing(doc: &mut DocumentMut, text: &str) {
    let old = doc.trailing().as_str().unwrap_or("").to_string();
    doc.set_trailing(format!("{text}{old}"));
}

/// Comment `text` from a removed `[[TAP]]` entry (or its body) goes to the
/// `[[TAP]]` now at `i`, else to the end of the document. A next table with
/// no comment of its own gets no extra blank line.
fn carry_to_tap(doc: &mut DocumentMut, i: usize, text: &str) {
    if text.trim().is_empty() {
        return;
    }
    let next = doc
        .get_mut("TAP")
        .and_then(Item::as_array_of_tables_mut)
        .and_then(|tables| tables.get_mut(i));
    match next {
        Some(next) => {
            let old = prefix_of(next.decor());
            let merged = if old.trim().is_empty() {
                text.to_string()
            } else {
                format!("{text}{old}")
            };
            next.decor_mut().set_prefix(merged);
        }
        None => {
            let old = doc.trailing().as_str().unwrap_or("").to_string();
            doc.set_trailing(format!("{old}{text}"));
        }
    }
}

fn prefix_of(decor: &toml_edit::Decor) -> String {
    decor
        .prefix()
        .and_then(|p| p.as_str())
        .unwrap_or("")
        .to_string()
}

/// Replace the value in place so a trailing comment on the line survives.
/// `table[key] = value(n)` would drop it.
fn set_integer(table: &mut Table, key: &str, n: i64) {
    match table.get_mut(key).and_then(Item::as_value_mut) {
        Some(existing) => {
            let decor = existing.decor().clone();
            let mut new = Value::from(n);
            *new.decor_mut() = decor;
            *existing = new;
        }
        None => table[key] = value(n),
    }
}

fn root_has_values(doc: &DocumentMut) -> bool {
    doc.iter().any(|(_, item)| item.is_value())
}

/// toml_edit renders root keys above every `[[TAP]]` but adds no blank line
/// when the file held only tables. Add one, once, so the result reads like
/// the README example.
fn separate_taps(doc: &mut DocumentMut, had_values: bool) {
    if had_values {
        return;
    }
    let Some(first) = doc
        .get_mut("TAP")
        .and_then(Item::as_array_of_tables_mut)
        .and_then(|tables| tables.get_mut(0))
    else {
        return;
    };
    let prefix = first
        .decor()
        .prefix()
        .and_then(|p| p.as_str())
        .unwrap_or("")
        .to_string();
    if !prefix.starts_with('\n') {
        first.decor_mut().set_prefix(format!("\n{prefix}"));
    }
}

/// Every token through `nosoak::parse_entry`, the config reader's parser.
/// All bad tokens go in one usage error; the result is lowercased.
fn validate_tokens(tokens: &[String]) -> Result<Vec<String>, Error> {
    let bad: Vec<String> = tokens
        .iter()
        .filter_map(|t| nosoak::parse_entry(t).err())
        .map(|reason| reason.trim_end_matches("; skipped").to_string())
        .collect();
    if !bad.is_empty() {
        return Err(Error::Usage(format!(
            "invalid NO_SOAK token(s):\n  {}",
            bad.join("\n  ")
        )));
    }
    Ok(tokens
        .iter()
        .map(|t| t.trim().to_ascii_lowercase())
        .collect())
}

/// `Ok(None)`: no top-level key. `Err`: the key exists but is not an array
/// of strings, which the reader ignores and this editor will not clobber.
fn no_soak_array(doc: &mut DocumentMut) -> Result<Option<&mut Array>, Error> {
    let Some(item) = doc.get_mut("NO_SOAK") else {
        return Ok(None);
    };
    match item.as_array_mut() {
        Some(arr) if arr.iter().all(|v| v.as_str().is_some()) => Ok(Some(arr)),
        _ => Err(Error::Refusal(
            "config: NO_SOAK is not an array of strings; fix it by hand".into(),
        )),
    }
}

fn no_soak_contains(arr: &Array, token: &str) -> bool {
    arr.iter().any(|v| {
        v.as_str()
            .is_some_and(|s| s.trim().eq_ignore_ascii_case(token))
    })
}

/// Append with the indentation of the last element when the array is written
/// one entry per line; a plain push would put the new entry on that line.
fn push_entry(arr: &mut Array, token: &str) {
    let last_prefix = arr
        .iter()
        .last()
        .and_then(|v| v.decor().prefix())
        .and_then(|p| p.as_str())
        .map(str::to_string);
    match last_prefix {
        Some(prefix) if prefix.contains('\n') => {
            let indent = prefix.rsplit('\n').next().unwrap_or("");
            let mut new = Value::from(token);
            new.decor_mut().set_prefix(format!("\n{indent}"));
            arr.push_formatted(new);
        }
        _ => arr.push(token),
    }
}

/// Append each token not already present (case-insensitive), lowercased.
pub fn no_soak_add(doc: &mut DocumentMut, tokens: &[String]) -> Result<Edit, Error> {
    let tokens = validate_tokens(tokens)?;
    let had_values = root_has_values(doc);
    if no_soak_array(doc)?.is_none() {
        doc["NO_SOAK"] = value(Array::new());
    }
    let arr = no_soak_array(doc)?.expect("NO_SOAK was just created");
    let mut edit = Edit::default();
    for token in tokens {
        if no_soak_contains(arr, &token) {
            edit.messages.push(format!("already in NO_SOAK: {token}"));
            continue;
        }
        push_entry(arr, &token);
        edit.changed = true;
        edit.messages.push(format!("added to NO_SOAK: {token}"));
    }
    separate_taps(doc, had_values);
    Ok(edit)
}

/// Remove each matching entry (case-insensitive). An absent token is
/// reported, not an error. An emptied list stays as `NO_SOAK = []`.
pub fn no_soak_remove(doc: &mut DocumentMut, tokens: &[String]) -> Result<Edit, Error> {
    let tokens = validate_tokens(tokens)?;
    let mut edit = Edit::default();
    let Some(arr) = no_soak_array(doc)? else {
        edit.messages = tokens
            .iter()
            .map(|t| format!("not in NO_SOAK: {t}"))
            .collect();
        return Ok(edit);
    };
    for token in tokens {
        let matches = |v: &Value| {
            v.as_str()
                .is_some_and(|s| s.trim().eq_ignore_ascii_case(&token))
        };
        // The first element's prefix and the last one's suffix hold the
        // array's padding (`[ "a"`, `"b"\n]`); hand them to whichever
        // element becomes first or last.
        let first_gone = arr
            .get(0)
            .filter(|v| matches(v))
            .map(|v| prefix_of(v.decor()));
        let last_gone = arr.iter().last().filter(|v| matches(v)).map(|v| {
            v.decor()
                .suffix()
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_string()
        });
        let before = arr.len();
        arr.retain(|v| !matches(v));
        if arr.len() == before {
            edit.messages.push(format!("not in NO_SOAK: {token}"));
            continue;
        }
        edit.changed = true;
        edit.messages.push(format!("removed from NO_SOAK: {token}"));
        if let Some(prefix) = first_gone
            && let Some(first) = arr.get_mut(0)
        {
            first.decor_mut().set_prefix(prefix);
        }
        if let Some(suffix) = last_gone
            && let Some(last) = arr.len().checked_sub(1).and_then(|i| arr.get_mut(i))
        {
            last.decor_mut().set_suffix(suffix);
        }
    }
    Ok(edit)
}

/// Same rule as the `[[TAP]]` reader: exactly two non-empty segments.
pub fn normalize_tap(raw: &str) -> Result<String, Error> {
    let lower = raw.trim().to_ascii_lowercase();
    if lower.split('/').count() != 2 || lower.split('/').any(str::is_empty) {
        return Err(Error::Usage(format!("tap must be user/repo, got {raw:?}")));
    }
    Ok(lower)
}

/// `Ok(None)`: no `TAP` key. `Err`: it exists but is not `[[TAP]]` tables.
fn tap_tables(doc: &mut DocumentMut) -> Result<Option<&mut ArrayOfTables>, Error> {
    let Some(item) = doc.get_mut("TAP") else {
        return Ok(None);
    };
    item.as_array_of_tables_mut().map(Some).ok_or_else(|| {
        Error::Refusal("config: TAP is not an array of tables; fix it by hand".into())
    })
}

/// Index of the last `[[TAP]]` whose `name` matches: the reader lets the
/// last duplicate win, so that is the entry whose hours take effect.
/// Entries without a string `name` never match.
fn tap_index(tables: &ArrayOfTables, tap: &str) -> Option<usize> {
    tables
        .iter()
        .enumerate()
        .filter(|(_, t)| {
            t.get("name")
                .and_then(Item::as_str)
                .is_some_and(|n| n.trim().eq_ignore_ascii_case(tap))
        })
        .last()
        .map(|(i, _)| i)
}

/// Set that tap's `soak_hours`, appending a `[[TAP]]` after the last one
/// when the tap has none.
pub fn tap_hours_set(doc: &mut DocumentMut, tap: &str, hours: SoakHours) -> Result<Edit, Error> {
    let tap = normalize_tap(tap)?;
    let n = i64::from(hours.get());
    if let Some(tables) = tap_tables(doc)?
        && let Some(i) = tap_index(tables, &tap)
    {
        let table = tables.get_mut(i).expect("index from iter");
        if table.get("soak_hours").and_then(Item::as_integer) == Some(n) {
            return Ok(Edit::unchanged(format!("{tap} soak_hours is already {n}")));
        }
        set_integer(table, "soak_hours", n);
        return Ok(Edit::changed_with(format!("{tap} soak_hours = {n}")));
    }
    let mut entry = Table::new();
    entry["name"] = value(tap.as_str());
    entry["soak_hours"] = value(n);
    match tap_tables(doc)? {
        Some(tables) => tables.push(entry),
        None => {
            let mut tables = ArrayOfTables::new();
            tables.push(entry);
            doc["TAP"] = Item::ArrayOfTables(tables);
        }
    }
    Ok(Edit::changed_with(format!(
        "{tap} added with soak_hours = {n}"
    )))
}

/// Remove that tap's `soak_hours`; an entry left with only `name` goes too,
/// and the `TAP` key goes when no entry remains.
pub fn tap_hours_clear(doc: &mut DocumentMut, tap: &str) -> Result<Edit, Error> {
    let tap = normalize_tap(tap)?;
    let Some(tables) = tap_tables(doc)? else {
        return Ok(Edit::unchanged(format!("{tap} has no [[TAP]] entry")));
    };
    let Some(i) = tap_index(tables, &tap) else {
        return Ok(Edit::unchanged(format!("{tap} has no [[TAP]] entry")));
    };
    let table = tables.get_mut(i).expect("index from iter");
    let Some(idx) = table.iter().position(|(k, _)| k == "soak_hours") else {
        return Ok(Edit::unchanged(format!(
            "{tap} has no soak_hours; it uses SOAK_HOURS"
        )));
    };
    let why = table
        .key("soak_hours")
        .map(|k| prefix_of(k.leaf_decor()))
        .unwrap_or_default();
    table.remove("soak_hours");
    let mut edit = Edit::changed_with(format!("{tap} soak_hours removed; it uses SOAK_HOURS"));
    if table.len() == 1 && table.contains_key("name") {
        let removed = prefix_of(table.decor());
        tables.remove(i);
        edit.messages
            .push(format!("{tap} [[TAP]] entry removed; only name was left"));
        if tables.is_empty() {
            doc.as_table_mut().remove("TAP");
        }
        // The removed entry's comments go to the next entry, or to the end
        // of the document when it was the last one.
        carry_to_tap(doc, i, &format!("{removed}{why}"));
    } else if !why.is_empty() {
        let next_key = table.iter_mut().nth(idx).map(|(mut key, item)| {
            if item.is_value() {
                let old = prefix_of(key.leaf_decor());
                key.leaf_decor_mut().set_prefix(format!("{why}{old}"));
                true
            } else {
                false
            }
        });
        if next_key != Some(true) {
            carry_to_tap(doc, i + 1, &why);
        }
    }
    Ok(edit)
}

/// The effective config, for `settings show`. `doc` is `None` when there is
/// no config file. Every parse note and warning is included, not only
/// under `-v`.
pub fn render_show(path: &Path, env: Option<&str>, doc: Option<&DocumentMut>) -> String {
    let contents = doc.map(ToString::to_string);
    let parsed = contents
        .as_deref()
        .map(config::parse_file)
        .unwrap_or_default();
    let mut out = String::new();
    match contents {
        Some(_) => out.push_str(&format!("config: {}\n", path.display())),
        None => out.push_str(&format!("config: {} (no config file)\n", path.display())),
    }
    // Same precedence as `config::resolve_hours`: env > file > 24, where an
    // env value that is not an integer >= 1 is ignored.
    let env_hours = env
        .and_then(|raw| raw.parse::<u32>().ok())
        .and_then(SoakHours::new);
    let (hours, source) = match (env_hours, parsed.soak_hours) {
        (Some(h), _) => (h, "BREWSOAK_SOAK_HOURS"),
        (None, Some(h)) => (h, "SOAK_HOURS in the file"),
        (None, None) => (SoakHours::DEFAULT, "default"),
    };
    out.push_str(&format!("soak hours: {} ({source})\n", hours.get()));
    let as_written: Vec<&str> = doc
        .and_then(|d| d.get("NO_SOAK"))
        .and_then(Item::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    if as_written.is_empty() {
        out.push_str("NO_SOAK: (none)\n");
    } else {
        out.push_str("NO_SOAK:\n");
        for entry in as_written {
            out.push_str(&format!("  {entry}\n"));
        }
    }
    if parsed.taps.is_empty() {
        out.push_str("taps: (none)\n");
    } else {
        out.push_str("taps:\n");
        for tap in &parsed.taps {
            match tap.soak_hours {
                Some(h) => out.push_str(&format!("  {}: {} (own)\n", tap.name, h.get())),
                None => out.push_str(&format!("  {}: {} (SOAK_HOURS)\n", tap.name, hours.get())),
            }
        }
    }
    for note in &parsed.notes {
        out.push_str(&format!("note: {note}\n"));
    }
    if let Some(raw) = env
        && env_hours.is_none()
    {
        out.push_str(&format!(
            "note: BREWSOAK_SOAK_HOURS={raw:?} is not an integer >= 1; ignored\n"
        ));
    }
    for warning in &parsed.warnings {
        out.push_str(&format!("warning: {warning}\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(s: &str) -> DocumentMut {
        s.parse().expect("test fixture is valid TOML")
    }

    const FULL: &str = "# header\nSOAK_HOURS = 48 # trailing\nNO_SOAK = [\"ericfitz/tap\", \"wget\"] # list\nunknown = true\n\n# taps\n[[TAP]]\nname = \"HashiCorp/tap\"\nsoak_hours = 72\n\n[[TAP]]\nname = \"cyclonedx/cyclonedx\"\n";

    #[test]
    fn clear_soak_hours_keeps_the_header_whatever_follows() {
        for (input, needle) in [
            (
                "# header\nSOAK_HOURS = 48\nNO_SOAK = [\"w\"]\n",
                "# header\nNO_SOAK",
            ),
            (
                "# header\nSOAK_HOURS = 48\n\n[s]\na = 1\n",
                "# header\n\n[s]",
            ),
            (
                "# header\nSOAK_HOURS = 48\n\n# taps\n[[TAP]]\nname = \"a/b\"\n",
                "# header\n\n# taps\n[[TAP]]",
            ),
            ("# header\nSOAK_HOURS = 48\n", "# header\n"),
        ] {
            let mut d = doc(input);
            assert!(clear_soak_hours(&mut d).changed);
            let text = d.to_string();
            assert!(text.contains(needle), "{input:?} -> {text:?}");
            assert!(text.parse::<DocumentMut>().is_ok(), "{text:?}");
            assert!(!text.contains("SOAK_HOURS"), "{text:?}");
        }
        let mut d = doc("# header\nSOAK_HOURS = 48\n");
        clear_soak_hours(&mut d);
        assert_eq!(d.to_string(), "# header\n");
    }

    #[test]
    fn clear_soak_hours_prepends_to_the_next_keys_own_comment() {
        let mut d = doc("# header\n\nSOAK_HOURS = 48 # t\n# about no_soak\nNO_SOAK = [\"wget\"]\n");
        clear_soak_hours(&mut d);
        let text = d.to_string();
        assert!(text.contains("# header\n"), "{text:?}");
        assert!(text.contains("# about no_soak\nNO_SOAK"), "{text:?}");
    }

    #[test]
    fn tap_hours_clear_last_entry_keeps_its_comment_in_the_document() {
        let mut d = doc("SOAK_HOURS = 2\n\n# the tap\n[[TAP]]\nname = \"a/b\"\nsoak_hours = 5\n");
        tap_hours_clear(&mut d, "a/b").unwrap();
        let text = d.to_string();
        assert!(text.contains("# the tap"), "{text:?}");
        assert!(!text.contains("TAP"), "{text:?}");
        assert!(text.parse::<DocumentMut>().is_ok());
    }

    #[test]
    fn tap_hours_clear_first_of_two_carries_its_comment_to_the_next_table() {
        let mut d = doc(
            "# one\n[[TAP]]\nname = \"a/b\"\nsoak_hours = 1\n\n# two\n[[TAP]]\nname = \"c/d\"\n",
        );
        tap_hours_clear(&mut d, "a/b").unwrap();
        let text = d.to_string();
        assert_eq!(text, "# one\n\n# two\n[[TAP]]\nname = \"c/d\"\n");
        assert!(text.parse::<DocumentMut>().is_ok());
    }

    #[test]
    fn tap_hours_clear_last_of_two_carries_its_comment_to_the_trailing_text() {
        let mut d =
            doc("[[TAP]]\nname = \"a/b\"\n\n# two\n[[TAP]]\nname = \"c/d\"\nsoak_hours = 1\n");
        tap_hours_clear(&mut d, "c/d").unwrap();
        let text = d.to_string();
        assert!(text.contains("# two"), "{text:?}");
        assert!(!text.contains("c/d"), "{text:?}");
        assert!(text.parse::<DocumentMut>().is_ok());
    }

    #[test]
    fn tap_hours_clear_keeps_the_comment_above_the_removed_key() {
        let mut d = doc("[[TAP]]\nname = \"a/b\"\n# why\nsoak_hours = 5\nextra = 1\n");
        tap_hours_clear(&mut d, "a/b").unwrap();
        assert_eq!(d.to_string(), "[[TAP]]\nname = \"a/b\"\n# why\nextra = 1\n");
        let mut d = doc("[[TAP]]\nname = \"a/b\"\n# why\nsoak_hours = 5\n");
        tap_hours_clear(&mut d, "a/b").unwrap();
        assert!(d.to_string().contains("# why"), "{d}");
        let mut d = doc(
            "[[TAP]]\nname = \"a/b\"\nextra = 1\n# why\nsoak_hours = 5\n\n[[TAP]]\nname = \"c/d\"\n",
        );
        tap_hours_clear(&mut d, "a/b").unwrap();
        let text = d.to_string();
        assert!(text.contains("# why\n[[TAP]]\nname = \"c/d\""), "{text:?}");
        assert!(text.parse::<DocumentMut>().is_ok());
    }

    #[test]
    fn clear_soak_hours_keeps_the_comment_when_it_is_not_the_first_key() {
        let mut d = doc("NO_SOAK = [\"w\"]\n# soak\nSOAK_HOURS = 48\n");
        clear_soak_hours(&mut d);
        assert_eq!(d.to_string(), "NO_SOAK = [\"w\"]\n# soak\n");
        let mut d = doc("NO_SOAK = [\"w\"]\n# soak\nSOAK_HOURS = 48\n\n[[TAP]]\nname = \"a/b\"\n");
        clear_soak_hours(&mut d);
        let text = d.to_string();
        assert!(text.contains("# soak\n"), "{text:?}");
        assert!(text.parse::<DocumentMut>().is_ok(), "{text:?}");
    }

    fn s(items: &[&str]) -> Vec<String> {
        items.iter().map(|i| (*i).to_string()).collect()
    }

    #[test]
    fn set_soak_hours_keeps_trailing_comment_and_other_keys() {
        let mut d = doc(FULL);
        let edit = set_soak_hours(&mut d, SoakHours::new(12).unwrap());
        assert_eq!(edit, Edit::changed_with("SOAK_HOURS = 12"));
        assert_eq!(
            d.to_string(),
            FULL.replace("SOAK_HOURS = 48 # trailing", "SOAK_HOURS = 12 # trailing")
        );
    }

    #[test]
    fn set_soak_hours_same_value_is_unchanged() {
        let mut d = doc(FULL);
        let edit = set_soak_hours(&mut d, SoakHours::new(48).unwrap());
        assert!(!edit.changed, "{edit:?}");
        assert_eq!(edit.messages, ["SOAK_HOURS is already 48"]);
        assert_eq!(d.to_string(), FULL);
    }

    #[test]
    fn set_soak_hours_24_removes_the_key_like_clear() {
        let mut d = doc(FULL);
        let edit = set_soak_hours(&mut d, SoakHours::DEFAULT);
        assert!(edit.changed);
        let text = d.to_string();
        assert!(!text.contains("SOAK_HOURS"), "{text}");
        assert!(text.starts_with("# header\n"), "{text}");
        assert!(text.contains("unknown = true"), "{text}");
        let again = clear_soak_hours(&mut d);
        assert!(!again.changed);
        assert!(again.messages[0].contains("not set"), "{again:?}");
    }

    #[test]
    fn set_soak_hours_overwrites_a_wrong_typed_value() {
        let mut d = doc("SOAK_HOURS = \"x\" # c\n");
        set_soak_hours(&mut d, SoakHours::new(9).unwrap());
        assert_eq!(d.to_string(), "SOAK_HOURS = 9 # c\n");
    }

    #[test]
    fn empty_file_gets_exactly_the_key() {
        let mut d = doc("");
        set_soak_hours(&mut d, SoakHours::new(48).unwrap());
        assert_eq!(d.to_string(), "SOAK_HOURS = 48\n");
        let mut d = doc("");
        no_soak_add(&mut d, &s(&["wget"])).unwrap();
        assert_eq!(d.to_string(), "NO_SOAK = [\"wget\"]\n");
    }

    #[test]
    fn top_level_keys_land_above_the_first_tap_in_a_tap_only_file() {
        let mut d = doc("[[TAP]]\nname = \"a/b\"\nsoak_hours = 72\n");
        set_soak_hours(&mut d, SoakHours::new(48).unwrap());
        no_soak_add(&mut d, &s(&["wget"])).unwrap();
        assert_eq!(
            d.to_string(),
            "SOAK_HOURS = 48\nNO_SOAK = [\"wget\"]\n\n[[TAP]]\nname = \"a/b\"\nsoak_hours = 72\n"
        );
        let parsed = config::parse_file(&d.to_string());
        assert_eq!(parsed.soak_hours.map(|h| h.get()), Some(48));
        assert!(parsed.no_soak.matches("homebrew/core", "wget"));
        assert!(parsed.warnings.is_empty(), "{:?}", parsed.warnings);
    }

    #[test]
    fn tap_only_file_with_a_leading_comment_keeps_it_below_the_new_keys() {
        let mut d = doc("# taps\n[[TAP]]\nname = \"a/b\"\n");
        set_soak_hours(&mut d, SoakHours::new(48).unwrap());
        assert_eq!(
            d.to_string(),
            "SOAK_HOURS = 48\n\n# taps\n[[TAP]]\nname = \"a/b\"\n"
        );
    }

    #[test]
    fn no_soak_add_appends_lowercased_and_skips_present_in_any_case() {
        let mut d = doc(FULL);
        let edit = no_soak_add(&mut d, &s(&["WGET", "Curl", "curl"])).unwrap();
        assert!(edit.changed);
        assert_eq!(
            edit.messages,
            [
                "already in NO_SOAK: wget",
                "added to NO_SOAK: curl",
                "already in NO_SOAK: curl"
            ]
        );
        assert_eq!(
            d.to_string(),
            FULL.replace(
                "NO_SOAK = [\"ericfitz/tap\", \"wget\"] # list",
                "NO_SOAK = [\"ericfitz/tap\", \"wget\", \"curl\"] # list"
            )
        );
    }

    #[test]
    fn no_soak_add_all_present_is_unchanged() {
        let mut d = doc(FULL);
        let edit = no_soak_add(&mut d, &s(&["wget"])).unwrap();
        assert!(!edit.changed);
        assert_eq!(d.to_string(), FULL);
    }

    #[test]
    fn no_soak_add_keeps_multiline_indentation() {
        let mut d = doc("NO_SOAK = [\n  \"wget\",\n  \"curl\",\n]\n");
        no_soak_add(&mut d, &s(&["x"])).unwrap();
        assert_eq!(
            d.to_string(),
            "NO_SOAK = [\n  \"wget\",\n  \"curl\",\n  \"x\",\n]\n"
        );
        let mut d = doc("NO_SOAK = [\"wget\"]\n");
        no_soak_add(&mut d, &s(&["x"])).unwrap();
        assert_eq!(d.to_string(), "NO_SOAK = [\"wget\", \"x\"]\n");
        let mut d = doc("NO_SOAK = []\n");
        no_soak_add(&mut d, &s(&["x"])).unwrap();
        assert_eq!(d.to_string(), "NO_SOAK = [\"x\"]\n");
    }

    #[test]
    fn no_soak_invalid_tokens_change_nothing_and_name_every_bad_token() {
        for op in [no_soak_add, no_soak_remove] {
            let mut d = doc(FULL);
            let err = op(&mut d, &s(&["curl", "a/b/c/d", "", "x//y"])).unwrap_err();
            match err {
                Error::Usage(m) => {
                    assert!(m.contains("\"a/b/c/d\""), "{m}");
                    assert!(m.contains("empty"), "{m}");
                    assert!(m.contains("\"x//y\""), "{m}");
                    assert!(!m.contains("skipped"), "{m}");
                }
                other => panic!("{other:?}"),
            }
            assert_eq!(d.to_string(), FULL);
        }
    }

    #[test]
    fn no_soak_remove_matches_case_insensitively_and_reports_absent() {
        let mut d = doc(FULL);
        let edit = no_soak_remove(&mut d, &s(&["WGET", "nope"])).unwrap();
        assert!(edit.changed);
        assert_eq!(
            edit.messages,
            ["removed from NO_SOAK: wget", "not in NO_SOAK: nope"]
        );
        assert_eq!(
            d.to_string(),
            FULL.replace(
                "NO_SOAK = [\"ericfitz/tap\", \"wget\"] # list",
                "NO_SOAK = [\"ericfitz/tap\"] # list"
            )
        );
    }

    #[test]
    fn no_soak_remove_first_entry_leaves_no_leading_space() {
        let mut d = doc("NO_SOAK = [\"a\", \"b\", \"c\"]\n");
        no_soak_remove(&mut d, &s(&["a"])).unwrap();
        assert_eq!(d.to_string(), "NO_SOAK = [\"b\", \"c\"]\n");
    }

    #[test]
    fn no_soak_remove_keeps_array_padding() {
        let cases = [
            ("[\"a\", \"b\", \"c\"]", "a", "[\"b\", \"c\"]"),
            ("[\"a\", \"b\", \"c\"]", "b", "[\"a\", \"c\"]"),
            ("[\"a\", \"b\", \"c\"]", "c", "[\"a\", \"b\"]"),
            ("[\"a\", \"b\"]", "b", "[\"a\"]"),
            ("[\"a\",\"b\"]", "a", "[\"b\"]"),
            ("[\"a\",\"b\"]", "b", "[\"a\"]"),
            ("[ \"a\", \"b\" ]", "a", "[ \"b\" ]"),
            ("[ \"a\", \"b\" ]", "b", "[ \"a\" ]"),
            ("[\n  \"a\",\n  \"b\"\n]", "a", "[\n  \"b\"\n]"),
            ("[\n  \"a\",\n  \"b\"\n]", "b", "[\n  \"a\"\n]"),
            ("[\n  \"a\",\n  \"b\",\n]", "b", "[\n  \"a\",\n]"),
            ("[\n  \"a\",\n  \"b\",\n]", "a", "[\n  \"b\",\n]"),
        ];
        for (arr, token, want) in cases {
            let mut d = doc(&format!("NO_SOAK = {arr}\n"));
            no_soak_remove(&mut d, &s(&[token])).unwrap();
            assert_eq!(
                d.to_string(),
                format!("NO_SOAK = {want}\n"),
                "{arr} - {token}"
            );
        }
        let mut d = doc("NO_SOAK = [\"a\", \"b\"]\n");
        no_soak_remove(&mut d, &s(&["a", "b"])).unwrap();
        assert_eq!(d.to_string(), "NO_SOAK = []\n");
    }

    #[test]
    fn no_soak_remove_without_key_or_last_entry() {
        let mut d = doc("");
        let edit = no_soak_remove(&mut d, &s(&["wget"])).unwrap();
        assert_eq!(edit, Edit::unchanged("not in NO_SOAK: wget"));
        assert_eq!(d.to_string(), "");
        let mut d = doc("NO_SOAK = [\"wget\"]\n");
        no_soak_remove(&mut d, &s(&["wget"])).unwrap();
        assert_eq!(d.to_string(), "NO_SOAK = []\n");
    }

    #[test]
    fn no_soak_not_an_array_of_strings_is_refused() {
        for contents in [
            "NO_SOAK = \"wget\"\n",
            "NO_SOAK = [1]\n",
            "NO_SOAK = [\"a\", 2]\n",
        ] {
            for op in [no_soak_add, no_soak_remove] {
                let mut d = doc(contents);
                match op(&mut d, &s(&["curl"])) {
                    Err(Error::Refusal(m)) => assert!(m.contains("NO_SOAK"), "{m}"),
                    other => panic!("{contents:?}: {other:?}"),
                }
                assert_eq!(d.to_string(), contents);
            }
        }
    }

    #[test]
    fn misplaced_no_soak_inside_tap_is_left_alone() {
        let mut d = doc("[[TAP]]\nname = \"a/b\"\nNO_SOAK = [\"wget\"]\n");
        no_soak_add(&mut d, &s(&["curl"])).unwrap();
        assert_eq!(
            d.to_string(),
            "NO_SOAK = [\"curl\"]\n\n[[TAP]]\nname = \"a/b\"\nNO_SOAK = [\"wget\"]\n"
        );
        assert_eq!(config::parse_file(&d.to_string()).warnings.len(), 1);
    }

    #[test]
    fn normalize_tap_accepts_two_segments_and_lowercases() {
        assert_eq!(normalize_tap(" HashiCorp/Tap ").unwrap(), "hashicorp/tap");
        assert_eq!(normalize_tap("homebrew/core").unwrap(), "homebrew/core");
        for bad in ["bad", "a/b/c", "/b", "a/", "", "a//b"] {
            match normalize_tap(bad) {
                Err(Error::Usage(m)) => assert!(m.contains("user/repo"), "{bad:?}: {m}"),
                other => panic!("{bad:?}: {other:?}"),
            }
        }
    }

    #[test]
    fn tap_hours_set_updates_the_matching_entry_case_insensitively() {
        let mut d = doc(FULL);
        let edit = tap_hours_set(&mut d, "hashicorp/TAP", SoakHours::new(10).unwrap()).unwrap();
        assert_eq!(edit, Edit::changed_with("hashicorp/tap soak_hours = 10"));
        assert_eq!(
            d.to_string(),
            FULL.replace("soak_hours = 72", "soak_hours = 10"),
            "name keeps the user's casing"
        );
        let again = tap_hours_set(&mut d, "hashicorp/tap", SoakHours::new(10).unwrap()).unwrap();
        assert!(!again.changed, "{again:?}");
        assert!(again.messages[0].contains("already 10"), "{again:?}");
    }

    #[test]
    fn tap_hours_set_adds_soak_hours_to_an_entry_without_one() {
        let mut d = doc(FULL);
        tap_hours_set(&mut d, "cyclonedx/cyclonedx", SoakHours::new(5).unwrap()).unwrap();
        assert!(
            d.to_string()
                .ends_with("[[TAP]]\nname = \"cyclonedx/cyclonedx\"\nsoak_hours = 5\n"),
            "{d}"
        );
    }

    #[test]
    fn tap_hours_set_edits_the_last_duplicate() {
        let mut d = doc("[[TAP]]\nname = \"a/b\"\nsoak_hours = 1\n\n[[TAP]]\nname = \"a/b\"\n");
        tap_hours_set(&mut d, "a/b", SoakHours::new(5).unwrap()).unwrap();
        assert_eq!(
            d.to_string(),
            "[[TAP]]\nname = \"a/b\"\nsoak_hours = 1\n\n[[TAP]]\nname = \"a/b\"\nsoak_hours = 5\n"
        );
        assert_eq!(
            config::parse_file(&d.to_string()).taps[0]
                .soak_hours
                .map(|h| h.get()),
            Some(5),
            "the reader honors the last duplicate, so that is the one edited"
        );
    }

    #[test]
    fn tap_hours_set_appends_a_new_entry_after_the_last() {
        let mut d = doc(FULL);
        let edit = tap_hours_set(&mut d, "New/Tap", SoakHours::new(10).unwrap()).unwrap();
        assert_eq!(
            edit,
            Edit::changed_with("new/tap added with soak_hours = 10")
        );
        assert_eq!(
            d.to_string(),
            format!("{FULL}\n[[TAP]]\nname = \"new/tap\"\nsoak_hours = 10\n")
        );
        let mut d = doc("");
        tap_hours_set(&mut d, "new/tap", SoakHours::new(10).unwrap()).unwrap();
        assert_eq!(
            d.to_string(),
            "[[TAP]]\nname = \"new/tap\"\nsoak_hours = 10\n"
        );
        let mut d = doc("SOAK_HOURS = 48\n");
        tap_hours_set(&mut d, "new/tap", SoakHours::new(10).unwrap()).unwrap();
        assert_eq!(
            d.to_string(),
            "SOAK_HOURS = 48\n\n[[TAP]]\nname = \"new/tap\"\nsoak_hours = 10\n"
        );
    }

    #[test]
    fn tap_entry_without_a_string_name_never_matches() {
        let mut d = doc("[[TAP]]\nname = 5\n\n[[TAP]]\nsoak_hours = 3\n");
        tap_hours_set(&mut d, "a/b", SoakHours::new(5).unwrap()).unwrap();
        assert!(
            d.to_string()
                .ends_with("[[TAP]]\nname = \"a/b\"\nsoak_hours = 5\n"),
            "{d}"
        );
        assert_eq!(d["TAP"].as_array_of_tables().unwrap().len(), 3);
    }

    #[test]
    fn tap_hours_set_rejects_bad_names_without_touching_the_doc() {
        let mut d = doc(FULL);
        for bad in ["bad", "a/b/c", "/b"] {
            assert!(matches!(
                tap_hours_set(&mut d, bad, SoakHours::new(5).unwrap()),
                Err(Error::Usage(_))
            ));
            assert!(matches!(tap_hours_clear(&mut d, bad), Err(Error::Usage(_))));
        }
        assert_eq!(d.to_string(), FULL);
    }

    #[test]
    fn tap_hours_clear_removes_only_soak_hours_when_other_keys_remain() {
        let mut d = doc("[[TAP]]\nname = \"a/b\"\nsoak_hours = 5\nextra = 1\n");
        let edit = tap_hours_clear(&mut d, "A/B").unwrap();
        assert_eq!(
            edit,
            Edit::changed_with("a/b soak_hours removed; it uses SOAK_HOURS")
        );
        assert_eq!(d.to_string(), "[[TAP]]\nname = \"a/b\"\nextra = 1\n");
    }

    #[test]
    fn tap_hours_clear_removes_the_whole_entry_when_only_name_is_left() {
        let mut d = doc(FULL);
        let edit = tap_hours_clear(&mut d, "hashicorp/tap").unwrap();
        assert!(edit.changed);
        assert_eq!(edit.messages.len(), 2, "{edit:?}");
        assert!(edit.messages[1].contains("entry removed"), "{edit:?}");
        let text = d.to_string();
        assert!(!text.contains("HashiCorp"), "{text}");
        assert!(
            text.contains("# taps\n[[TAP]]\nname = \"cyclonedx/cyclonedx\"\n"),
            "{text}"
        );
        assert!(text.starts_with("# header\n"), "{text}");
        assert!(text.contains("unknown = true"), "{text}");
    }

    #[test]
    fn tap_hours_clear_last_entry_leaves_an_empty_document() {
        let mut d = doc("[[TAP]]\nname = \"a/b\"\nsoak_hours = 5\n");
        tap_hours_clear(&mut d, "a/b").unwrap();
        assert!(d.get("TAP").is_none(), "{d}");
        assert!(d.to_string().trim().is_empty(), "{d:?}");
    }

    #[test]
    fn tap_hours_clear_absent_tap_or_absent_hours_is_unchanged() {
        let mut d = doc(FULL);
        let edit = tap_hours_clear(&mut d, "nope/tap").unwrap();
        assert!(!edit.changed);
        assert!(edit.messages[0].contains("no [[TAP]] entry"), "{edit:?}");
        let edit = tap_hours_clear(&mut d, "cyclonedx/cyclonedx").unwrap();
        assert!(!edit.changed);
        assert!(edit.messages[0].contains("no soak_hours"), "{edit:?}");
        assert_eq!(d.to_string(), FULL);
        let mut d = doc("");
        assert!(!tap_hours_clear(&mut d, "a/b").unwrap().changed);
    }

    #[test]
    fn tap_not_an_array_of_tables_is_refused() {
        for contents in ["TAP = 5\n", "[TAP]\nname = \"a/b\"\n"] {
            let mut d = doc(contents);
            match tap_hours_set(&mut d, "a/b", SoakHours::new(5).unwrap()) {
                Err(Error::Refusal(m)) => assert!(m.contains("TAP"), "{m}"),
                other => panic!("{contents:?}: {other:?}"),
            }
            assert!(matches!(
                tap_hours_clear(&mut d, "a/b"),
                Err(Error::Refusal(_))
            ));
            assert_eq!(d.to_string(), contents);
        }
    }

    fn show(contents: Option<&str>, env: Option<&str>) -> String {
        let d = contents.map(doc);
        render_show(
            Path::new("/home/x/.config/brewsoak/config.toml"),
            env,
            d.as_ref(),
        )
    }

    #[test]
    fn render_show_lists_everything_from_the_file() {
        let text = show(Some(FULL), None);
        assert_eq!(
            text,
            "config: /home/x/.config/brewsoak/config.toml\n\
             soak hours: 48 (SOAK_HOURS in the file)\n\
             NO_SOAK:\n  ericfitz/tap\n  wget\n\
             taps:\n  hashicorp/tap: 72 (own)\n  cyclonedx/cyclonedx: 48 (SOAK_HOURS)\n"
        );
    }

    #[test]
    fn render_show_env_overrides_file_and_tap_defaults() {
        let text = show(Some(FULL), Some("36"));
        assert!(
            text.contains("soak hours: 36 (BREWSOAK_SOAK_HOURS)\n"),
            "{text}"
        );
        assert!(
            text.contains("  cyclonedx/cyclonedx: 36 (SOAK_HOURS)\n"),
            "{text}"
        );
        assert!(text.contains("  hashicorp/tap: 72 (own)\n"), "{text}");
    }

    #[test]
    fn render_show_without_a_file() {
        let text = show(None, None);
        assert_eq!(
            text,
            "config: /home/x/.config/brewsoak/config.toml (no config file)\n\
             soak hours: 24 (default)\nNO_SOAK: (none)\ntaps: (none)\n"
        );
    }

    #[test]
    fn render_show_notes_an_invalid_env_value() {
        for bad in ["nope", "0"] {
            let text = show(Some("SOAK_HOURS = 8\n"), Some(bad));
            assert!(
                text.contains("soak hours: 8 (SOAK_HOURS in the file)\n"),
                "{text}"
            );
            assert!(
                text.contains(&format!(
                    "note: BREWSOAK_SOAK_HOURS={bad:?} is not an integer >= 1; ignored\n"
                )),
                "{text}"
            );
        }
    }

    #[test]
    fn render_show_prints_entries_as_written_plus_notes_and_warnings() {
        let text = show(
            Some(
                "NO_SOAK = [\"WGet\", \"a/b/c/d\"]\n\n[[TAP]]\nname = \"a/b\"\nNO_SOAK = [\"x\"]\n\n[[TAP]]\nname = \"bad\"\n",
            ),
            None,
        );
        assert!(
            text.contains("NO_SOAK:\n  WGet\n  a/b/c/d\n"),
            "as written: {text}"
        );
        assert!(
            text.contains("note: config: NO_SOAK entry \"a/b/c/d\""),
            "{text}"
        );
        assert!(
            text.contains("note: config: [[TAP]] name \"bad\""),
            "{text}"
        );
        assert!(
            text.contains("warning: config: NO_SOAK inside [[TAP]] a/b"),
            "{text}"
        );
        assert!(text.contains("  a/b: 24 (SOAK_HOURS)\n"), "{text}");
    }
}
