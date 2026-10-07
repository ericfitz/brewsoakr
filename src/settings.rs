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

/// `parse_entry`'s reasons end `; skipped`, which describes the reader, not
/// an edit.
fn trim_skipped(reason: &str) -> String {
    reason.trim_end_matches("; skipped").to_string()
}

/// Every token through `nosoak::parse_entry`, the config reader's parser.
/// All bad tokens go in one usage error; the result is lowercased.
fn validate_tokens(tokens: &[String]) -> Result<Vec<String>, Error> {
    let bad: Vec<String> = tokens
        .iter()
        .filter_map(|t| nosoak::parse_entry(t).err())
        .map(|reason| trim_skipped(&reason))
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

/// `Ok(None)`: no top-level key. `Err`: the key exists but is not an array,
/// which the reader ignores and no editor will clobber.
fn no_soak_any_array(doc: &mut DocumentMut) -> Result<Option<&mut Array>, Error> {
    let Some(item) = doc.get_mut("NO_SOAK") else {
        return Ok(None);
    };
    let lone = lone_valid_string(item);
    match item.as_array_mut() {
        Some(arr) => Ok(Some(arr)),
        None if lone => Err(Error::Refusal(
            "config: NO_SOAK is not an array; run brewsoak settings repair".into(),
        )),
        None => Err(Error::Refusal(
            "config: NO_SOAK is not an array; fix it by hand".into(),
        )),
    }
}

/// Like `no_soak_any_array`, but an array with a non-string element is
/// refused too: add and remove leave it for `settings repair`.
fn no_soak_array(doc: &mut DocumentMut) -> Result<Option<&mut Array>, Error> {
    match no_soak_any_array(doc)? {
        Some(arr) if arr.iter().any(|v| v.as_str().is_none()) => Err(Error::Refusal(
            "config: NO_SOAK contains non-string entries; run brewsoak settings repair".into(),
        )),
        other => Ok(other),
    }
}

/// Drop every element `gone` matches. The first element's prefix and the
/// last one's suffix hold the array's padding (`[ "a"`, `"b"\n]`); hand them
/// to whichever element becomes first or last. Returns how many went.
fn retain_keeping_padding(arr: &mut Array, gone: impl Fn(&Value) -> bool) -> usize {
    let first_gone = arr.get(0).filter(|v| gone(v)).map(|v| prefix_of(v.decor()));
    let last_gone = arr.iter().last().filter(|v| gone(v)).map(|v| {
        v.decor()
            .suffix()
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string()
    });
    let before = arr.len();
    arr.retain(|v| !gone(v));
    let removed = before - arr.len();
    if removed == 0 {
        return 0;
    }
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
    removed
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
        if retain_keeping_padding(arr, matches) == 0 {
            edit.messages.push(format!("not in NO_SOAK: {token}"));
            continue;
        }
        edit.changed = true;
        edit.messages.push(format!("removed from NO_SOAK: {token}"));
    }
    Ok(edit)
}

/// The element as the file spells it, without its padding or comments.
fn as_written(v: &Value) -> String {
    let mut bare = v.clone();
    *bare.decor_mut() = toml_edit::Decor::default();
    bare.to_string()
}

fn decor_text(s: Option<&toml_edit::RawString>) -> String {
    s.and_then(|s| s.as_str()).unwrap_or("").to_string()
}

fn is_multiline(arr: &Array) -> bool {
    arr.trailing().as_str().is_some_and(|s| s.contains('\n'))
        || arr.iter().any(|v| {
            decor_text(v.decor().prefix()).contains('\n')
                || decor_text(v.decor().suffix()).contains('\n')
        })
}

/// Split at the first newline: the text on the previous element's line,
/// and everything from the newline on.
fn split_line(s: &str) -> (&str, &str) {
    s.find('\n').map_or((s, ""), |i| s.split_at(i))
}

/// `retain_keeping_padding` for an array written one entry per line. A
/// comment after an element's comma is stored in the next element's prefix
/// (the last one's in the array's trailing text), so dropping an element
/// must leave its predecessor's comment behind and drop its own.
fn remove_keeping_lines(arr: &mut Array, gone: impl Fn(&Value) -> bool) {
    let mut i = 0;
    while i < arr.len() {
        if !arr.get(i).is_some_and(&gone) {
            i += 1;
            continue;
        }
        let prefix = decor_text(arr.get(i).and_then(|v| v.decor().prefix()));
        let (head, _) = split_line(&prefix);
        if i + 1 < arr.len() {
            let next = decor_text(arr.get(i + 1).and_then(|v| v.decor().prefix()));
            let new = if prefix.contains('\n') && next.contains('\n') {
                format!("{head}{}", split_line(&next).1)
            } else if i == 0 {
                prefix.clone()
            } else {
                next
            };
            if let Some(v) = arr.get_mut(i + 1) {
                v.decor_mut().set_prefix(new);
            }
        } else if arr.trailing_comma() {
            let trailing = decor_text(Some(arr.trailing()));
            if prefix.contains('\n') && trailing.contains('\n') {
                arr.set_trailing(format!("{head}{}", split_line(&trailing).1));
            }
        } else if i > 0 {
            let suffix = decor_text(arr.get(i).and_then(|v| v.decor().suffix()));
            let new = if prefix.contains('\n') && suffix.contains('\n') {
                format!("{head}{}", split_line(&suffix).1)
            } else {
                suffix
            };
            if let Some(v) = arr.get_mut(i - 1) {
                v.decor_mut().set_suffix(new);
            }
        }
        arr.remove(i);
    }
    if arr.is_empty() {
        arr.set_trailing_comma(false);
    }
}

/// Why the reader drops this element, or `None` when it keeps it.
fn invalid_reason(v: &Value) -> Option<String> {
    match v.as_str() {
        None => Some("not a string".into()),
        Some(s) => nosoak::parse_entry(s).err().map(|r| trim_skipped(&r)),
    }
}

const REPAIR_NOTHING: &str = "nothing to repair";

/// A top-level `NO_SOAK` that is a lone string the reader would accept as an
/// entry: `repair` wraps it in an array.
fn lone_valid_string(item: &Item) -> bool {
    item.as_str()
        .is_some_and(|s| nosoak::parse_entry(s).is_ok())
}

/// Step A: wrap a lone valid string in an array, keeping it as written.
fn repair_convert_lone_string(doc: &mut DocumentMut, messages: &mut Vec<String>) {
    let Some(item) = doc.get_mut("NO_SOAK") else {
        return;
    };
    if !lone_valid_string(item) {
        return;
    }
    let Some(old) = item.as_value() else {
        return;
    };
    let decor = old.decor().clone();
    let mut entry = old.clone();
    *entry.decor_mut() = toml_edit::Decor::default();
    messages.push(format!(
        "converted NO_SOAK to an array: {}",
        as_written(&entry)
    ));
    let mut arr = Array::new();
    arr.push_formatted(entry);
    let mut new = Value::Array(arr);
    *new.decor_mut() = decor;
    *item = Item::Value(new);
}

/// Step B: drop the elements of the top-level array the reader rejects.
fn repair_top_array(doc: &mut DocumentMut, messages: &mut Vec<String>) {
    let Some(arr) = doc.get_mut("NO_SOAK").and_then(Item::as_array_mut) else {
        return;
    };
    let before = messages.len();
    messages.extend(arr.iter().filter_map(|v| {
        invalid_reason(v)
            .map(|reason| format!("removed from NO_SOAK: {} ({reason})", as_written(v)))
    }));
    if messages.len() == before {
        return;
    }
    if is_multiline(arr) {
        remove_keeping_lines(arr, |v| invalid_reason(v).is_some());
    } else {
        retain_keeping_padding(arr, |v| invalid_reason(v).is_some());
    }
}

/// One element of a `NO_SOAK` found inside a `[[TAP]]` table.
enum Misplaced {
    Valid(String),
    Invalid { written: String, reason: String },
}

fn misplaced_pieces(item: &Item) -> Vec<Misplaced> {
    let piece = |v: &Value| match invalid_reason(v) {
        Some(reason) => Misplaced::Invalid {
            written: as_written(v),
            reason,
        },
        None => Misplaced::Valid(v.as_str().unwrap_or_default().to_string()),
    };
    match item {
        Item::Value(Value::Array(arr)) => arr.iter().map(piece).collect(),
        Item::Value(v) => vec![piece(v)],
        other => vec![Misplaced::Invalid {
            written: other.type_name().to_string(),
            reason: "not a string".into(),
        }],
    }
}

fn tap_label(table: &Table) -> String {
    table
        .get("name")
        .and_then(Item::as_str)
        .unwrap_or("(unnamed)")
        .to_string()
}

/// Remove `key` from the `[[TAP]]` table at `i`. A comment above it goes to
/// the next key, else to the next `[[TAP]]`, else to the end of the file.
fn remove_tap_key(doc: &mut DocumentMut, i: usize, key: &str) {
    let tables = doc
        .get_mut("TAP")
        .and_then(Item::as_array_of_tables_mut)
        .expect("TAP was checked");
    let table = tables.get_mut(i).expect("index in range");
    let Some(idx) = table.iter().position(|(k, _)| k == key) else {
        return;
    };
    let why = table
        .key(key)
        .map(|k| prefix_of(k.leaf_decor()))
        .unwrap_or_default();
    table.remove(key);
    if why.is_empty() {
        return;
    }
    let carried = table.iter_mut().nth(idx).is_some_and(|(mut next, item)| {
        item.is_value() && {
            let old = prefix_of(next.leaf_decor());
            next.leaf_decor_mut().set_prefix(format!("{why}{old}"));
            true
        }
    });
    if !carried {
        carry_to_tap(doc, i + 1, &why);
    }
}

/// Remove the `[[TAP]]` table at `i`; its comments go to the next table.
fn remove_tap_table(doc: &mut DocumentMut, i: usize) {
    let tables = doc
        .get_mut("TAP")
        .and_then(Item::as_array_of_tables_mut)
        .expect("TAP was checked");
    let removed = prefix_of(tables.get(i).expect("index in range").decor());
    tables.remove(i);
    if tables.is_empty() {
        doc.as_table_mut().remove("TAP");
    }
    carry_to_tap(doc, i, &removed);
}

/// Step C: move the valid entries of a `NO_SOAK` written inside a `[[TAP]]`
/// table to the top-level list and delete the misplaced key.
fn repair_misplaced_no_soak(doc: &mut DocumentMut, messages: &mut Vec<String>) {
    let had_values = root_has_values(doc);
    let mut created = false;
    let count = doc
        .get("TAP")
        .and_then(Item::as_array_of_tables)
        .map_or(0, ArrayOfTables::len);
    for i in 0..count {
        let found = doc
            .get("TAP")
            .and_then(Item::as_array_of_tables)
            .and_then(|t| t.get(i))
            .and_then(|t| {
                t.get("NO_SOAK")
                    .map(|item| (tap_label(t), misplaced_pieces(item)))
            });
        let Some((label, pieces)) = found else {
            continue;
        };
        if pieces.is_empty() {
            messages.push(format!("removed empty NO_SOAK from [[TAP]] {label}"));
        }
        for piece in pieces {
            match piece {
                Misplaced::Invalid { written, reason } => messages.push(format!(
                    "removed from [[TAP]] {label} NO_SOAK: {written} ({reason})"
                )),
                Misplaced::Valid(entry) => {
                    if doc.get("NO_SOAK").is_none() {
                        doc["NO_SOAK"] = value(Array::new());
                        created = true;
                    }
                    let arr = doc
                        .get_mut("NO_SOAK")
                        .and_then(Item::as_array_mut)
                        .expect("top-level NO_SOAK is an array here");
                    if no_soak_contains(arr, entry.trim()) {
                        messages.push(format!("already in NO_SOAK: {entry}"));
                    } else {
                        push_entry(arr, &entry);
                        messages.push(format!("moved to NO_SOAK from [[TAP]] {label}: {entry}"));
                    }
                }
            }
        }
        remove_tap_key(doc, i, "NO_SOAK");
    }
    if created {
        separate_taps(doc, had_values);
    }
}

/// The reader's name rule: a string of exactly two non-empty segments.
/// `Err` carries the reader's reason, without `; skipped`.
fn reader_tap_name(table: &Table) -> Result<String, String> {
    let Some(name) = table.get("name").and_then(Item::as_str) else {
        return Err("[[TAP]] entry is missing name".into());
    };
    let lower = name.trim().to_ascii_lowercase();
    if lower.split('/').count() != 2 || lower.split('/').any(str::is_empty) {
        return Err(format!("[[TAP]] name {name:?} is not user/repo"));
    }
    Ok(lower)
}

fn tap_len(doc: &DocumentMut) -> usize {
    doc.get("TAP")
        .and_then(Item::as_array_of_tables)
        .map_or(0, ArrayOfTables::len)
}

fn tap_table(doc: &DocumentMut, i: usize) -> &Table {
    doc.get("TAP")
        .and_then(Item::as_array_of_tables)
        .and_then(|t| t.get(i))
        .expect("index in range")
}

/// Step D: drop `[[TAP]]` entries and keys the reader rejects, and earlier
/// duplicates (the reader lets the last one win).
fn repair_taps(doc: &mut DocumentMut, messages: &mut Vec<String>) {
    // Bad or missing names: the whole table goes.
    let mut i = 0;
    while i < tap_len(doc) {
        match reader_tap_name(tap_table(doc, i)) {
            Ok(_) => i += 1,
            Err(reason) => {
                messages.push(format!("removed [[TAP]] entry: {reason}"));
                remove_tap_table(doc, i);
            }
        }
    }
    // Duplicates are judged as the reader does, before bad soak_hours is
    // dropped, so the entry that wins does not change.
    let mut i = 0;
    while i < tap_len(doc) {
        let me = reader_tap_name(tap_table(doc, i)).expect("names were checked");
        let later = (i + 1..tap_len(doc))
            .any(|j| reader_tap_name(tap_table(doc, j)).is_ok_and(|other| other == me));
        if later {
            messages.push(format!(
                "removed duplicate [[TAP]] {me} (the last one is kept)"
            ));
            remove_tap_table(doc, i);
        } else {
            i += 1;
        }
    }
    let mut i = 0;
    while i < tap_len(doc) {
        let table = tap_table(doc, i);
        let bad = table.get("soak_hours").filter(|item| {
            item.as_integer()
                .and_then(|n| u32::try_from(n).ok())
                .and_then(SoakHours::new)
                .is_none()
        });
        let Some(bad) = bad else {
            i += 1;
            continue;
        };
        let written = bad
            .as_value()
            .map_or_else(|| bad.type_name().to_string(), as_written);
        let name = reader_tap_name(table).expect("names were checked");
        messages.push(format!(
            "removed [[TAP]] {name} soak_hours {written}: not an integer >= 1"
        ));
        remove_tap_key(doc, i, "soak_hours");
        let table = tap_table(doc, i);
        if table.len() == 1 && table.contains_key("name") {
            remove_tap_table(doc, i);
        } else {
            i += 1;
        }
    }
}

/// Make the file's `NO_SOAK` and `[[TAP]]` entries what the reader accepts,
/// so it stops noting or warning about them. In order: a lone valid string
/// becomes a one-element array; invalid elements leave the top-level array;
/// entries of a misplaced `NO_SOAK` inside `[[TAP]]` move to the top-level
/// list; invalid `[[TAP]]` entries, bad `soak_hours` and earlier duplicates
/// go. Valid entries keep their text, order, comments and layout. A value
/// that cannot be repaired is refused before anything is touched.
pub fn no_soak_repair(doc: &mut DocumentMut) -> Result<Edit, Error> {
    if let Some(item) = doc.get("NO_SOAK")
        && !item.is_array()
        && !lone_valid_string(item)
    {
        return Err(Error::Refusal(
            "config: NO_SOAK is not an array of strings and cannot be repaired; fix it by hand"
                .into(),
        ));
    }
    tap_tables(doc)?;
    let before = doc.to_string();
    let mut messages = Vec::new();
    repair_convert_lone_string(doc, &mut messages);
    repair_top_array(doc, &mut messages);
    repair_misplaced_no_soak(doc, &mut messages);
    repair_taps(doc, &mut messages);
    // The document, not the message list, says whether anything changed, so
    // a step that edits without reporting is still written (and visible).
    if doc.to_string() == before {
        return Ok(Edit::unchanged(REPAIR_NOTHING));
    }
    if messages.is_empty() {
        messages.push("repaired the file".into());
    }
    Ok(Edit {
        changed: true,
        messages,
    })
}

/// Same rule as the `[[TAP]]` reader: exactly two non-empty segments.
pub fn normalize_tap(raw: &str) -> Result<String, Error> {
    let lower = raw.trim().to_ascii_lowercase();
    if lower.split('/').count() != 2 || lower.split('/').any(str::is_empty) {
        return Err(Error::Usage(format!("tap must be user/repo, got {raw:?}")));
    }
    Ok(lower)
}

/// `Ok(None)`: no `TAP` key, or an empty `TAP = []`. `Err`: it exists but is not `[[TAP]]` tables.
fn tap_tables(doc: &mut DocumentMut) -> Result<Option<&mut ArrayOfTables>, Error> {
    let Some(item) = doc.get_mut("TAP") else {
        return Ok(None);
    };
    if item.as_array().is_some_and(Array::is_empty) {
        return Ok(None);
    }
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

    fn repair(contents: &str) -> (DocumentMut, Result<Edit, Error>) {
        let mut d = doc(contents);
        let r = no_soak_repair(&mut d);
        (d, r)
    }

    #[test]
    fn repair_removes_invalid_strings_and_keeps_valid_ones_as_written() {
        let (d, r) =
            repair("NO_SOAK = [\"WGet\", \"a//b\", \" Ericfitz/Tap \", \"a/b/c/d\", \"\"]\n");
        let edit = r.unwrap();
        assert!(edit.changed);
        assert_eq!(
            edit.messages,
            [
                "removed from NO_SOAK: \"a//b\" (NO_SOAK entry \"a//b\" has an empty path segment)",
                "removed from NO_SOAK: \"a/b/c/d\" (NO_SOAK entry \"a/b/c/d\" has more than two slashes)",
                "removed from NO_SOAK: \"\" (NO_SOAK entry is empty)",
            ]
        );
        assert_eq!(d.to_string(), "NO_SOAK = [\"WGet\", \" Ericfitz/Tap \"]\n");
    }

    #[test]
    fn repair_removes_non_string_elements() {
        let (d, r) = repair("NO_SOAK = [\"wget\", 3, true, [1], { a = 1 }]\n");
        let edit = r.unwrap();
        assert!(edit.changed);
        assert_eq!(
            edit.messages,
            [
                "removed from NO_SOAK: 3 (not a string)",
                "removed from NO_SOAK: true (not a string)",
                "removed from NO_SOAK: [1] (not a string)",
                "removed from NO_SOAK: { a = 1 } (not a string)",
            ]
        );
        assert_eq!(d.to_string(), "NO_SOAK = [\"wget\"]\n");
    }

    #[test]
    fn repair_with_nothing_to_do_is_unchanged() {
        for contents in [
            "",
            "SOAK_HOURS = 6\n",
            "NO_SOAK = []\n",
            "NO_SOAK = [\"wget\", \"A/B\"]\n",
        ] {
            let (d, r) = repair(contents);
            assert_eq!(
                r.unwrap(),
                Edit::unchanged("nothing to repair"),
                "{contents:?}"
            );
            assert_eq!(d.to_string(), contents);
        }
    }

    #[test]
    fn repair_keeps_comments() {
        let (d, r) = repair(
            "# why\nNO_SOAK = [\n  \"wget\", # keep\n  \"a//b\", # gone\n  \"curl\", # also keep\n] # tail\n",
        );
        assert!(r.unwrap().changed);
        assert_eq!(
            d.to_string(),
            "# why\nNO_SOAK = [\n  \"wget\", # keep\n  \"curl\", # also keep\n] # tail\n"
        );
    }

    #[test]
    fn repair_keeps_the_comments_of_surviving_elements_at_every_position() {
        let cases = [
            (
                "[ # open\n  \"\", # gone\n  \"a\", # c1\n  # lead b\n  \"b\", # c2\n  # end\n]",
                "[ # open\n  \"a\", # c1\n  # lead b\n  \"b\", # c2\n  # end\n]",
            ),
            (
                "[\n  \"a\", # c1\n  \"b//c\", # gone\n]",
                "[\n  \"a\", # c1\n]",
            ),
            (
                "[\n  \"a\", # c1\n  \"b//c\" # gone\n]",
                "[\n  \"a\" # c1\n]",
            ),
            (
                "[\n  \"a\", # c1\n  \"\", # gone1\n  \"b//c\", # gone2\n  \"d\", # c4\n]",
                "[\n  \"a\", # c1\n  \"d\", # c4\n]",
            ),
        ];
        for (arr, want) in cases {
            let (d, r) = repair(&format!("NO_SOAK = {arr}\n"));
            assert!(r.unwrap().changed, "{arr}");
            assert_eq!(d.to_string(), format!("NO_SOAK = {want}\n"), "{arr}");
        }
    }

    #[test]
    fn repair_keeps_array_layout() {
        let cases = [
            ("[ \"\", \"wget\", \"a//b\" ]", "[ \"wget\" ]"),
            (
                "[ \"\", \"a\", \"x/\", \"b\", \"c//d\" ]",
                "[ \"a\", \"b\" ]",
            ),
            ("[\"\",\"a\"]", "[\"a\"]"),
            ("[\"a\",\"\"]", "[\"a\"]"),
            ("[ \"\", \"a//b\" ]", "[]"),
            (
                "[\n  \"\",\n  \"a\",\n  \"b//c\",\n  \"d\",\n  \"e/f/g/h\",\n]",
                "[\n  \"a\",\n  \"d\",\n]",
            ),
            (
                "[\n  \"\",\n  \"a\",\n  \"b//c\",\n  \"d\",\n  \"e/f/g/h\"\n]",
                "[\n  \"a\",\n  \"d\"\n]",
            ),
            ("[\n  \"a\",\n  3,\n]", "[\n  \"a\",\n]"),
        ];
        for (arr, want) in cases {
            let (d, r) = repair(&format!("NO_SOAK = {arr}\n"));
            assert!(r.unwrap().changed, "{arr}");
            assert_eq!(d.to_string(), format!("NO_SOAK = {want}\n"), "{arr}");
        }
    }

    #[test]
    fn repair_converts_a_lone_valid_string_to_an_array() {
        let (d, r) = repair("# c\nNO_SOAK = \"WGet\" # why\n");
        let edit = r.unwrap();
        assert_eq!(edit.messages, ["converted NO_SOAK to an array: \"WGet\""]);
        assert_eq!(d.to_string(), "# c\nNO_SOAK = [\"WGet\"] # why\n");
    }

    #[test]
    fn repair_refuses_what_it_cannot_convert_and_changes_nothing() {
        for contents in [
            "NO_SOAK = \"a//b\"\n",
            "NO_SOAK = \"\"\n",
            "NO_SOAK = 3\n",
            "NO_SOAK = true\n",
            "NO_SOAK = { a = 1 }\n",
            "[NO_SOAK]\na = 1\n",
        ] {
            let (d, r) = repair(contents);
            match r {
                Err(Error::Refusal(m)) => assert_eq!(
                    m,
                    "config: NO_SOAK is not an array of strings and cannot be repaired; fix it by hand"
                ),
                other => panic!("{contents:?}: {other:?}"),
            }
            assert_eq!(d.to_string(), contents);
        }
        let contents = "NO_SOAK = [\"a//b\"]\nTAP = 5\n";
        let (d, r) = repair(contents);
        match r {
            Err(Error::Refusal(m)) => assert!(m.contains("TAP is not an array of tables"), "{m}"),
            other => panic!("{other:?}"),
        }
        assert_eq!(d.to_string(), contents, "refused before any edit");
    }

    #[test]
    fn repair_moves_a_misplaced_no_soak_and_dedupes() {
        let (d, r) = repair(
            "NO_SOAK = [\"Wget\"]\n\n[[TAP]]\nname = \"a/b\"\n# misplaced\nNO_SOAK = [\"wget\", \"Curl\", \"x//y\", 3, \"curl\"]\nsoak_hours = 5\n\n[[TAP]]\nname = \"c/d\"\nNO_SOAK = \"jq\"\n\n[[TAP]]\nNO_SOAK = 7\n",
        );
        let edit = r.unwrap();
        assert_eq!(
            edit.messages,
            [
                "already in NO_SOAK: wget",
                "moved to NO_SOAK from [[TAP]] a/b: Curl",
                "removed from [[TAP]] a/b NO_SOAK: \"x//y\" (NO_SOAK entry \"x//y\" has an empty path segment)",
                "removed from [[TAP]] a/b NO_SOAK: 3 (not a string)",
                "already in NO_SOAK: curl",
                "moved to NO_SOAK from [[TAP]] c/d: jq",
                "removed from [[TAP]] (unnamed) NO_SOAK: 7 (not a string)",
                "removed [[TAP]] entry: [[TAP]] entry is missing name",
            ]
        );
        assert_eq!(
            d.to_string(),
            "NO_SOAK = [\"Wget\", \"Curl\", \"jq\"]\n\n[[TAP]]\nname = \"a/b\"\n# misplaced\nsoak_hours = 5\n\n[[TAP]]\nname = \"c/d\"\n"
        );
    }

    #[test]
    fn repair_creates_the_top_level_no_soak_above_the_first_tap() {
        let (d, r) = repair("[[TAP]]\nname = \"a/b\"\nNO_SOAK = [\"wget\"]\n");
        assert!(r.unwrap().changed);
        assert_eq!(
            d.to_string(),
            "NO_SOAK = [\"wget\"]\n\n[[TAP]]\nname = \"a/b\"\n"
        );
        let (d, r) = repair("SOAK_HOURS = 2\n\n[[TAP]]\nname = \"a/b\"\nNO_SOAK = [\"wget\"]\n");
        assert!(r.unwrap().changed);
        assert_eq!(
            d.to_string(),
            "SOAK_HOURS = 2\nNO_SOAK = [\"wget\"]\n\n[[TAP]]\nname = \"a/b\"\n"
        );
    }

    #[test]
    fn repair_with_only_invalid_misplaced_entries_creates_nothing() {
        let (d, r) = repair("[[TAP]]\nname = \"a/b\"\nNO_SOAK = [\"\"]\n");
        assert!(r.unwrap().changed);
        assert_eq!(d.to_string(), "[[TAP]]\nname = \"a/b\"\n");
    }

    #[test]
    fn repair_salvages_a_misplaced_no_soak_from_a_table_it_removes() {
        let (d, r) = repair("[[TAP]]\nname = \"bad\"\nNO_SOAK = [\"wget\"]\n");
        let edit = r.unwrap();
        assert_eq!(
            edit.messages,
            [
                "moved to NO_SOAK from [[TAP]] bad: wget",
                "removed [[TAP]] entry: [[TAP]] name \"bad\" is not user/repo",
            ]
        );
        assert_eq!(d.to_string(), "NO_SOAK = [\"wget\"]\n");
    }

    #[test]
    fn repair_removes_taps_with_a_missing_or_bad_name() {
        let (d, r) = repair(
            "# one\n[[TAP]]\nsoak_hours = 5\n\n[[TAP]]\nname = 3\n\n[[TAP]]\nname = \"a/b/c\"\n\n# keep\n[[TAP]]\nname = \"ok/tap\"\n",
        );
        let edit = r.unwrap();
        assert_eq!(
            edit.messages,
            [
                "removed [[TAP]] entry: [[TAP]] entry is missing name",
                "removed [[TAP]] entry: [[TAP]] entry is missing name",
                "removed [[TAP]] entry: [[TAP]] name \"a/b/c\" is not user/repo",
            ]
        );
        let text = d.to_string();
        assert!(
            text.ends_with("# keep\n[[TAP]]\nname = \"ok/tap\"\n"),
            "{text:?}"
        );
        assert!(text.contains("# one"), "{text:?}");
        assert!(
            !text.contains("soak_hours") && !text.contains("a/b/c"),
            "{text:?}"
        );
    }

    #[test]
    fn repair_drops_a_bad_soak_hours_and_a_table_left_with_only_a_name() {
        let (d, r) = repair(
            "[[TAP]]\nname = \"a/b\"\nsoak_hours = 0\n\n[[TAP]]\nname = \"C/D\"\n# why\nsoak_hours = \"x\"\nextra = 1\n\n[[TAP]]\nname = \"e/f\"\nsoak_hours = 9\n",
        );
        let edit = r.unwrap();
        assert_eq!(
            edit.messages,
            [
                "removed [[TAP]] a/b soak_hours 0: not an integer >= 1",
                "removed [[TAP]] c/d soak_hours \"x\": not an integer >= 1",
            ]
        );
        assert_eq!(
            d.to_string(),
            "\n[[TAP]]\nname = \"C/D\"\n# why\nextra = 1\n\n[[TAP]]\nname = \"e/f\"\nsoak_hours = 9\n"
        );
    }

    #[test]
    fn repair_reports_an_empty_misplaced_no_soak_it_removes() {
        for (contents, want) in [
            (
                "[[TAP]]\nname = \"a/b\"\nNO_SOAK = []\n",
                "[[TAP]]\nname = \"a/b\"\n",
            ),
            (
                "[[TAP]]\nname = \"a/b\"\nsoak_hours = 0\nNO_SOAK = []\n",
                "",
            ),
        ] {
            let (d, r) = repair(contents);
            let edit = r.unwrap();
            assert!(edit.changed, "{contents:?}");
            assert_eq!(
                edit.messages[0], "removed empty NO_SOAK from [[TAP]] a/b",
                "{contents:?}"
            );
            assert_eq!(d.to_string(), want);
        }
    }

    #[test]
    fn repair_names_the_type_of_a_table_valued_soak_hours() {
        let (d, r) = repair("[[TAP]]\nname = \"a/b\"\n[TAP.soak_hours]\nx = 1\n");
        let edit = r.unwrap();
        assert_eq!(
            edit.messages,
            ["removed [[TAP]] a/b soak_hours table: not an integer >= 1"]
        );
        assert!(edit.messages.iter().all(|m| !m.contains('\n')));
        assert_eq!(d.to_string(), "");
    }

    #[test]
    fn an_empty_tap_array_means_no_taps() {
        let (d, r) = repair("TAP = []\n");
        assert_eq!(r.unwrap(), Edit::unchanged("nothing to repair"));
        assert_eq!(d.to_string(), "TAP = []\n");
        let mut d = doc("TAP = []\n");
        let edit = tap_hours_clear(&mut d, "a/b").unwrap();
        assert!(!edit.changed);
        let edit = tap_hours_set(&mut d, "a/b", SoakHours::new(5).unwrap()).unwrap();
        assert!(edit.changed);
        assert!(config::parse_file(&d.to_string()).notes.is_empty(), "{d}");
        let mut d = doc("TAP = [{ name = \"a/b\" }]\n");
        assert!(matches!(
            tap_hours_clear(&mut d, "a/b"),
            Err(Error::Refusal(_))
        ));
    }

    #[test]
    fn add_and_remove_on_a_lone_valid_string_point_at_repair() {
        for op in [no_soak_add, no_soak_remove] {
            let contents = "NO_SOAK = \"wget\"\n";
            let mut d = doc(contents);
            match op(&mut d, &s(&["curl"])) {
                Err(Error::Refusal(m)) => {
                    assert_eq!(
                        m,
                        "config: NO_SOAK is not an array; run brewsoak settings repair"
                    );
                }
                other => panic!("{other:?}"),
            }
            assert_eq!(d.to_string(), contents);
            for contents in ["NO_SOAK = \"a//b\"\n", "NO_SOAK = 3\n"] {
                let mut d = doc(contents);
                match op(&mut d, &s(&["curl"])) {
                    Err(Error::Refusal(m)) => assert!(m.ends_with("fix it by hand"), "{m}"),
                    other => panic!("{other:?}"),
                }
            }
        }
    }

    #[test]
    fn repair_keeps_the_last_duplicate_tap() {
        let (d, r) = repair(
            "[[TAP]]\nname = \"a/b\"\nsoak_hours = 5\n\n[[TAP]]\nname = \"c/d\"\n\n[[TAP]]\nname = \" A/B \"\nsoak_hours = 7\n",
        );
        let edit = r.unwrap();
        assert_eq!(
            edit.messages,
            ["removed duplicate [[TAP]] a/b (the last one is kept)"]
        );
        assert_eq!(
            d.to_string(),
            "\n[[TAP]]\nname = \"c/d\"\n\n[[TAP]]\nname = \" A/B \"\nsoak_hours = 7\n"
        );
        // The winner keeps its own state even when its soak_hours is bad:
        // the reader would already have used SOAK_HOURS for it.
        let (d, r) = repair(
            "[[TAP]]\nname = \"a/b\"\nsoak_hours = 5\n\n[[TAP]]\nname = \"a/b\"\nsoak_hours = 0\nx = 1\n",
        );
        assert_eq!(r.unwrap().messages.len(), 2);
        assert_eq!(d.to_string(), "\n[[TAP]]\nname = \"a/b\"\nx = 1\n");
    }

    #[test]
    fn repair_of_a_messy_file_leaves_nothing_for_the_reader_to_report() {
        let messy = "# top\nNO_SOAK = \"Wget\"\nSOAK_HOURS = 12\n\n[[TAP]]\nname = \"a/b\"\nNO_SOAK = [\"curl\", \"\", 4]\nsoak_hours = 0\n\n[[TAP]]\nname = \"nope\"\n\n[[TAP]]\nsoak_hours = 2\n\n[[TAP]]\nname = \"A/B\"\nsoak_hours = 3\n\n[[TAP]]\nname = \"c/d\"\nsoak_hours = \"x\"\n";
        assert!(!config::parse_file(messy).notes.is_empty());
        let (mut d, r) = repair(messy);
        assert!(r.unwrap().changed);
        let parsed = config::parse_file(&d.to_string());
        assert!(parsed.notes.is_empty(), "{:?}\n{d}", parsed.notes);
        assert!(parsed.warnings.is_empty(), "{:?}\n{d}", parsed.warnings);
        assert!(parsed.no_soak.matches("homebrew/core", "wget"));
        assert!(parsed.no_soak.matches("homebrew/core", "curl"));
        assert_eq!(parsed.soak_hours, SoakHours::new(12));
        let once = d.to_string();
        assert_eq!(
            no_soak_repair(&mut d).unwrap(),
            Edit::unchanged("nothing to repair")
        );
        assert_eq!(d.to_string(), once, "second run");
    }

    #[test]
    fn repair_twice_is_a_no_op() {
        let (mut d, r) = repair("NO_SOAK = [\"wget\", \"\", 3]\n");
        assert!(r.unwrap().changed);
        let once = d.to_string();
        let edit = no_soak_repair(&mut d).unwrap();
        assert_eq!(edit, Edit::unchanged("nothing to repair"));
        assert_eq!(d.to_string(), once);
    }

    #[test]
    fn add_and_remove_on_a_mixed_array_point_at_repair() {
        for op in [no_soak_add, no_soak_remove] {
            let contents = "NO_SOAK = [\"a\", 2]\n";
            let mut d = doc(contents);
            match op(&mut d, &s(&["curl"])) {
                Err(Error::Refusal(m)) => assert_eq!(
                    m,
                    "config: NO_SOAK contains non-string entries; run brewsoak settings repair"
                ),
                other => panic!("{other:?}"),
            }
            assert_eq!(d.to_string(), contents);
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
