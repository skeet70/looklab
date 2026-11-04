// SPDX-License-Identifier: AGPL-3.0-or-later

mod bodies;
mod body;
mod compare;
mod landing;
mod log;
mod look;
mod looks;
mod misc;
mod privacy;

pub use bodies::BodiesPage;
pub use body::BodyDetail;
pub use compare::ComparePage;
pub use landing::Landing;
pub use log::LogPage;
pub use look::LookDetail;
pub use looks::LooksPage;
pub use misc::{MatchPlaceholder, NotFound};
pub use privacy::PrivacyPage;
