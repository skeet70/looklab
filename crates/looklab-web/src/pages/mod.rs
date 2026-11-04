// SPDX-License-Identifier: AGPL-3.0-or-later

mod bodies;
mod body;
mod compare;
mod landing;
mod log;
mod look;
mod looks;
mod misc;
mod modqueue;
mod privacy;
mod proposal;
mod propose;

pub use bodies::BodiesPage;
pub use body::BodyDetail;
pub use compare::ComparePage;
pub use landing::Landing;
pub use log::LogPage;
pub use look::LookDetail;
pub use looks::LooksPage;
pub use misc::{MatchPlaceholder, NotFound};
pub use modqueue::ModQueuePage;
pub use privacy::PrivacyPage;
pub use proposal::ProposalDetail;
pub use propose::ProposePage;
