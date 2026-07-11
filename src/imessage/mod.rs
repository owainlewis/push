//! Reading from and sending to the macOS Messages app.

mod attributed_body;
mod poller;
mod sender;

pub use poller::Poller;
pub use sender::Sender;
