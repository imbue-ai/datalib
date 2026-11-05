//! Per-method response-shape knowledge for raw-API capture.

pub const M_AUTH_TEST: &str = "auth.test";
pub const M_CHANNELS: &str = "conversations.list";
pub const M_USERS: &str = "users.list";
pub const M_USER_INFO: &str = "users.info";
pub const M_HISTORY: &str = "conversations.history";
pub const M_REPLIES: &str = "conversations.replies";
/// Not in Slack's published API: the web client's own call, which a
/// browser-session token may make. One call answers every conversation.
pub const M_COUNTS: &str = "client.counts";
pub const M_BOOKMARKS: &str = "bookmarks.list";
/// Also the web client's own: "Saved for later".
pub const M_SAVED: &str = "saved.list";
/// Answers only a token that may search, which a browser session's is.
pub const M_SEARCH: &str = "search.messages";
