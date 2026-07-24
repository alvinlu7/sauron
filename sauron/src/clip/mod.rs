//! Agent Clipboard CLI shared by `sauron clip ...` and the standalone `clip`
//! binary.

pub mod store;

use std::collections::{BTreeMap, BTreeSet};
use std::io::{IsTerminal, Read};
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use store::{
    parse_tag_text, preview, ClipResult, Item, ListOptions, PutOptions, Store, UpdateOptions,
};

#[allow(dead_code)] // used by the standalone binary, not by `sauron`
pub fn run_from_env() -> i32 {
    run(std::env::args().skip(1).collect())
}

pub fn run(mut args: Vec<String>) -> i32 {
    let json_mode = args.iter().any(|arg| arg == "--json");
    match execute(&mut args) {
        Ok(code) => code,
        Err(error) => {
            if json_mode {
                let _ = print_json(&json!({
                    "ok": false,
                    "error": "error",
                    "message": error,
                }));
            } else {
                eprintln!("error: {error}");
            }
            1
        }
    }
}

fn execute(args: &mut Vec<String>) -> ClipResult<i32> {
    let db = take_global_db(args)?;
    let command = args.first().cloned().ok_or_else(|| usage().to_string())?;
    args.remove(0);
    if matches!(command.as_str(), "-h" | "--help" | "help") {
        println!("{}", usage());
        return Ok(0);
    }

    let path = db.as_deref().map(Path::new);
    let read_only = matches!(
        command.as_str(),
        "get" | "copy" | "search" | "list" | "recent" | "export" | "stats"
    );
    let mut store = if read_only {
        Store::open_for_read(path)?
    } else {
        Store::open(path)?
    };
    match command.as_str() {
        "put" => put(&mut store, args, false),
        "update" => put(&mut store, args, true),
        "get" => get(&store, args, false),
        "copy" => get(&store, args, true),
        "search" => search(&store, args),
        "list" => list(&store, args, false),
        "recent" => list(&store, args, true),
        "pin" => pin(&mut store, args),
        "delete" => delete(&mut store, args),
        "export" => export(&store, args),
        "import" => import(&mut store, args),
        "stats" => stats(&store, args),
        "doctor" => doctor(&store, args),
        "gc" => gc(&mut store, args),
        _ => Err(format!("unknown command: {command}\n{}", usage())),
    }
}

fn put(store: &mut Store, args: &[String], update: bool) -> ClipResult<i32> {
    let parsed = Parsed::new(
        args,
        &[
            "value",
            "file",
            "namespace",
            "kind",
            "tags",
            "source",
            "ttl",
            "metadata",
            "supersedes-key",
        ],
        &["validated", "unvalidated", "pin", "force-pin", "json"],
    )?;
    let key = parsed
        .positionals
        .first()
        .ok_or_else(|| "key is required".to_string())?;
    let positional = parsed.positionals.get(1).cloned();
    if parsed.positionals.len() > 2 {
        return Err("too many positional values".into());
    }
    let value = read_value(
        positional,
        parsed.options.get("value").cloned(),
        parsed.options.get("file").map(PathBuf::from),
    )?;
    let json_mode = parsed.flags.contains("json");
    if update {
        if parsed.flags.contains("validated") && parsed.flags.contains("unvalidated") {
            return fail("use only one of --validated or --unvalidated", json_mode);
        }
        let validated = if parsed.flags.contains("validated") {
            Some(true)
        } else if parsed.flags.contains("unvalidated") {
            Some(false)
        } else {
            None
        };
        let item = store.update(
            key,
            &value,
            UpdateOptions {
                namespace: parsed.options.get("namespace").cloned(),
                kind: parsed.options.get("kind").cloned(),
                tags: parsed
                    .options
                    .get("tags")
                    .map(|text| parse_tag_text(Some(text))),
                source: parsed
                    .options
                    .get("source")
                    .map(|source| Some(source.clone())),
                ttl: parsed.options.get("ttl").cloned(),
                metadata: parsed
                    .options
                    .get("metadata")
                    .map(|text| parse_metadata(text))
                    .transpose()?,
                validated,
                supersedes_key: parsed
                    .options
                    .get("supersedes-key")
                    .map(|key| Some(key.clone())),
                pinned: parsed.flags.contains("pin").then_some(true),
                force_pin: parsed.flags.contains("force-pin"),
            },
        )?;
        match item {
            Some(item) => emit_item(&item, json_mode),
            None => not_found(key, json_mode),
        }
    } else {
        let validated = parsed.flags.contains("validated").then_some(true);
        let item = store.put(
            key,
            &value,
            PutOptions {
                namespace: parsed
                    .options
                    .get("namespace")
                    .cloned()
                    .unwrap_or_else(|| "default".into()),
                kind: parsed
                    .options
                    .get("kind")
                    .cloned()
                    .unwrap_or_else(|| "fact".into()),
                tags: parse_tag_text(parsed.options.get("tags").map(String::as_str)),
                source: parsed.options.get("source").cloned(),
                ttl: parsed.options.get("ttl").cloned(),
                metadata: parsed
                    .options
                    .get("metadata")
                    .map(|text| parse_metadata(text))
                    .transpose()?
                    .unwrap_or_else(|| json!({})),
                validated,
                supersedes_key: parsed.options.get("supersedes-key").cloned(),
                pinned: parsed.flags.contains("pin"),
                force_pin: parsed.flags.contains("force-pin"),
            },
        )?;
        emit_item(&item, json_mode)
    }
}

fn get(store: &Store, args: &[String], raw: bool) -> ClipResult<i32> {
    let parsed = Parsed::new(args, &[], if raw { &[] } else { &["json"] })?;
    let key = one_positional(&parsed, "key")?;
    match store.get(key)? {
        Some(item) if raw => {
            print!("{}", item.value);
            Ok(0)
        }
        Some(item) if parsed.flags.contains("json") => {
            print_json(&item.to_json())?;
            Ok(0)
        }
        Some(item) => {
            print!("{}", item.value);
            if !item.value.ends_with('\n') {
                println!();
            }
            Ok(0)
        }
        None if parsed.flags.contains("json") => {
            print_json(&json!({"ok": false, "error": "not_found", "key": key}))?;
            Ok(1)
        }
        None => Ok(1),
    }
}

fn search(store: &Store, args: &[String]) -> ClipResult<i32> {
    let parsed = Parsed::new(args, &["namespace", "kind", "tags", "limit"], &["json"])?;
    if parsed.positionals.len() > 1 {
        return Err("search accepts at most one query".into());
    }
    let limit = parsed.usize_option("limit", 20)?;
    let tags = parse_tag_text(parsed.options.get("tags").map(String::as_str));
    let items = store.search(
        parsed.positionals.first().map(String::as_str),
        parsed.options.get("namespace").map(String::as_str),
        parsed.options.get("kind").map(String::as_str),
        &tags,
        limit,
    )?;
    emit_items(&items, parsed.flags.contains("json"))
}

fn list(store: &Store, args: &[String], recent: bool) -> ClipResult<i32> {
    let parsed = if recent {
        Parsed::new(args, &[], &["json"])?
    } else {
        Parsed::new(
            args,
            &["recent", "limit", "namespace", "kind", "tags"],
            &["pinned", "validated", "json"],
        )?
    };
    let limit = if recent {
        match parsed.positionals.as_slice() {
            [] => 20,
            [value] => value
                .parse::<usize>()
                .map_err(|_| format!("invalid limit: {value}"))?,
            _ => return Err("recent accepts at most one limit".into()),
        }
    } else {
        if !parsed.positionals.is_empty() {
            return Err("list does not accept positional arguments".into());
        }
        parsed
            .options
            .get("recent")
            .or_else(|| parsed.options.get("limit"))
            .map(|value| {
                value
                    .parse::<usize>()
                    .map_err(|_| format!("invalid limit: {value}"))
            })
            .transpose()?
            .unwrap_or(20)
    };
    let items = store.list(
        limit,
        &ListOptions {
            namespace: parsed.options.get("namespace").cloned(),
            kind: parsed.options.get("kind").cloned(),
            tags: parse_tag_text(parsed.options.get("tags").map(String::as_str)),
            pinned: parsed.flags.contains("pinned").then_some(true),
            validated: parsed.flags.contains("validated").then_some(true),
        },
    )?;
    emit_items(&items, parsed.flags.contains("json"))
}

fn pin(store: &mut Store, args: &[String]) -> ClipResult<i32> {
    let parsed = Parsed::new(args, &[], &["off", "force", "json"])?;
    let key = one_positional(&parsed, "key")?;
    let json_mode = parsed.flags.contains("json");
    match store.pin(
        key,
        !parsed.flags.contains("off"),
        parsed.flags.contains("force"),
    )? {
        Some(item) if json_mode => {
            print_json(&item.to_json())?;
            Ok(0)
        }
        Some(item) => {
            println!("{}\tpinned={}", item.key, item.pinned);
            Ok(0)
        }
        None => not_found(key, json_mode),
    }
}

fn delete(store: &mut Store, args: &[String]) -> ClipResult<i32> {
    let parsed = Parsed::new(args, &[], &["json"])?;
    let key = one_positional(&parsed, "key")?;
    let deleted = store.delete(key)?;
    if parsed.flags.contains("json") {
        let mut value = json!({"ok": deleted, "deleted": deleted, "key": key});
        if !deleted {
            value["error"] = json!("not_found");
        }
        print_json(&value)?;
    } else if deleted {
        println!("{key}");
    }
    Ok(if deleted { 0 } else { 1 })
}

fn export(store: &Store, args: &[String]) -> ClipResult<i32> {
    let parsed = Parsed::new(
        args,
        &["namespace", "kind", "tags", "format", "limit", "out"],
        &[],
    )?;
    if !parsed.positionals.is_empty() {
        return Err("export does not accept positional arguments".into());
    }
    let output = store.export(
        parsed.options.get("namespace").cloned(),
        parsed.options.get("kind").cloned(),
        parse_tag_text(parsed.options.get("tags").map(String::as_str)),
        parsed
            .options
            .get("format")
            .map(String::as_str)
            .unwrap_or("json"),
        parsed.usize_option("limit", 1000)?,
    )?;
    if let Some(path) = parsed.options.get("out") {
        std::fs::write(path, output).map_err(|e| format!("write {path}: {e}"))?;
    } else {
        print!("{output}");
    }
    Ok(0)
}

fn import(store: &mut Store, args: &[String]) -> ClipResult<i32> {
    let parsed = Parsed::new(args, &[], &["no-overwrite", "json"])?;
    let path = one_positional(&parsed, "file")?;
    let payload = std::fs::read_to_string(path).map_err(|e| format!("read {path}: {e}"))?;
    let count = store.import(&payload, !parsed.flags.contains("no-overwrite"))?;
    if parsed.flags.contains("json") {
        print_json(&json!({"imported": count}))?;
    } else {
        println!("{count}");
    }
    Ok(0)
}

fn stats(store: &Store, args: &[String]) -> ClipResult<i32> {
    let parsed = Parsed::new(args, &[], &["json"])?;
    if !parsed.positionals.is_empty() {
        return Err("stats does not accept positional arguments".into());
    }
    let value = store.stats()?;
    if parsed.flags.contains("json") {
        print_json(&value)?;
    } else {
        println!("items\t{}", value["items"]);
        println!("pinned\t{}", value["pinned"]);
        println!("validated\t{}", value["validated"]);
        println!("fts_enabled\t{}", value["fts_enabled"]);
        emit_counts("namespace", &value["namespaces"]);
        emit_counts("kind", &value["kinds"]);
        emit_counts("tag", &value["tags"]);
    }
    Ok(0)
}

fn doctor(store: &Store, args: &[String]) -> ClipResult<i32> {
    let parsed = Parsed::new(args, &[], &["json"])?;
    if !parsed.positionals.is_empty() {
        return Err("doctor does not accept positional arguments".into());
    }
    let value = store.doctor()?;
    let ok = value["ok"].as_bool().unwrap_or(false);
    if parsed.flags.contains("json") {
        print_json(&value)?;
    } else {
        println!("database\t{}", value["database"].as_str().unwrap_or(""));
        println!("schema_version\t{}", value["schema_version"]);
        println!("ok\t{ok}");
        println!(
            "integrity_check\t{}",
            value["integrity_check"].as_str().unwrap_or("")
        );
    }
    Ok(if ok { 0 } else { 1 })
}

fn gc(store: &mut Store, args: &[String]) -> ClipResult<i32> {
    let parsed = Parsed::new(args, &[], &["json"])?;
    if !parsed.positionals.is_empty() {
        return Err("gc does not accept positional arguments".into());
    }
    let count = store.gc()?;
    if parsed.flags.contains("json") {
        print_json(&json!({"deleted": count}))?;
    } else {
        println!("{count}");
    }
    Ok(0)
}

fn take_global_db(args: &mut Vec<String>) -> ClipResult<Option<String>> {
    let mut db = None;
    let mut index = 0;
    while index < args.len() {
        if matches!(args[index].as_str(), "--db" | "--db-path") {
            if db.is_some() {
                return Err("database path specified more than once".into());
            }
            if index + 1 >= args.len() {
                return Err(format!("{} requires a value", args[index]));
            }
            db = Some(args.remove(index + 1));
            args.remove(index);
        } else if let Some(value) = args[index]
            .strip_prefix("--db=")
            .or_else(|| args[index].strip_prefix("--db-path="))
        {
            if db.is_some() {
                return Err("database path specified more than once".into());
            }
            db = Some(value.to_string());
            args.remove(index);
        } else {
            index += 1;
        }
    }
    Ok(db)
}

fn read_value(
    positional: Option<String>,
    option: Option<String>,
    file: Option<PathBuf>,
) -> ClipResult<String> {
    let sources = usize::from(positional.is_some())
        + usize::from(option.is_some())
        + usize::from(file.is_some());
    if sources > 1 {
        return Err("use only one of positional value, --value, or --file".into());
    }
    if let Some(value) = positional.or(option) {
        return Ok(value);
    }
    if let Some(path) = file {
        return std::fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()));
    }
    if !std::io::stdin().is_terminal() {
        let mut value = String::new();
        std::io::stdin()
            .read_to_string(&mut value)
            .map_err(|e| format!("read stdin: {e}"))?;
        return Ok(value);
    }
    Err("provide a positional value, --value, --file, or stdin".into())
}

fn parse_metadata(text: &str) -> ClipResult<Value> {
    let value =
        serde_json::from_str::<Value>(text).map_err(|e| format!("invalid metadata: {e}"))?;
    if value.is_object() {
        Ok(value)
    } else {
        Err("--metadata must be a JSON object".into())
    }
}

fn emit_item(item: &Item, json_mode: bool) -> ClipResult<i32> {
    if json_mode {
        print_json(&item.to_json())?;
    } else {
        println!("{}", item.key);
    }
    Ok(0)
}

fn emit_items(items: &[Item], json_mode: bool) -> ClipResult<i32> {
    if json_mode {
        print_json(&Value::Array(items.iter().map(Item::to_json).collect()))?;
    } else {
        for item in items {
            let marker = if item.pinned { "*" } else { "-" };
            let validation = if item.validated {
                "validated"
            } else {
                "unvalidated"
            };
            println!(
                "{marker}\t{}\t{}\t{}\t{validation}\t{}\t{}",
                item.key,
                item.namespace,
                item.kind,
                item.tags.join(","),
                preview(&item.value, 80)
            );
        }
    }
    Ok(0)
}

fn not_found(key: &str, json_mode: bool) -> ClipResult<i32> {
    if json_mode {
        print_json(&json!({"ok": false, "error": "not_found", "key": key}))?;
    }
    Ok(1)
}

fn fail(message: &str, json_mode: bool) -> ClipResult<i32> {
    if json_mode {
        print_json(&json!({"ok": false, "error": "error", "message": message}))?;
        Ok(1)
    } else {
        Err(message.to_string())
    }
}

fn print_json(value: &Value) -> ClipResult<()> {
    println!(
        "{}",
        serde_json::to_string_pretty(value).map_err(|e| e.to_string())?
    );
    Ok(())
}

fn emit_counts(prefix: &str, value: &Value) {
    if let Some(map) = value.as_object() {
        for (name, count) in map {
            println!("{prefix}\t{name}\t{count}");
        }
    }
}

fn one_positional<'a>(parsed: &'a Parsed, name: &str) -> ClipResult<&'a str> {
    match parsed.positionals.as_slice() {
        [value] => Ok(value),
        [] => Err(format!("{name} is required")),
        _ => Err(format!("too many positional arguments for {name}")),
    }
}

#[derive(Debug)]
struct Parsed {
    options: BTreeMap<String, String>,
    flags: BTreeSet<String>,
    positionals: Vec<String>,
}

impl Parsed {
    fn new(args: &[String], value_options: &[&str], flag_options: &[&str]) -> ClipResult<Self> {
        let values = value_options.iter().copied().collect::<BTreeSet<_>>();
        let flags = flag_options.iter().copied().collect::<BTreeSet<_>>();
        let mut parsed = Self {
            options: BTreeMap::new(),
            flags: BTreeSet::new(),
            positionals: Vec::new(),
        };
        let mut index = 0;
        while index < args.len() {
            let arg = &args[index];
            if arg == "--" {
                parsed.positionals.extend(args[index + 1..].iter().cloned());
                break;
            }
            if let Some(raw) = arg.strip_prefix("--") {
                let (name, inline) = raw
                    .split_once('=')
                    .map_or((raw, None), |(name, value)| (name, Some(value)));
                if values.contains(name) {
                    let value = match inline {
                        Some(value) => value.to_string(),
                        None => {
                            index += 1;
                            args.get(index)
                                .cloned()
                                .ok_or_else(|| format!("--{name} requires a value"))?
                        }
                    };
                    parsed.options.insert(name.to_string(), value);
                } else if flags.contains(name) && inline.is_none() {
                    parsed.flags.insert(name.to_string());
                } else {
                    return Err(format!("unknown option: --{name}"));
                }
            } else {
                parsed.positionals.push(arg.clone());
            }
            index += 1;
        }
        Ok(parsed)
    }

    fn usize_option(&self, name: &str, default: usize) -> ClipResult<usize> {
        self.options
            .get(name)
            .map(|value| {
                value
                    .parse::<usize>()
                    .map_err(|_| format!("invalid {name}: {value}"))
            })
            .transpose()
            .map(|value| value.unwrap_or(default))
    }
}

fn usage() -> &'static str {
    "usage: clip [--db PATH] <command> [options]

Indexed local key-value clipboard for coding agents.

commands:
  put update get copy search list recent pin delete
  export import stats doctor gc"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_accepts_inline_and_separate_options() {
        let args = vec![
            "query".into(),
            "--namespace=project".into(),
            "--limit".into(),
            "5".into(),
            "--json".into(),
        ];
        let parsed = Parsed::new(&args, &["namespace", "limit"], &["json"]).unwrap();
        assert_eq!(parsed.positionals, ["query"]);
        assert_eq!(parsed.options["namespace"], "project");
        assert_eq!(parsed.usize_option("limit", 20).unwrap(), 5);
        assert!(parsed.flags.contains("json"));
    }
}
