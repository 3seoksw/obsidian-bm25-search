use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, hash_map::DefaultHasher},
    env,
    error::Error,
    fs,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
};
use tantivy::{
    Index, IndexWriter, TantivyDocument, Term,
    collector::TopDocs,
    doc,
    query::QueryParser,
    schema::{Field, STORED, STRING, Schema, TEXT, Value},
};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

struct Fields {
    path: Field,
    heading: Field,
    body: Field,
    start: Field,
    end: Field,
}
fn schema() -> (Schema, Fields) {
    let mut b = Schema::builder();
    let fields = Fields {
        path: b.add_text_field("path", STRING | STORED),
        heading: b.add_text_field("heading", TEXT | STORED),
        body: b.add_text_field("body", TEXT | STORED),
        start: b.add_u64_field("start_line", STORED),
        end: b.add_u64_field("end_line", STORED),
    };
    (b.build(), fields)
}

struct Section {
    heading: String,
    body: String,
    start: usize,
    end: usize,
}
fn sections(text: &str) -> Vec<Section> {
    let lines: Vec<_> = text.lines().collect();
    let mut start = 0;
    if lines.first() == Some(&"---")
        && let Some(end) = lines.iter().skip(1).position(|line| *line == "---")
    {
        start = end + 2;
    }
    let mut output = Vec::new();
    let mut heading = String::new();
    let mut section_start = start;
    let mut fence: Option<(char, usize)> = None;
    for (i, line) in lines.iter().enumerate().skip(start) {
        let trimmed = line.trim_start();
        let marker = trimmed.chars().next().unwrap_or(' ');
        let count = trimmed.chars().take_while(|c| *c == marker).count();
        if let Some((open, length)) = fence {
            if marker == open && count >= length && trimmed[count..].trim().is_empty() {
                fence = None;
            }
            continue;
        }
        if (marker == '`' || marker == '~') && count >= 3 {
            fence = Some((marker, count));
            continue;
        }
        let hashes = line.chars().take_while(|c| *c == '#').count();
        if (1..=6).contains(&hashes) && (*line).as_bytes().get(hashes) == Some(&b' ') {
            let body = lines[section_start..i].join("\n");
            if !body.trim().is_empty() {
                output.push(Section {
                    heading,
                    body,
                    start: section_start + 1,
                    end: i,
                });
            }
            heading = line[hashes..].trim().to_owned();
            section_start = i;
        }
    }
    let body = lines[section_start..].join("\n");
    if !body.trim().is_empty() {
        output.push(Section {
            heading,
            body,
            start: section_start + 1,
            end: lines.len(),
        });
    }
    output
}

fn markdown_files(dir: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    let mut entries = fs::read_dir(dir)?.collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            continue;
        }
        if kind.is_dir() {
            markdown_files(&entry.path(), files)?;
        } else if kind.is_file()
            && entry
                .path()
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
        {
            files.push(entry.path());
        }
    }
    Ok(())
}

// Resolve existing ancestors before creating anything, including symlinked parents.
fn resolved_destination(path: &Path) -> Result<PathBuf> {
    if fs::symlink_metadata(path).is_ok() {
        return Ok(path.canonicalize()?);
    }
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let name = path.file_name().ok_or("Index path must name a directory")?;
    Ok(resolved_destination(parent)?.join(name))
}

const MANIFEST: &str = "osb-manifest.json";

// Remembers what each note looked like when it was indexed, so `sync` can touch only what changed.
// It lives in the index directory, outside the vault.
#[derive(Serialize, Deserialize)]
struct Manifest {
    vault: PathBuf,
    files: BTreeMap<String, String>,
}

// shortcut: DefaultHasher is not guaranteed stable across Rust releases. A change only makes the
// next sync re-index every note once.
fn fingerprint(text: &str) -> String {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    format!("{:016x}-{}", hasher.finish(), text.len())
}

fn read_notes(vault: &Path) -> Result<BTreeMap<String, String>> {
    let mut paths = Vec::new();
    markdown_files(vault, &mut paths)?;
    let mut notes = BTreeMap::new();
    for path in paths {
        let text = fs::read_to_string(&path)
            .map_err(|e| format!("Cannot read {}: {e}", path.display()))?;
        let relative = path
            .strip_prefix(vault)?
            .to_string_lossy()
            .replace('\\', "/");
        notes.insert(relative, text);
    }
    Ok(notes)
}

fn add_note(writer: &IndexWriter, f: &Fields, relative: &str, text: &str) -> Result<()> {
    for section in sections(text) {
        writer.add_document(doc!(f.path => relative, f.heading => section.heading,
            f.body => section.body, f.start => section.start as u64, f.end => section.end as u64))?;
    }
    Ok(())
}

fn locate(vault: &Path, destination: &Path) -> Result<(PathBuf, PathBuf)> {
    let vault = vault.canonicalize()?;
    if !vault.is_dir() {
        return Err("Vault must be a directory".into());
    }
    let destination = resolved_destination(destination)?;
    if destination.starts_with(&vault) || vault.starts_with(&destination) {
        return Err("Index must be outside the vault, not a parent of it".into());
    }
    Ok((vault, destination))
}

fn write_manifest(destination: &Path, manifest: &Manifest) -> Result<()> {
    let temporary = destination.join(format!("{MANIFEST}.tmp"));
    fs::write(&temporary, serde_json::to_vec(manifest)?)?;
    fs::rename(temporary, destination.join(MANIFEST))?;
    Ok(())
}

fn fingerprints(notes: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    notes
        .iter()
        .map(|(path, text)| (path.clone(), fingerprint(text)))
        .collect()
}

fn build(vault: &Path, destination: &Path) -> Result<()> {
    let (vault, destination) = locate(vault, destination)?;
    let notes = read_notes(&vault)?;
    let (schema, f) = schema();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&destination)?;
    }
    #[cfg(not(unix))]
    fs::create_dir_all(&destination)?;
    let index = if destination.join("meta.json").exists() {
        let index = Index::open_in_dir(&destination)?;
        if index.schema() != schema {
            return Err("Existing index has a different schema".into());
        }
        index
    } else {
        if fs::read_dir(&destination)?.next().is_some() {
            return Err("Refusing to use a non-empty directory without a search index".into());
        }
        Index::create_in_dir(&destination, schema)?
    };
    let mut writer: IndexWriter = index.writer(50_000_000)?;
    writer.delete_all_documents()?;
    for (relative, text) in &notes {
        add_note(&writer, &f, relative, text)?;
    }
    writer.commit()?;
    writer.wait_merging_threads()?;
    let files = notes.len();
    let manifest = Manifest {
        vault,
        files: fingerprints(&notes),
    };
    write_manifest(&destination, &manifest)?;
    let reader = index.reader()?;
    let sections = reader.searcher().num_docs();
    println!(
        "{}",
        serde_json::json!({"files": files, "sections": sections, "index": destination})
    );
    Ok(())
}

// Brings an existing index up to date by re-indexing only added or edited notes and dropping
// deleted ones. An index with no manifest (built by an older `index`) or a missing index is
// built from scratch.
fn sync(vault: &Path, destination: &Path) -> Result<()> {
    let (vault, destination) = locate(vault, destination)?;
    let manifest: Option<Manifest> = fs::read(destination.join(MANIFEST))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok());
    let Some(manifest) = manifest.filter(|_| destination.join("meta.json").exists()) else {
        return build(&vault, &destination);
    };
    if manifest.vault != vault {
        return Err("Index was built from a different vault; run `osb index` to rebuild it".into());
    }
    let (schema, f) = schema();
    let index = Index::open_in_dir(&destination)?;
    if index.schema() != schema {
        return Err("Existing index has a different schema".into());
    }
    let notes = read_notes(&vault)?;
    let current = fingerprints(&notes);
    let changed: Vec<&String> = current
        .iter()
        .filter(|(path, hash)| manifest.files.get(*path) != Some(*hash))
        .map(|(path, _)| path)
        .collect();
    let removed: Vec<&String> = manifest
        .files
        .keys()
        .filter(|path| !current.contains_key(*path))
        .collect();
    let added = changed
        .iter()
        .filter(|path| !manifest.files.contains_key(**path))
        .count();
    let updated = changed.len() - added;
    if !changed.is_empty() || !removed.is_empty() {
        let mut writer: IndexWriter = index.writer(50_000_000)?;
        for path in changed.iter().chain(&removed) {
            writer.delete_term(Term::from_field_text(f.path, path));
        }
        for path in &changed {
            add_note(&writer, &f, path, &notes[*path])?;
        }
        writer.commit()?;
        writer.wait_merging_threads()?;
        write_manifest(
            &destination,
            &Manifest {
                vault,
                files: current.clone(),
            },
        )?;
    }
    println!(
        "{}",
        serde_json::json!({
            "added": added,
            "updated": updated,
            "removed": removed.len(),
            "unchanged": current.len() - changed.len(),
            "index": destination,
        })
    );
    Ok(())
}

#[derive(Serialize)]
struct Hit {
    score: f32,
    path: String,
    heading: String,
    start_line: u64,
    end_line: u64,
    excerpt: String,
}
fn search(directory: &Path, query: &str, limit: usize) -> Result<()> {
    if query.trim().is_empty() {
        return Err("Query must not be empty".into());
    }
    if !(1..=50).contains(&limit) {
        return Err("Limit must be between 1 and 50".into());
    }
    let index = Index::open_in_dir(directory)?;
    let (expected, f) = schema();
    if index.schema() != expected {
        return Err("Unsupported index schema".into());
    }
    let reader = index.reader()?;
    let searcher = reader.searcher();
    let mut parser = QueryParser::for_index(&index, vec![f.heading, f.body]);
    parser.set_field_boost(f.heading, 2.0);
    let query = parser.parse_query(query)?;
    let mut hits = Vec::new();
    for (score, address) in searcher.search(&query, &TopDocs::with_limit(limit).order_by_score())? {
        let document: TantivyDocument = searcher.doc(address)?;
        let text = |field| {
            document
                .get_first(field)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_owned()
        };
        let number = |field| {
            document
                .get_first(field)
                .and_then(|v| v.as_u64())
                .unwrap_or(0)
        };
        hits.push(Hit {
            score,
            path: text(f.path),
            heading: text(f.heading),
            start_line: number(f.start),
            end_line: number(f.end),
            excerpt: text(f.body).chars().take(600).collect(),
        });
    }
    println!("{}", serde_json::json!({"hits": hits}));
    Ok(())
}

fn run() -> Result<()> {
    let args: Vec<_> = env::args().skip(1).collect();
    match args.as_slice() {
        [command, vault, index] if command == "index" => build(Path::new(vault), Path::new(index)),
        [command, vault, index] if command == "sync" => sync(Path::new(vault), Path::new(index)),
        [command, index, query] if command == "search" => search(Path::new(index), query, 5),
        [command, index, query, flag, limit] if command == "search" && flag == "--limit" => {
            search(Path::new(index), query, limit.parse()?)
        }
        _ => Err(
            "Usage: osb index <vault> <index> | osb sync <vault> <index> | osb search <index> <query> [--limit 1..50]".into(),
        ),
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("osb: {error}");
        std::process::exit(1);
    }
}
