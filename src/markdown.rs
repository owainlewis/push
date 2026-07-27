//! Renders Markdown for channel-specific rich text formats.
//!
//! Slack uses `mrkdwn`, while Telegram accepts a small HTML subset through
//! `parse_mode=HTML`. Unsupported structures become readable plain text and
//! channel control characters are entity-escaped.

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};

#[cfg(test)]
pub fn to_slack_mrkdwn(markdown: &str) -> String {
    render_slack_mrkdwn(markdown, false)
}

pub(crate) const SLACK_FORMAT_MARKER: char = '\u{E000}';

pub(crate) fn to_slack_mrkdwn_for_chunking(markdown: &str) -> String {
    render_slack_mrkdwn(markdown, true)
}

fn render_slack_mrkdwn(markdown: &str, mark_delimiters: bool) -> String {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    let parser = Parser::new_ext(markdown, options);

    let mut out = SlackOutput::with_capacity(markdown.len(), mark_delimiters);
    let mut list_stack: Vec<Option<u64>> = Vec::new();
    let mut item_stack: Vec<bool> = Vec::new();
    let mut in_heading = false;
    let mut in_code_block = false;

    for event in parser {
        match event {
            Event::Start(tag) => match tag {
                Tag::Strong if !in_heading => out.push_delimiter("*"),
                Tag::Emphasis => out.push_delimiter("_"),
                Tag::Strikethrough => out.push_delimiter("~"),
                Tag::Heading { .. } => {
                    slack_ensure_block_start(&mut out, &item_stack);
                    out.push_delimiter("*");
                    in_heading = true;
                }
                Tag::Paragraph => match item_stack.last_mut() {
                    Some(has_content) if !*has_content => *has_content = true,
                    _ => slack_ensure_blank_line(&mut out),
                },
                Tag::BlockQuote(_) => {
                    slack_ensure_block_start(&mut out, &item_stack);
                    out.quote_depth += 1;
                }
                Tag::CodeBlock(_) => {
                    slack_ensure_block_start(&mut out, &item_stack);
                    out.push_delimiter("```");
                    out.push("\n");
                    in_code_block = true;
                }
                Tag::List(start) => {
                    list_stack.push(start);
                    slack_ensure_newline(&mut out);
                }
                Tag::Item => {
                    slack_ensure_newline(&mut out);
                    let depth = list_stack.len().saturating_sub(1);
                    out.push(&"  ".repeat(depth));
                    match list_stack.last_mut() {
                        Some(Some(number)) => {
                            out.push(&format!("{number}. "));
                            *number += 1;
                        }
                        _ => out.push("• "),
                    }
                    item_stack.push(false);
                }
                Tag::Link { dest_url, .. } => {
                    out.push("<");
                    out.push(&slack_escape_url(&dest_url));
                    out.push("|");
                }
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Strong if !in_heading => out.push_delimiter("*"),
                TagEnd::Emphasis => out.push_delimiter("_"),
                TagEnd::Strikethrough => out.push_delimiter("~"),
                TagEnd::Heading(_) => {
                    in_heading = false;
                    out.push_delimiter("*");
                    out.push("\n");
                }
                TagEnd::Paragraph => out.push("\n"),
                TagEnd::BlockQuote(_) => {
                    slack_ensure_newline(&mut out);
                    out.quote_depth = out.quote_depth.saturating_sub(1);
                    if item_stack.is_empty() {
                        slack_ensure_blank_line(&mut out);
                    }
                }
                TagEnd::CodeBlock => {
                    in_code_block = false;
                    out.push_delimiter("```");
                    out.push("\n");
                }
                TagEnd::List(_) => {
                    list_stack.pop();
                    out.push("\n");
                }
                TagEnd::Item => {
                    item_stack.pop();
                    slack_ensure_newline(&mut out);
                }
                TagEnd::Link => out.push(">"),
                _ => {}
            },
            Event::Text(text) => {
                if let Some(has_content) = item_stack.last_mut() {
                    *has_content = true;
                }
                if in_code_block {
                    out.push(&slack_escape_code(&text));
                } else {
                    out.push(&slack_escape_text(&text));
                }
            }
            Event::Code(code) => {
                if let Some(has_content) = item_stack.last_mut() {
                    *has_content = true;
                }
                if code.contains('`') {
                    // Slack has no documented variable-length inline code
                    // delimiter, so keep the content readable without turning
                    // surrounding prose into a fenced code block.
                    out.push(&slack_escape_text(&code));
                } else {
                    out.push_delimiter("`");
                    out.push(&slack_escape_code(&code));
                    out.push_delimiter("`");
                }
            }
            Event::SoftBreak | Event::HardBreak => out.push("\n"),
            Event::Rule => {
                slack_ensure_blank_line(&mut out);
                out.push("———\n");
            }
            Event::TaskListMarker(done) => {
                out.push(if done { "☑ " } else { "☐ " });
            }
            Event::Html(html) | Event::InlineHtml(html) => {
                out.push(&slack_escape_text(&html));
            }
            _ => {}
        }
    }

    out.finish()
}

struct SlackOutput {
    text: String,
    quote_depth: usize,
    line_start: bool,
    mark_delimiters: bool,
}

impl SlackOutput {
    fn with_capacity(capacity: usize, mark_delimiters: bool) -> Self {
        Self {
            text: String::with_capacity(capacity),
            quote_depth: 0,
            line_start: true,
            mark_delimiters,
        }
    }

    fn push(&mut self, text: &str) {
        for character in text.chars() {
            if self.line_start && character != '\n' && self.quote_depth > 0 {
                self.text.push_str(&"> ".repeat(self.quote_depth));
                self.line_start = false;
            }
            self.text.push(character);
            self.line_start = character == '\n';
        }
    }

    fn push_delimiter(&mut self, delimiter: &str) {
        if self.mark_delimiters {
            self.push(&SLACK_FORMAT_MARKER.to_string());
        }
        self.push(delimiter);
    }

    fn finish(self) -> String {
        self.text.trim().to_string()
    }
}

fn slack_escape_controls(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace(
            SLACK_FORMAT_MARKER,
            &format!("{SLACK_FORMAT_MARKER}\u{200B}"),
        )
}

fn slack_escape_text(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in slack_escape_controls(text).chars() {
        escaped.push(character);
        if matches!(character, '*' | '_' | '~' | '`') {
            escaped.push('\u{200B}');
        }
    }
    escaped
}

fn slack_escape_code(text: &str) -> String {
    slack_escape_controls(text).replace("```", "``\u{200B}`")
}

fn slack_escape_url(url: &str) -> String {
    slack_escape_controls(url).replace('|', "%7C")
}

fn slack_ensure_newline(out: &mut SlackOutput) {
    if !out.text.is_empty() && !out.text.ends_with('\n') {
        out.push("\n");
    }
}

fn slack_ensure_blank_line(out: &mut SlackOutput) {
    if out.text.is_empty() {
        return;
    }
    while !out.text.ends_with("\n\n") {
        out.push("\n");
    }
}

fn slack_ensure_block_start(out: &mut SlackOutput, item_stack: &[bool]) {
    if !matches!(item_stack.last(), Some(false)) {
        slack_ensure_blank_line(out);
    }
}

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
    fn renders_slack_bold_strikethrough_and_headings() {
        let mrkdwn = to_slack_mrkdwn("# Title\n\n**Bold** and ~~gone~~");
        assert_eq!(mrkdwn, "*Title*\n\n*Bold* and ~gone~");
    }

    #[test]
    fn renders_slack_links_and_escapes_control_characters() {
        let mrkdwn =
            to_slack_mrkdwn("[Push & docs](https://example.com/a|b?a=1&b=2) <unsafe> & text");
        assert_eq!(
            mrkdwn,
            "<https://example.com/a%7Cb?a=1&amp;b=2|Push &amp; docs> &lt;unsafe&gt; &amp; text"
        );
    }

    #[test]
    fn renders_slack_blockquotes_and_lists() {
        let mrkdwn = to_slack_mrkdwn("> quoted\n> twice\n\n- one\n- two\n\n1. first\n2. second");
        assert_eq!(
            mrkdwn,
            "> quoted\n> twice\n\n• one\n• two\n\n1. first\n2. second"
        );
    }

    #[test]
    fn preserves_slack_inline_and_fenced_code_contents() {
        let mrkdwn = to_slack_mrkdwn("Use `**raw** & <tag>`.\n\n```rust\n**raw** & <tag>\n```");
        assert_eq!(
            mrkdwn,
            "Use `**raw** &amp; &lt;tag&gt;`.\n\n```\n**raw** &amp; &lt;tag&gt;\n```"
        );
    }

    #[test]
    fn keeps_backticks_in_code_spans_without_creating_a_block() {
        let mrkdwn = to_slack_mrkdwn("Use ``a ` b`` here.");
        assert_eq!(mrkdwn, "Use a `\u{200B} b here.");
    }

    #[test]
    fn preserves_triple_backticks_inside_slack_code() {
        let mrkdwn = to_slack_mrkdwn("````\na ``` b\n````");
        assert_eq!(mrkdwn, "```\na ``\u{200B}` b\n```");
    }

    #[test]
    fn keeps_escaped_markdown_markers_literal_for_slack() {
        let mrkdwn = to_slack_mrkdwn(r"\*literal\* and \_plain\_");
        assert_eq!(
            mrkdwn,
            "*\u{200B}literal*\u{200B} and _\u{200B}plain_\u{200B}"
        );
    }

    #[test]
    fn renders_mixed_slack_formatting() {
        let mrkdwn = to_slack_mrkdwn(
            "## **Title:**\n\n> Read [the *guide*](https://example.com).\n\n- `code`",
        );
        assert_eq!(
            mrkdwn,
            "*Title:*\n\n> Read <https://example.com|the _guide_>.\n\n• `code`"
        );
    }

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
