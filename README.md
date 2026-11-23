# Obsidian Second Brain

Obsidian Second Brain is a local, read-only search tool for Markdown notes, such as an Obsidian vault.
It builds a Tantivy BM25 index and returns ranked sections with file paths, headings, line ranges, and excerpts.
The `osb` CLI does not call an LLM, change notes, or run a background service.
The optional OMP adapter can call a helper model to rewrite queries when you enable it (see below).

## Requirements

- Rust 1.100 or newer.
- Cargo, which downloads the locked dependencies during the first build.

## Build and test

```sh
cargo build --release --locked
cargo test --locked
cargo clippy --all-targets --locked -- -D warnings
```

The executable is `target/release/osb`.

## Use it

Set the vault path and an index path outside the vault:

```sh
export OSB_VAULT_PATH="$HOME/Documents/Obsidian"
export OSB_INDEX_PATH="$HOME/Library/Caches/obsidian-second-brain/index"
```

Build the index:

```sh
./target/release/osb index "$OSB_VAULT_PATH" "$OSB_INDEX_PATH"
```

Indexing reads non-hidden `.md` files recursively.
It splits files at ATX headings, omits YAML frontmatter, and ignores headings inside fenced code blocks.
Unreadable notes and invalid UTF-8 cause the command to fail.
Rebuilding replaces the existing index.

Bring an existing index up to date:

```sh
./target/release/osb sync "$OSB_VAULT_PATH" "$OSB_INDEX_PATH"
```

`sync` re-indexes only added or edited notes and drops deleted ones, and it prints the counts.
It keeps a manifest of note hashes (`osb-manifest.json`) inside the index directory, never in the vault.
It builds the index from scratch if none exists or if the index has no manifest, and it refuses an index that was built from a different vault.
`search` stays read-only and never changes the index.

Search the index:

```sh
./target/release/osb search "$OSB_INDEX_PATH" 'agent societies' --limit 3
```

The command prints JSON with a `hits` array.
Each hit includes a BM25 score, vault-relative path, heading, one-based inclusive line range, and an excerpt of up to 600 characters.
The excerpt starts at the section and does not highlight matches.
Queries use Tantivy syntax.
Plain terms use `OR`, and sections that match more terms rank higher; prefix a term with `+` or use `AND` to require it.

## Use it in an OMP session

`integrations/omp/obsidian-search.ts` registers an `obsidian_search` tool that calls `osb search`.
Load it for one session, so your normal OMP setup does not change:

```sh
OSB_BIN="$PWD/target/release/osb" OSB_INDEX_PATH="$OSB_INDEX_PATH" \
  omp -e integrations/omp/obsidian-search.ts
```

The tool returns the top 3 hits by default; set `OSB_LIMIT` to change that.
Query rewriting needs three more variables:

- `OSB_REWRITE_MODEL`: the helper model, for example `anthropic/claude-haiku-5-5`.
- `OSB_HELPER_SESSION_DIR`: where the helper's session log goes.
- `OSB_OMP_CONFIG`: an OMP config overlay file for the helper. A minimal one that keeps the helper stateless is `memory: {backend: "off"}`.

With all three set, a helper model turns each request into search terms before the search runs.
The helper prompt is `REWRITE_PROMPT` at the top of the adapter. It keeps the question's own words and adds at most four synonyms.
Without `OSB_REWRITE_MODEL` the tool searches with the agent's own words, and setting it without the other two makes every search fail with an error.
You must build the executable and the index before the first search.
The tool never changes the index, so run `osb sync` to pick up new and edited notes.

## Privacy and limitations

- The index contains copies of note text.
Keep it outside Git, cloud-shared directories, and the vault.
- Only Markdown body text and headings are searchable.
There is no PDF/OCR, filename search, frontmatter tag search, embedding model, or automatic note rewriting.
- Unicode words are supported, but language-specific segmentation and morphological analysis are not.
Korean exact-word searches are tested.
Semantic and multilingual retrieval quality are not established.
- `sync` hashes every note on each run, so its cost grows with vault size (about 7 ms for 227 notes when nothing changed).
There is no file watcher, so the index is only as fresh as the last `sync` or `index`.
Line ranges in older search results can become stale after note edits.

## Scope

This project provides the local search tool and an OMP adapter.
A Hermes adapter and reviewed LLM-assisted note updates are not built.
