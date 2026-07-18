//! Renders Markdown to the HTML subset accepted by Telegram's `parse_mode=HTML`.
//!
//! Telegram only allows a small set of inline tags (`b`, `i`, `s`, `u`, `code`,
//! `pre`, `a`, `blockquote`). Everything else must become plain text: headings
//! render as bold lines, list items as bullet lines, and tables fall back to
//! their raw text. All text content is entity-escaped so untrusted job output
//! cannot inject tags.

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};

pub fn to_telegram_html(markdown: &str) -> String {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    let parser = Parser::new_ext(markdown, options);

    let mut out = String::with_capacity(markdown.len());
    // Stack of ordered-list counters; `None` marks an unordered list.
    let mut list_stack: Vec<Option<u64>> = Vec::new();
    // Tracks whether each active list item has emitted content. Loose-list
    // paragraphs must not separate the marker from their first text event.
    let mut item_stack: Vec<bool> = Vec::new();
    let mut code_block_has_lang = false;

    for event in parser {
        match event {
            Event::Start(tag) => match tag {
                Tag::Strong => out.push_str("<b>"),
                Tag::Emphasis => out.push_str("<i>"),
                Tag::Strikethrough => out.push_str("<s>"),
                Tag::Heading { .. } => {
                    ensure_block_start(&mut out, &item_stack);
                    out.push_str("<b>");
                }
                Tag::Paragraph => match item_stack.last_mut() {
                    Some(has_content) if !*has_content => *has_content = true,
                    _ => ensure_blank_line(&mut out),
                },
                Tag::BlockQuote(_) => {
                    ensure_block_start(&mut out, &item_stack);
                    out.push_str("<blockquote>");
                }
                Tag::CodeBlock(kind) => {
                    ensure_block_start(&mut out, &item_stack);
                    code_block_has_lang = false;
                    match kind {
                        CodeBlockKind::Fenced(lang) if !lang.is_empty() => {
                            code_block_has_lang = true;
                            out.push_str(&format!(
                                "<pre><code class=\"language-{}\">",
                                escape(&lang)
                            ));
                        }
                        _ => out.push_str("<pre>"),
                    }
                }
                Tag::List(start) => {
                    list_stack.push(start);
                    ensure_newline(&mut out);
                }
                Tag::Item => {
                    ensure_newline(&mut out);
                    let depth = list_stack.len().saturating_sub(1);
                    out.push_str(&"  ".repeat(depth));
                    match list_stack.last_mut() {
                        Some(Some(number)) => {
                            out.push_str(&format!("{number}. "));
                            *number += 1;
                        }
                        _ => out.push_str("• "),
                    }
                    item_stack.push(false);
                }
                Tag::Link { dest_url, .. } => {
                    out.push_str(&format!("<a href=\"{}\">", escape(&dest_url)));
                }
                // Tables and other unsupported blocks flow through as text.
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Strong => out.push_str("</b>"),
                TagEnd::Emphasis => out.push_str("</i>"),
                TagEnd::Strikethrough => out.push_str("</s>"),
                TagEnd::Heading(_) => {
                    out.push_str("</b>");
                    out.push('\n');
                }
                TagEnd::Paragraph => out.push('\n'),
                TagEnd::BlockQuote(_) => {
                    out.push_str("</blockquote>\n");
                }
                TagEnd::CodeBlock => {
                    if code_block_has_lang {
                        out.push_str("</code></pre>\n");
                    } else {
                        out.push_str("</pre>\n");
                    }
                    code_block_has_lang = false;
                }
                TagEnd::List(_) => {
                    list_stack.pop();
                    out.push('\n');
                }
                TagEnd::Item => {
                    item_stack.pop();
                    ensure_newline(&mut out);
                }
                TagEnd::Link => out.push_str("</a>"),
                _ => {}
            },
            Event::Text(text) => {
                if let Some(has_content) = item_stack.last_mut() {
                    *has_content = true;
                }
                out.push_str(&escape(&text));
            }
            Event::Code(code) => {
                if let Some(has_content) = item_stack.last_mut() {
                    *has_content = true;
                }
                out.push_str("<code>");
                out.push_str(&escape(&code));
                out.push_str("</code>");
            }
            Event::SoftBreak => out.push('\n'),
            Event::HardBreak => out.push('\n'),
            Event::Rule => {
                ensure_blank_line(&mut out);
                out.push_str("———\n");
            }
            Event::TaskListMarker(done) => {
                out.push_str(if done { "☑ " } else { "☐ " });
            }
            Event::Html(html) | Event::InlineHtml(html) => {
                // Raw HTML in the source is untrusted; show it escaped.
                out.push_str(&escape(&html));
            }
            _ => {}
        }
    }

    out.trim().to_string()
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn ensure_newline(out: &mut String) {
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
}

fn ensure_blank_line(out: &mut String) {
    if out.is_empty() {
        return;
    }
    while !out.ends_with("\n\n") {
        out.push('\n');
    }
}

fn ensure_block_start(out: &mut String, item_stack: &[bool]) {
    if !matches!(item_stack.last(), Some(false)) {
        ensure_blank_line(out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_bold_italic_and_code() {
        let html = to_telegram_html("**bold** and *italic* and `code`");
        assert_eq!(html, "<b>bold</b> and <i>italic</i> and <code>code</code>");
    }

    #[test]
    fn renders_headings_as_bold_lines() {
        let html = to_telegram_html("## Section\n\nBody text");
        assert_eq!(html, "<b>Section</b>\n\nBody text");
    }

    #[test]
    fn renders_links() {
        let html = to_telegram_html("[Push](https://github.com/owainlewis/push)");
        assert_eq!(
            html,
            "<a href=\"https://github.com/owainlewis/push\">Push</a>"
        );
    }

    #[test]
    fn renders_unordered_lists_as_bullets() {
        let html = to_telegram_html("- one\n- two");
        assert_eq!(html, "• one\n• two");
    }

    #[test]
    fn renders_ordered_lists_with_numbers() {
        let html = to_telegram_html("1. first\n2. second");
        assert_eq!(html, "1. first\n2. second");
    }

    #[test]
    fn keeps_loose_list_markers_attached_to_the_first_paragraph() {
        let html = to_telegram_html("- first paragraph\n\n  second paragraph\n- next");

        assert!(html.starts_with("• first paragraph"));
        assert!(!html.contains("• \n\nfirst paragraph"));
        assert!(html.contains("• next"));
    }

    #[test]
    fn keeps_nested_list_markers_attached_to_their_text() {
        let html = to_telegram_html("- parent\n  - child");

        assert_eq!(html, "• parent\n  • child");
    }

    #[test]
    fn keeps_list_markers_attached_to_block_content() {
        let quote = to_telegram_html("- > quoted");
        let code = to_telegram_html("- ```\ncode\n```");

        assert!(quote.starts_with("• <blockquote>"));
        assert!(!quote.contains("• \n\n<blockquote>"));
        assert!(code.starts_with("• <pre>"));
        assert!(!code.contains("• \n\n<pre>"));
    }

    #[test]
    fn escapes_html_in_text() {
        let html = to_telegram_html("a <script> & b");
        assert_eq!(html, "a &lt;script&gt; &amp; b");
    }

    #[test]
    fn renders_fenced_code_blocks() {
        let html = to_telegram_html("```\nlet x = 1;\n```");
        assert!(html.starts_with("<pre>"));
        assert!(html.contains("let x = 1;"));
        assert!(html.trim_end().ends_with("</pre>"));
    }

    #[test]
    fn plain_text_passes_through() {
        let html = to_telegram_html("Just a sentence.");
        assert_eq!(html, "Just a sentence.");
    }
}
