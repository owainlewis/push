use std::fs;
use std::path::{Path, PathBuf};

use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

#[test]
fn local_documentation_links_resolve() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut markdown = vec![root.join("README.md")];
    collect_markdown(&root.join("docs"), &mut markdown);

    let mut broken = Vec::new();
    for file in markdown {
        let source = fs::read_to_string(&file).unwrap();
        for event in Parser::new(&source) {
            let Event::Start(Tag::Link { dest_url, .. }) = event else {
                continue;
            };
            let destination = dest_url.as_ref();
            if destination.starts_with('#') && destination.len() == 1 {
                continue;
            }
            if destination.starts_with('/')
                || destination.contains("://")
                || destination.starts_with("mailto:")
            {
                continue;
            }
            let (path, fragment) = destination
                .split_once('#')
                .map_or((destination, None), |(path, fragment)| {
                    (path, Some(fragment))
                });
            let path = path.split('?').next().unwrap_or_default();
            let resolved = if path.is_empty() {
                file.clone()
            } else {
                file.parent().unwrap().join(path)
            };
            if !resolved.exists() {
                broken.push(format!(
                    "{} -> {destination}",
                    relative(root, &file).display()
                ));
            } else if let Some(fragment) = fragment {
                let target = fs::read_to_string(&resolved).unwrap();
                if !heading_anchors(&target)
                    .iter()
                    .any(|anchor| anchor == fragment)
                {
                    broken.push(format!(
                        "{} -> {destination} (missing anchor)",
                        relative(root, &file).display()
                    ));
                }
            }
        }
    }

    assert!(
        broken.is_empty(),
        "broken local documentation links:\n{}",
        broken.join("\n")
    );
}

fn heading_anchors(markdown: &str) -> Vec<String> {
    let mut anchors = Vec::new();
    let mut heading = None;
    for event in Parser::new_ext(markdown, Options::ENABLE_HEADING_ATTRIBUTES) {
        match event {
            Event::Start(Tag::Heading { id, .. }) => {
                heading = Some((String::new(), id.map(|id| id.to_string())));
            }
            Event::Text(text) | Event::Code(text) => {
                if let Some((heading, _)) = heading.as_mut() {
                    heading.push_str(&text);
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                if let Some((heading, _)) = heading.as_mut() {
                    heading.push(' ');
                }
            }
            Event::End(TagEnd::Heading(_)) => {
                if let Some((heading, id)) = heading.take() {
                    anchors.push(id.unwrap_or_else(|| slug(&heading)));
                }
            }
            _ => {}
        }
    }
    anchors
}

fn slug(heading: &str) -> String {
    let mut slug = String::new();
    let mut separator = false;
    for character in heading.trim_end_matches('#').trim().chars() {
        if character.is_alphanumeric() || character == '_' {
            if separator && !slug.is_empty() {
                slug.push('-');
            }
            separator = false;
            slug.extend(character.to_lowercase());
        } else if character.is_whitespace() || character == '-' {
            separator = true;
        }
    }
    slug
}

fn collect_markdown(directory: &Path, files: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(directory).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            collect_markdown(&path, files);
        } else if path.extension().is_some_and(|extension| extension == "md") {
            files.push(path);
        }
    }
}

fn relative<'a>(root: &Path, path: &'a Path) -> &'a Path {
    path.strip_prefix(root).unwrap_or(path)
}
