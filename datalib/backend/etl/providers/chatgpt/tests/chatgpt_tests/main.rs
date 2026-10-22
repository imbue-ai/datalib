//! Every hermetic ChatGPT test, one binary: each module is one
//! behaviour.

mod branched_chat;
mod chatgpt_render;
mod chatgpt_translate;
mod export_then_api;
mod incremental_skip;
mod interrupt;
mod live;
mod playback_roundtrip;
mod run_problems;
mod token_renewal;
mod upgrade;
mod volatile_metadata;
