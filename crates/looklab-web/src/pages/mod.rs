// SPDX-License-Identifier: AGPL-3.0-or-later

mod bodies;
mod body;
mod compare;
pub(crate) mod contribute;
mod landing;
mod log;
mod look;
mod looks;
pub(crate) mod match_page;
mod misc;
mod modqueue;
mod passkeys;
mod pattern;
mod privacy;
mod proposal;
mod propose;
mod status;
mod verify;

pub use bodies::BodiesPage;
pub use body::BodyDetail;
pub use compare::ComparePage;
pub use contribute::ContributePage;
pub use landing::Landing;
pub use log::LogPage;
pub use look::LookDetail;
pub use looks::LooksPage;
pub use match_page::{MatchPage, MatchResultPage};
pub use misc::NotFound;
pub use modqueue::ModQueuePage;
pub use passkeys::PasskeysPage;
pub use pattern::PatternPage;
pub use privacy::PrivacyPage;
pub use proposal::ProposalDetail;
pub use propose::ProposePage;
pub use status::StatusPage;
pub use verify::VerifyPage;
