//! The `period_key` of the one document that holds a whole chat. Every
//! chat source renders a chat whole; chat-common still reads the key to
//! leave the title without a period suffix.

pub struct Period;

impl Period {
    pub const fn key_for_all() -> &'static str {
        "all"
    }
}
