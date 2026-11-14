//! CommonMark syntax only; a resolved URL is not accepted policy or a verified
//! heading/symbol. Parse each complete, already bounded snapshot independently.
use pulldown_cmark::{BrokenLink, Event, LinkType, Options, Parser, Tag, TagEnd};
use std::collections::BTreeMap;

pub(crate) struct Link {
    pub offset: usize,
    pub end: usize,
    pub target: String,
    pub kind: &'static str,
    pub reference_label: Option<String>,
    pub definition_byte_range: Option<[usize; 2]>,
    pub unresolved_status: Option<&'static str>,
}

pub(crate) struct Parsed {
    pub links: Vec<Link>,
    /// One omission per category, with first source offset and total count.
    pub issues: BTreeMap<&'static str, (usize, usize)>,
}

pub(super) fn path_candidate(value: &str) -> bool {
    value.contains(['/', '\\'])
        || value.rsplit_once('.').is_some_and(|(_, extension)| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "md" | "mdc"
                    | "markdown"
                    | "vb"
                    | "cs"
                    | "ts"
                    | "tsx"
                    | "js"
                    | "jsx"
                    | "mjs"
                    | "rs"
                    | "py"
                    | "go"
                    | "java"
                    | "sql"
                    | "aspx"
                    | "ascx"
                    | "config"
                    | "json"
                    | "yaml"
                    | "yml"
                    | "css"
                    | "resx"
                    | "xml"
            )
        })
}

impl Parsed {
    fn issue(&mut self, status: &'static str, offset: usize) {
        let row = self.issues.entry(status).or_insert((offset, 0));
        row.1 += 1;
    }
}

pub(crate) fn parse(content: &str, available: usize) -> Parsed {
    let mut parsed = Parsed {
        links: Vec::new(),
        issues: BTreeMap::new(),
    };
    // Undefined [words] are ordinary prose. Explicit full/collapsed references
    // remain observable, but their placeholder never reaches path resolution.
    let callback = |broken: BrokenLink<'_>| match broken.link_type {
        LinkType::Reference | LinkType::Collapsed => Some(("".into(), "".into())),
        _ => None,
    };
    // Wiki syntax is recognized only to disclose its dialect-dependent target;
    // do not guess a filename, extension, alias or fragment convention.
    let mut parser =
        Parser::new_with_broken_link_callback(content, Options::ENABLE_WIKILINKS, Some(callback))
            .into_offset_iter();
    let mut image_depth = 0usize;
    while let Some((event, span)) = parser.next() {
        // Image descriptions render as alternative text. Mark the asset once;
        // nested formatting/link syntax does not create guidance dependencies.
        match &event {
            Event::End(TagEnd::Image) => {
                image_depth = image_depth.saturating_sub(1);
                continue;
            }
            Event::Start(Tag::Image { .. }) if image_depth > 0 => {
                image_depth += 1;
                continue;
            }
            _ if image_depth > 0 => continue,
            _ => {}
        }
        let (link_type, destination, id, kind) = match event {
            Event::Start(Tag::Link {
                link_type,
                dest_url,
                id,
                ..
            }) => (link_type, dest_url, id, "link"),
            Event::Start(Tag::Image {
                link_type,
                dest_url,
                id,
                ..
            }) => {
                image_depth += 1;
                (link_type, dest_url, id, "image")
            }
            Event::Code(value) if path_candidate(&value) => {
                if parsed.links.len() >= available {
                    parsed.issue("edge_budget_omitted", span.start);
                } else if value.len() > 512 {
                    parsed.issue("target_length_omitted", span.start);
                } else {
                    parsed.links.push(Link {
                        offset: span.start,
                        end: span.end,
                        target: value.into_string(),
                        kind: "inline_code_path",
                        reference_label: None,
                        definition_byte_range: None,
                        unresolved_status: None,
                    });
                }
                continue;
            }
            Event::Html(_) | Event::InlineHtml(_) => {
                parsed.issue("html_reference_syntax_unverified", span.start);
                continue;
            }
            _ => continue,
        };
        if parsed.links.len() >= available {
            parsed.issue("edge_budget_omitted", span.start);
            continue;
        }
        let unknown = matches!(
            link_type,
            LinkType::ReferenceUnknown | LinkType::CollapsedUnknown | LinkType::ShortcutUnknown
        );
        let target = if unknown {
            id.as_ref()
        } else {
            destination.as_ref()
        };
        if target.len() > 512 || id.len() > 512 {
            parsed.issue("target_length_omitted", span.start);
            continue;
        }
        let reference = matches!(
            link_type,
            LinkType::Reference | LinkType::Collapsed | LinkType::Shortcut
        );
        let definition_byte_range = if reference {
            parser
                .reference_definitions()
                .get(id.as_ref())
                .map(|definition| [definition.span.start, definition.span.end])
        } else {
            None
        };
        let unresolved_status = if unknown {
            Some("unresolved_reference_label")
        } else if matches!(link_type, LinkType::WikiLink { .. }) {
            Some("wiki_reference_syntax_unverified")
        } else if kind == "image" {
            // Binary assets are not UTF-8 guidance. Retain the source occurrence
            // without opening it or recursively treating it as authored policy.
            Some("image_reference_unverified")
        } else {
            None
        };
        parsed.links.push(Link {
            offset: span.start,
            end: span.end,
            target: target.into(),
            kind,
            reference_label: (reference || unknown).then(|| id.to_string()),
            definition_byte_range,
            unresolved_status,
        });
    }
    parsed
}
