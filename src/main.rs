use serde::Serialize;
use std::{
    env,
    error::Error,
    fs,
    path::{Path, PathBuf},
};
use tantivy::{
    Index, TantivyDocument,
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

fn build(vault: &Path, destination: &Path) -> Result<()> {
    let vault = vault.canonicalize()?;
    if !vault.is_dir() {
        return Err("Vault must be a directory".into());
    }
    let destination = resolved_destination(destination)?;
    if destination.starts_with(&vault) || vault.starts_with(&destination) {
        return Err("Index must be outside the vault, not a parent of it".into());
    }
    let mut paths = Vec::new();
    markdown_files(&vault, &mut paths)?;
    let mut documents = Vec::new();
    let (schema, f) = schema();
    for path in &paths {
        let text =
            fs::read_to_string(path).map_err(|e| format!("Cannot read {}: {e}", path.display()))?;
        let relative = path
            .strip_prefix(&vault)?
            .to_string_lossy()
            .replace('\\', "/");
        for section in sections(&text) {
            documents.push(doc!(f.path => relative.clone(), f.heading => section.heading,
                f.body => section.body, f.start => section.start as u64, f.end => section.end as u64));
        }
    }
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
    let mut writer = index.writer::<TantivyDocument>(50_000_000)?;
    writer.delete_all_documents()?;
    let count = documents.len();
    for document in documents {
        writer.add_document(document)?;
    }
    writer.commit()?;
    writer.wait_merging_threads()?;
    println!(
        "{}",
        serde_json::json!({"files": paths.len(), "sections": count, "index": destination})
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
        [command, index, query] if command == "search" => search(Path::new(index), query, 5),
        [command, index, query, flag, limit] if command == "search" && flag == "--limit" => {
            search(Path::new(index), query, limit.parse()?)
        }
        _ => Err(
            "Usage: osb index <vault> <index> | osb search <index> <query> [--limit 1..50]".into(),
        ),
    }
}
fn main() {
    if let Err(error) = run() {
        eprintln!("osb: {error}");
        std::process::exit(1);
    }
}
