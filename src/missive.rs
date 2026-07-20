//! Missive REST API input and output using outbound-only polling.

use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use reqwest::{Client, Response, StatusCode};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::Mutex as AsyncMutex;

use crate::channel::RawMessage;

const API_BASE: &str = "https://public.missiveapp.com/v1";
const MAX_POST_CHARS: usize = 8_000;
const COMMENTS_PER_PAGE: usize = 10;
const MAX_PAGES_PER_POLL: usize = 100;

#[derive(Clone)]
pub struct Missive {
    token: String,
    conversation_ids: Vec<String>,
    conversation_set: HashSet<String>,
    allow_user_ids: HashSet<String>,
    inbox: Arc<Mutex<Inbox>>,
    client: Client,
    api_base: String,
    request_gate: Arc<AsyncMutex<Option<Instant>>>,
    request_interval: Duration,
}

struct Inbox {
    connection: Connection,
    path: String,
}

#[derive(Debug, Deserialize)]
struct CommentsResponse {
    #[serde(default)]
    comments: Vec<Comment>,
}

#[derive(Debug, Deserialize)]
struct Comment {
    id: String,
    #[serde(default)]
    body: String,
    created_at: i64,
    author: Option<Author>,
}

#[derive(Debug, Deserialize)]
struct Author {
    id: String,
}

impl Missive {
    pub fn new(
        token: String,
        conversation_ids: Vec<String>,
        allow_user_ids: Vec<String>,
        state_path: &str,
    ) -> Result<Self> {
        let inbox_path = format!("{state_path}.missive-inbox.db");
        Self::with_api_base(
            token,
            conversation_ids,
            allow_user_ids,
            &inbox_path,
            API_BASE.to_string(),
        )
    }

    fn with_api_base(
        token: String,
        conversation_ids: Vec<String>,
        allow_user_ids: Vec<String>,
        inbox_path: &str,
        api_base: String,
    ) -> Result<Self> {
        let conversation_ids = conversation_ids
            .into_iter()
            .map(|id| id.trim().to_string())
            .collect::<Vec<_>>();
        Ok(Self {
            token,
            conversation_set: conversation_ids.iter().cloned().collect(),
            conversation_ids,
            allow_user_ids: allow_user_ids
                .into_iter()
                .map(|id| id.trim().to_string())
                .collect(),
            inbox: Arc::new(Mutex::new(Inbox::open(inbox_path)?)),
            client: Client::builder()
                .timeout(Duration::from_secs(25))
                .build()
                .context("build Missive HTTP client")?,
            api_base,
            request_gate: Arc::new(AsyncMutex::new(None)),
            request_interval: if cfg!(test) {
                Duration::ZERO
            } else {
                Duration::from_secs(1)
            },
        })
    }

    pub fn allows_user(&self, user: &str) -> bool {
        self.allow_user_ids.contains(user)
    }

    pub fn allows_conversation(&self, conversation: &str) -> bool {
        self.conversation_set.contains(conversation)
    }

    pub async fn poll(&self, since: i64) -> Result<Vec<RawMessage>> {
        self.refresh(true).await?;
        self.inbox.lock().unwrap().after(since)
    }

    pub async fn latest_cursor(&self) -> Result<i64> {
        // Seed the local deduplication inbox before returning the cursor so a
        // first run never replays existing Missive history.
        self.refresh(false).await?;
        self.inbox.lock().unwrap().latest_cursor()
    }

    pub async fn send_message(&self, conversation: &str, text: &str) -> Result<()> {
        if !self.allows_conversation(conversation) {
            bail!("Missive delivery conversation is not allowlisted");
        }
        let body = json!({
            "posts": {
                "conversation": conversation,
                "markdown": text,
                "username": "Push",
                "notification": {
                    "title": "Push",
                    "body": "Assistant replied"
                }
            }
        });
        let url = format!("{}/posts", self.api_base.trim_end_matches('/'));
        let response = self
            .send_with_retry(|| self.client.post(&url).bearer_auth(&self.token).json(&body))
            .await
            .context("create Missive post")?;
        if !response.status().is_success() {
            bail!("Missive create post failed with HTTP {}", response.status());
        }
        Ok(())
    }

    async fn refresh(&self, paginate: bool) -> Result<()> {
        for conversation in &self.conversation_ids {
            self.refresh_conversation(conversation, paginate).await?;
        }
        Ok(())
    }

    async fn refresh_conversation(&self, conversation: &str, paginate: bool) -> Result<()> {
        let mut until = None;
        let mut pending = Vec::new();
        let mut complete = false;
        for _ in 0..MAX_PAGES_PER_POLL {
            let response = self.fetch_comments(conversation, until).await?;
            if response.comments.is_empty() {
                complete = true;
                break;
            }
            let page_len = response.comments.len();
            let mut reached_known_event = false;
            let mut oldest = i64::MAX;
            let mut newest = i64::MIN;
            for comment in response.comments {
                oldest = oldest.min(comment.created_at);
                newest = newest.max(comment.created_at);
                let known = self.inbox.lock().unwrap().contains(&comment.id)?;
                reached_known_event |= known;
                if !known {
                    pending.push(comment);
                }
            }
            if !paginate || reached_known_event || page_len < COMMENTS_PER_PAGE || oldest == newest
            {
                complete = true;
                break;
            }
            until = Some(oldest);
        }
        if !complete {
            bail!("Missive comment pagination exceeded the per-poll safety limit");
        }

        pending.sort_by_key(|comment| comment.created_at);
        let mut inbox = self.inbox.lock().unwrap();
        for comment in pending {
            let author = comment
                .author
                .as_ref()
                .map(|author| author.id.as_str())
                .unwrap_or_default();
            let allowed = self.allows_user(author);
            let text = if allowed { comment.body.as_str() } else { "" };
            inbox.insert(
                &comment.id,
                conversation,
                author,
                text,
                !comment.body.trim().is_empty() && !author.is_empty(),
            )?;
        }
        Ok(())
    }

    async fn fetch_comments(
        &self,
        conversation: &str,
        until: Option<i64>,
    ) -> Result<CommentsResponse> {
        let url = format!(
            "{}/conversations/{conversation}/comments",
            self.api_base.trim_end_matches('/')
        );
        let response = self
            .send_with_retry(|| {
                let request = self.client.get(&url).bearer_auth(&self.token);
                match until {
                    Some(value) => request.query(&[("until", value)]),
                    None => request,
                }
            })
            .await
            .context("list Missive comments")?;
        let status = response.status();
        if !status.is_success() {
            bail!("Missive list comments failed with HTTP {status}");
        }
        response
            .json()
            .await
            .with_context(|| format!("decode Missive comments response ({status})"))
    }

    async fn send_with_retry(
        &self,
        request: impl Fn() -> reqwest::RequestBuilder,
    ) -> Result<Response> {
        let mut previous = self.request_gate.lock().await;
        if let Some(last_request) = *previous {
            let remaining = self.request_interval.saturating_sub(last_request.elapsed());
            if !remaining.is_zero() {
                tokio::time::sleep(remaining).await;
            }
        }
        let mut response = request().send().await.context("call Missive API")?;
        *previous = Some(Instant::now());
        if response.status() == StatusCode::TOO_MANY_REQUESTS {
            tokio::time::sleep(retry_after(response.headers())).await;
            response = request().send().await.context("retry Missive API")?;
            *previous = Some(Instant::now());
        }
        Ok(response)
    }
}

impl Inbox {
    fn open(path: &str) -> Result<Self> {
        if let Some(parent) = Path::new(path).parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create Missive inbox directory {}", parent.display()))?;
        }
        let connection =
            Connection::open(path).with_context(|| format!("open Missive inbox {path}"))?;
        crate::util::restrict_permissions(Path::new(path), false)
            .with_context(|| format!("restrict Missive inbox permissions {path}"))?;
        connection
            .busy_timeout(Duration::from_secs(5))
            .context("configure Missive inbox busy timeout")?;
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS missive_comments (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                comment_id TEXT NOT NULL UNIQUE,
                conversation_id TEXT NOT NULL,
                user_id TEXT NOT NULL,
                text TEXT NOT NULL,
                is_supported INTEGER NOT NULL
            );",
        )?;
        Ok(Self {
            connection,
            path: path.to_string(),
        })
    }

    fn contains(&self, comment_id: &str) -> Result<bool> {
        self.connection
            .query_row(
                "SELECT 1 FROM missive_comments WHERE comment_id = ?1",
                [comment_id],
                |_| Ok(()),
            )
            .optional()
            .map(|value| value.is_some())
            .context("check Missive comment deduplication state")
    }

    fn insert(
        &mut self,
        comment_id: &str,
        conversation_id: &str,
        user_id: &str,
        text: &str,
        is_supported: bool,
    ) -> Result<()> {
        self.connection.execute(
            "INSERT INTO missive_comments (
                comment_id, conversation_id, user_id, text, is_supported
             ) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(comment_id) DO NOTHING",
            params![comment_id, conversation_id, user_id, text, is_supported],
        )?;
        Ok(())
    }

    fn latest_cursor(&self) -> Result<i64> {
        self.connection
            .query_row("SELECT MAX(id) FROM missive_comments", [], |row| row.get(0))
            .optional()?
            .flatten()
            .map_or(Ok(0), Ok)
    }

    fn after(&self, since: i64) -> Result<Vec<RawMessage>> {
        let mut statement = self.connection.prepare(
            "SELECT id, comment_id, conversation_id, user_id, text, is_supported
             FROM missive_comments WHERE id > ?1 ORDER BY id",
        )?;
        let messages = statement
            .query_map([since], |row| {
                Ok(RawMessage {
                    row_id: row.get(0)?,
                    provider_event_id: Some(row.get(1)?),
                    channel: "missive",
                    chat_identifier: row.get(2)?,
                    handle: row.get(3)?,
                    text: row.get(4)?,
                    is_supported: row.get(5)?,
                    is_group: false,
                    voice: None,
                    is_from_me: false,
                    thread_id: None,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()
            .with_context(|| format!("read pending Missive inbox events from {}", self.path))?;
        Ok(messages)
    }
}

fn retry_after(headers: &reqwest::header::HeaderMap) -> Duration {
    let seconds = headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(1);
    Duration::from_secs(seconds)
}

pub fn split_text(text: &str) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    for character in text.chars() {
        if current.chars().count() == MAX_POST_CHARS {
            chunks.push(std::mem::take(&mut current));
        }
        current.push(character);
    }
    if !current.is_empty() || chunks.is_empty() {
        chunks.push(current);
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn server(responses: Vec<String>) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        let counter = Arc::new(AtomicUsize::new(0));
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let mut buffer = vec![0; 16_384];
                let size = stream.read(&mut buffer).await.unwrap();
                captured
                    .lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&buffer[..size]).to_string());
                let index = counter.fetch_add(1, Ordering::SeqCst);
                let response = responses.get(index).or_else(|| responses.last()).unwrap();
                stream.write_all(response.as_bytes()).await.unwrap();
            }
        });
        (format!("http://{address}/v1"), requests)
    }

    fn http_json(status: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn rate_limited() -> String {
        "HTTP/1.1 429 Too Many Requests\r\nretry-after: 0\r\ncontent-type: application/json\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}".to_string()
    }

    fn client(api_base: String) -> Missive {
        let inbox =
            std::env::temp_dir().join(format!("push-missive-test-{}.db", uuid::Uuid::new_v4()));
        Missive::with_api_base(
            "secret-token".to_string(),
            vec!["conv-1".to_string()],
            vec!["user-1".to_string()],
            inbox.to_str().unwrap(),
            api_base,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn polls_deduplicates_and_redacts_unallowlisted_comments() {
        let body = r#"{"comments":[{"id":"c2","body":"private","created_at":2,"author":{"id":"user-2"}},{"id":"c1","body":"hello","created_at":1,"author":{"id":"user-1"}}]}"#;
        let (api, _) = server(vec![http_json("200 OK", body)]).await;
        let missive = client(api);

        let messages = missive.poll(0).await.unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].text, "hello");
        assert_eq!(messages[1].text, "");
        assert_eq!(messages[1].handle, "user-2");
        let cursor = messages.last().unwrap().row_id;
        assert!(missive.poll(cursor).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn first_cursor_skips_existing_history() {
        let body =
            r#"{"comments":[{"id":"c1","body":"old","created_at":1,"author":{"id":"user-1"}}]}"#;
        let (api, _) = server(vec![http_json("200 OK", body)]).await;
        let missive = client(api);

        let cursor = missive.latest_cursor().await.unwrap();
        assert!(cursor > 0);
        assert!(missive.poll(cursor).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn pagination_uses_the_oldest_timestamp_and_preserves_chronology() {
        let comments = (2..=11)
            .rev()
            .map(|created_at| {
                format!(
                    r#"{{"id":"c{created_at}","body":"{created_at}","created_at":{created_at},"author":{{"id":"user-1"}}}}"#
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let first = format!(r#"{{"comments":[{comments}]}}"#);
        let second =
            r#"{"comments":[{"id":"c1","body":"1","created_at":1,"author":{"id":"user-1"}}]}"#;
        let (api, requests) = server(vec![
            http_json("200 OK", &first),
            http_json("200 OK", second),
        ])
        .await;
        let missive = client(api);

        let messages = missive.poll(0).await.unwrap();
        assert_eq!(messages.len(), 11);
        assert_eq!(messages.first().unwrap().text, "1");
        assert_eq!(messages.last().unwrap().text, "11");
        assert!(requests.lock().unwrap()[1]
            .starts_with("GET /v1/conversations/conv-1/comments?until=2 HTTP/1.1"));
    }

    #[tokio::test]
    async fn posts_wrapped_markdown_to_the_exact_allowlisted_conversation() {
        let (api, requests) = server(vec![http_json("201 Created", "{}")]).await;
        let missive = client(api);

        missive.send_message("conv-1", "**hello**").await.unwrap();
        let request = &requests.lock().unwrap()[0];
        assert!(request.starts_with("POST /v1/posts HTTP/1.1"));
        assert!(request.contains("authorization: Bearer secret-token"));
        assert!(request.contains(r#""conversation":"conv-1""#));
        assert!(request.contains(r#""markdown":"**hello**""#));
        assert!(!request.contains("secret-token\""));
    }

    #[tokio::test]
    async fn retries_one_rate_limited_request_without_exposing_the_response() {
        let (api, requests) = server(vec![
            rate_limited(),
            http_json("201 Created", r#"{"posts":{"id":"post-1"}}"#),
        ])
        .await;
        let missive = client(api);

        missive.send_message("conv-1", "hello").await.unwrap();

        assert_eq!(requests.lock().unwrap().len(), 2);
    }

    #[test]
    fn splitting_is_unicode_safe_and_bounded() {
        let chunks = split_text(&format!("{}🦀", "a".repeat(MAX_POST_CHARS)));
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].chars().count(), MAX_POST_CHARS);
        assert_eq!(chunks[1], "🦀");
    }
}
