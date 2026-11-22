// SPDX-License-Identifier: GPL-3.0-or-later
//! patchbay-notify: the PBX bot core, independent of Asterisk and of the web framework.
//!
//! - [`event`]: business events from the phone system (call.incoming, call.answered, …)
//! - [`bot`]: message lifecycle, routing, linking, commands and button actions
//! - [`pbx`]: what the bot needs from the phone system (implemented by patchbay-web)
//! - [`platform`]: the `ChatPlatform` trait; adapters: [`mattermost`] (complete),
//!   [`telegram`] and [`matrix`] (stubs)
//! - [`model`]: platform-neutral messages and button actions

pub mod bot;
pub mod commands;
pub mod event;
pub mod i18n;
pub mod matrix;
pub mod mattermost;
pub mod model;
pub mod pbx;
pub mod platform;
pub mod telegram;

pub use async_trait::async_trait;
pub use bot::{Bot, BotConfig, LinkMode};
pub use event::PbxEvent;
pub use platform::{ChatPlatform, InboundHandler};
