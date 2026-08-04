//! Reading from and sending to the macOS Messages app.

mod attachments;
mod attributed_body;
mod poller;
mod sender;

pub use attachments::{download as download_image, needs_conversion, Attachment};
pub use poller::Poller;
pub use sender::Sender;
