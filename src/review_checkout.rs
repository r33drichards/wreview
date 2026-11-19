use anyhow::{Context, Result, ensure};
use flate2::read::GzDecoder;
use serde_json::{Value, json};
use std::{collections::BTreeMap, io::Read, path::Component};

pub const ARCHIVE_LIMIT: usize = 32 * 1024 * 1024;
const EXPANDED_LIMIT: u64 = 128 * 1024 * 1024;
const FILE_LIMIT: u64 = 4 * 1024 * 1024;
const ENTRY_LIMIT: usize = 50_000;

pub struct Checkout {
    files: BTreeMap<String, Option<String>>,
}

impl Checkout {
    pub fn open(archive: &[u8]) -> Result<Self> {
        let mut tar = tar::Archive::new(GzDecoder::new(archive).take(EXPANDED_LIMIT + 1));
        let mut files = BTreeMap::new();
        let mut expanded = 0;
        for (index, entry) in tar.entries()?.enumerate() {
            ensure!(
                index < ENTRY_LIMIT,
                "Repository archive contains too many entries"
            );
            let mut entry = entry?;
            ensure!(
                entry.size() <= EXPANDED_LIMIT - expanded,
                "Repository archive exceeds size limit"
            );
            expanded += entry.size();
            if !entry.header().entry_type().is_file() {
                continue;
            }
            let path = entry.path()?;
            // GitHub wraps the archive in a single generated directory.
            let parts: Option<Vec<_>> = path
                .components()
                .skip(1)
                .map(|component| {
                    if let Component::Normal(part) = component {
                        part.to_str()
                    } else {
                        None
                    }
                })
                .collect();
            let Some(parts) = parts.filter(|parts| !parts.is_empty()) else {
                continue;
            };
            let path = parts.join("/");
            let text = if entry.size() <= FILE_LIMIT {
                let mut bytes = Vec::new();
                entry.read_to_end(&mut bytes)?;
                String::from_utf8(bytes).ok()
            } else {
                None
            };
            files.insert(path, text);
        }
        ensure!(
            !files.is_empty(),
            "Repository archive contains no regular files"
        );
        Ok(Self { files })
    }

    pub fn list(&self, args: &Value) -> Result<Value> {
        let prefix = args["prefix"].as_str().unwrap_or("");
        let offset = offset(args)?;
        let mut matching = self
            .files
            .range(prefix.to_string()..)
            .take_while(|(path, _)| path.starts_with(prefix))
            .skip(offset);
        let paths: Vec<_> = matching.by_ref().take(100).map(|(path, _)| path).collect();
        Ok(json!({"paths": paths, "next_offset": matching.next().map(|_| offset + paths.len())}))
    }

    pub fn read(&self, args: &Value) -> Result<Value> {
        let path = args["path"].as_str().context("Missing repository path")?;
        let text = self
            .files
            .get(path)
            .context("File is absent at the review commit")?
            .as_deref()
            .context("File exceeds the text limit or is not UTF-8")?;
        let offset = offset(args)?;
        let mut lines = text.lines().skip(offset);
        let mut page = Vec::new();
        let mut bytes = 0;
        for line in lines.by_ref().take(100) {
            let display: String = line.chars().take(500).collect();
            bytes += display.len();
            page.push(display);
            if bytes >= 12_000 {
                break;
            }
        }
        Ok(json!({"path":path, "first_line":offset + 1, "lines":page,
            "next_offset":lines.next().map(|_| offset + page.len())}))
    }

    pub fn search(&self, args: &Value) -> Result<Value> {
        let query = args["query"]
            .as_str()
            .filter(|q| !q.is_empty() && q.len() <= 256)
            .context("Search query must be 1–256 bytes")?;
        let prefix = args["prefix"].as_str().unwrap_or("");
        let offset = offset(args)?;
        let mut matches = self
            .files
            .range(prefix.to_string()..)
            .take_while(|(path, _)| path.starts_with(prefix))
            .filter_map(|(path, text)| text.as_ref().map(|text| (path, text)))
            .flat_map(|(path, text)| {
                text.lines().enumerate()
                .filter(move |(_, line)| line.contains(query))
                .map(move |(index, line)| json!({
                    "path":path, "line":index + 1, "text":line.chars().take(300).collect::<String>()
                }))
            })
            .skip(offset);
        let results: Vec<_> = matches.by_ref().take(30).collect();
        Ok(json!({"matches":results, "next_offset":matches.next().map(|_| offset + results.len())}))
    }
}

fn offset(args: &Value) -> Result<usize> {
    let Some(value) = args.get("offset") else {
        return Ok(0);
    };
    let offset = value
        .as_u64()
        .context("Offset must be a nonnegative integer")?;
    ensure!(offset <= 1_000_000, "Offset exceeds limit");
    Ok(offset as usize)
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{Compression, write::GzEncoder};

    #[test]
    fn searches_and_reads_an_unchanged_file_from_the_archive() {
        let mut tar = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::default()));
        let text = b"fn main() {\n    invoke_changed_function();\n}\n";
        let mut entry = tar::Header::new_gnu();
        entry.set_mode(0o644);
        entry.set_size(text.len() as u64);
        entry.set_cksum();
        tar.append_data(&mut entry, "owner-repo-sha/src/unchanged.rs", &text[..])
            .unwrap();
        let archive = tar.into_inner().unwrap().finish().unwrap();

        let checkout = Checkout::open(&archive).unwrap();
        let discovered = checkout.list(&json!({"prefix":"src/"})).unwrap();
        assert_eq!(discovered["paths"], json!(["src/unchanged.rs"]));
        let matches = checkout.search(&json!({"query":"invoke_changed"})).unwrap();
        assert_eq!(matches["matches"][0]["line"], 2);
        let page = checkout
            .read(&json!({"path":"src/unchanged.rs","offset":1}))
            .unwrap();
        assert_eq!(page["lines"][0], "    invoke_changed_function();");
        assert!(checkout.read(&json!({"path":"../outside"})).is_err());
    }
}
