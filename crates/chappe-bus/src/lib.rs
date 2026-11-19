//! chappe-bus: a message log with rules, the registry of agent sessions, and the
//! owner's page. Design: `docs/architecture/agent-coordination.md`.

pub mod api;
pub mod auth;
pub mod chat;
pub mod config;
pub mod forge;
pub mod hooks;
pub mod html;
pub mod links;
pub mod message;
pub mod messaging;
pub mod pages;
pub mod registry;
pub mod store;
pub mod wakes;
pub mod web;
